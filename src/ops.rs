//! Write operations: the verbs from app-spec §5, plus demo seeding.
//!
//! Each function takes `&Connection` and returns `Result<()>` (or an id). Multi-row
//! operations run inside a transaction so a failure can't leave a half-stamped month.

use crate::calc;
use crate::currency::Currency;
use crate::models::*;
use crate::money::Money;
use crate::queries;
use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Mint a fresh random id. UUIDs (not autoincrement) keep the file portable and let a
/// future multi-device sync merge without primary-key collisions.
fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Persist the app-wide display currency into the `setting` table (upsert). Because
/// the row lives in the database, the choice syncs with the budget. The caller is
/// responsible for updating the in-process active currency (`currency::set_active`).
pub fn set_currency(conn: &Connection, currency: Currency) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('currency', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![currency.code],
    )
    .context("saving currency setting")?;
    Ok(())
}

/// Persist the global `default_envelope_mode` (upsert). This seeds the mode of
/// envelope series created afterwards (see `queries::default_mode`); existing
/// envelope instances keep whatever mode they were stamped with — flipping the
/// default deliberately does not rewrite history.
pub fn set_default_envelope_mode(conn: &Connection, mode: Mode) -> Result<()> {
    let value = match mode {
        Mode::Automatic => "automatic",
        Mode::Manual => "manual",
    };
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('default_envelope_mode', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![value],
    )
    .context("saving default envelope mode setting")?;
    Ok(())
}

/// Persist the figure requested by credit-card amount prompts. Accounts continue to store
/// available credit; current balance is converted at the input boundary.
pub fn set_credit_card_entry_mode(conn: &Connection, mode: CreditCardEntryMode) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('credit_card_entry_mode', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![mode.as_str()],
    )
    .context("saving credit-card entry mode setting")?;
    Ok(())
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
        insert_instance_from_entry(&tx, &month_id, &entry, days_in_month)?;
    }

    tx.commit()?;
    Ok(month_id)
}

/// Insert one fresh instance (envelope or standalone txn) for a plan entry into a month.
/// The intrinsic fields come from the shared `series`; the amount from this plan's item.
/// `stamped_amount == amount` at stamp time. Shared by fresh stamp, merge, and replace.
fn insert_instance_from_entry(
    conn: &Connection,
    month_id: &str,
    entry: &PlanEntry,
    days_in_month: i64,
) -> Result<()> {
    let s = &entry.series;
    match s.kind {
        Kind::Envelope => {
            let amount = calc::monthlyized_envelope_amount(
                entry.amount,
                s.period_type.unwrap_or(PeriodType::Monthly),
                days_in_month,
            );
            conn.execute(
                "INSERT INTO envelope
                   (id, month_id, series_id, label, amount_cents,
                    stamped_amount_cents, period_type, mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    new_id(),
                    month_id,
                    s.id, // series_id = the durable series identity
                    s.label,
                    amount,
                    amount,
                    calc::active_period(s.period_type.unwrap_or(PeriodType::Monthly)),
                    // Envelope series always carry a concrete mode (enforced at creation and
                    // by the series CHECK), so this copies a real value into the frozen snapshot.
                    s.mode.expect("envelope series must have a mode"),
                ],
            )?;
        }
        Kind::Transaction => {
            conn.execute(
                "INSERT INTO txn
                   (id, month_id, series_id, label, direction,
                    amount_cents, stamped_amount_cents, settled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                rusqlite::params![
                    new_id(),
                    month_id,
                    s.id,
                    s.label,
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

/// Legacy predicate for seriesless hand-entered data — standalone one-offs,
/// manual-envelope spending, OR an ad-hoc envelope. The active Replace UI uses
/// `month_has_items_outside_plan` because series-backed month additions are now normal.
pub fn month_has_handentered(conn: &Connection, month_id: &str) -> Result<bool> {
    // Two `series_id IS NULL` checks, one per table — a hand-entered txn or an ad-hoc
    // envelope each counts. `OR EXISTS` short-circuits, so we stop at the first hit.
    let has: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM txn      WHERE month_id = ?1 AND series_id IS NULL)
             OR EXISTS(SELECT 1 FROM envelope WHERE month_id = ?1 AND series_id IS NULL)",
        [month_id],
        |r| r.get(0),
    )?;
    Ok(has)
}

/// Does this month contain data outside the target plan? This is the Replace guard in the
/// universal-series model: rows are preserved or wiped based on plan membership, not on
/// whether they carry a series id.
pub fn month_has_items_outside_plan(
    conn: &Connection,
    month_id: &str,
    plan_id: &str,
) -> Result<bool> {
    let entries = queries::load_plan_entries(conn, plan_id)?;
    let mut remaining = occurrence_counts(&entries);

    for txn in queries::load_txns(conn, month_id)? {
        let Some(series_id) = txn.series_id.as_deref() else {
            return Ok(true);
        };
        if txn.envelope_id.is_some() {
            return Ok(true);
        }
        if !consume_occurrence(&mut remaining, Kind::Transaction, series_id) {
            return Ok(true);
        }
    }

    for envelope in queries::load_envelopes(conn, month_id)? {
        let Some(series_id) = envelope.series_id.as_deref() else {
            return Ok(true);
        };
        if !consume_occurrence(&mut remaining, Kind::Envelope, series_id) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Merge a plan into an existing month: additive, refreshing the planned baseline.
/// - A matching **unsettled** instance is refreshed to the plan's values.
/// - A matching **settled** transaction is left untouched (it holds a real actual).
/// - A plan entry with no instance is inserted.
///
/// Nothing is ever deleted. Repeated occurrences of a series are matched in stable row
/// order.
pub fn restamp_merge(conn: &mut Connection, month_id: &str, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let entries = queries::load_plan_entries(&tx, plan_id)?;
    let txns = queries::load_txns(&tx, month_id)?;
    let envelopes = queries::load_envelopes(&tx, month_id)?;
    let days_in_month = queries::month_days(&tx, month_id)?;
    let mut matched_txns: HashSet<String> = HashSet::new();
    let mut matched_envelopes: HashSet<String> = HashSet::new();

    for entry in &entries {
        match entry.series.kind {
            Kind::Envelope => {
                if let Some(envelope) =
                    next_matching_envelope(&envelopes, &matched_envelopes, &entry.series.id)
                {
                    matched_envelopes.insert(envelope.id.clone());
                    refresh_envelope(&tx, &envelope.id, entry, days_in_month)?;
                } else {
                    insert_instance_from_entry(&tx, month_id, entry, days_in_month)?;
                }
            }
            Kind::Transaction => {
                if let Some(txn) = next_matching_txn(&txns, &matched_txns, &entry.series.id) {
                    matched_txns.insert(txn.id.clone());
                    if !txn.settled {
                        refresh_txn(&tx, &txn.id, entry)?;
                    }
                } else {
                    insert_instance_from_entry(&tx, month_id, entry, days_in_month)?;
                }
            }
        }
    }
    tx.execute(
        "UPDATE month SET plan_id = ?1 WHERE id = ?2",
        rusqlite::params![plan_id, month_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Replace a month from a plan (clean slate). Instances whose series is in the target plan
/// are reset in place (amount & stamped reset, unsettled, coded fields refreshed) —
/// resetting rather than delete+recreate keeps instance ids stable so manual-envelope
/// spending stays linked. Rows outside the target plan are wiped unless
/// `keep_outside_plan`.
pub fn restamp_replace(
    conn: &mut Connection,
    month_id: &str,
    plan_id: &str,
    keep_outside_plan: bool,
) -> Result<()> {
    let tx = conn.transaction()?;
    let entries = queries::load_plan_entries(&tx, plan_id)?;
    let txns = queries::load_txns(&tx, month_id)?;
    let envelopes = queries::load_envelopes(&tx, month_id)?;
    let days_in_month = queries::month_days(&tx, month_id)?;
    let mut matched_txns: HashSet<String> = HashSet::new();
    let mut matched_envelopes: HashSet<String> = HashSet::new();

    for entry in &entries {
        match entry.series.kind {
            Kind::Envelope => {
                if let Some(envelope) =
                    next_matching_envelope(&envelopes, &matched_envelopes, &entry.series.id)
                {
                    matched_envelopes.insert(envelope.id.clone());
                    refresh_envelope(&tx, &envelope.id, entry, days_in_month)?;
                } else {
                    insert_instance_from_entry(&tx, month_id, entry, days_in_month)?;
                }
            }
            Kind::Transaction => {
                if let Some(txn) = next_matching_txn(&txns, &matched_txns, &entry.series.id) {
                    matched_txns.insert(txn.id.clone());
                    refresh_txn(&tx, &txn.id, entry)?;
                } else {
                    insert_instance_from_entry(&tx, month_id, entry, days_in_month)?;
                }
            }
        }
    }

    if !keep_outside_plan {
        for txn in &txns {
            if !matched_txns.contains(&txn.id) {
                tx.execute("DELETE FROM txn WHERE id = ?1", [&txn.id])?;
            }
        }
        for envelope in &envelopes {
            if !matched_envelopes.contains(&envelope.id) {
                tx.execute(
                    "UPDATE txn SET envelope_id = NULL WHERE envelope_id = ?1",
                    [&envelope.id],
                )?;
                tx.execute("DELETE FROM envelope WHERE id = ?1", [&envelope.id])?;
            }
        }
    }

    tx.execute(
        "UPDATE month SET plan_id = ?1 WHERE id = ?2",
        rusqlite::params![plan_id, month_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn next_matching_txn<'a>(
    txns: &'a [Txn],
    matched: &HashSet<String>,
    series_id: &str,
) -> Option<&'a Txn> {
    txns.iter().find(|txn| {
        txn.envelope_id.is_none()
            && txn.series_id.as_deref() == Some(series_id)
            && !matched.contains(&txn.id)
    })
}

fn next_matching_envelope<'a>(
    envelopes: &'a [Envelope],
    matched: &HashSet<String>,
    series_id: &str,
) -> Option<&'a Envelope> {
    envelopes.iter().find(|envelope| {
        envelope.series_id.as_deref() == Some(series_id) && !matched.contains(&envelope.id)
    })
}

fn occurrence_counts(entries: &[PlanEntry]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for entry in entries {
        *counts
            .entry(occurrence_key(entry.series.kind, &entry.series.id))
            .or_insert(0) += 1;
    }
    counts
}

fn consume_occurrence(counts: &mut HashMap<String, usize>, kind: Kind, series_id: &str) -> bool {
    let Some(count) = counts.get_mut(&occurrence_key(kind, series_id)) else {
        return false;
    };
    if *count == 0 {
        return false;
    }
    *count -= 1;
    true
}

fn occurrence_key(kind: Kind, series_id: &str) -> String {
    format!("{}:{series_id}", kind.as_str())
}

/// Reset an envelope instance to a plan entry's planned values.
fn refresh_envelope(
    conn: &Connection,
    id: &str,
    entry: &PlanEntry,
    days_in_month: i64,
) -> Result<()> {
    let s = &entry.series;
    let amount = calc::monthlyized_envelope_amount(
        entry.amount,
        s.period_type.unwrap_or(PeriodType::Monthly),
        days_in_month,
    );
    conn.execute(
        "UPDATE envelope
         SET amount_cents = ?1, stamped_amount_cents = ?2, label = ?3,
             period_type = ?4, mode = ?5
        WHERE id = ?6",
        rusqlite::params![
            amount,
            amount,
            s.label,
            calc::active_period(s.period_type.unwrap_or(PeriodType::Monthly)),
            s.mode.expect("envelope series must have a mode"),
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
         SET amount_cents = ?1, stamped_amount_cents = ?2, label = ?3,
             direction = ?4, settled = 0, date_paid = NULL
         WHERE id = ?5",
        rusqlite::params![
            entry.amount,
            entry.amount,
            s.label,
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
pub fn mark_paid(
    conn: &Connection,
    txn_id: &str,
    actual: Money,
    date_paid: Option<&str>,
) -> Result<()> {
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

/// Create a checking account. Carry balance starts unset, which the calculation treats as
/// zero; it can be edited later from the Accounts panel.
pub fn create_checking_account(conn: &Connection, name: &str, balance: Money) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO account
            (id, name, type, balance_cents, credit_limit_cents, available_credit_cents,
             carry_balance_cents)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL)",
        rusqlite::params![id, name, AccountType::Checking.as_str(), balance],
    )?;
    Ok(id)
}

/// Create a credit-card account. `balance_cents` is unused for cards; owed is derived from
/// limit minus available credit.
pub fn create_credit_card_account(
    conn: &Connection,
    name: &str,
    credit_limit: Money,
    available_credit: Money,
) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO account
            (id, name, type, balance_cents, credit_limit_cents, available_credit_cents,
             carry_balance_cents)
         VALUES (?1, ?2, ?3, 0, ?4, ?5, NULL)",
        rusqlite::params![
            id,
            name,
            AccountType::CreditCard.as_str(),
            credit_limit,
            available_credit
        ],
    )?;
    Ok(id)
}

/// Rename an account. The name is cosmetic; transactions reference the id.
pub fn rename_account(conn: &Connection, account_id: &str, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE account SET name = ?1 WHERE id = ?2",
        rusqlite::params![name, account_id],
    )?;
    Ok(())
}

/// Set an account's carry balance. The stored amount is unsigned-by-convention; the sign
/// is derived from the account type by `Account::carry_adjustment`.
pub fn set_account_carry_balance(
    conn: &Connection,
    account_id: &str,
    carry_balance: Money,
) -> Result<()> {
    conn.execute(
        "UPDATE account SET carry_balance_cents = ?1 WHERE id = ?2",
        rusqlite::params![carry_balance, account_id],
    )?;
    Ok(())
}

/// Delete an account if it is not referenced by transactions. Returns `false` when the
/// delete is blocked so the UI can show a status instead of surfacing a SQLite FK error.
pub fn delete_account(conn: &Connection, account_id: &str) -> Result<bool> {
    let references: i64 = conn.query_row(
        "SELECT COUNT(*) FROM txn WHERE account_id = ?1",
        [account_id],
        |r| r.get(0),
    )?;
    if references > 0 {
        return Ok(false);
    }

    conn.execute("DELETE FROM account WHERE id = ?1", [account_id])?;
    Ok(true)
}

// --- Plan management -----------------------------------------------------------

/// Create an empty plan and return its id.
pub fn create_plan(conn: &Connection, name: &str) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO plan (id, name) VALUES (?1, ?2)",
        rusqlite::params![id, name],
    )?;
    Ok(id)
}

/// Rename a plan. Labels are cosmetic — no instance references a plan's name.
pub fn rename_plan(conn: &Connection, plan_id: &str, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE plan SET name = ?1 WHERE id = ?2",
        rusqlite::params![name, plan_id],
    )?;
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

/// Create a new series (a durable recurring-item definition).
///
/// `mode` is where the global default is applied — ONCE, here, at creation. An envelope
/// series that passes `None` is seeded with the current `default_envelope_mode` and frozen;
/// changing the global default later never touches it (that's the whole point — see
/// migration 004). Transaction series have no mode and keep `None`.
pub fn create_series(
    conn: &Connection,
    kind: Kind,
    label: &str,
    direction: Option<Direction>,
    period_type: Option<PeriodType>,
    mode: Option<Mode>,
) -> Result<String> {
    let mode = match kind {
        Kind::Envelope => Some(match mode {
            Some(m) => m,
            None => crate::queries::default_mode(conn)?,
        }),
        Kind::Transaction => None, // transactions carry no mode
    };
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
pub fn add_plan_item(
    conn: &Connection,
    plan_id: &str,
    series_id: &str,
    amount: Money,
) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO plan_item (id, plan_id, series_id, amount_cents) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, plan_id, series_id, amount],
    )?;
    Ok(id)
}

/// Create a brand-new transaction series and add it to the plan. Returns the plan_item id
/// so the caller can select it after the prompt flow inserts it.
pub fn add_new_transaction(
    conn: &Connection,
    plan_id: &str,
    label: &str,
    direction: Direction,
    amount: Money,
) -> Result<String> {
    let series_id = create_series(conn, Kind::Transaction, label, Some(direction), None, None)?;
    add_plan_item(conn, plan_id, &series_id, amount)
}

/// Create a brand-new envelope series (monthly, mode inherited) and add it to the plan.
/// Returns the plan_item id so the caller can select it after insertion.
pub fn add_new_envelope(
    conn: &Connection,
    plan_id: &str,
    label: &str,
    amount: Money,
) -> Result<String> {
    let series_id = create_series(
        conn,
        Kind::Envelope,
        label,
        None,
        Some(PeriodType::Monthly),
        None,
    )?;
    add_plan_item(conn, plan_id, &series_id, amount)
}

/// Add an existing series to a plan (the reuse picker). Returns the plan_item id.
pub fn add_existing_series(conn: &Connection, plan_id: &str, series_id: &str) -> Result<String> {
    add_plan_item(conn, plan_id, series_id, Money::ZERO)
}

/// Per-plan: set this plan's budgeted amount for an item. Edits `plan_item`, not `series`.
pub fn set_item_amount(conn: &Connection, item_id: &str, amount: Money) -> Result<()> {
    conn.execute(
        "UPDATE plan_item SET amount_cents = ?1 WHERE id = ?2",
        rusqlite::params![amount, item_id],
    )?;
    Ok(())
}

// The following edit the shared SERIES, so they affect every plan that includes it.

pub fn set_series_label(conn: &Connection, series_id: &str, label: &str) -> Result<()> {
    conn.execute(
        "UPDATE series SET label = ?1 WHERE id = ?2",
        rusqlite::params![label, series_id],
    )?;
    Ok(())
}

pub fn set_series_direction(
    conn: &Connection,
    series_id: &str,
    direction: Direction,
) -> Result<()> {
    conn.execute(
        "UPDATE series SET direction = ?1 WHERE id = ?2",
        rusqlite::params![direction, series_id],
    )?;
    Ok(())
}

/// Set an envelope series' mode to a concrete value. There is no "inherit" state anymore:
/// mode is frozen at creation, and this is how the user explicitly changes it afterwards.
pub fn set_series_mode(conn: &Connection, series_id: &str, mode: Mode) -> Result<()> {
    conn.execute(
        "UPDATE series SET mode = ?1 WHERE id = ?2",
        rusqlite::params![mode, series_id],
    )?;
    Ok(())
}

pub fn set_series_period(conn: &Connection, series_id: &str, period: PeriodType) -> Result<()> {
    let period = calc::active_period(period);
    let current = conn
        .query_row(
            "SELECT period_type FROM series WHERE id = ?1",
            [series_id],
            |r| r.get::<_, Option<PeriodType>>("period_type"),
        )
        .optional()?
        .flatten()
        .map(calc::active_period)
        .unwrap_or(PeriodType::Monthly);

    if current != period {
        match (current, period) {
            (PeriodType::Monthly, PeriodType::Daily) => {
                conn.execute(
                    "UPDATE plan_item
                     SET amount_cents = CAST(ROUND(amount_cents / 30.0) AS INTEGER)
                     WHERE series_id = ?1",
                    [series_id],
                )?;
            }
            (PeriodType::Daily, PeriodType::Monthly) => {
                conn.execute(
                    "UPDATE plan_item SET amount_cents = amount_cents * 30 WHERE series_id = ?1",
                    [series_id],
                )?;
            }
            _ => {}
        }
    }

    conn.execute(
        "UPDATE series SET period_type = ?1 WHERE id = ?2",
        rusqlite::params![period, series_id],
    )?;
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

// --- Month items ---------------------------------------------------------------

/// Add a standalone transaction instance for an existing series to a month. This is the
/// dashboard's normal add path: the row is month-owned, but still carries the durable
/// series id so trends and restamp matching can recognize it.
pub fn add_series_txn_instance(
    conn: &Connection,
    month_id: &str,
    series_id: &str,
    amount: Money,
) -> Result<String> {
    let series = queries::get_series(conn, series_id)?
        .with_context(|| format!("series not found: {series_id}"))?;
    if series.kind != Kind::Transaction {
        anyhow::bail!("series is not a transaction");
    }

    let id = new_id();
    conn.execute(
        "INSERT INTO txn
           (id, month_id, series_id, label, direction,
            amount_cents, stamped_amount_cents, settled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
        rusqlite::params![
            id,
            month_id,
            series.id,
            series.label,
            series.direction.unwrap_or(Direction::Out),
            amount,
            amount,
        ],
    )?;
    Ok(id)
}

/// Add an envelope instance for an existing series to a month.
pub fn add_series_envelope_instance(
    conn: &Connection,
    month_id: &str,
    series_id: &str,
    amount: Money,
) -> Result<String> {
    let series = queries::get_series(conn, series_id)?
        .with_context(|| format!("series not found: {series_id}"))?;
    if series.kind != Kind::Envelope {
        anyhow::bail!("series is not an envelope");
    }
    let period = calc::active_period(series.period_type.unwrap_or(PeriodType::Monthly));
    let days_in_month = queries::month_days(conn, month_id)?;
    let amount = calc::monthlyized_envelope_amount(amount, period, days_in_month);

    let id = new_id();
    conn.execute(
        "INSERT INTO envelope
           (id, month_id, series_id, label, amount_cents,
            stamped_amount_cents, period_type, mode)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            id,
            month_id,
            series.id,
            series.label,
            amount,
            amount,
            period,
            series.mode.expect("envelope series must have a mode"),
        ],
    )?;
    Ok(id)
}

// --- Legacy/seriesless month items --------------------------------------------
//
// The normal dashboard add path now creates or reuses a series and calls the helpers
// above. These older helpers still support legacy rows and manual-envelope spending,
// where `series_id = NULL` marks data outside the plan's series set.

/// Add a standalone (no-envelope) one-off transaction to a month — an ad-hoc bill or bit of
/// income. `series_id` and `stamped_amount` stay NULL (nothing to revert to), and it starts
/// unsettled. Returns the new txn id so the caller can select it for editing.
pub fn add_oneoff_txn(
    conn: &Connection,
    month_id: &str,
    label: &str,
    direction: Direction,
    amount: Money,
) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO txn (id, month_id, label, direction, amount_cents, settled)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![id, month_id, label, direction, amount],
    )?;
    Ok(id)
}

/// Add an ad-hoc envelope straight into a month (no series behind it). `stamped_amount`
/// equals `amount` at creation, mirroring a stamped envelope, so "revert to planned" has a
/// sensible target even though there's no plan. Returns the new envelope id.
pub fn add_oneoff_envelope(
    conn: &Connection,
    month_id: &str,
    label: &str,
    amount: Money,
    period_type: PeriodType,
    mode: Mode,
) -> Result<String> {
    let period_type = calc::active_period(period_type);
    let days_in_month = queries::month_days(conn, month_id)?;
    let amount = calc::monthlyized_envelope_amount(amount, period_type, days_in_month);
    let id = new_id();
    conn.execute(
        "INSERT INTO envelope
           (id, month_id, series_id, label, amount_cents,
            stamped_amount_cents, period_type, mode)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![id, month_id, label, amount, amount, period_type, mode],
    )?;
    Ok(id)
}

/// Record a transaction inside an envelope. It's a hand-entered txn (`series_id NULL`), linked
/// by `envelope_id`, direction Out, and marked settled since it records money already spent.
/// Manual envelopes consume these transactions; automatic envelopes retain them as a record
/// while continuing to consume only by elapsed time. Returns the new txn id.
pub fn add_envelope_spending(
    conn: &Connection,
    month_id: &str,
    envelope_id: &str,
    label: &str,
    amount: Money,
) -> Result<String> {
    let id = new_id();
    let inserted = conn.execute(
        "INSERT INTO txn (id, month_id, envelope_id, label, direction, amount_cents, settled)
         SELECT ?1, ?2, id, ?4, 'out', ?5, 1
         FROM envelope
         WHERE id = ?3 AND month_id = ?2",
        rusqlite::params![id, month_id, envelope_id, label, amount],
    )?;
    if inserted == 0 {
        anyhow::bail!("envelope not found in the selected month");
    }
    Ok(id)
}

// Per-field editors for a month's instances. These edit the *instance* (this month's
// copy), never the shared series.

pub fn set_txn_label(conn: &Connection, txn_id: &str, label: &str) -> Result<()> {
    conn.execute(
        "UPDATE txn SET label = ?1 WHERE id = ?2",
        rusqlite::params![label, txn_id],
    )?;
    Ok(())
}

pub fn set_txn_amount(conn: &Connection, txn_id: &str, amount: Money) -> Result<()> {
    conn.execute(
        "UPDATE txn SET amount_cents = ?1 WHERE id = ?2",
        rusqlite::params![amount, txn_id],
    )?;
    Ok(())
}

pub fn set_txn_direction(conn: &Connection, txn_id: &str, direction: Direction) -> Result<()> {
    conn.execute(
        "UPDATE txn SET direction = ?1 WHERE id = ?2",
        rusqlite::params![direction, txn_id],
    )?;
    Ok(())
}

/// Delete a single transaction instance from a month. A standalone txn has no children;
/// envelope-spending txns are deleted here too when their envelope is removed.
pub fn delete_txn(conn: &Connection, txn_id: &str) -> Result<()> {
    conn.execute("DELETE FROM txn WHERE id = ?1", [txn_id])?;
    Ok(())
}

pub fn set_envelope_label(conn: &Connection, id: &str, label: &str) -> Result<()> {
    conn.execute(
        "UPDATE envelope SET label = ?1 WHERE id = ?2",
        rusqlite::params![label, id],
    )?;
    Ok(())
}

pub fn set_envelope_amount(conn: &Connection, id: &str, amount: Money) -> Result<()> {
    conn.execute(
        "UPDATE envelope SET amount_cents = ?1 WHERE id = ?2",
        rusqlite::params![amount, id],
    )?;
    Ok(())
}

pub fn set_envelope_mode(conn: &Connection, id: &str, mode: Mode) -> Result<()> {
    conn.execute(
        "UPDATE envelope SET mode = ?1 WHERE id = ?2",
        rusqlite::params![mode, id],
    )?;
    Ok(())
}

pub fn set_envelope_period(conn: &Connection, id: &str, period: PeriodType) -> Result<()> {
    conn.execute(
        "UPDATE envelope SET period_type = ?1 WHERE id = ?2",
        rusqlite::params![period, id],
    )?;
    Ok(())
}

/// Delete an envelope instance and any spending filed in it. The `txn.envelope_id` foreign
/// key means we must clear the children first, so this runs in a transaction: drop the
/// envelope's txns, then the envelope.
pub fn delete_envelope(conn: &mut Connection, id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM txn WHERE envelope_id = ?1", [id])?;
    tx.execute("DELETE FROM envelope WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(())
}

// --- Demo seeding --------------------------------------------------------------

/// Number of days in the given calendar month. Computed by asking "what's day 1 of next
/// month?" and stepping back one day.
pub fn days_in_month(year: i32, month: u32) -> i64 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    let last_this = first_next.pred_opt().unwrap();
    last_this.day() as i64
}

/// If the database is empty, lay down a generic starter template: a couple of accounts, one
/// plan with the common bills/envelopes, and this month already stamped from it. All budgeted
/// amounts are $0 placeholders the user fills in. Idempotent — does nothing once any month
/// exists.
pub fn seed_starter(conn: &mut Connection) -> Result<()> {
    if queries::current_month(conn)?.is_some() {
        return Ok(()); // already has data
    }

    // Accounts. A checking account starting empty, and a fresh credit card with its full
    // limit available (owed = 5000 − 5000 = 0). The card's placeholder limit is expressed
    // in the active currency so a non-dollar budget seeds a sensible round number.
    let currency = crate::currency::active();
    let card_limit = Money::from_major(5000.0, currency);
    let checking = new_id();
    let card = new_id();
    conn.execute(
        "INSERT INTO account (id, name, type, balance_cents) VALUES (?1,?2,?3,?4)",
        rusqlite::params![
            checking,
            "Checking",
            AccountType::Checking.as_str(),
            Money::ZERO
        ],
    )?;
    conn.execute(
        "INSERT INTO account (id, name, type, balance_cents, credit_limit_cents, available_credit_cents)
         VALUES (?1,?2,?3,0,?4,?5)",
        rusqlite::params![
            card,
            "Credit Card",
            AccountType::CreditCard.as_str(),
            card_limit,
            card_limit
        ],
    )?;

    // A plan and its recurring items. Every amount is a $0 placeholder.
    let plan_id = new_id();
    conn.execute(
        "INSERT INTO plan (id, name) VALUES (?1, ?2)",
        rusqlite::params![plan_id, "Monthly Budget"],
    )?;

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

    let zero = Money::from_dollars(0.0);

    // Income.
    add(
        Kind::Transaction,
        "Paycheck",
        Some(Direction::In),
        zero,
        None,
        None,
    )?;
    // Recurring bills. Internet and Phone are separate lines.
    add(
        Kind::Transaction,
        "Rent/Mortgage",
        Some(Direction::Out),
        zero,
        None,
        None,
    )?;
    add(
        Kind::Transaction,
        "Utilities",
        Some(Direction::Out),
        zero,
        None,
        None,
    )?;
    add(
        Kind::Transaction,
        "Internet",
        Some(Direction::Out),
        zero,
        None,
        None,
    )?;
    add(
        Kind::Transaction,
        "Phone",
        Some(Direction::Out),
        zero,
        None,
        None,
    )?;
    // A monthly set-aside for savings (modeled as an outflow — there's no transfer concept).
    add(
        Kind::Transaction,
        "Savings",
        Some(Direction::Out),
        zero,
        None,
        None,
    )?;
    // Everyday spending envelopes, mixing automatic/manual to show both modes.
    add(
        Kind::Envelope,
        "Groceries",
        None,
        zero,
        Some(PeriodType::Monthly),
        Some(Mode::Automatic),
    )?;
    add(
        Kind::Envelope,
        "Dining Out",
        None,
        zero,
        Some(PeriodType::Monthly),
        Some(Mode::Manual),
    )?;
    add(
        Kind::Envelope,
        "Transportation",
        None,
        zero,
        Some(PeriodType::Monthly),
        Some(Mode::Automatic),
    )?;
    add(
        Kind::Envelope,
        "Personal",
        None,
        zero,
        Some(PeriodType::Monthly),
        Some(Mode::Manual),
    )?;

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
    use crate::currency;
    use crate::db;

    #[test]
    fn currency_setting_round_trips_and_upserts() {
        let conn = db::open_in_memory().unwrap();
        // Fresh database: no currency chosen yet.
        assert_eq!(queries::currency(&conn).unwrap(), None);

        let eur = currency::by_code("EUR").unwrap();
        set_currency(&conn, eur).unwrap();
        assert_eq!(queries::currency(&conn).unwrap(), Some(eur));

        // Changing it upserts in place rather than inserting a second row.
        let jpy = currency::by_code("JPY").unwrap();
        set_currency(&conn, jpy).unwrap();
        assert_eq!(queries::currency(&conn).unwrap(), Some(jpy));
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM setting WHERE key='currency'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn credit_card_entry_mode_defaults_and_round_trips() {
        let conn = db::open_in_memory().unwrap();
        assert_eq!(
            queries::credit_card_entry_mode(&conn).unwrap(),
            CreditCardEntryMode::AvailableCredit
        );

        set_credit_card_entry_mode(&conn, CreditCardEntryMode::CurrentBalance).unwrap();
        assert_eq!(
            queries::credit_card_entry_mode(&conn).unwrap(),
            CreditCardEntryMode::CurrentBalance
        );

        set_credit_card_entry_mode(&conn, CreditCardEntryMode::AvailableCredit).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM setting WHERE key='credit_card_entry_mode'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn unknown_currency_code_is_preserved_not_overwritten() {
        use queries::CurrencySetting;
        let conn = db::open_in_memory().unwrap();
        // A newer app version stored a code this build doesn't recognize.
        conn.execute(
            "INSERT INTO setting (key, value) VALUES ('currency', 'XTS')",
            [],
        )
        .unwrap();

        // The three-state reader surfaces it as Unknown (not Unset), so startup knows
        // not to clobber it — while the convenience getter still reports None.
        assert_eq!(
            queries::currency_setting(&conn).unwrap(),
            CurrencySetting::Unknown("XTS".into())
        );
        assert_eq!(queries::currency(&conn).unwrap(), None);

        // No row exists at all -> Unset, the only case safe to auto-populate.
        let fresh = db::open_in_memory().unwrap();
        assert_eq!(
            queries::currency_setting(&fresh).unwrap(),
            CurrencySetting::Unset
        );
    }

    #[test]
    fn stamp_copies_items_and_freezes_amounts() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();

        let month = queries::current_month(&conn).unwrap().unwrap();
        let txns = queries::load_txns(&conn, &month.id).unwrap();
        let envelopes = queries::load_envelopes(&conn, &month.id).unwrap();

        assert_eq!(txns.len(), 6); // paycheck, rent, utilities, internet, phone, savings
        assert_eq!(envelopes.len(), 4); // groceries, dining, transportation, personal

        // Give one plan item a concrete amount, then stamp a fresh month and confirm the
        // instance's stamped_amount is frozen equal to its amount at stamp time.
        let plan_id = month.plan_id.clone().unwrap();
        let rent_entry = queries::load_plan_entries(&conn, &plan_id)
            .unwrap()
            .into_iter()
            .find(|e| e.series.label == "Rent/Mortgage")
            .unwrap();
        set_item_amount(&conn, &rent_entry.item_id, Money::from_dollars(1800.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_id, "2099-01", start, 31).unwrap();
        let txns = queries::load_txns(&conn, &month_id).unwrap();
        let rent = txns.iter().find(|t| t.label == "Rent/Mortgage").unwrap();
        assert_eq!(rent.amount, Money::from_dollars(1800.0));
        assert_eq!(rent.stamped_amount, Some(Money::from_dollars(1800.0)));
        assert!(!rent.settled);
    }

    #[test]
    fn stamping_daily_envelope_stores_monthly_total() {
        let mut conn = db::open_in_memory().unwrap();
        let plan_id = create_plan(&conn, "Daily rates").unwrap();
        let lunch = create_series(
            &conn,
            Kind::Envelope,
            "Lunch",
            None,
            Some(PeriodType::Daily),
            Some(Mode::Automatic),
        )
        .unwrap();
        add_plan_item(&conn, &plan_id, &lunch, Money::from_dollars(15.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_id, "2026-09", start, 30).unwrap();
        let envelopes = queries::load_envelopes(&conn, &month_id).unwrap();
        let lunch = envelopes
            .iter()
            .find(|envelope| envelope.label == "Lunch")
            .unwrap();

        assert_eq!(lunch.period_type, PeriodType::Daily);
        assert_eq!(lunch.amount, Money::from_dollars(450.0));
        assert_eq!(lunch.stamped_amount, Money::from_dollars(450.0));
    }

    #[test]
    fn changing_series_period_converts_plan_amounts() {
        let conn = db::open_in_memory().unwrap();
        let plan_id = create_plan(&conn, "Rates").unwrap();
        let lunch = create_series(
            &conn,
            Kind::Envelope,
            "Lunch",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Automatic),
        )
        .unwrap();
        let item_id = add_plan_item(&conn, &plan_id, &lunch, Money::from_dollars(450.0)).unwrap();

        set_series_period(&conn, &lunch, PeriodType::Daily).unwrap();
        let entries = queries::load_plan_entries(&conn, &plan_id).unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.item_id == item_id)
            .unwrap();
        assert_eq!(entry.series.period_type, Some(PeriodType::Daily));
        assert_eq!(entry.amount, Money::from_dollars(15.0));

        set_series_period(&conn, &lunch, PeriodType::Monthly).unwrap();
        let entries = queries::load_plan_entries(&conn, &plan_id).unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.item_id == item_id)
            .unwrap();
        assert_eq!(entry.series.period_type, Some(PeriodType::Monthly));
        assert_eq!(entry.amount, Money::from_dollars(450.0));
    }

    #[test]
    fn changing_series_period_uses_nearest_thirty_day_equivalent() {
        let conn = db::open_in_memory().unwrap();
        let plan_id = create_plan(&conn, "Rates").unwrap();
        let lunch = create_series(
            &conn,
            Kind::Envelope,
            "Lunch",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Automatic),
        )
        .unwrap();
        let item_id = add_plan_item(&conn, &plan_id, &lunch, Money::from_dollars(100.0)).unwrap();

        set_series_period(&conn, &lunch, PeriodType::Daily).unwrap();
        let daily = queries::load_plan_entries(&conn, &plan_id).unwrap();
        let daily = daily.iter().find(|entry| entry.item_id == item_id).unwrap();
        assert_eq!(daily.amount, Money::from_dollars(3.33));

        set_series_period(&conn, &lunch, PeriodType::Monthly).unwrap();
        let monthly = queries::load_plan_entries(&conn, &plan_id).unwrap();
        let monthly = monthly
            .iter()
            .find(|entry| entry.item_id == item_id)
            .unwrap();
        assert_eq!(monthly.amount, Money::from_dollars(99.90));
    }

    #[test]
    fn plan_crud_then_stamp() {
        let mut conn = db::open_in_memory().unwrap();

        // Build a plan from scratch. Create series explicitly so we know their ids.
        let plan_id = create_plan(&conn, "Tight Month").unwrap();
        let rent_series = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&conn, &plan_id, &rent_series, Money::from_dollars(1500.0)).unwrap();
        let groc_series = create_series(
            &conn,
            Kind::Envelope,
            "Groceries",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Automatic),
        )
        .unwrap();
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
        assert_eq!(
            stamped_txns[0].series_id.as_deref(),
            Some(rent_series.as_str())
        );

        // Deleting the plan leaves the stamped month intact (severed link).
        delete_plan(&mut conn, &plan_id).unwrap();
        assert!(queries::get_plan(&conn, &plan_id).unwrap().is_none());
        let after = queries::load_txns(&conn, &month.id).unwrap();
        assert_eq!(after.len(), 1, "stamped month survives plan deletion");
    }

    #[test]
    fn series_month_usage_counts_instances_and_delete_guards_plans() {
        let mut conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let rent = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();

        // Not stamped yet: no month instance carries the series id.
        assert_eq!(queries::series_month_usage(&conn, &rent).unwrap(), 0);
        // ...but a plan uses it, so delete is blocked.
        assert!(delete_series(&conn, &rent).is_err());

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

        // After stamping, exactly one instance references the series (the warn-in-months case).
        assert_eq!(queries::series_month_usage(&conn, &rent).unwrap(), 1);

        // Removing it from the plan clears the live reference; the stamped month still counts,
        // and delete now succeeds (orphaning that historical id, by design).
        let item = queries::load_plan_entries(&conn, &plan).unwrap()[0]
            .item_id
            .clone();
        delete_plan_item(&conn, &item).unwrap();
        assert_eq!(queries::series_month_usage(&conn, &rent).unwrap(), 1);
        delete_series(&conn, &rent).unwrap();
        assert_eq!(queries::series_month_usage(&conn, &rent).unwrap(), 1);
    }

    #[test]
    fn mark_and_unmark_paid_roundtrip() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();

        // Give the Utilities bill a planned $140, then stamp a fresh month so the instance
        // freezes that as its stamped amount (the value a revert restores to).
        let plan_id = month.plan_id.clone().unwrap();
        let utilities_entry = queries::load_plan_entries(&conn, &plan_id)
            .unwrap()
            .into_iter()
            .find(|e| e.series.label == "Utilities")
            .unwrap();
        set_item_amount(&conn, &utilities_entry.item_id, Money::from_dollars(140.0)).unwrap();
        let start = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_id, "2099-01", start, 31).unwrap();
        let txns = queries::load_txns(&conn, &month_id).unwrap();
        let utilities = txns.iter().find(|t| t.label == "Utilities").unwrap();

        // Pay it at a corrected actual.
        mark_paid(
            &conn,
            &utilities.id,
            Money::from_dollars(152.30),
            Some("2026-07-05"),
        )
        .unwrap();
        let after = queries::load_txns(&conn, &month_id).unwrap();
        let utilities = after.iter().find(|t| t.label == "Utilities").unwrap();
        assert!(utilities.settled);
        assert_eq!(utilities.amount, Money::from_dollars(152.30));
        assert_eq!(calc::txn_remaining(utilities), Money::ZERO);

        // Un-mark with revert -> back to the planned $140.
        unmark_paid(&conn, &utilities.id, true).unwrap();
        let after = queries::load_txns(&conn, &month_id).unwrap();
        let utilities = after.iter().find(|t| t.label == "Utilities").unwrap();
        assert!(!utilities.settled);
        assert_eq!(utilities.amount, Money::from_dollars(140.0));
    }

    #[test]
    fn cross_plan_series_continuity_on_merge() {
        let mut conn = db::open_in_memory().unwrap();
        // One shared Rent series, included in two different plans at different amounts.
        let rent = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let plan_a = create_plan(&conn, "Normal").unwrap();
        add_plan_item(&conn, &plan_a, &rent, Money::from_dollars(1800.0)).unwrap();
        let plan_b = create_plan(&conn, "Tight").unwrap();
        add_plan_item(&conn, &plan_b, &rent, Money::from_dollars(1500.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_a, "2026-09", start, 30).unwrap();

        // Merge a DIFFERENT plan into the month — matching is by the shared series id.
        restamp_merge(&mut conn, &month_id, &plan_b).unwrap();

        let txns = queries::load_txns(&conn, &month_id).unwrap();
        let rent_rows: Vec<_> = txns
            .iter()
            .filter(|t| t.series_id.as_deref() == Some(rent.as_str()))
            .collect();
        assert_eq!(
            rent_rows.len(),
            1,
            "no duplicate — matched, not re-inserted"
        );
        assert_eq!(
            rent_rows[0].amount,
            Money::from_dollars(1500.0),
            "refreshed to plan B"
        );
    }

    #[test]
    fn merge_protects_settled_refreshes_unsettled_and_adds_new() {
        let mut conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let rent = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let rent_item = add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();
        let elec = create_series(
            &conn,
            Kind::Transaction,
            "Electric",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let elec_item = add_plan_item(&conn, &plan, &elec, Money::from_dollars(100.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

        // Settle Rent at a corrected actual.
        let txns = queries::load_txns(&conn, &month_id).unwrap();
        let rent_txn = txns
            .iter()
            .find(|t| t.series_id.as_deref() == Some(rent.as_str()))
            .unwrap();
        mark_paid(&conn, &rent_txn.id, Money::from_dollars(1234.0), None).unwrap();

        // Change both plan amounts and add a brand-new series.
        set_item_amount(&conn, &rent_item, Money::from_dollars(1100.0)).unwrap();
        set_item_amount(&conn, &elec_item, Money::from_dollars(120.0)).unwrap();
        let water = create_series(
            &conn,
            Kind::Transaction,
            "Water",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&conn, &plan, &water, Money::from_dollars(40.0)).unwrap();

        restamp_merge(&mut conn, &month_id, &plan).unwrap();

        let after = queries::load_txns(&conn, &month_id).unwrap();
        let get = |sid: &str| {
            after
                .iter()
                .find(|t| t.series_id.as_deref() == Some(sid))
                .unwrap()
        };
        assert_eq!(
            get(&rent).amount,
            Money::from_dollars(1234.0),
            "settled Rent protected"
        );
        assert!(get(&rent).settled);
        assert_eq!(
            get(&elec).amount,
            Money::from_dollars(120.0),
            "unsettled Electric refreshed"
        );
        assert_eq!(
            get(&water).amount,
            Money::from_dollars(40.0),
            "new series inserted"
        );
    }

    #[test]
    fn replace_wipes_or_keeps_handentered() {
        // Helper builds a month with a bill, a manual envelope, a one-off, and manual spending.
        fn setup() -> (Connection, String, String, String) {
            let mut conn = db::open_in_memory().unwrap();
            let plan = create_plan(&conn, "P").unwrap();
            let rent = create_series(
                &conn,
                Kind::Transaction,
                "Rent",
                Some(Direction::Out),
                None,
                None,
            )
            .unwrap();
            add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();
            let dining = create_series(
                &conn,
                Kind::Envelope,
                "Dining",
                None,
                Some(PeriodType::Monthly),
                Some(Mode::Manual),
            )
            .unwrap();
            add_plan_item(&conn, &plan, &dining, Money::from_dollars(300.0)).unwrap();

            let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
            let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

            // Settle Rent so we can confirm Replace unsettles it.
            let rent_txn_id = {
                let txns = queries::load_txns(&conn, &month_id).unwrap();
                txns.iter()
                    .find(|t| t.series_id.as_deref() == Some(rent.as_str()))
                    .unwrap()
                    .id
                    .clone()
            };
            mark_paid(&conn, &rent_txn_id, Money::from_dollars(1000.0), None).unwrap();

            let dining_env_id = {
                let envs = queries::load_envelopes(&conn, &month_id).unwrap();
                envs.iter()
                    .find(|e| e.label == "Dining")
                    .unwrap()
                    .id
                    .clone()
            };

            // A standalone one-off (no series, no envelope).
            conn.execute(
                "INSERT INTO txn (id, month_id, label, direction, amount_cents, settled)
                 VALUES ('oneoff', ?1, 'Gift', 'out', 5000, 0)",
                [&month_id],
            )
            .unwrap();
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
            assert!(
                txns.iter().all(|t| t.series_id.is_some()),
                "all one-offs + manual spending wiped"
            );
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
            assert_eq!(
                spend.envelope_id.as_deref(),
                Some(dining.as_str()),
                "spending still linked"
            );
        }
    }

    #[test]
    fn card_owed_recomputes_from_limit_and_available() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let card_id = {
            let accts = queries::load_accounts(&conn).unwrap();
            let card = accts.iter().find(|a| a.name == "Credit Card").unwrap();
            card.id.clone()
        };

        // Draw the card down so it carries a balance, then confirm owed recomputes.
        set_available_credit(&conn, &card_id, Money::from_dollars(4150.0)).unwrap();
        let accts = queries::load_accounts(&conn).unwrap();
        let card = accts.iter().find(|a| a.id == card_id).unwrap();
        assert_eq!(card.owed(), Money::from_dollars(850.0)); // 5000 − 4150

        set_credit_limit(&conn, &card_id, Money::from_dollars(6000.0)).unwrap();
        let accts = queries::load_accounts(&conn).unwrap();
        let card = accts.iter().find(|a| a.id == card_id).unwrap();
        assert_eq!(card.owed(), Money::from_dollars(1850.0)); // 6000 − 4150
    }

    #[test]
    fn creates_accounts_without_carry_balance() {
        let conn = db::open_in_memory().unwrap();

        let checking_id =
            create_checking_account(&conn, "Everyday", Money::from_dollars(1200.0)).unwrap();
        let card_id = create_credit_card_account(
            &conn,
            "Rewards",
            Money::from_dollars(5000.0),
            Money::from_dollars(4250.0),
        )
        .unwrap();

        let accounts = queries::load_accounts(&conn).unwrap();
        let checking = accounts.iter().find(|a| a.id == checking_id).unwrap();
        assert_eq!(checking.account_type, AccountType::Checking);
        assert_eq!(checking.balance, Money::from_dollars(1200.0));
        assert_eq!(checking.credit_limit, None);
        assert_eq!(checking.available_credit, None);
        assert_eq!(checking.carry_balance, None);

        let card = accounts.iter().find(|a| a.id == card_id).unwrap();
        assert_eq!(card.account_type, AccountType::CreditCard);
        assert_eq!(card.balance, Money::ZERO);
        assert_eq!(card.credit_limit, Some(Money::from_dollars(5000.0)));
        assert_eq!(card.available_credit, Some(Money::from_dollars(4250.0)));
        assert_eq!(card.carry_balance, None);
        assert_eq!(card.owed(), Money::from_dollars(750.0));
    }

    #[test]
    fn accounts_sort_by_type_then_display_balance_descending() {
        let conn = db::open_in_memory().unwrap();
        create_credit_card_account(
            &conn,
            "Low Owed Card",
            Money::from_dollars(1000.0),
            Money::from_dollars(900.0),
        )
        .unwrap();
        create_checking_account(&conn, "Low Checking", Money::from_dollars(100.0)).unwrap();
        create_credit_card_account(
            &conn,
            "High Owed Card",
            Money::from_dollars(1000.0),
            Money::from_dollars(300.0),
        )
        .unwrap();
        create_checking_account(&conn, "High Checking", Money::from_dollars(200.0)).unwrap();

        let names: Vec<_> = queries::load_accounts(&conn)
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "High Checking",
                "Low Checking",
                "High Owed Card",
                "Low Owed Card"
            ]
        );
    }

    #[test]
    fn account_carry_balance_flows_into_whats_left() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let today = Local::now().date_naive();
        let before = crate::view::MonthView::build_for(&conn, today, today.year(), today.month())
            .unwrap()
            .unwrap();

        let checking = before
            .accounts
            .iter()
            .find(|a| a.account_type == AccountType::Checking)
            .unwrap();
        let card = before
            .accounts
            .iter()
            .find(|a| a.account_type == AccountType::CreditCard)
            .unwrap();
        set_account_carry_balance(&conn, &checking.id, Money::from_dollars(500.0)).unwrap();
        set_account_carry_balance(&conn, &card.id, Money::from_dollars(300.0)).unwrap();

        let after = crate::view::MonthView::build_for(&conn, today, today.year(), today.month())
            .unwrap()
            .unwrap();
        assert_eq!(after.whats_left.checking_buffer, Money::from_dollars(500.0));
        assert_eq!(after.whats_left.card_carry, Money::from_dollars(300.0));
        assert_eq!(
            after.whats_left.carry_adjustment,
            Money::from_dollars(-200.0)
        );
        assert_eq!(
            after.whats_left.whats_left,
            before.whats_left.whats_left - Money::from_dollars(200.0)
        );
    }

    #[test]
    fn delete_account_is_blocked_while_referenced_by_transactions() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let account_id =
            create_checking_account(&conn, "Bills", Money::from_dollars(100.0)).unwrap();

        conn.execute(
            "INSERT INTO txn (id, month_id, account_id, label, direction, amount_cents, settled)
             VALUES ('linked', ?1, ?2, 'Linked Bill', 'out', 1000, 0)",
            rusqlite::params![month.id, account_id],
        )
        .unwrap();

        assert!(
            !delete_account(&conn, &account_id).unwrap(),
            "delete is rejected while txns reference the account"
        );
        assert!(
            queries::load_accounts(&conn)
                .unwrap()
                .iter()
                .any(|a| a.id == account_id),
            "blocked account remains"
        );

        conn.execute("UPDATE txn SET account_id = NULL WHERE id = 'linked'", [])
            .unwrap();
        assert!(
            delete_account(&conn, &account_id).unwrap(),
            "delete succeeds once unreferenced"
        );
    }

    #[test]
    fn delete_series_blocked_while_referenced() {
        let conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let s = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let item = add_plan_item(&conn, &plan, &s, Money::from_dollars(10.0)).unwrap();

        assert!(
            delete_series(&conn, &s).is_err(),
            "blocked while a plan uses it"
        );
        delete_plan_item(&conn, &item).unwrap();
        assert!(
            delete_series(&conn, &s).is_ok(),
            "allowed once unreferenced"
        );
    }

    #[test]
    fn adhoc_items_are_hand_entered_and_seriesless() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();

        // A fresh stamped month from the demo has no hand-entered data yet.
        assert!(!month_has_handentered(&conn, &month.id).unwrap());

        // Add an ad-hoc bill and an ad-hoc envelope.
        let txn_id = add_oneoff_txn(
            &conn,
            &month.id,
            "Concert",
            Direction::Out,
            Money::from_dollars(120.0),
        )
        .unwrap();
        let env_id = add_oneoff_envelope(
            &conn,
            &month.id,
            "Fun",
            Money::from_dollars(200.0),
            PeriodType::Monthly,
            Mode::Manual,
        )
        .unwrap();

        // Both are seriesless (the ad-hoc marker) and now count as hand-entered.
        let txn = queries::load_txns(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == txn_id)
            .unwrap();
        assert!(
            txn.series_id.is_none() && txn.envelope_id.is_none() && txn.stamped_amount.is_none()
        );
        let env = queries::load_envelopes(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|e| e.id == env_id)
            .unwrap();
        assert!(env.series_id.is_none());
        assert!(
            month_has_handentered(&conn, &month.id).unwrap(),
            "ad-hoc envelope alone trips the flag"
        );
    }

    #[test]
    fn month_adds_series_backed_budget_items() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let plan_id = queries::plans(&conn).unwrap()[0].id.clone();

        let concert = create_series(
            &conn,
            Kind::Transaction,
            "Concert",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let txn_id =
            add_series_txn_instance(&conn, &month.id, &concert, Money::from_dollars(120.0))
                .unwrap();
        let second_txn_id =
            add_series_txn_instance(&conn, &month.id, &concert, Money::from_dollars(80.0)).unwrap();

        let fun = create_series(
            &conn,
            Kind::Envelope,
            "Fun",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Manual),
        )
        .unwrap();
        let env_id =
            add_series_envelope_instance(&conn, &month.id, &fun, Money::from_dollars(200.0))
                .unwrap();

        let txn = queries::load_txns(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == txn_id)
            .unwrap();
        assert_eq!(txn.series_id.as_deref(), Some(concert.as_str()));
        assert_eq!(txn.stamped_amount, Some(Money::from_dollars(120.0)));
        let second_txn = queries::load_txns(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == second_txn_id)
            .unwrap();
        assert_eq!(second_txn.series_id.as_deref(), Some(concert.as_str()));
        assert_ne!(txn.id, second_txn.id);
        assert_eq!(second_txn.amount, Money::from_dollars(80.0));

        let env = queries::load_envelopes(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|e| e.id == env_id)
            .unwrap();
        assert_eq!(env.series_id.as_deref(), Some(fun.as_str()));
        assert_eq!(env.stamped_amount, Money::from_dollars(200.0));
        assert!(
            month_has_items_outside_plan(&conn, &month.id, &plan_id).unwrap(),
            "series-backed month additions are outside the stamped plan until added there"
        );
    }

    #[test]
    fn replace_can_wipe_or_keep_series_backed_items_outside_plan() {
        fn setup() -> (Connection, String, String, String) {
            let mut conn = db::open_in_memory().unwrap();
            let plan = create_plan(&conn, "P").unwrap();
            let rent = create_series(
                &conn,
                Kind::Transaction,
                "Rent",
                Some(Direction::Out),
                None,
                None,
            )
            .unwrap();
            add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();
            let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
            let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

            let concert = create_series(
                &conn,
                Kind::Transaction,
                "Concert",
                Some(Direction::Out),
                None,
                None,
            )
            .unwrap();
            add_series_txn_instance(&conn, &month_id, &concert, Money::from_dollars(120.0))
                .unwrap();
            (conn, plan, month_id, concert)
        }

        {
            let (mut conn, plan, month_id, concert) = setup();
            restamp_replace(&mut conn, &month_id, &plan, false).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter()
                    .all(|t| t.series_id.as_deref() != Some(concert.as_str())),
                "outside-plan series wiped"
            );
        }

        {
            let (mut conn, plan, month_id, concert) = setup();
            restamp_replace(&mut conn, &month_id, &plan, true).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter()
                    .any(|t| t.series_id.as_deref() == Some(concert.as_str())),
                "outside-plan series kept"
            );
        }
    }

    #[test]
    fn merge_adds_repeated_plan_occurrences_without_collapsing_series() {
        let mut conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let paycheck = create_series(
            &conn,
            Kind::Transaction,
            "Solace Paycheck",
            Some(Direction::In),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&conn, &plan, &paycheck, Money::from_dollars(5000.0)).unwrap();

        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();
        add_plan_item(&conn, &plan, &paycheck, Money::from_dollars(4500.0)).unwrap();

        restamp_merge(&mut conn, &month_id, &plan).unwrap();

        let mut amounts = queries::load_txns(&conn, &month_id)
            .unwrap()
            .into_iter()
            .filter(|t| t.series_id.as_deref() == Some(paycheck.as_str()))
            .map(|t| t.amount)
            .collect::<Vec<_>>();
        amounts.sort_by_key(|amount| amount.cents());
        assert_eq!(
            amounts,
            vec![Money::from_dollars(4500.0), Money::from_dollars(5000.0)]
        );
    }

    #[test]
    fn replace_matches_repeated_occurrences_and_can_keep_extras() {
        fn setup() -> (Connection, String, String, String) {
            let mut conn = db::open_in_memory().unwrap();
            let plan = create_plan(&conn, "P").unwrap();
            let paycheck = create_series(
                &conn,
                Kind::Transaction,
                "Solace Paycheck",
                Some(Direction::In),
                None,
                None,
            )
            .unwrap();
            add_plan_item(&conn, &plan, &paycheck, Money::from_dollars(5000.0)).unwrap();
            let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
            let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();
            add_series_txn_instance(&conn, &month_id, &paycheck, Money::from_dollars(4500.0))
                .unwrap();
            (conn, plan, month_id, paycheck)
        }

        {
            let (mut conn, plan, month_id, paycheck) = setup();
            assert!(month_has_items_outside_plan(&conn, &month_id, &plan).unwrap());
            restamp_replace(&mut conn, &month_id, &plan, false).unwrap();
            let rows = queries::load_txns(&conn, &month_id)
                .unwrap()
                .into_iter()
                .filter(|t| t.series_id.as_deref() == Some(paycheck.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1, "extra occurrence wiped");
            assert_eq!(rows[0].amount, Money::from_dollars(5000.0));
        }

        {
            let (mut conn, plan, month_id, paycheck) = setup();
            restamp_replace(&mut conn, &month_id, &plan, true).unwrap();
            let rows = queries::load_txns(&conn, &month_id)
                .unwrap()
                .into_iter()
                .filter(|t| t.series_id.as_deref() == Some(paycheck.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 2, "extra occurrence kept");
        }
    }

    #[test]
    fn feeding_a_manual_envelope_consumes_it() {
        use crate::calc;
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();

        let env_id = add_oneoff_envelope(
            &conn,
            &month.id,
            "Fun",
            Money::from_dollars(200.0),
            PeriodType::Monthly,
            Mode::Manual,
        )
        .unwrap();
        add_envelope_spending(
            &conn,
            &month.id,
            &env_id,
            "Lunch",
            Money::from_dollars(30.0),
        )
        .unwrap();
        add_envelope_spending(
            &conn,
            &month.id,
            &env_id,
            "Movie",
            Money::from_dollars(20.0),
        )
        .unwrap();

        // Manual consumed = sum of filed spending, independent of elapsed time.
        let env = queries::load_envelopes(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|e| e.id == env_id)
            .unwrap();
        let env_txns = queries::load_txns(&conn, &month.id)
            .unwrap()
            .into_iter()
            .filter(|t| t.envelope_id.as_deref() == Some(env_id.as_str()))
            .collect::<Vec<_>>();
        let consumed = calc::envelope_consumed(&env, Mode::Manual, &env_txns, 0.0);
        assert_eq!(consumed, Money::from_dollars(50.0));
    }

    #[test]
    fn envelope_transactions_require_an_envelope_in_the_same_month() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let current = queries::current_month(&conn).unwrap().unwrap();
        let plan_id = queries::plans(&conn).unwrap()[0].id.clone();
        let other_month = stamp(
            &mut conn,
            &plan_id,
            "2030-01",
            NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
            31,
        )
        .unwrap();
        let manual = add_oneoff_envelope(
            &conn,
            &current.id,
            "Fun",
            Money::from_dollars(200.0),
            PeriodType::Monthly,
            Mode::Manual,
        )
        .unwrap();
        let automatic = add_oneoff_envelope(
            &conn,
            &current.id,
            "Groceries",
            Money::from_dollars(200.0),
            PeriodType::Monthly,
            Mode::Automatic,
        )
        .unwrap();

        assert!(
            add_envelope_spending(
                &conn,
                &other_month,
                &manual,
                "Lunch",
                Money::from_dollars(10.0),
            )
            .is_err()
        );
        let txn_id = add_envelope_spending(
            &conn,
            &current.id,
            &automatic,
            "Lunch",
            Money::from_dollars(10.0),
        )
        .unwrap();
        let recorded = queries::load_envelope_txns(&conn, &current.id, &automatic).unwrap();
        assert!(recorded.iter().any(|txn| txn.id == txn_id));

        let err = conn
            .execute(
                "INSERT INTO txn (id, month_id, envelope_id, label, direction, amount_cents, settled)
                 VALUES (?1, ?2, ?3, 'Lunch', 'out', ?4, 1)",
                rusqlite::params![new_id(), other_month, manual, Money::from_dollars(10.0)],
            )
            .unwrap_err();
        assert!(err.to_string().contains("another month"));
    }

    #[test]
    fn replace_wipes_or_keeps_adhoc_envelope() {
        // Build a month with one plan bill plus an ad-hoc manual envelope that has spending.
        fn setup() -> (Connection, String, String, String) {
            let mut conn = db::open_in_memory().unwrap();
            let plan = create_plan(&conn, "P").unwrap();
            let rent = create_series(
                &conn,
                Kind::Transaction,
                "Rent",
                Some(Direction::Out),
                None,
                None,
            )
            .unwrap();
            add_plan_item(&conn, &plan, &rent, Money::from_dollars(1000.0)).unwrap();
            let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
            let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

            let env_id = add_oneoff_envelope(
                &conn,
                &month_id,
                "Fun",
                Money::from_dollars(200.0),
                PeriodType::Monthly,
                Mode::Manual,
            )
            .unwrap();
            add_envelope_spending(
                &conn,
                &month_id,
                &env_id,
                "Lunch",
                Money::from_dollars(30.0),
            )
            .unwrap();
            (conn, plan, month_id, env_id)
        }

        // Wipe: the ad-hoc envelope and its spending are both gone; the plan bill survives.
        {
            let (mut conn, plan, month_id, env_id) = setup();
            restamp_replace(&mut conn, &month_id, &plan, false).unwrap();
            let envs = queries::load_envelopes(&conn, &month_id).unwrap();
            assert!(envs.iter().all(|e| e.id != env_id), "ad-hoc envelope wiped");
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter().all(|t| t.envelope_id.is_none()),
                "its spending wiped too"
            );
            assert!(txns.iter().any(|t| t.label == "Rent"), "plan bill kept");
        }

        // Keep: the ad-hoc envelope and its spending both remain, still linked.
        {
            let (mut conn, plan, month_id, env_id) = setup();
            restamp_replace(&mut conn, &month_id, &plan, true).unwrap();
            let envs = queries::load_envelopes(&conn, &month_id).unwrap();
            assert!(envs.iter().any(|e| e.id == env_id), "ad-hoc envelope kept");
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter()
                    .any(|t| t.envelope_id.as_deref() == Some(env_id.as_str())),
                "spending still linked"
            );
        }
    }
}
