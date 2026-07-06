//! Reading rows out of the database into our structs.
//!
//! Each `map_*` closure receives a `&Row` and returns one struct. Because `Money` and
//! the enums implement `FromSql` (see models.rs), `row.get("amount_cents")` yields a
//! `Money` and `row.get("direction")` yields a `Direction` directly — no manual parsing.

use crate::models::*;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{Connection, Row};

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
        "SELECT id, name, type, balance_cents, protected FROM account ORDER BY name",
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

pub fn load_envelopes(conn: &Connection, month_id: &str) -> Result<Vec<Envelope>> {
    let mut stmt = conn.prepare(
        "SELECT id, month_id, series_id, label, category, amount_cents,
                stamped_amount_cents, period_type, mode
         FROM envelope WHERE month_id = ?1 ORDER BY label",
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
         FROM txn WHERE month_id = ?1 ORDER BY direction DESC, label",
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
                plan: Plan { id: r.get("id")?, name: r.get("name")? },
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
        Ok(Plan { id: r.get("id")?, name: r.get("name")? })
    })?;
    match rows.next() {
        None => Ok(None),
        Some(p) => Ok(Some(p?)),
    }
}

/// All items in a plan, envelopes first then transactions, each alphabetical — a stable
/// order so the editor's selection doesn't jump around as you rename things.
pub fn load_plan_items(conn: &Connection, plan_id: &str) -> Result<Vec<PlanItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, kind, label, slug, category, direction,
                amount_cents, period_type, mode
         FROM plan_item WHERE plan_id = ?1
         ORDER BY kind, label",
    )?;
    let rows = stmt
        .query_map([plan_id], |r| {
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
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
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

// --- Row mappers ---------------------------------------------------------------

fn map_account(r: &Row) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get("id")?,
        name: r.get("name")?,
        account_type: r.get("type")?, // AccountType::FromSql
        balance: r.get("balance_cents")?, // Money::FromSql
        protected: r.get("protected")?, // INTEGER 0/1 -> bool
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
        mode: r.get("mode")?, // Option<Mode>: NULL -> None
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
        let start_date = NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d")
            .with_context(|| format!("month {} has invalid start_date {:?}", self.id, self.start_date))?;
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
