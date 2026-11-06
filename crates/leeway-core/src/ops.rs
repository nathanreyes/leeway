//! Write operations: the verbs from app-spec §5, plus demo seeding.
//!
//! Each function takes `&Connection` and returns `Result<()>` (or an id). Multi-row
//! operations run inside a transaction so a failure can't leave a half-stamped month.

use crate::calc;
use crate::currency::Currency;
use crate::forecast::{self, ResolvedPlanEntry};
use crate::models::*;
use crate::money::Money;
use crate::queries;
use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Create a random UUID for a new row.
fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Namespace for ids derived from natural keys. Never change it: stored ids depend on it.
const LEEWAY_NAMESPACE: Uuid = Uuid::from_u128(0x6f1c_2b8e_4d3a_4e5f_9a7b_1c2d_3e4f_5a6b);

/// The fixed id of the month labelled `label` (`YYYY-MM`). Every device derives the same id,
/// so a month stamped offline on two devices is one row once they sync.
pub fn month_id(label: &str) -> String {
    Uuid::new_v5(&LEEWAY_NAMESPACE, format!("month:{label}").as_bytes()).to_string()
}

/// The id of the row that stamping makes for plan item `item_id` in `month_id`. Every device
/// derives the same id, so a month stamped offline on two devices holds one copy of each item
/// once they sync. Restamp can need a second row for an item whose row now stands for another
/// occurrence of its series; that row gets a random id.
fn stamped_id(conn: &Connection, table: &str, month_id: &str, item_id: &str) -> Result<String> {
    let id = Uuid::new_v5(
        &LEEWAY_NAMESPACE,
        format!("stamp:{month_id}:{item_id}").as_bytes(),
    )
    .to_string();
    let taken: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?1)"),
        [&id],
        |r| r.get(0),
    )?;
    Ok(if taken { new_id() } else { id })
}

/// Persist the app-wide display currency into the `setting` table (upsert). Because
/// the row lives in the database, backups retain the choice. The caller is
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
    stamp_filtered(conn, plan_id, label, start_date, days_in_month, &[])
}

/// Stamp a plan while leaving selected plan items out of this snapshot. The plan itself
/// stays unchanged; this is the stamp window's one-time "off" choice.
pub fn stamp_filtered(
    conn: &mut Connection,
    plan_id: &str,
    label: &str,
    start_date: NaiveDate,
    days_in_month: i64,
    excluded_item_ids: &[String],
) -> Result<String> {
    // A transaction: everything below commits together or not at all.
    let tx = conn.transaction()?;
    let month_id = stamp_in_transaction(
        &tx,
        plan_id,
        label,
        start_date,
        days_in_month,
        excluded_item_ids,
    )?;
    tx.commit()?;
    Ok(month_id)
}

/// Stamp several months as one write, so a failure cannot leave half the batch behind.
pub fn stamp_batch_filtered(
    conn: &mut Connection,
    plan_id: &str,
    months: &[(String, NaiveDate, i64)],
    excluded_item_ids: &[String],
) -> Result<Vec<String>> {
    let tx = conn.transaction()?;
    let mut month_ids = Vec::with_capacity(months.len());
    for (label, start_date, days_in_month) in months {
        month_ids.push(stamp_in_transaction(
            &tx,
            plan_id,
            label,
            *start_date,
            *days_in_month,
            excluded_item_ids,
        )?);
    }
    tx.commit()?;
    Ok(month_ids)
}

/// Stamp several plans into several new months as one write. Each plan contributes all of
/// its enabled entries to each month.
pub fn stamp_plans_batch_filtered(
    conn: &mut Connection,
    plans: &[(String, Vec<String>)],
    months: &[(String, NaiveDate, i64)],
) -> Result<Vec<String>> {
    if plans.is_empty() {
        anyhow::bail!("select at least one plan");
    }
    let tx = conn.transaction()?;
    let mut month_ids = Vec::with_capacity(months.len());
    for (label, start_date, days_in_month) in months {
        let month_id = month_id(label);
        tx.execute(
            "INSERT INTO month (id, plan_id, label, start_date, days_in_month)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                month_id,
                plans.first().map(|(plan_id, _)| plan_id),
                label,
                start_date.format("%Y-%m-%d").to_string(),
                days_in_month,
            ],
        )?;
        for (plan_id, excluded_item_ids) in plans {
            insert_plan_instances(
                &tx,
                &month_id,
                plan_id,
                *start_date,
                *days_in_month,
                excluded_item_ids,
            )?;
        }
        month_ids.push(month_id);
    }
    tx.commit()?;
    Ok(month_ids)
}

fn stamp_in_transaction(
    conn: &Connection,
    plan_id: &str,
    label: &str,
    start_date: NaiveDate,
    days_in_month: i64,
    excluded_item_ids: &[String],
) -> Result<String> {
    let month_id = month_id(label);

    conn.execute(
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

    insert_plan_instances(
        conn,
        &month_id,
        plan_id,
        start_date,
        days_in_month,
        excluded_item_ids,
    )?;

    Ok(month_id)
}

fn insert_plan_instances(
    conn: &Connection,
    month_id: &str,
    plan_id: &str,
    start_date: NaiveDate,
    days_in_month: i64,
    excluded_item_ids: &[String],
) -> Result<()> {
    let mut entries = forecast::resolve_plan_entries(conn, plan_id, start_date, days_in_month)?;
    entries.retain(|entry| !excluded_item_ids.contains(&entry.entry.item_id));

    // The caller owns the transaction. The resolver has already removed off-season entries
    // and chosen one amount for each remaining item. The instance keeps no live link to it.
    for entry in entries {
        insert_instance_from_entry(conn, month_id, &entry)?;
    }
    Ok(())
}

/// Insert one fresh instance (envelope or standalone txn) for a plan entry into a month.
/// The intrinsic fields come from the shared `series`; the amount from this plan's item.
/// `stamped_amount == amount` at stamp time. Shared by fresh stamp and restamp.
fn insert_instance_from_entry(
    conn: &Connection,
    month_id: &str,
    resolved: &ResolvedPlanEntry,
) -> Result<()> {
    let entry = &resolved.entry;
    let s = &entry.series;
    match s.kind {
        Kind::Envelope => {
            let amount = resolved.amount;
            conn.execute(
                "INSERT INTO envelope
                   (id, month_id, series_id, label, amount_cents,
                    stamped_amount_cents, period_type, mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    stamped_id(conn, "envelope", month_id, &entry.item_id)?,
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
                    stamped_id(conn, "txn", month_id, &entry.item_id)?,
                    month_id,
                    s.id,
                    s.label,
                    s.direction.unwrap_or(Direction::Out),
                    resolved.amount,
                    resolved.amount,
                ],
            )?;
        }
    }
    Ok(())
}

// --- Restamp -------------------------------------------------------------------

/// Resolve the plan entries that apply to an already-stamped month, deriving the target
/// date and day count from its `month` row.
fn active_entries_for_month(
    conn: &Connection,
    month_id: &str,
    plan_id: &str,
    excluded_item_ids: &[String],
) -> Result<Vec<ResolvedPlanEntry>> {
    let month = queries::month_by_id(conn, month_id)?
        .with_context(|| format!("month not found: {month_id}"))?;
    let mut entries =
        forecast::resolve_plan_entries(conn, plan_id, month.start_date, month.days_in_month)?;
    entries.retain(|entry| !excluded_item_ids.contains(&entry.entry.item_id));
    Ok(entries)
}

/// Legacy predicate for seriesless hand-entered data — standalone one-offs,
/// manual-envelope spending, OR an ad-hoc envelope. The restamp UI uses
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

/// Does this month contain data outside the target plan? This decides whether restamp
/// offers to remove items the plan doesn't have. Membership is by plan entry, not by
/// whether a row carries a series id; envelope spending follows its envelope.
pub fn month_has_items_outside_plan(
    conn: &Connection,
    month_id: &str,
    plan_id: &str,
) -> Result<bool> {
    let entries = queries::load_plan_entries(conn, plan_id)?;
    let mut remaining = occurrence_counts(&entries);

    for txn in queries::load_txns(conn, month_id)? {
        if txn.envelope_id.is_some() {
            // Spending belongs to its envelope; only the envelope can be outside the plan.
            continue;
        }
        let Some(series_id) = txn.series_id.as_deref() else {
            return Ok(true);
        };
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

/// Restamp a plan into an existing month, refreshing the planned baseline.
/// - A matching envelope is refreshed in place, so its spending stays linked.
/// - A matching **unsettled** transaction is refreshed to the plan's values.
/// - A matching **settled** transaction is left untouched (it holds a real actual).
/// - A plan entry with no instance is inserted.
///
/// Repeated occurrences of a series are matched in stable row order. With
/// `remove_unplanned`, rows the plan doesn't have are deleted: unmatched standalone
/// transactions (settled or not) and unmatched envelopes with their spending. An item the
/// plan doesn't run in this month is not an entry here, so it counts as one the plan
/// doesn't have — that is how a seasonal envelope stamped into the wrong month is cleaned
/// up. Without the option nothing is deleted.
pub fn restamp(
    conn: &mut Connection,
    month_id: &str,
    plan_id: &str,
    remove_unplanned: bool,
) -> Result<()> {
    restamp_filtered(conn, month_id, plan_id, remove_unplanned, &[])
}

/// Restamp a month while honoring one-time item exclusions from the stamp window.
pub fn restamp_filtered(
    conn: &mut Connection,
    month_id: &str,
    plan_id: &str,
    remove_unplanned: bool,
    excluded_item_ids: &[String],
) -> Result<()> {
    let tx = conn.transaction()?;
    let entries = active_entries_for_month(&tx, month_id, plan_id, excluded_item_ids)?;
    let txns = queries::load_txns(&tx, month_id)?;
    let envelopes = queries::load_envelopes(&tx, month_id)?;
    let mut matched_txns: HashSet<String> = HashSet::new();
    let mut matched_envelopes: HashSet<String> = HashSet::new();

    for entry in &entries {
        match entry.entry.series.kind {
            Kind::Envelope => {
                if let Some(envelope) =
                    next_matching_envelope(&envelopes, &matched_envelopes, &entry.entry.series.id)
                {
                    matched_envelopes.insert(envelope.id.clone());
                    refresh_envelope(&tx, &envelope.id, entry)?;
                } else {
                    insert_instance_from_entry(&tx, month_id, entry)?;
                }
            }
            Kind::Transaction => {
                if let Some(txn) = next_matching_txn(&txns, &matched_txns, &entry.entry.series.id) {
                    matched_txns.insert(txn.id.clone());
                    if !txn.settled {
                        refresh_txn(&tx, &txn.id, entry)?;
                    }
                } else {
                    insert_instance_from_entry(&tx, month_id, entry)?;
                }
            }
        }
    }

    // A what-if's removal is intent: rows for an item it removes go without the option,
    // except paid ones and envelopes holding spending. The option still removes those.
    let month = queries::month_by_id(&tx, month_id)?
        .with_context(|| format!("month not found: {month_id}"))?;
    let mut seen_txns = matched_txns.clone();
    let mut seen_envelopes = matched_envelopes.clone();
    for removed in queries::removed_entries(&tx, plan_id)?
        .iter()
        .filter(|entry| entry.runs_in(month.start_date))
    {
        match removed.series.kind {
            Kind::Envelope => {
                if let Some(envelope) =
                    next_matching_envelope(&envelopes, &seen_envelopes, &removed.series.id)
                {
                    seen_envelopes.insert(envelope.id.clone());
                    let spent = txns
                        .iter()
                        .any(|t| t.envelope_id.as_deref() == Some(&envelope.id));
                    if !spent {
                        tx.execute("DELETE FROM envelope WHERE id = ?1", [&envelope.id])?;
                        matched_envelopes.insert(envelope.id.clone());
                    }
                }
            }
            Kind::Transaction => {
                if let Some(txn) = next_matching_txn(&txns, &seen_txns, &removed.series.id) {
                    seen_txns.insert(txn.id.clone());
                    if !txn.settled {
                        tx.execute("DELETE FROM txn WHERE id = ?1", [&txn.id])?;
                        matched_txns.insert(txn.id.clone());
                    }
                }
            }
        }
    }

    if remove_unplanned {
        // Spending is never matched itself; it lives or dies with its envelope.
        for txn in &txns {
            if txn.envelope_id.is_none() && !matched_txns.contains(&txn.id) {
                tx.execute("DELETE FROM txn WHERE id = ?1", [&txn.id])?;
            }
        }
        for envelope in &envelopes {
            if !matched_envelopes.contains(&envelope.id) {
                tx.execute("DELETE FROM txn WHERE envelope_id = ?1", [&envelope.id])?;
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
fn refresh_envelope(conn: &Connection, id: &str, resolved: &ResolvedPlanEntry) -> Result<()> {
    let entry = &resolved.entry;
    let s = &entry.series;
    let amount = resolved.amount;
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

/// Reset an unsettled standalone txn instance to a plan entry's planned values.
fn refresh_txn(conn: &Connection, id: &str, resolved: &ResolvedPlanEntry) -> Result<()> {
    let entry = &resolved.entry;
    let s = &entry.series;
    conn.execute(
        "UPDATE txn
         SET amount_cents = ?1, stamped_amount_cents = ?2, label = ?3,
             direction = ?4
         WHERE id = ?5",
        rusqlite::params![
            resolved.amount,
            resolved.amount,
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

/// Create an empty plan and return its id. The first plan becomes Main.
pub fn create_plan(conn: &Connection, name: &str) -> Result<String> {
    let id = new_id();
    conn.execute(
        "INSERT INTO plan (id, name) VALUES (?1, ?2)",
        rusqlite::params![id, name],
    )?;
    if queries::main_plan_id(conn)?.is_none() {
        set_main_plan(conn, &id)?;
    }
    Ok(id)
}

fn set_main_plan(conn: &Connection, plan_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('main_plan_id', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [plan_id],
    )?;
    Ok(())
}

/// Name Main again: the plan, not a what-if, with the most stamped months (ties: most
/// items, then name). With no plans, clears the setting.
pub fn reset_main_plan(conn: &Connection) -> Result<()> {
    let id: Option<String> = conn
        .query_row(
            "SELECT p.id FROM plan p
             WHERE p.base_plan_id IS NULL
             ORDER BY (SELECT COUNT(*) FROM month m WHERE m.plan_id = p.id) DESC,
                      (SELECT COUNT(*) FROM plan_item pi WHERE pi.plan_id = p.id) DESC,
                      p.name, p.id
             LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => set_main_plan(conn, &id),
        None => {
            conn.execute("DELETE FROM setting WHERE key = 'main_plan_id'", [])?;
            Ok(())
        }
    }
}

// --- What-ifs ------------------------------------------------------------------

/// Create an empty what-if on Main and return its id. With no changes it stamps exactly
/// as Main does.
pub fn create_what_if(conn: &Connection, name: &str) -> Result<String> {
    let main = queries::main_plan_id(conn)?.context("create a plan first")?;
    let id = new_id();
    conn.execute(
        "INSERT INTO plan (id, name, base_plan_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, name, main],
    )?;
    Ok(id)
}

/// The `plan_item` row to edit for `item_id` as seen in `plan_id`. On a what-if, editing
/// one of Main's items makes (or reuses) the what-if's changed copy of it, so the edit
/// stays in the what-if. Anywhere else it's `item_id` itself.
pub fn editable_item(conn: &Connection, plan_id: &str, item_id: &str) -> Result<String> {
    let Some(base_item) = base_item_in(conn, plan_id, item_id)? else {
        return Ok(item_id.to_string());
    };
    if let Some(change) = change_row(conn, plan_id, &base_item)? {
        conn.execute("UPDATE plan_item SET removed = 0 WHERE id = ?1", [&change])?;
        return Ok(change);
    }
    let id = new_id();
    conn.execute(
        "INSERT INTO plan_item
           (id, plan_id, series_id, amount_cents, active_months, forecast_method,
            starts_on, ends_on, base_item_id)
         SELECT ?1, ?2, series_id, amount_cents, active_months, forecast_method,
                starts_on, ends_on, id
         FROM plan_item WHERE id = ?3",
        rusqlite::params![id, plan_id, base_item],
    )?;
    Ok(id)
}

/// Take `item_id` out of `plan_id`. On a what-if, one of Main's items is marked removed
/// rather than deleted; anything else is deleted outright.
pub fn remove_plan_item(conn: &Connection, plan_id: &str, item_id: &str) -> Result<()> {
    if base_item_in(conn, plan_id, item_id)?.is_some() {
        let change = editable_item(conn, plan_id, item_id)?;
        conn.execute("UPDATE plan_item SET removed = 1 WHERE id = ?1", [&change])?;
        return Ok(());
    }
    delete_plan_item(conn, item_id)
}

/// Undo a what-if's change to one of Main's items, so it follows Main again.
pub fn use_main_version(conn: &Connection, plan_id: &str, item_id: &str) -> Result<()> {
    if let Some(base_item) = base_item_in(conn, plan_id, item_id)?
        && let Some(change) = change_row(conn, plan_id, &base_item)?
    {
        conn.execute("DELETE FROM plan_item WHERE id = ?1", [&change])?;
    }
    Ok(())
}

/// Write a what-if's changes into Main and delete the what-if: changed items overwrite
/// Main's, removed ones leave Main, and added ones move to Main.
pub fn apply_what_if(conn: &mut Connection, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let plan = queries::get_plan(&tx, plan_id)?.context("plan not found")?;
    let main = plan
        .base_plan_id
        .context("only a what-if has changes to apply")?;
    let changes: Vec<(String, String, bool)> = tx
        .prepare(
            "SELECT id, base_item_id, removed FROM plan_item
             WHERE plan_id = ?1 AND base_item_id IS NOT NULL",
        )?
        .query_map([plan_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (change, base_item, removed) in changes {
        if removed {
            // Deleting Main's item also deletes every what-if's change to it.
            delete_plan_item(&tx, &base_item)?;
            continue;
        }
        tx.execute(
            "UPDATE plan_item SET
               (amount_cents, active_months, forecast_method, starts_on, ends_on) =
               (SELECT amount_cents, active_months, forecast_method, starts_on, ends_on
                FROM plan_item WHERE id = ?1)
             WHERE id = ?2",
            [&change, &base_item],
        )?;
        tx.execute("DELETE FROM plan_item WHERE id = ?1", [&change])?;
    }
    tx.execute(
        "UPDATE plan_item SET plan_id = ?1 WHERE plan_id = ?2",
        [&main, plan_id],
    )?;
    tx.execute("DELETE FROM plan WHERE id = ?1", [plan_id])?;
    tx.commit()?;
    Ok(())
}

/// Turn a what-if into a plan of its own that no longer follows Main. Its items stay as
/// they resolve today; changed items keep their ids.
pub fn make_standalone(conn: &mut Connection, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let plan = queries::get_plan(&tx, plan_id)?.context("plan not found")?;
    if !plan.is_what_if() {
        return Ok(());
    }
    let entries = queries::load_plan_entries(&tx, plan_id)?;
    tx.execute(
        "DELETE FROM plan_item WHERE plan_id = ?1 AND removed = 1",
        [plan_id],
    )?;
    tx.execute(
        "UPDATE plan_item SET base_item_id = NULL WHERE plan_id = ?1",
        [plan_id],
    )?;
    for entry in entries.iter().filter(|e| e.origin == ItemOrigin::Inherited) {
        tx.execute(
            "INSERT INTO plan_item
               (id, plan_id, series_id, amount_cents, active_months, forecast_method,
                starts_on, ends_on)
             SELECT ?1, ?2, series_id, amount_cents, active_months, forecast_method,
                    starts_on, ends_on
             FROM plan_item WHERE id = ?3",
            rusqlite::params![new_id(), plan_id, entry.item_id],
        )?;
    }
    tx.execute(
        "UPDATE plan SET base_plan_id = NULL WHERE id = ?1",
        [plan_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Move a standalone plan's items into Main and delete the plan. This is how an add-on
/// plan from before what-ifs joins Main.
pub fn merge_into_main(conn: &mut Connection, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let main = queries::main_plan_id(&tx)?.context("there is no main plan")?;
    let plan = queries::get_plan(&tx, plan_id)?.context("plan not found")?;
    if plan.id == main || plan.is_what_if() {
        anyhow::bail!("only a standalone plan can merge into Main");
    }
    if !queries::what_ifs_of(&tx, plan_id)?.is_empty() {
        anyhow::bail!("this plan has what-ifs");
    }
    tx.execute(
        "UPDATE plan_item SET plan_id = ?1 WHERE plan_id = ?2",
        [&main, plan_id],
    )?;
    tx.execute("DELETE FROM plan WHERE id = ?1", [plan_id])?;
    tx.commit()?;
    Ok(())
}

/// If `plan_id` is a what-if and `item_id` is one of Main's items, Main's item id.
fn base_item_in(conn: &Connection, plan_id: &str, item_id: &str) -> Result<Option<String>> {
    let found = conn
        .query_row(
            "SELECT pi.id FROM plan p JOIN plan_item pi ON pi.plan_id = p.base_plan_id
             WHERE p.id = ?1 AND pi.id = ?2",
            [plan_id, item_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found)
}

/// The what-if's row that changes or removes Main's item `base_item`.
fn change_row(conn: &Connection, plan_id: &str, base_item: &str) -> Result<Option<String>> {
    let id = conn
        .query_row(
            "SELECT id FROM plan_item WHERE plan_id = ?1 AND base_item_id = ?2",
            [plan_id, base_item],
            |r| r.get(0),
        )
        .optional()?;
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
///
/// Main can't be deleted while it has what-ifs; once it's gone another plan becomes Main.
pub fn delete_plan(conn: &mut Connection, plan_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    if !queries::what_ifs_of(&tx, plan_id)?.is_empty() {
        anyhow::bail!("make this plan's what-ifs standalone or delete them first");
    }
    // A what-if's changes point at Main's items, so they go before the items they could
    // otherwise block.
    tx.execute(
        "DELETE FROM plan_item WHERE plan_id = ?1 AND base_item_id IS NOT NULL",
        [plan_id],
    )?;
    tx.execute("DELETE FROM plan_item WHERE plan_id = ?1", [plan_id])?;
    tx.execute("DELETE FROM plan WHERE id = ?1", [plan_id])?;
    if queries::main_plan_id(&tx)?.is_none() {
        reset_main_plan(&tx)?;
    }
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
    drop_change_matching_main(conn, item_id)
}

/// Remember which amount source this plan item should try on its next stamp. Historical
/// methods fall back to the static amount when their required observations are absent.
pub fn set_item_forecast_method(
    conn: &Connection,
    item_id: &str,
    method: ForecastMethod,
) -> Result<()> {
    conn.execute(
        "UPDATE plan_item SET forecast_method = ?1 WHERE id = ?2",
        rusqlite::params![method, item_id],
    )?;
    drop_change_matching_main(conn, item_id)
}

/// Per-plan: which months this plan stamps the item in. Like the amount, this edits
/// `plan_item` — the same series can be seasonal in one plan and always-on in another.
/// [`MonthSet::ALL`] writes NULL, so an item the user never restricted is indistinguishable
/// from one that predates the column.
pub fn set_item_active_months(conn: &Connection, item_id: &str, months: MonthSet) -> Result<()> {
    conn.execute(
        "UPDATE plan_item SET active_months = ?1 WHERE id = ?2",
        rusqlite::params![months.to_db(), item_id],
    )?;
    drop_change_matching_main(conn, item_id)
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
///
/// What-ifs' changes to the item go with it.
pub fn delete_plan_item(conn: &Connection, item_id: &str) -> Result<()> {
    conn.execute("DELETE FROM plan_item WHERE base_item_id = ?1", [item_id])?;
    conn.execute("DELETE FROM plan_item WHERE id = ?1", [item_id])?;
    Ok(())
}

/// Per-plan: the first and last month the item runs (`YYYY-MM`), `None` for open-ended.
/// A one-off has both set to the same month.
pub fn set_item_dates(
    conn: &Connection,
    item_id: &str,
    starts_on: Option<&str>,
    ends_on: Option<&str>,
) -> Result<()> {
    let starts = starts_on.map(month_label).transpose()?;
    let ends = ends_on.map(month_label).transpose()?;
    if let (Some(starts), Some(ends)) = (&starts, &ends)
        && starts > ends
    {
        anyhow::bail!("the last month comes before the first");
    }
    conn.execute(
        "UPDATE plan_item SET starts_on = ?1, ends_on = ?2 WHERE id = ?3",
        rusqlite::params![starts, ends, item_id],
    )?;
    drop_change_matching_main(conn, item_id)
}

/// A what-if's change that has come back to match Main's item exactly is no change: delete
/// it, so the item follows Main again.
fn drop_change_matching_main(conn: &Connection, item_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM plan_item AS c
         WHERE c.id = ?1 AND c.removed = 0
           AND EXISTS (SELECT 1 FROM plan_item b
                       WHERE b.id = c.base_item_id
                         AND b.amount_cents = c.amount_cents
                         AND b.active_months IS c.active_months
                         AND b.forecast_method = c.forecast_method
                         AND b.starts_on IS c.starts_on
                         AND b.ends_on IS c.ends_on)",
        [item_id],
    )?;
    Ok(())
}

/// `label` as a stored month (`YYYY-MM`), or an error naming it.
fn month_label(label: &str) -> Result<String> {
    let month = parse_month_label(label).with_context(|| format!("“{label}” isn't a month"))?;
    Ok(month.format("%Y-%m").to_string())
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
    if queries::main_plan_id(conn)?.is_none() {
        set_main_plan(conn, &plan_id)?;
    }

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
    fn stamped_months_get_ids_from_their_labels() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let plan_id = queries::current_month(&conn)
            .unwrap()
            .unwrap()
            .plan_id
            .unwrap();

        let start = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        let id = stamp(&mut conn, &plan_id, "2099-01", start, 31).unwrap();
        assert_eq!(id, month_id("2099-01"));
        assert_ne!(month_id("2099-01"), month_id("2099-02"));
    }

    /// Ids of a month's stamped envelopes and standalone transactions, sorted.
    fn stamped_row_ids(conn: &Connection, month_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = queries::load_envelopes(conn, month_id)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .chain(
                queries::load_txns(conn, month_id)
                    .unwrap()
                    .into_iter()
                    .filter(|t| t.envelope_id.is_none())
                    .map(|t| t.id),
            )
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn stamping_a_month_on_two_devices_makes_the_same_rows() {
        let mut phone = db::open_in_memory().unwrap();
        seed_starter(&mut phone).unwrap();
        let plan_id = queries::current_month(&phone)
            .unwrap()
            .unwrap()
            .plan_id
            .unwrap();
        // A second device holding the same budget, synced before either stamps.
        let mut tablet = Connection::open_in_memory().unwrap();
        rusqlite::backup::Backup::new(&phone, &mut tablet)
            .unwrap()
            .run_to_completion(64, std::time::Duration::ZERO, None)
            .unwrap();

        let start = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        let month = stamp(&mut phone, &plan_id, "2099-01", start, 31).unwrap();
        stamp(&mut tablet, &plan_id, "2099-01", start, 31).unwrap();

        let ids = stamped_row_ids(&phone, &month);
        assert_eq!(ids.len(), 10);
        assert_eq!(ids, stamped_row_ids(&tablet, &month));
    }

    #[test]
    fn restamp_brings_back_a_deleted_row_with_its_id() {
        let mut conn = db::open_in_memory().unwrap();
        seed_starter(&mut conn).unwrap();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let plan_id = month.plan_id.unwrap();
        let rent = queries::load_txns(&conn, &month.id)
            .unwrap()
            .into_iter()
            .find(|t| t.label == "Rent/Mortgage")
            .unwrap();

        delete_txn(&conn, &rent.id).unwrap();
        restamp(&mut conn, &month.id, &plan_id, false).unwrap();

        assert!(
            queries::load_txns(&conn, &month.id)
                .unwrap()
                .iter()
                .any(|t| t.id == rent.id)
        );
        let tombstones: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM deletion WHERE row_id = ?1",
                [&rent.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tombstones, 0);
    }

    #[test]
    fn restamp_adds_a_row_when_another_occurrence_holds_its_id() {
        // Two items share a series. Deleting either row and restamping must give two rows
        // again, even when the surviving row carries the id the missing item would derive.
        for deleted in 0..2 {
            let mut conn = db::open_in_memory().unwrap();
            let plan = create_plan(&conn, "P").unwrap();
            let paycheck = create_series(
                &conn,
                Kind::Transaction,
                "Paycheck",
                Some(Direction::In),
                None,
                None,
            )
            .unwrap();
            add_plan_item(&conn, &plan, &paycheck, Money::from_dollars(5000.0)).unwrap();
            add_plan_item(&conn, &plan, &paycheck, Money::from_dollars(4500.0)).unwrap();
            let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
            let month_id = stamp(&mut conn, &plan, "2026-09", start, 30).unwrap();

            let rows = queries::load_txns(&conn, &month_id).unwrap();
            delete_txn(&conn, &rows[deleted].id).unwrap();
            restamp(&mut conn, &month_id, &plan, false).unwrap();

            assert_eq!(queries::load_txns(&conn, &month_id).unwrap().len(), 2);
        }
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

    /// A plan holding one always-on bill and one envelope restricted to March, July, and
    /// November — the birthday-gifts shape. Returns `(conn, plan_id, gifts_series_id)`.
    fn seasonal_plan() -> (Connection, String, String) {
        let conn = db::open_in_memory().unwrap();
        let plan_id = create_plan(&conn, "Baseline").unwrap();

        let rent = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&conn, &plan_id, &rent, Money::from_dollars(1500.0)).unwrap();

        let gifts = create_series(
            &conn,
            Kind::Envelope,
            "Kid gifts",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Automatic),
        )
        .unwrap();
        let item = add_plan_item(&conn, &plan_id, &gifts, Money::from_dollars(120.0)).unwrap();
        set_item_active_months(&conn, &item, MonthSet::parse("mar,jul,nov").unwrap()).unwrap();

        (conn, plan_id, gifts)
    }

    #[test]
    fn active_months_default_to_every_month() {
        let conn = db::open_in_memory().unwrap();
        let plan_id = create_plan(&conn, "P").unwrap();
        let series = create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&conn, &plan_id, &series, Money::from_dollars(10.0)).unwrap();

        // A plan item nobody restricted stores NULL and reads back as ALL, so every row
        // written before the column existed keeps its old behavior.
        let entries = queries::load_plan_entries(&conn, &plan_id).unwrap();
        assert_eq!(entries[0].active_months, MonthSet::ALL);
        let stored: Option<i64> = conn
            .query_row("SELECT active_months FROM plan_item", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);

        // Setting it back to "all" returns the column to NULL rather than a full mask.
        set_item_active_months(&conn, &entries[0].item_id, MonthSet::parse("mar").unwrap())
            .unwrap();
        set_item_active_months(&conn, &entries[0].item_id, MonthSet::ALL).unwrap();
        let stored: Option<i64> = conn
            .query_row("SELECT active_months FROM plan_item", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
    }

    #[test]
    fn stamp_skips_items_outside_their_active_months() {
        let (mut conn, plan_id, _) = seasonal_plan();

        let march = NaiveDate::from_ymd_opt(2027, 3, 1).unwrap();
        let in_season = stamp(&mut conn, &plan_id, "2027-03", march, 31).unwrap();
        let april = NaiveDate::from_ymd_opt(2027, 4, 1).unwrap();
        let off_season = stamp(&mut conn, &plan_id, "2027-04", april, 30).unwrap();

        let stamped_envelopes = queries::load_envelopes(&conn, &in_season).unwrap();
        assert_eq!(stamped_envelopes.len(), 1);
        assert_eq!(stamped_envelopes[0].amount, Money::from_dollars(120.0));
        assert!(
            queries::load_envelopes(&conn, &off_season)
                .unwrap()
                .is_empty()
        );

        // The always-on bill is untouched by any of this.
        assert_eq!(queries::load_txns(&conn, &in_season).unwrap().len(), 1);
        assert_eq!(queries::load_txns(&conn, &off_season).unwrap().len(), 1);
    }

    #[test]
    fn restamp_neither_adds_nor_removes_an_out_of_season_item() {
        let (mut conn, plan_id, gifts) = seasonal_plan();
        let april = NaiveDate::from_ymd_opt(2027, 4, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_id, "2027-04", april, 30).unwrap();

        // Restamping an off-season month does not bring the seasonal item in.
        restamp(&mut conn, &month_id, &plan_id, false).unwrap();
        assert!(
            queries::load_envelopes(&conn, &month_id)
                .unwrap()
                .is_empty()
        );

        // And once one is there by hand, a plain restamp leaves it alone — it never deletes.
        add_series_envelope_instance(&conn, &month_id, &gifts, Money::from_dollars(80.0)).unwrap();
        restamp(&mut conn, &month_id, &plan_id, false).unwrap();
        let envelopes = queries::load_envelopes(&conn, &month_id).unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].amount, Money::from_dollars(80.0));
    }

    #[test]
    fn removal_treats_an_out_of_season_item_as_outside_the_plan() {
        let (mut conn, plan_id, gifts) = seasonal_plan();
        let april = NaiveDate::from_ymd_opt(2027, 4, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_id, "2027-04", april, 30).unwrap();
        add_series_envelope_instance(&conn, &month_id, &gifts, Money::from_dollars(80.0)).unwrap();

        // Without removal it stays, because April isn't one of its months.
        restamp(&mut conn, &month_id, &plan_id, false).unwrap();
        assert_eq!(queries::load_envelopes(&conn, &month_id).unwrap().len(), 1);

        // With removal it goes — this is how a gift envelope stamped into the wrong month
        // gets cleaned up.
        restamp(&mut conn, &month_id, &plan_id, true).unwrap();
        assert!(
            queries::load_envelopes(&conn, &month_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restamp_refreshes_an_in_season_item() {
        let (mut conn, plan_id, _) = seasonal_plan();
        let july = NaiveDate::from_ymd_opt(2027, 7, 1).unwrap();
        let month_id = stamp(&mut conn, &plan_id, "2027-07", july, 31).unwrap();

        let envelope = queries::load_envelopes(&conn, &month_id).unwrap().remove(0);
        set_envelope_amount(&conn, &envelope.id, Money::from_dollars(5.0)).unwrap();

        restamp(&mut conn, &month_id, &plan_id, true).unwrap();
        let envelopes = queries::load_envelopes(&conn, &month_id).unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0].id, envelope.id,
            "reset in place, not recreated"
        );
        assert_eq!(envelopes[0].amount, Money::from_dollars(120.0));
    }

    /// Main with rent $1,500, dining $200, and groceries $600, plus a what-if on it.
    fn main_and_what_if() -> (Connection, String, String, [String; 3]) {
        let conn = db::open_in_memory().unwrap();
        let main = create_plan(&conn, "Main").unwrap();
        let rent = add_new_transaction(
            &conn,
            &main,
            "Rent",
            Direction::Out,
            Money::from_dollars(1500.0),
        )
        .unwrap();
        let dining = add_new_transaction(
            &conn,
            &main,
            "Dining",
            Direction::Out,
            Money::from_dollars(200.0),
        )
        .unwrap();
        let groceries =
            add_new_envelope(&conn, &main, "Groceries", Money::from_dollars(600.0)).unwrap();
        let what_if = create_what_if(&conn, "Tight month").unwrap();
        (conn, main, what_if, [rent, dining, groceries])
    }

    fn amounts(conn: &Connection, plan_id: &str) -> Vec<(String, Money)> {
        queries::load_plan_entries(conn, plan_id)
            .unwrap()
            .into_iter()
            .map(|e| (e.series.label, e.amount))
            .collect()
    }

    #[test]
    fn first_plan_becomes_main() {
        let (conn, main, what_if, _) = main_and_what_if();
        assert_eq!(queries::main_plan_id(&conn).unwrap(), Some(main.clone()));
        let plan = queries::get_plan(&conn, &what_if).unwrap().unwrap();
        assert_eq!(plan.base_plan_id, Some(main));
    }

    #[test]
    fn what_if_follows_main_except_where_it_changes() {
        let (conn, main, what_if, [rent, dining, groceries]) = main_and_what_if();
        assert_eq!(amounts(&conn, &what_if), amounts(&conn, &main));

        let change = editable_item(&conn, &what_if, &groceries).unwrap();
        set_item_amount(&conn, &change, Money::from_dollars(450.0)).unwrap();
        remove_plan_item(&conn, &what_if, &dining).unwrap();
        add_new_transaction(
            &conn,
            &what_if,
            "Car",
            Direction::Out,
            Money::from_dollars(340.0),
        )
        .unwrap();
        // Main's later edits reach the what-if.
        set_item_amount(&conn, &rent, Money::from_dollars(1600.0)).unwrap();

        let entries = queries::load_plan_entries(&conn, &what_if).unwrap();
        let labels: Vec<_> = entries.iter().map(|e| e.series.label.as_str()).collect();
        assert_eq!(labels, ["Groceries", "Rent", "Car"]);
        assert_eq!(
            entries[0].item_id, groceries,
            "changed items keep Main's id"
        );
        assert_eq!(
            entries[0].origin,
            ItemOrigin::Changed {
                base_amount: Money::from_dollars(600.0)
            }
        );
        assert_eq!(entries[0].amount, Money::from_dollars(450.0));
        assert_eq!(entries[1].amount, Money::from_dollars(1600.0));
        assert_eq!(entries[1].origin, ItemOrigin::Inherited);
        assert_eq!(entries[2].origin, ItemOrigin::Own);
        // Main is untouched.
        assert_eq!(
            amounts(&conn, &main),
            [
                ("Groceries".to_string(), Money::from_dollars(600.0)),
                ("Rent".to_string(), Money::from_dollars(1600.0)),
                ("Dining".to_string(), Money::from_dollars(200.0)),
            ]
        );

        use_main_version(&conn, &what_if, &groceries).unwrap();
        assert_eq!(
            queries::load_plan_entries(&conn, &what_if).unwrap()[0].amount,
            Money::from_dollars(600.0)
        );
    }

    #[test]
    fn a_change_back_to_mains_values_is_no_change() {
        let (conn, _, what_if, [_, _, groceries]) = main_and_what_if();
        let change = editable_item(&conn, &what_if, &groceries).unwrap();
        set_item_amount(&conn, &change, Money::from_dollars(450.0)).unwrap();
        assert_eq!(
            queries::load_plan_entries(&conn, &what_if).unwrap()[0].origin,
            ItemOrigin::Changed {
                base_amount: Money::from_dollars(600.0)
            }
        );
        set_item_amount(&conn, &change, Money::from_dollars(600.0)).unwrap();
        assert_eq!(
            queries::load_plan_entries(&conn, &what_if).unwrap()[0].origin,
            ItemOrigin::Inherited
        );
    }

    #[test]
    fn stamping_a_what_if_and_main_give_the_same_row_ids() {
        let (mut conn, main, what_if, [_, _, groceries]) = main_and_what_if();
        let change = editable_item(&conn, &what_if, &groceries).unwrap();
        set_item_amount(&conn, &change, Money::from_dollars(450.0)).unwrap();
        let start = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let from_main = stamp(&mut conn, &main, "2026-12", start, 31).unwrap();
        let ids = stamped_row_ids(&conn, &from_main);
        restamp(&mut conn, &from_main, &what_if, false).unwrap();
        assert_eq!(stamped_row_ids(&conn, &from_main), ids);
        let envelope = &queries::load_envelopes(&conn, &from_main).unwrap()[0];
        assert_eq!(envelope.amount, Money::from_dollars(450.0));
    }

    #[test]
    fn restamping_from_a_what_if_drops_what_it_removes_unless_paid() {
        let (mut conn, main, what_if, [rent, dining, groceries]) = main_and_what_if();
        let start = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let month = stamp(&mut conn, &main, "2026-12", start, 31).unwrap();
        let rent_row = queries::load_txns(&conn, &month)
            .unwrap()
            .into_iter()
            .find(|t| t.label == "Rent")
            .unwrap();
        mark_paid(&conn, &rent_row.id, Money::from_dollars(1500.0), None).unwrap();
        for item in [&rent, &dining, &groceries] {
            remove_plan_item(&conn, &what_if, item).unwrap();
        }

        restamp(&mut conn, &month, &what_if, false).unwrap();

        let txns: Vec<_> = queries::load_txns(&conn, &month)
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(txns, ["Rent"], "paid rent stays; unpaid dining goes");
        assert!(queries::load_envelopes(&conn, &month).unwrap().is_empty());
    }

    #[test]
    fn apply_what_if_writes_its_changes_into_main() {
        let (mut conn, main, what_if, [_, dining, groceries]) = main_and_what_if();
        let change = editable_item(&conn, &what_if, &groceries).unwrap();
        set_item_amount(&conn, &change, Money::from_dollars(450.0)).unwrap();
        remove_plan_item(&conn, &what_if, &dining).unwrap();
        add_new_transaction(
            &conn,
            &what_if,
            "Car",
            Direction::Out,
            Money::from_dollars(340.0),
        )
        .unwrap();

        apply_what_if(&mut conn, &what_if).unwrap();

        assert!(queries::get_plan(&conn, &what_if).unwrap().is_none());
        assert_eq!(
            amounts(&conn, &main),
            [
                ("Groceries".to_string(), Money::from_dollars(450.0)),
                ("Rent".to_string(), Money::from_dollars(1500.0)),
                ("Car".to_string(), Money::from_dollars(340.0)),
            ]
        );
    }

    #[test]
    fn make_standalone_keeps_what_the_what_if_resolves_to() {
        let (mut conn, main, what_if, [rent, dining, _]) = main_and_what_if();
        remove_plan_item(&conn, &what_if, &dining).unwrap();
        let before = amounts(&conn, &what_if);

        make_standalone(&mut conn, &what_if).unwrap();
        set_item_amount(&conn, &rent, Money::from_dollars(1600.0)).unwrap();

        assert!(
            !queries::get_plan(&conn, &what_if)
                .unwrap()
                .unwrap()
                .is_what_if()
        );
        assert_eq!(amounts(&conn, &what_if), before, "no longer follows Main");
        assert_eq!(amounts(&conn, &main).len(), 3);
    }

    #[test]
    fn deleting_mains_item_drops_what_if_changes_and_main_needs_its_what_ifs_gone() {
        let (mut conn, main, what_if, [_, dining, _]) = main_and_what_if();
        remove_plan_item(&conn, &what_if, &dining).unwrap();
        delete_plan_item(&conn, &dining).unwrap();
        assert!(
            queries::removed_entries(&conn, &what_if)
                .unwrap()
                .is_empty()
        );

        assert!(delete_plan(&mut conn, &main).is_err());
        delete_plan(&mut conn, &what_if).unwrap();
        let other = create_plan(&conn, "Other").unwrap();
        delete_plan(&mut conn, &main).unwrap();
        assert_eq!(queries::main_plan_id(&conn).unwrap(), Some(other));
    }

    #[test]
    fn merge_into_main_moves_an_add_on_plans_items() {
        let (mut conn, main, _, _) = main_and_what_if();
        let kids = create_plan(&conn, "Kids").unwrap();
        add_new_envelope(&conn, &kids, "Gifts", Money::from_dollars(120.0)).unwrap();
        merge_into_main(&mut conn, &kids).unwrap();
        assert!(queries::get_plan(&conn, &kids).unwrap().is_none());
        assert_eq!(amounts(&conn, &main).len(), 4);
        assert!(merge_into_main(&mut conn, &main).is_err());
    }

    #[test]
    fn dated_items_stamp_only_inside_their_months() {
        let (mut conn, main, what_if, [rent, _, _]) = main_and_what_if();
        let car = add_new_transaction(
            &conn,
            &main,
            "Car",
            Direction::Out,
            Money::from_dollars(340.0),
        )
        .unwrap();
        set_item_dates(&conn, &car, Some("2027-01"), Some("2027-06")).unwrap();
        assert!(set_item_dates(&conn, &car, Some("2027-06"), Some("2027-01")).is_err());
        assert!(set_item_dates(&conn, &car, Some("June"), None).is_err());
        remove_plan_item(&conn, &what_if, &rent).unwrap();

        let labels = |conn: &Connection, month: &str| -> Vec<String> {
            queries::load_txns(conn, month)
                .unwrap()
                .into_iter()
                .map(|t| t.label)
                .collect()
        };
        let dec = stamp(
            &mut conn,
            &main,
            "2026-12",
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            31,
        )
        .unwrap();
        let jan = stamp(
            &mut conn,
            &main,
            "2027-01",
            NaiveDate::from_ymd_opt(2027, 1, 1).unwrap(),
            31,
        )
        .unwrap();
        assert!(!labels(&conn, &dec).contains(&"Car".to_string()));
        assert!(labels(&conn, &jan).contains(&"Car".to_string()));

        let skipped: Vec<_> = queries::skipped_entries(
            &conn,
            &what_if,
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
        )
        .unwrap()
        .into_iter()
        .map(|(e, reason)| (e.series.label, reason))
        .collect();
        assert_eq!(
            skipped,
            [
                ("Car".to_string(), SkipReason::StartsLater),
                ("Rent".to_string(), SkipReason::Removed),
            ]
        );
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
    fn cross_plan_series_continuity_on_restamp() {
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

        // Restamp a DIFFERENT plan into the month — matching is by the shared series id.
        restamp(&mut conn, &month_id, &plan_b, false).unwrap();

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
    fn restamp_protects_settled_refreshes_unsettled_and_adds_new() {
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

        restamp(&mut conn, &month_id, &plan, false).unwrap();

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
    fn restamp_removes_or_keeps_handentered() {
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

            // Settle Rent so we can confirm a restamp leaves it paid.
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

        // Removal: the one-off goes; Rent stays paid; spending in the kept Dining envelope
        // stays linked.
        {
            let (mut conn, plan, month_id, dining) = setup();
            restamp(&mut conn, &month_id, &plan, true).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(txns.iter().all(|t| t.id != "oneoff"), "one-off removed");
            let rent = txns.iter().find(|t| t.label == "Rent").unwrap();
            assert!(rent.settled, "restamp never unsettles");
            let spend = txns.iter().find(|t| t.id == "spend").unwrap();
            assert_eq!(spend.envelope_id.as_deref(), Some(dining.as_str()));
        }

        // Keep hand-entered: one-off stays; manual spending stays linked to its envelope.
        {
            let (mut conn, plan, month_id, dining) = setup();
            restamp(&mut conn, &month_id, &plan, false).unwrap();
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

    /// A plan with Rent (bill) and Dining (manual envelope), stamped into Sept 2026.
    fn rent_and_dining_month() -> (Connection, String, String, String, String) {
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
        let rent_txn = queries::load_txns(&conn, &month_id).unwrap().remove(0).id;
        let dining_env = queries::load_envelopes(&conn, &month_id)
            .unwrap()
            .remove(0)
            .id;
        (conn, plan, month_id, rent_txn, dining_env)
    }

    #[test]
    fn restamp_leaves_settled_rows_unchanged() {
        for remove_unplanned in [false, true] {
            let (mut conn, plan, month_id, rent_txn, _) = rent_and_dining_month();
            mark_paid(
                &conn,
                &rent_txn,
                Money::from_dollars(987.0),
                Some("2026-09-03"),
            )
            .unwrap();
            let rent_item = queries::load_plan_entries(&conn, &plan)
                .unwrap()
                .into_iter()
                .find(|e| e.series.label == "Rent")
                .unwrap()
                .item_id;
            set_item_amount(&conn, &rent_item, Money::from_dollars(1100.0)).unwrap();
            let before = queries::load_txns(&conn, &month_id)
                .unwrap()
                .into_iter()
                .find(|t| t.id == rent_txn)
                .unwrap();

            restamp(&mut conn, &month_id, &plan, remove_unplanned).unwrap();

            let after = queries::load_txns(&conn, &month_id).unwrap();
            let rent: Vec<_> = after.iter().filter(|t| t.label == "Rent").collect();
            assert_eq!(rent.len(), 1, "matched, not re-inserted");
            let rent = rent[0];
            assert_eq!(rent.id, before.id);
            assert!(rent.settled);
            assert_eq!(rent.amount, Money::from_dollars(987.0));
            assert_eq!(rent.stamped_amount, before.stamped_amount);
            assert_eq!(rent.date_paid, before.date_paid);
        }
    }

    #[test]
    fn spending_in_a_kept_envelope_survives_removal() {
        let (mut conn, plan, month_id, _, dining_env) = rent_and_dining_month();
        let spend = add_envelope_spending(
            &conn,
            &month_id,
            &dining_env,
            "Lunch",
            Money::from_dollars(25.0),
        )
        .unwrap();
        assert!(
            !month_has_items_outside_plan(&conn, &month_id, &plan).unwrap(),
            "spending alone is not outside the plan"
        );

        restamp(&mut conn, &month_id, &plan, true).unwrap();

        let envs = queries::load_envelopes(&conn, &month_id).unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].id, dining_env, "refreshed in place");
        let lunch = queries::load_txns(&conn, &month_id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == spend)
            .unwrap();
        assert_eq!(lunch.envelope_id.as_deref(), Some(dining_env.as_str()));
        assert_eq!(lunch.amount, Money::from_dollars(25.0));
    }

    #[test]
    fn removal_deletes_only_unmatched_rows() {
        let (mut conn, plan, month_id, rent_txn, dining_env) = rent_and_dining_month();
        mark_paid(&conn, &rent_txn, Money::from_dollars(1000.0), None).unwrap();
        let lunch = add_envelope_spending(
            &conn,
            &month_id,
            &dining_env,
            "Lunch",
            Money::from_dollars(25.0),
        )
        .unwrap();

        // Rows the plan doesn't have: a paid one-off, a series-backed bill, and an ad-hoc
        // envelope with its own spending.
        let gift = add_oneoff_txn(
            &conn,
            &month_id,
            "Gift",
            Direction::Out,
            Money::from_dollars(50.0),
        )
        .unwrap();
        mark_paid(&conn, &gift, Money::from_dollars(50.0), None).unwrap();
        let concert = create_series(
            &conn,
            Kind::Transaction,
            "Concert",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_series_txn_instance(&conn, &month_id, &concert, Money::from_dollars(120.0)).unwrap();
        let fun = add_oneoff_envelope(
            &conn,
            &month_id,
            "Fun",
            Money::from_dollars(200.0),
            PeriodType::Monthly,
            Mode::Manual,
        )
        .unwrap();
        add_envelope_spending(&conn, &month_id, &fun, "Movie", Money::from_dollars(15.0)).unwrap();
        assert!(month_has_items_outside_plan(&conn, &month_id, &plan).unwrap());

        restamp(&mut conn, &month_id, &plan, true).unwrap();

        let mut txn_ids: Vec<_> = queries::load_txns(&conn, &month_id)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        txn_ids.sort();
        let mut expected = vec![rent_txn, lunch];
        expected.sort();
        assert_eq!(
            txn_ids, expected,
            "only the plan bill and kept spending remain"
        );
        let env_ids: Vec<_> = queries::load_envelopes(&conn, &month_id)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(env_ids, vec![dining_env]);
        assert!(!month_has_items_outside_plan(&conn, &month_id, &plan).unwrap());
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
    fn restamp_can_remove_or_keep_series_backed_items_outside_plan() {
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
            restamp(&mut conn, &month_id, &plan, true).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter()
                    .all(|t| t.series_id.as_deref() != Some(concert.as_str())),
                "outside-plan series removed"
            );
        }

        {
            let (mut conn, plan, month_id, concert) = setup();
            restamp(&mut conn, &month_id, &plan, false).unwrap();
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter()
                    .any(|t| t.series_id.as_deref() == Some(concert.as_str())),
                "outside-plan series kept"
            );
        }
    }

    #[test]
    fn restamp_adds_repeated_plan_occurrences_without_collapsing_series() {
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

        restamp(&mut conn, &month_id, &plan, false).unwrap();

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
    fn restamp_matches_repeated_occurrences_and_can_remove_extras() {
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
            restamp(&mut conn, &month_id, &plan, true).unwrap();
            let rows = queries::load_txns(&conn, &month_id)
                .unwrap()
                .into_iter()
                .filter(|t| t.series_id.as_deref() == Some(paycheck.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1, "extra occurrence removed");
            assert_eq!(rows[0].amount, Money::from_dollars(5000.0));
        }

        {
            let (mut conn, plan, month_id, paycheck) = setup();
            restamp(&mut conn, &month_id, &plan, false).unwrap();
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
    fn restamp_removes_or_keeps_adhoc_envelope() {
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

        // Removal: the ad-hoc envelope and its spending are both gone; the plan bill survives.
        {
            let (mut conn, plan, month_id, env_id) = setup();
            restamp(&mut conn, &month_id, &plan, true).unwrap();
            let envs = queries::load_envelopes(&conn, &month_id).unwrap();
            assert!(
                envs.iter().all(|e| e.id != env_id),
                "ad-hoc envelope removed"
            );
            let txns = queries::load_txns(&conn, &month_id).unwrap();
            assert!(
                txns.iter().all(|t| t.envelope_id.is_none()),
                "its spending removed too"
            );
            assert!(txns.iter().any(|t| t.label == "Rent"), "plan bill kept");
        }

        // Keep: the ad-hoc envelope and its spending both remain, still linked.
        {
            let (mut conn, plan, month_id, env_id) = setup();
            restamp(&mut conn, &month_id, &plan, false).unwrap();
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
