//! Reading rows out of the database into our structs.
//!
//! Each `map_*` closure receives a `&Row` and returns one struct. Because `Money` and
//! the enums implement `FromSql` (see models.rs), `row.get("amount_cents")` yields a
//! `Money` and `row.get("direction")` yields a `Direction` directly — no manual parsing.

use crate::models::*;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, Row};
use std::collections::HashSet;

/// The global default envelope mode (from the `setting` table). Falls back to
/// Automatic if the row is somehow missing.
pub fn default_mode(conn: &Connection) -> Result<Mode> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM setting WHERE key = 'default_envelope_mode'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(match value.as_deref() {
        Some("manual") => Mode::Manual,
        _ => Mode::Automatic,
    })
}

pub fn load_accounts(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, type, balance_cents, credit_limit_cents, available_credit_cents,
                carry_balance_cents
         FROM account ORDER BY name",
    )?;
    // `query_map` runs the SQL and applies the mapper to each row; `collect` gathers the
    // results, and the `?` after it fails fast if any row failed to map.
    let rows = stmt
        .query_map([], map_account)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The "current" month = the most recently started one. Returns `None` on a fresh
/// database with nothing stamped yet.
pub fn current_month(conn: &Connection) -> Result<Option<Month>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, label, start_date, days_in_month
         FROM month ORDER BY start_date DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map([], map_month_raw)?;
    match rows.next() {
        None => Ok(None),
        Some(raw) => Ok(Some(raw?.parse()?)), // parse the date string -> NaiveDate here
    }
}

/// The stamped month for a specific `YYYY-MM` label, or `None` if that period was never
/// stamped. This is the month-navigation counterpart to `current_month`: the dashboard now
/// views one *chosen* period (which may not exist yet) rather than always the latest one.
pub fn month_by_label(conn: &Connection, label: &str) -> Result<Option<Month>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, label, start_date, days_in_month
         FROM month WHERE label = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query_map([label], map_month_raw)?;
    match rows.next() {
        None => Ok(None),
        Some(raw) => Ok(Some(raw?.parse()?)), // same string -> NaiveDate parse as current_month
    }
}

pub fn load_envelopes(conn: &Connection, month_id: &str) -> Result<Vec<Envelope>> {
    let mut stmt = conn.prepare(
        "SELECT id, month_id, series_id, label, category, amount_cents,
                stamped_amount_cents, period_type, mode
         FROM envelope WHERE month_id = ?1 ORDER BY label, id",
    )?;
    let rows = stmt
        .query_map([month_id], map_envelope)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn load_txns(conn: &Connection, month_id: &str) -> Result<Vec<Txn>> {
    let mut stmt = conn.prepare(
        "SELECT id, month_id, series_id, envelope_id, account_id, label, category,
                direction, amount_cents, stamped_amount_cents, settled, date_paid
         FROM txn WHERE month_id = ?1 ORDER BY direction DESC, label, id",
    )?;
    let rows = stmt
        .query_map([month_id], map_txn)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn plans(conn: &Connection) -> Result<Vec<Plan>> {
    let mut stmt = conn.prepare("SELECT id, name FROM plan ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Plan {
                id: r.get("id")?,
                name: r.get("name")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A plan plus how many items it holds — enough to render the plans list.
pub struct PlanSummary {
    pub plan: Plan,
    pub item_count: i64,
}

/// All plans with their item counts. The `LEFT JOIN` keeps a brand-new empty plan in
/// the list (a plain JOIN would drop it); `COUNT(plan_item.id)` then reads 0 for it.
pub fn plan_summaries(conn: &Connection) -> Result<Vec<PlanSummary>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, COUNT(pi.id) AS item_count
         FROM plan p
         LEFT JOIN plan_item pi ON pi.plan_id = p.id
         GROUP BY p.id, p.name
         ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PlanSummary {
                plan: Plan {
                    id: r.get("id")?,
                    name: r.get("name")?,
                },
                item_count: r.get("item_count")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Fetch a single plan by id, or `None` if it was deleted.
pub fn get_plan(conn: &Connection, plan_id: &str) -> Result<Option<Plan>> {
    let mut stmt = conn.prepare("SELECT id, name FROM plan WHERE id = ?1")?;
    let mut rows = stmt.query_map([plan_id], |r| {
        Ok(Plan {
            id: r.get("id")?,
            name: r.get("name")?,
        })
    })?;
    match rows.next() {
        None => Ok(None),
        Some(p) => Ok(Some(p?)),
    }
}

/// All entries in a plan (plan_item JOIN series), envelopes first then transactions, each
/// alphabetical — a stable order so the editor's selection doesn't jump as you rename.
pub fn load_plan_entries(conn: &Connection, plan_id: &str) -> Result<Vec<PlanEntry>> {
    let mut stmt = conn.prepare(
        "SELECT pi.id AS item_id, pi.plan_id AS plan_id, pi.amount_cents AS amount_cents,
                s.id AS series_id, s.kind AS kind, s.label AS label, s.category AS category,
                s.direction AS direction, s.period_type AS period_type, s.mode AS mode
         FROM plan_item pi JOIN series s ON s.id = pi.series_id
         WHERE pi.plan_id = ?1
         ORDER BY s.kind, s.label, pi.id",
    )?;
    let rows = stmt
        .query_map([plan_id], map_plan_entry)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every series, for the reuse picker.
pub fn list_series(conn: &Connection) -> Result<Vec<Series>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, label, category, direction, period_type, mode
         FROM series ORDER BY kind, label",
    )?;
    let rows = stmt
        .query_map([], map_series)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_series(conn: &Connection, series_id: &str) -> Result<Option<Series>> {
    let row = conn
        .query_row(
            "SELECT id, kind, label, category, direction, period_type, mode
             FROM series WHERE id = ?1",
            [series_id],
            map_series,
        )
        .optional()?;
    Ok(row)
}

pub fn plan_has_series(conn: &Connection, plan_id: &str, series_id: &str) -> Result<bool> {
    let has: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM plan_item WHERE plan_id = ?1 AND series_id = ?2
         )",
        rusqlite::params![plan_id, series_id],
        |r| r.get(0),
    )?;
    Ok(has)
}

pub fn month_has_budget_series(
    conn: &Connection,
    month_id: &str,
    series_id: &str,
    kind: Kind,
) -> Result<bool> {
    let sql = match kind {
        Kind::Transaction => {
            "SELECT EXISTS(
                 SELECT 1 FROM txn
                 WHERE month_id = ?1 AND series_id = ?2 AND envelope_id IS NULL
             )"
        }
        Kind::Envelope => {
            "SELECT EXISTS(
                 SELECT 1 FROM envelope WHERE month_id = ?1 AND series_id = ?2
             )"
        }
    };
    let has: bool = conn.query_row(sql, rusqlite::params![month_id, series_id], |r| r.get(0))?;
    Ok(has)
}

/// The set of series ids already included in a plan — so the picker can mark/skip them.
pub fn series_in_plan(conn: &Connection, plan_id: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT series_id FROM plan_item WHERE plan_id = ?1")?;
    let ids = stmt
        .query_map([plan_id], |r| r.get::<_, String>("series_id"))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(ids)
}

fn map_series(r: &Row) -> rusqlite::Result<Series> {
    Ok(Series {
        id: r.get("id")?,
        kind: r.get("kind")?,
        label: r.get("label")?,
        category: r.get("category")?,
        direction: r.get("direction")?,
        period_type: r.get("period_type")?,
        mode: r.get("mode")?,
    })
}

fn map_plan_entry(r: &Row) -> rusqlite::Result<PlanEntry> {
    Ok(PlanEntry {
        item_id: r.get("item_id")?,
        plan_id: r.get("plan_id")?,
        amount: r.get("amount_cents")?,
        series: Series {
            id: r.get("series_id")?,
            kind: r.get("kind")?,
            label: r.get("label")?,
            category: r.get("category")?,
            direction: r.get("direction")?,
            period_type: r.get("period_type")?,
            mode: r.get("mode")?,
        },
    })
}

/// Is there already a month with this label? Used to stop stamping the same month twice.
pub fn month_label_exists(conn: &Connection, label: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM month WHERE label = ?1",
        [label],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// The id of the month with this label, if any — so a restamp can target it.
pub fn month_id_for_label(conn: &Connection, label: &str) -> Result<Option<String>> {
    let id = conn
        .query_row(
            "SELECT id FROM month WHERE label = ?1 LIMIT 1",
            [label],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(id)
}

// --- Row mappers ---------------------------------------------------------------

fn map_account(r: &Row) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get("id")?,
        name: r.get("name")?,
        account_type: r.get("type")?,     // AccountType::FromSql
        balance: r.get("balance_cents")?, // Money::FromSql
        credit_limit: r.get("credit_limit_cents")?, // Option<Money>: NULL -> None
        available_credit: r.get("available_credit_cents")?,
        carry_balance: r.get("carry_balance_cents")?, // Option<Money>: NULL -> None
    })
}

fn map_envelope(r: &Row) -> rusqlite::Result<Envelope> {
    Ok(Envelope {
        id: r.get("id")?,
        month_id: r.get("month_id")?,
        series_id: r.get("series_id")?,
        label: r.get("label")?,
        category: r.get("category")?,
        amount: r.get("amount_cents")?,
        stamped_amount: r.get("stamped_amount_cents")?,
        period_type: r.get("period_type")?,
        mode: r.get("mode")?, // Mode: NOT NULL since migration 004 (frozen at stamp time)
    })
}

fn map_txn(r: &Row) -> rusqlite::Result<Txn> {
    Ok(Txn {
        id: r.get("id")?,
        month_id: r.get("month_id")?,
        series_id: r.get("series_id")?,
        envelope_id: r.get("envelope_id")?,
        account_id: r.get("account_id")?,
        label: r.get("label")?,
        category: r.get("category")?,
        direction: r.get("direction")?,
        amount: r.get("amount_cents")?,
        stamped_amount: r.get("stamped_amount_cents")?,
        settled: r.get("settled")?,
        date_paid: r.get("date_paid")?,
    })
}

/// A month straight from SQLite, with `start_date` still an unparsed string. We read it
/// this way because date parsing can fail, and doing that in a separate step lets us
/// attach a helpful error message rather than a bare SQLite conversion failure.
struct MonthRaw {
    id: String,
    plan_id: Option<String>,
    label: String,
    start_date: String,
    days_in_month: i64,
}

impl MonthRaw {
    fn parse(self) -> Result<Month> {
        let start_date =
            NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d").with_context(|| {
                format!(
                    "month {} has invalid start_date {:?}",
                    self.id, self.start_date
                )
            })?;
        Ok(Month {
            id: self.id,
            plan_id: self.plan_id,
            label: self.label,
            start_date,
            days_in_month: self.days_in_month,
        })
    }
}

fn map_month_raw(r: &Row) -> rusqlite::Result<MonthRaw> {
    Ok(MonthRaw {
        id: r.get("id")?,
        plan_id: r.get("plan_id")?,
        label: r.get("label")?,
        start_date: r.get("start_date")?,
        days_in_month: r.get("days_in_month")?,
    })
}
