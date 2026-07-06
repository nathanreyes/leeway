//! Write operations: the verbs from app-spec §5, plus demo seeding.
//!
//! Each function takes `&Connection` and returns `Result<()>` (or an id). Multi-row
//! operations run inside a transaction so a failure can't leave a half-stamped month.

use crate::models::*;
use crate::money::Money;
use crate::queries;
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate};
use rusqlite::Connection;
use uuid::Uuid;

/// Mint a fresh random id. UUIDs (not autoincrement) keep the file portable and let a
/// future multi-device sync merge without primary-key collisions.
fn new_id() -> String {
    Uuid::new_v4().to_string()
}

// --- Stamping (§5) -------------------------------------------------------------

/// Copy a plan's items into concrete instances for a new month, then sever the link:
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

    // Read the plan's items (inside the same tx for a consistent view).
    let items = {
        let mut stmt = tx.prepare(
            "SELECT id, plan_id, kind, label, slug, category, direction,
                    amount_cents, period_type, mode
             FROM plan_item WHERE plan_id = ?1",
        )?;
        stmt.query_map([plan_id], |r| {
            Ok(PlanItem {
                id: r.get("id")?,
                plan_id: r.get("plan_id")?,
                kind: r.get("kind")?,
                label: r.get("label")?,
                slug: r.get("slug")?,
                category: r.get("category")?,
                direction: r.get("direction")?,
                amount: r.get("amount_cents")?,
                period_type: r.get("period_type")?,
                mode: r.get("mode")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for item in items {
        match item.kind {
            Kind::Envelope => {
                tx.execute(
                    "INSERT INTO envelope
                       (id, month_id, series_id, label, category, amount_cents,
                        stamped_amount_cents, period_type, mode)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        new_id(),
                        month_id,
                        item.id, // series_id = the durable plan_item id
                        item.label,
                        item.category,
                        item.amount,
                        item.amount, // stamped_amount == amount at stamp time
                        item.period_type.unwrap_or(PeriodType::Monthly),
                        item.mode,
                    ],
                )?;
            }
            Kind::Transaction => {
                tx.execute(
                    "INSERT INTO txn
                       (id, month_id, series_id, label, category, direction,
                        amount_cents, stamped_amount_cents, settled)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                    rusqlite::params![
                        new_id(),
                        month_id,
                        item.id,
                        item.label,
                        item.category,
                        item.direction.unwrap_or(Direction::Out),
                        item.amount,
                        item.amount,
                    ],
                )?;
            }
        }
    }

    tx.commit()?;
    Ok(month_id)
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

/// Update an account's ground-truth balance (entered by hand).
pub fn set_balance(conn: &Connection, account_id: &str, balance: Money) -> Result<()> {
    conn.execute(
        "UPDATE account SET balance_cents = ?1 WHERE id = ?2",
        rusqlite::params![balance, account_id],
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

/// Add a new transaction definition to a plan, with sensible defaults the user then
/// edits in place (label "New bill", $0, outgoing). Returns the new item's id.
pub fn add_transaction_item(conn: &Connection, plan_id: &str) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO plan_item (id, plan_id, kind, label, direction, amount_cents)
         VALUES (?1, ?2, 'transaction', 'New bill', 'out', 0)",
        rusqlite::params![id, plan_id],
    )?;
    Ok(id)
}

/// Add a new envelope definition to a plan (label "New envelope", $0, monthly, mode
/// inherited from the global default). Returns the new item's id.
pub fn add_envelope_item(conn: &Connection, plan_id: &str) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO plan_item (id, plan_id, kind, label, amount_cents, period_type)
         VALUES (?1, ?2, 'envelope', 'New envelope', 0, 'monthly')",
        rusqlite::params![id, plan_id],
    )?;
    Ok(id)
}

pub fn set_item_label(conn: &Connection, item_id: &str, label: &str) -> Result<()> {
    conn.execute("UPDATE plan_item SET label = ?1 WHERE id = ?2", rusqlite::params![label, item_id])?;
    Ok(())
}

pub fn set_item_amount(conn: &Connection, item_id: &str, amount: Money) -> Result<()> {
    conn.execute("UPDATE plan_item SET amount_cents = ?1 WHERE id = ?2", rusqlite::params![amount, item_id])?;
    Ok(())
}

pub fn set_item_direction(conn: &Connection, item_id: &str, direction: Direction) -> Result<()> {
    conn.execute("UPDATE plan_item SET direction = ?1 WHERE id = ?2", rusqlite::params![direction, item_id])?;
    Ok(())
}

/// Set an envelope item's mode. `None` clears the column so it inherits the global default.
pub fn set_item_mode(conn: &Connection, item_id: &str, mode: Option<Mode>) -> Result<()> {
    conn.execute("UPDATE plan_item SET mode = ?1 WHERE id = ?2", rusqlite::params![mode, item_id])?;
    Ok(())
}

pub fn set_item_period(conn: &Connection, item_id: &str, period: PeriodType) -> Result<()> {
    conn.execute("UPDATE plan_item SET period_type = ?1 WHERE id = ?2", rusqlite::params![period, item_id])?;
    Ok(())
}

pub fn delete_plan_item(conn: &Connection, item_id: &str) -> Result<()> {
    conn.execute("DELETE FROM plan_item WHERE id = ?1", [item_id])?;
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

    // Accounts (ground-truth balances).
    let checking = new_id();
    let card = new_id();
    conn.execute(
        "INSERT INTO account (id, name, type, balance_cents, protected) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![checking, "Checking", AccountType::Checking.as_str(), Money::from_dollars(4200.0), false],
    )?;
    conn.execute(
        "INSERT INTO account (id, name, type, balance_cents, protected) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![card, "Credit Card", AccountType::CreditCard.as_str(), Money::from_dollars(-850.0), true],
    )?;

    // A plan and its recurring items.
    let plan_id = new_id();
    conn.execute("INSERT INTO plan (id, name) VALUES (?1, ?2)", rusqlite::params![plan_id, "Normal Month"])?;

    // Helper closure to insert a plan_item. `move` isn't needed; it borrows plan_id.
    let add_item = |kind: Kind,
                        label: &str,
                        direction: Option<Direction>,
                        amount: Money,
                        period: Option<PeriodType>,
                        mode: Option<Mode>|
     -> Result<()> {
        conn.execute(
            "INSERT INTO plan_item (id, plan_id, kind, label, direction, amount_cents, period_type, mode)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                new_id(),
                plan_id,
                kind.as_str(),
                label,
                direction.map(|d| d.as_str()),
                amount,
                period.map(|p| p.as_str()),
                mode.map(|m| m.as_str()),
            ],
        )?;
        Ok(())
    };

    add_item(Kind::Transaction, "Paycheck", Some(Direction::In), Money::from_dollars(5200.0), None, None)?;
    add_item(Kind::Transaction, "Rent", Some(Direction::Out), Money::from_dollars(1800.0), None, None)?;
    add_item(Kind::Transaction, "Electric", Some(Direction::Out), Money::from_dollars(140.0), None, None)?;
    add_item(Kind::Transaction, "Internet", Some(Direction::Out), Money::from_dollars(70.0), None, None)?;
    add_item(Kind::Envelope, "Groceries", None, Money::from_dollars(2000.0), Some(PeriodType::Monthly), Some(Mode::Automatic))?;
    add_item(Kind::Envelope, "Dining", None, Money::from_dollars(300.0), Some(PeriodType::Monthly), Some(Mode::Manual))?;

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

        // Build a plan from scratch.
        let plan_id = create_plan(&conn, "Tight Month").unwrap();
        let bill = add_transaction_item(&conn, &plan_id).unwrap();
        set_item_label(&conn, &bill, "Rent").unwrap();
        set_item_amount(&conn, &bill, Money::from_dollars(1500.0)).unwrap();
        let env = add_envelope_item(&conn, &plan_id).unwrap();
        set_item_label(&conn, &env, "Groceries").unwrap();
        set_item_amount(&conn, &env, Money::from_dollars(600.0)).unwrap();

        let items = queries::load_plan_items(&conn, &plan_id).unwrap();
        assert_eq!(items.len(), 2);

        // Stamp it, then confirm the month is an independent snapshot.
        let start = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        stamp(&mut conn, &plan_id, "2026-08", start, 31).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let stamped_txns = queries::load_txns(&conn, &month.id).unwrap();
        assert_eq!(stamped_txns.len(), 1);
        assert_eq!(stamped_txns[0].amount, Money::from_dollars(1500.0));
        // series_id on the instance equals the durable plan_item id.
        assert_eq!(stamped_txns[0].series_id.as_deref(), Some(bill.as_str()));

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
}
