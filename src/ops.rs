//! Write operations: the verbs from app-spec §5, plus demo seeding.
//!
//! Each function takes `&Connection` and returns `Result<()>` (or an id). Multi-row
//! operations run inside a transaction so a failure can't leave a half-stamped month.

use crate::models::*;
use crate::money::Money;
use crate::queries;
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Mint a fresh random id. UUIDs (not autoincrement) keep the file portable and let a
/// future multi-device sync merge without primary-key collisions.
fn new_id() -> String {
    Uuid::new_v4().to_string()
}

// --- Stamping (§5) -------------------------------------------------------------

/// Copy a plan's entries into concrete instances for a new month, then sever the link:
/// the month is an independent snapshot from here on. Returns the new month id.
pub fn stamp(
    conn: &mut Connection,
    plan_id: &str,
    label: &str,
    start_date: NaiveDate,
    days_in_month: i64,
) -> Result<String> {
    // A transaction: everything below commits together or not at all.
    let tx = conn.transaction()?;
    let month_id = new_id();

    tx.execute(
        "INSERT INTO month (id, plan_id, label, start_date, days_in_month)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            month_id,
            plan_id,
            label,
            start_date.format("%Y-%m-%d").to_string(),
            days_in_month
        ],
    )?;

    // `&tx` coerces to `&Connection`, so query/insert helpers run inside this transaction.
    for entry in queries::load_plan_entries(&tx, plan_id)? {
        insert_instance_from_entry(&tx, &month_id, &entry)?;
    }

    tx.commit()?;
    Ok(month_id)
}

/// Insert one fresh instance (envelope or standalone txn) for a plan entry into a month.
/// The intrinsic fields come from the shared `series`; the amount from this plan's item.
/// `stamped_amount == amount` at stamp time. Shared by fresh stamp, merge, and replace.
fn insert_instance_from_entry(conn: &Connection, month_id: &str, entry: &PlanEntry) -> Result<()> {
    let s = &entry.series;
    match s.kind {
        Kind::Envelope => {
            conn.execute(
                "INSERT INTO envelope
                   (id, month_id, series_id, label, category, amount_cents,
                    stamped_amount_cents, period_type, mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    new_id(),
                    month_id,
                    s.id, // series_id = the durable series identity
                    s.label,
                    s.category,
                    entry.amount,
                    entry.amount,
                    s.period_type.unwrap_or(PeriodType::Monthly),
                    s.mode,
                ],
            )?;
        }
        Kind::Transaction => {
            conn.execute(
                "INSERT INTO txn
                   (id, month_id, series_id, label, category, direction,
                    amount_cents, stamped_amount_cents, settled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                rusqlite::params![
                    new_id(),
                    month_id,
                    s.id,
                    s.label,
                    s.category,
                    s.direction.unwrap_or(Direction::Out),
                    entry.amount,
                    entry.amount,
                ],
            )?;
        }
    }
    Ok(())
}

// --- Restamp: Merge / Replace --------------------------------------------------

/// Does this month contain hand-entered data — standalone one-offs OR manual-envelope
/// spending? Both have `series_id IS NULL` (they didn't come from a plan). Drives whether
/// Replace stops to ask before wiping.
pub fn month_has_handentered(conn: &Connection, month_id: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM txn WHERE month_id = ?1 AND series_id IS NULL",
        [month_id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Merge a plan into an existing month: additive, refreshing the planned baseline.
/// - A matching **unsettled** instance is refreshed to the plan's values.
/// - A matching **settled** transaction is left untouched (it holds a real actual).
/// - A plan entry with no instance is inserted.
/// Nothing is ever deleted. Works with ANY plan because matching is by shared `series_id`.
pub fn restamp_merge(conn: &mut Connection, month_id: &str, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    for entry in queries::load_plan_entries(&tx, plan_id)? {
        match entry.series.kind {
            Kind::Envelope => match find_month_envelope(&tx, month_id, &entry.series.id)? {
                Some(id) => refresh_envelope(&tx, &id, &entry)?,
                None => insert_instance_from_entry(&tx, month_id, &entry)?,
            },
            Kind::Transaction => match find_month_txn(&tx, month_id, &entry.series.id)? {
                Some((id, settled)) => {
                    if !settled {
                        refresh_txn(&tx, &id, &entry)?;
                    }
                }
                None => insert_instance_from_entry(&tx, month_id, &entry)?,
            },
        }
    }
    tx.execute("UPDATE month SET plan_id = ?1 WHERE id = ?2", rusqlite::params![plan_id, month_id])?;
    tx.commit()?;
    Ok(())
}

/// Replace a month from a plan (clean slate). Plan-derived instances are reset in place
/// (amount & stamped reset, unsettled, coded fields refreshed) — resetting rather than
/// delete+recreate keeps instance ids stable so manual-envelope spending stays linked.
/// Plan-derived instances no longer in the plan are removed. Hand-entered data (one-offs
/// and manual spending) is wiped unless `keep_handentered`.
pub fn restamp_replace(
    conn: &mut Connection,
    month_id: &str,
    plan_id: &str,
    keep_handentered: bool,
) -> Result<()> {
    let tx = conn.transaction()?;
    let entries = queries::load_plan_entries(&tx, plan_id)?;
    let entry_by_series: HashMap<&str, &PlanEntry> =
        entries.iter().map(|e| (e.series.id.as_str(), e)).collect();

    let txns = queries::load_txns(&tx, month_id)?;
    let envelopes = queries::load_envelopes(&tx, month_id)?;
    let mut seen: HashSet<String> = HashSet::new();

    // Transactions.
    for t in &txns {
        match t.series_id.as_deref() {
            None => {
                // Hand-entered (one-off or manual-envelope spending).
                if !keep_handentered {
                    tx.execute("DELETE FROM txn WHERE id = ?1", [&t.id])?;
                }
            }
            Some(sid) => {
                if let Some(entry) = entry_by_series.get(sid) {
                    refresh_txn(&tx, &t.id, entry)?; // reset in place (unsettles it)
                    seen.insert(sid.to_string());
                } else {
                    // Plan-derived but removed from the plan.
                    tx.execute("DELETE FROM txn WHERE id = ?1", [&t.id])?;
                }
            }
        }
    }

    // Envelopes (always plan-derived — series_id NOT NULL).
    for e in &envelopes {
        if let Some(entry) = entry_by_series.get(e.series_id.as_str()) {
            refresh_envelope(&tx, &e.id, entry)?;
            seen.insert(e.series_id.clone());
        } else {
            // Removed from the plan: detach any surviving manual spending to standalone
            // (so kept hand-entered data isn't orphaned by the FK), then drop the envelope.
            tx.execute("UPDATE txn SET envelope_id = NULL WHERE envelope_id = ?1", [&e.id])?;
            tx.execute("DELETE FROM envelope WHERE id = ?1", [&e.id])?;
        }
    }

    // Plan entries with no existing instance → insert fresh.
    for entry in &entries {
        if !seen.contains(&entry.series.id) {
            insert_instance_from_entry(&tx, month_id, entry)?;
        }
    }

    tx.execute("UPDATE month SET plan_id = ?1 WHERE id = ?2", rusqlite::params![plan_id, month_id])?;
    tx.commit()?;
    Ok(())
}

/// Find a month's envelope instance for a series, if present.
fn find_month_envelope(conn: &Connection, month_id: &str, series_id: &str) -> Result<Option<String>> {
    let id = conn
        .query_row(
            "SELECT id FROM envelope WHERE month_id = ?1 AND series_id = ?2 LIMIT 1",
            rusqlite::params![month_id, series_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(id)
}

/// Find a month's standalone txn instance for a series, returning its id and settled flag.
fn find_month_txn(conn: &Connection, month_id: &str, series_id: &str) -> Result<Option<(String, bool)>> {
    let row = conn
        .query_row(
            "SELECT id, settled FROM txn WHERE month_id = ?1 AND series_id = ?2 LIMIT 1",
            rusqlite::params![month_id, series_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
        )
        .optional()?;
    Ok(row)
}

/// Reset an envelope instance to a plan entry's planned values.
fn refresh_envelope(conn: &Connection, id: &str, entry: &PlanEntry) -> Result<()> {
    let s = &entry.series;
    conn.execute(
        "UPDATE envelope
         SET amount_cents = ?1, stamped_amount_cents = ?2, label = ?3, category = ?4,
             period_type = ?5, mode = ?6
         WHERE id = ?7",
        rusqlite::params![
            entry.amount,
            entry.amount,
            s.label,
            s.category,
            s.period_type.unwrap_or(PeriodType::Monthly),
            s.mode,
            id
        ],
    )?;
    Ok(())
}

/// Reset a standalone txn instance to a plan entry's planned values, unsettling it.
fn refresh_txn(conn: &Connection, id: &str, entry: &PlanEntry) -> Result<()> {
    let s = &entry.series;
    conn.execute(
        "UPDATE txn
         SET amount_cents = ?1, stamped_amount_cents = ?2, label = ?3, category = ?4,
             direction = ?5, settled = 0, date_paid = NULL
         WHERE id = ?6",
        rusqlite::params![
            entry.amount,
            entry.amount,
            s.label,
            s.category,
            s.direction.unwrap_or(Direction::Out),
            id
        ],
    )?;
    Ok(())
}

// --- Mark paid / un-mark (§5) --------------------------------------------------

/// Mark a transaction settled at `actual` (prefilled with its current amount, editable).
/// Because `amount_cents` is NOT NULL, "marking paid requires an amount" holds by
/// construction — there's never a null to chase.
pub fn mark_paid(conn: &Connection, txn_id: &str, actual: Money, date_paid: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE txn SET amount_cents = ?1, settled = 1, date_paid = ?2 WHERE id = ?3",
        rusqlite::params![actual, date_paid, txn_id],
    )?;
    Ok(())
}

/// Un-settle a transaction. `revert_to_planned` chooses whether to restore the stamped
/// value or keep whatever real figure was entered (default: keep — never silently lose
/// a real number).
pub fn unmark_paid(conn: &Connection, txn_id: &str, revert_to_planned: bool) -> Result<()> {
    if revert_to_planned {
        // COALESCE guards one-offs, whose stamped_amount is NULL: keep current amount.
        conn.execute(
            "UPDATE txn
             SET settled = 0,
                 amount_cents = COALESCE(stamped_amount_cents, amount_cents),
                 date_paid = NULL
             WHERE id = ?1",
            [txn_id],
        )?;
    } else {
        conn.execute(
            "UPDATE txn SET settled = 0, date_paid = NULL WHERE id = ?1",
            [txn_id],
        )?;
    }
    Ok(())
}

/// Flip a transaction's settled flag. Used by the dashboard's one-key toggle: settling
/// keeps the prefilled amount; un-settling keeps the entered figure (no revert).
pub fn toggle_settled(conn: &Connection, txn_id: &str, currently_settled: bool) -> Result<()> {
    if currently_settled {
        unmark_paid(conn, txn_id, false)
    } else {
        conn.execute("UPDATE txn SET settled = 1 WHERE id = ?1", [txn_id])?;
        Ok(())
    }
}

/// Update a checking account's ground-truth balance (entered by hand).
pub fn set_balance(conn: &Connection, account_id: &str, balance: Money) -> Result<()> {
    conn.execute(
        "UPDATE account SET balance_cents = ?1 WHERE id = ?2",
        rusqlite::params![balance, account_id],
    )?;
    Ok(())
}

/// Set a credit card's total limit. Owed is derived (`limit − available`).
pub fn set_credit_limit(conn: &Connection, account_id: &str, limit: Money) -> Result<()> {
    conn.execute(
        "UPDATE account SET credit_limit_cents = ?1 WHERE id = ?2",
        rusqlite::params![limit, account_id],
    )?;
    Ok(())
}

/// Set a credit card's available credit (the frequently-updated figure). Owed is derived.
pub fn set_available_credit(conn: &Connection, account_id: &str, available: Money) -> Result<()> {
    conn.execute(
        "UPDATE account SET available_credit_cents = ?1 WHERE id = ?2",
        rusqlite::params![available, account_id],
    )?;
    Ok(())
}

// --- Plan management -----------------------------------------------------------

/// Create an empty plan and return its id.
pub fn create_plan(conn: &Connection, name: &str) -> Result<String> {
    let id = new_id();
    conn.execute("INSERT INTO plan (id, name) VALUES (?1, ?2)", rusqlite::params![id, name])?;
    Ok(id)
}

/// Rename a plan. Labels are cosmetic — no instance references a plan's name.
pub fn rename_plan(conn: &Connection, plan_id: &str, name: &str) -> Result<()> {
    conn.execute("UPDATE plan SET name = ?1 WHERE id = ?2", rusqlite::params![name, plan_id])?;
    Ok(())
}

/// Delete a plan and its items. Already-stamped months are untouched: `month.plan_id` is
/// a plain record with no foreign key, and each instance carries a *copied* `series_id`,
/// so the snapshot stands on its own (app-spec §2). We delete the child `plan_item` rows
/// first inside a transaction because they DO have a live foreign key to `plan`.
pub fn delete_plan(conn: &mut Connection, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM plan_item WHERE plan_id = ?1", [plan_id])?;
    tx.execute("DELETE FROM plan WHERE id = ?1", [plan_id])?;
    tx.commit()?;
    Ok(())
}

/// Create a new series (a durable recurring-item definition). Category starts NULL.
pub fn create_series(
    conn: &Connection,
    kind: Kind,
    label: &str,
    direction: Option<Direction>,
    period_type: Option<PeriodType>,
    mode: Option<Mode>,
) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO series (id, kind, label, direction, period_type, mode)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, kind, label, direction, period_type, mode],
    )?;
    Ok(id)
}

/// Add an existing series to a plan at $0 (the per-plan budgeted amount, edited later).
/// Returns the new plan_item id.
pub fn add_plan_item(conn: &Connection, plan_id: &str, series_id: &str, amount: Money) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO plan_item (id, plan_id, series_id, amount_cents) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, plan_id, series_id, amount],
    )?;
    Ok(id)
}

/// Create a brand-new transaction series (label "New bill", outgoing) and add it to the
/// plan. Returns the plan_item id so the caller can select it for editing.
pub fn add_new_transaction(conn: &Connection, plan_id: &str) -> Result<String> {
    let series_id = create_series(conn, Kind::Transaction, "New bill", Some(Direction::Out), None, None)?;
    add_plan_item(conn, plan_id, &series_id, Money::ZERO)
}

/// Create a brand-new envelope series (label "New envelope", monthly, mode inherited) and
/// add it to the plan. Returns the plan_item id.
pub fn add_new_envelope(conn: &Connection, plan_id: &str) -> Result<String> {
    let series_id = create_series(conn, Kind::Envelope, "New envelope", None, Some(PeriodType::Monthly), None)?;
    add_plan_item(conn, plan_id, &series_id, Money::ZERO)
}

/// Add an existing series to a plan (the reuse picker). Returns the plan_item id.
pub fn add_existing_series(conn: &Connection, plan_id: &str, series_id: &str) -> Result<String> {
    add_plan_item(conn, plan_id, series_id, Money::ZERO)
}

/// Per-plan: set this plan's budgeted amount for an item. Edits `plan_item`, not `series`.
pub fn set_item_amount(conn: &Connection, item_id: &str, amount: Money) -> Result<()> {
    conn.execute("UPDATE plan_item SET amount_cents = ?1 WHERE id = ?2", rusqlite::params![amount, item_id])?;
    Ok(())
}

// The following edit the shared SERIES, so they affect every plan that includes it.

pub fn set_series_label(conn: &Connection, series_id: &str, label: &str) -> Result<()> {
    conn.execute("UPDATE series SET label = ?1 WHERE id = ?2", rusqlite::params![label, series_id])?;
    Ok(())
}

pub fn set_series_direction(conn: &Connection, series_id: &str, direction: Direction) -> Result<()> {
    conn.execute("UPDATE series SET direction = ?1 WHERE id = ?2", rusqlite::params![direction, series_id])?;
    Ok(())
}

/// `None` clears the column so the envelope inherits the global default mode.
pub fn set_series_mode(conn: &Connection, series_id: &str, mode: Option<Mode>) -> Result<()> {
    conn.execute("UPDATE series SET mode = ?1 WHERE id = ?2", rusqlite::params![mode, series_id])?;
    Ok(())
}

pub fn set_series_period(conn: &Connection, series_id: &str, period: PeriodType) -> Result<()> {
    conn.execute("UPDATE series SET period_type = ?1 WHERE id = ?2", rusqlite::params![period, series_id])?;
    Ok(())
}

/// Remove an item from a plan. The series survives (it may be used by other plans, and
/// past stamped instances reference its id).
pub fn delete_plan_item(conn: &Connection, item_id: &str) -> Result<()> {
    conn.execute("DELETE FROM plan_item WHERE id = ?1", [item_id])?;
    Ok(())
}

/// Delete a series, but only if no plan still references it. We guard against live *plan*
/// references, NOT against historical stamped instances: orphaning `series_id` on past
/// envelopes/txns is the accepted, intentional outcome. Those instances are self-contained
/// snapshots (they copied their fields at stamp time) and don't need the series row — so we
/// don't keep the row alive for them, and `series_id` is a soft reference rather than a live
/// FK precisely so this delete is allowed. See the `envelope` table comment in schema.sql.
pub fn delete_series(conn: &Connection, series_id: &str) -> Result<()> {
    let refs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM plan_item WHERE series_id = ?1",
        [series_id],
        |r| r.get(0),
    )?;
    if refs > 0 {
        anyhow::bail!("series is used by {refs} plan item(s); remove it from those plans first");
    }
    conn.execute("DELETE FROM series WHERE id = ?1", [series_id])?;
    Ok(())
}

// --- Demo seeding --------------------------------------------------------------

/// Number of days in the given calendar month. Computed by asking "what's day 1 of next
/// month?" and stepping back one day.
pub fn days_in_month(year: i32, month: u32) -> i64 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    let last_this = first_next.pred_opt().unwrap();
    last_this.day() as i64
}

/// If the database is empty, populate a realistic starter: a couple of accounts, one
/// plan with a few items, and this month already stamped from it. Idempotent — does
/// nothing once any month exists.
pub fn seed_demo(conn: &mut Connection) -> Result<()> {
    if queries::current_month(conn)?.is_some() {
        return Ok(()); // already has data
    }

    // Accounts. Checking holds a spendable balance; the card holds limit + available
    // (owed = 5000 − 4150 = 850).
    let checking = new_id();
    let card = new_id();
    conn.execute(
        "INSERT INTO account (id, name, type, balance_cents) VALUES (?1,?2,?3,?4)",
        rusqlite::params![checking, "Checking", AccountType::Checking.as_str(), Money::from_dollars(4200.0)],
    )?;
    conn.execute(
        "INSERT INTO account (id, name, type, balance_cents, credit_limit_cents, available_credit_cents)
         VALUES (?1,?2,?3,0,?4,?5)",
        rusqlite::params![
            card,
            "Credit Card",
            AccountType::CreditCard.as_str(),
            Money::from_dollars(5000.0),
            Money::from_dollars(4150.0)
        ],
    )?;

    // A plan and its recurring items.
    let plan_id = new_id();
    conn.execute("INSERT INTO plan (id, name) VALUES (?1, ?2)", rusqlite::params![plan_id, "Normal Month"])?;

    // Helper: create a series and immediately add it to the plan at `amount`.
    // `conn` is borrowed by each call, so this is a plain closure over plan_id.
    let add = |kind: Kind,
               label: &str,
               direction: Option<Direction>,
               amount: Money,
               period: Option<PeriodType>,
               mode: Option<Mode>|
     -> Result<()> {
        let series_id = create_series(conn, kind, label, direction, period, mode)?;
        add_plan_item(conn, &plan_id, &series_id, amount)?;
        Ok(())
    };

    add(Kind::Transaction, "Paycheck", Some(Direction::In), Money::from_dollars(5200.0), None, None)?;
    add(Kind::Transaction, "Rent", Some(Direction::Out), Money::from_dollars(1800.0), None, None)?;
    add(Kind::Transaction, "Electric", Some(Direction::Out), Money::from_dollars(140.0), None, None)?;
    add(Kind::Transaction, "Internet", Some(Direction::Out), Money::from_dollars(70.0), None, None)?;
    add(Kind::Envelope, "Groceries", None, Money::from_dollars(2000.0), Some(PeriodType::Monthly), Some(Mode::Automatic))?;
    add(Kind::Envelope, "Dining", None, Money::from_dollars(300.0), Some(PeriodType::Monthly), Some(Mode::Manual))?;

    // Stamp it for the current calendar month.
    let today = Local::now().date_naive();
    let start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
    let dim = days_in_month(today.year(), today.month());
    let label = start.format("%Y-%m").to_string();
    stamp(conn, &plan_id, &label, start, dim)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calc;
    use crate::db;

    #[test]
    fn stamp_copies_items_and_freezes_amounts() {
        let mut conn = db::open_in_memory().unwrap();
        seed_demo(&mut conn).unwrap();

        let month = queries::current_month(&conn).unwrap().unwrap();
        let txns = queries::load_txns(&conn, &month.id).unwrap();
        let envelopes = queries::load_envelopes(&conn, &month.id).unwrap();

        assert_eq!(txns.len(), 4); // paycheck, rent, electric, internet
        assert_eq!(envelopes.len(), 2); // groceries, dining

        // stamped_amount is frozen equal to amount at stamp time.
        let rent = txns.iter().find(|t| t.label == "Rent").unwrap();
        assert_eq!(rent.amount, Money::from_dollars(1800.0));
        assert_eq!(rent.stamped_amount, Some(Money::from_dollars(1800.0)));
        assert!(!rent.settled);
    }

    #[test]
    fn plan_crud_then_stamp() {
        let mut conn = db::open_in_memory().unwrap();

        // Build a plan from scratch. Create series explicitly so we know their ids.
        let plan_id = create_plan(&conn, "Tight Month").unwrap();
        let rent_series = create_series(&conn, Kind::Transaction, "Rent", Some(Direction::Out), None, None).unwrap();
        add_plan_item(&conn, &plan_id, &rent_series, Money::from_dollars(1500.0)).unwrap();
        let groc_series = create_series(&conn, Kind::Envelope, "Groceries", None, Some(PeriodType::Monthly), Some(Mode::Automatic)).unwrap();
        add_plan_item(&conn, &plan_id, &groc_series, Money::from_dollars(600.0)).unwrap();

        let entries = queries::load_plan_entries(&conn, &plan_id).unwrap();
        assert_eq!(entries.len(), 2);

        // Stamp it, then confirm the month is an independent snapshot.
        let start = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        stamp(&mut conn, &plan_id, "2026-08", start, 31).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let stamped_txns = queries::load_txns(&conn, &month.id).unwrap();
        assert_eq!(stamped_txns.len(), 1);
        assert_eq!(stamped_txns[0].amount, Money::from_dollars(1500.0));
        // series_id on the instance equals the durable series identity.
        assert_eq!(stamped_txns[0].series_id.as_deref(), Some(rent_series.as_str()));

        // Deleting the plan leaves the stamped month intact (severed link).
        delete_plan(&mut conn, &plan_id).unwrap();
        assert!(queries::get_plan(&conn, &plan_id).unwrap().is_none());
        let after = queries::load_txns(&conn, &month.id).unwrap();
        assert_eq!(after.len(), 1, "stamped month survives plan deletion");
    }

    #[test]
    fn mark_and_unmark_paid_roundtrip() {
        let mut conn = db::open_in_memory().unwrap();
        seed_demo(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let txns = queries::load_txns(&conn, &month.id).unwrap();
        let electric = txns.iter().find(|t| t.label == "Electric").unwrap();

        // Pay it at a corrected actual.
        mark_paid(&conn, &electric.id, Money::from_dollars(152.30), Some("2026-07-05")).unwrap();
        let after = queries::load_txns(&conn, &month.id).unwrap();
        let electric = after.iter().find(|t| t.label == "Electric").unwrap();
        assert!(electric.settled);
        assert_eq!(electric.amount, Money::from_dollars(152.30));
        assert_eq!(calc::txn_remaining(electric), Money::ZERO);

        // Un-mark with revert -> back to the planned $140.
        unmark_paid(&conn, &electric.id, true).unwrap();
        let after = queries::load_txns(&conn, &month.id).unwrap();
        let electric = after.iter().find(|t| t.label == "Electric").unwrap();
        assert!(!electric.settled);
        assert_eq!(electric.amount, Money::from_dollars(140.0));
    }

    #[test]
    fn cross_plan_series_continuity_on_merge() {
        let mut conn = db::open_in_memory().unwrap();
        // One shared Rent series, included in two different plans at different amounts.
        let rent = create_series(&conn, Kind::Transaction, "Rent", Some(Direction::Out), None, None).unwrap();
        let plan_a = create_plan(&conn, "Normal").unwrap();
        add_plan_item(&conn, &plan_a, &rent, Money::from_dollars(1800.0)).unwrap();
        let plan_b = create_plan(&conn, "Tight").unwrap();
        add_plan_item(&conn, &plan_b, &rent, Money::from_dollars(1500.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_a, "2026-09", start, 30).unwrap();

        // Merge a DIFFERENT plan into the month — matching is by the shared series id.
        restamp_merge(&mut conn, &month_id, &plan_b).unwrap();

        let txns = queries::load_txns(&conn, &month_id).unwrap();
        let rent_rows: Vec<_> = txns.iter().filter(|t| t.series_id.as_deref() == Some(rent.as_str())).collect();
        assert_eq!(rent_rows.len(), 1, "no duplicate — matched, not re-inserted");
        assert_eq!(rent_rows[0].amount, Money::from_dollars(1500.0), "refreshed to plan B");
    }

    #[test]
    fn merge_protects_settled_refreshes_unsettled_and_adds_new() {
        let mut conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let rent = create_series(&conn, Kind::Transaction, "Rent", Some(Direction::Out), None, None).unwrap();
        let rent_item = add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();
        let elec = create_series(&conn, Kind::Transaction, "Electric", Some(Direction::Out), None, None).unwrap();
        let elec_item = add_plan_item(&conn, &plan, &elec, Money::from_dollars(100.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

        // Settle Rent at a corrected actual.
        let txns = queries::load_txns(&conn, &month_id).unwrap();
        let rent_txn = txns.iter().find(|t| t.series_id.as_deref() == Some(rent.as_str())).unwrap();
        mark_paid(&conn, &rent_txn.id, Money::from_dollars(1234.0), None).unwrap();

        // Change both plan amounts and add a brand-new series.
        set_item_amount(&conn, &rent_item, Money::from_dollars(1100.0)).unwrap();
        set_item_amount(&conn, &elec_item, Money::from_dollars(120.0)).unwrap();
        let water = create_series(&conn, Kind::Transaction, "Water", Some(Direction::Out), None, None).unwrap();
        add_plan_item(&conn, &plan, &water, Money::from_dollars(40.0)).unwrap();

        restamp_merge(&mut conn, &month_id, &plan).unwrap();

        let after = queries::load_txns(&conn, &month_id).unwrap();
        let get = |sid: &str| after.iter().find(|t| t.series_id.as_deref() == Some(sid)).unwrap();
        assert_eq!(get(&rent).amount, Money::from_dollars(1234.0), "settled Rent protected");
        assert!(get(&rent).settled);
        assert_eq!(get(&elec).amount, Money::from_dollars(120.0), "unsettled Electric refreshed");
        assert_eq!(get(&water).amount, Money::from_dollars(40.0), "new series inserted");
    }

    #[test]
    fn replace_wipes_or_keeps_handentered() {
        // Helper builds a month with a bill, a manual envelope, a one-off, and manual spending.
        fn setup() -> (Connection, String, String, String) {
            let mut conn = db::open_in_memory().unwrap();
            let plan = create_plan(&conn, "P").unwrap();
            let rent = create_series(&conn, Kind::Transaction, "Rent", Some(Direction::Out), None, None).unwrap();
            add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();
            let dining = create_series(&conn, Kind::Envelope, "Dining", None, Some(PeriodType::Monthly), Some(Mode::Manual)).unwrap();
            add_plan_item(&conn, &plan, &dining, Money::from_dollars(300.0)).unwrap();

            let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
            let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

            // Settle Rent so we can confirm Replace unsettles it.
            let rent_txn_id = {
                let txns = queries::load_txns(&conn, &month_id).unwrap();
                txns.iter().find(|t| t.series_id.as_deref() == Some(rent.as_str())).unwrap().id.clone()
            };
            mark_paid(&conn, &rent_txn_id, Money::from_dollars(1000.0), None).unwrap();

            let dining_env_id = {
                let envs = queries::load_envelopes(&conn, &month_id).unwrap();
                envs.iter().find(|e| e.label == "Dining").unwrap().id.clone()
            };

            // A standalone one-off (no series, no envelope).
            conn.execute(
                "INSERT INTO txn (id, month_id, label, direction, amount_cents, settled)
                 VALUES ('oneoff', ?1, 'Gift', 'out', 5000, 0)",
                [&month_id],
            ).unwrap();
            // Manual-envelope spending (no series, envelope_id set).
            conn.execute(
                "INSERT INTO txn (id, month_id, envelope_id, label, direction, amount_cents, settled)
                 VALUES ('spend', ?1, ?2, 'Lunch', 'out', 1200, 0)",
                rusqlite::params![month_id, dining_env_id],
            ).unwrap();

            (conn, plan, month_id, dining_env_id)
        }

        // Full wipe: hand-entered gone; Rent reset & unsettled.
        {
            let (mut conn, plan, month_id, _dining) = setup();
            restamp_replace(&mut conn, &month_id, &plan, false).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(txns.iter().all(|t| t.series_id.is_some()), "all one-offs + manual spending wiped");
            let rent = txns.iter().find(|t| t.label == "Rent").unwrap();
            assert!(!rent.settled, "Replace unsettles plan items");
        }

        // Keep hand-entered: one-off stays; manual spending stays linked to its envelope.
        {
            let (mut conn, plan, month_id, dining) = setup();
            restamp_replace(&mut conn, &month_id, &plan, true).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(txns.iter().any(|t| t.id == "oneoff"), "one-off kept");
            let spend = txns.iter().find(|t| t.id == "spend").unwrap();
            assert_eq!(spend.envelope_id.as_deref(), Some(dining.as_str()), "spending still linked");
        }
    }

    #[test]
    fn card_owed_recomputes_from_limit_and_available() {
        let mut conn = db::open_in_memory().unwrap();
        seed_demo(&mut conn).unwrap();
        let card_id = {
            let accts = queries::load_accounts(&conn).unwrap();
            let card = accts.iter().find(|a| a.name == "Credit Card").unwrap();
            assert_eq!(card.owed(), Money::from_dollars(850.0)); // 5000 − 4150
            card.id.clone()
        };

        set_credit_limit(&conn, &card_id, Money::from_dollars(6000.0)).unwrap();
        let accts = queries::load_accounts(&conn).unwrap();
        let card = accts.iter().find(|a| a.id == card_id).unwrap();
        assert_eq!(card.owed(), Money::from_dollars(1850.0)); // 6000 − 4150
    }

    #[test]
    fn delete_series_blocked_while_referenced() {
        let conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let s = create_series(&conn, Kind::Transaction, "Rent", Some(Direction::Out), None, None).unwrap();
        let item = add_plan_item(&conn, &plan, &s, Money::from_dollars(10.0)).unwrap();

        assert!(delete_series(&conn, &s).is_err(), "blocked while a plan uses it");
        delete_plan_item(&conn, &item).unwrap();
        assert!(delete_series(&conn, &s).is_ok(), "allowed once unreferenced");
    }
}
