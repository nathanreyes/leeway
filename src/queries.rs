//! Reading rows out of the database into our structs.
//!
//! Each `map_*` closure receives a `&Row` and returns one struct. Because `Money` and
//! the enums implement `FromSql` (see models.rs), `row.get("amount_cents")` yields a
//! `Money` and `row.get("direction")` yields a `Direction` directly — no manual parsing.

use crate::currency::{self, Currency};
use crate::models::*;
use crate::money::Money;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};

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

/// Which figure credit-card amount prompts ask for. Available credit remains the safe
/// default for budgets created before this preference existed and for unknown values
/// written by a newer version.
pub fn credit_card_entry_mode(conn: &Connection) -> Result<CreditCardEntryMode> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM setting WHERE key = 'credit_card_entry_mode'",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("reading credit-card entry mode setting")?;
    Ok(match value.as_deref() {
        Some("current_balance") => CreditCardEntryMode::CurrentBalance,
        _ => CreditCardEntryMode::AvailableCredit,
    })
}

/// The stored state of the `currency` setting. Distinguishes a fresh budget with
/// no row (safe to detect + persist a default) from one whose row names a code
/// this build doesn't recognize — e.g. written by a newer app version. The latter
/// must be preserved verbatim, never overwritten, or a downgrade would silently
/// destroy the user's original choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrencySetting {
    /// No `currency` row yet — a brand-new budget that hasn't chosen one.
    Unset,
    /// A row naming a currency this build recognizes.
    Known(Currency),
    /// A row present but naming a code we don't recognize; preserved verbatim.
    Unknown(String),
}

/// Read the `currency` setting as a three-state value (from the `setting` table's
/// `currency` key). The value travels with the database, so a synced budget carries
/// its currency to every device. Callers that only need the recognized currency and
/// can fall back on anything else should use [`currency`] instead.
pub fn currency_setting(conn: &Connection) -> Result<CurrencySetting> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM setting WHERE key = 'currency'",
            [],
            |r| r.get(0),
        )
        .optional()
        .context("reading currency setting")?;
    Ok(match value {
        None => CurrencySetting::Unset,
        Some(code) => match currency::by_code(&code) {
            Some(chosen) => CurrencySetting::Known(chosen),
            None => CurrencySetting::Unknown(code),
        },
    })
}

/// The app-wide display currency (from the `setting` table's `currency` key).
/// Returns `None` when the key is absent (a fresh budget that hasn't chosen one
/// yet) or names a code we don't recognize, so the caller can fall back to
/// locale detection. The value travels with the database, so a synced budget
/// carries its currency to every device.
pub fn currency(conn: &Connection) -> Result<Option<Currency>> {
    Ok(match currency_setting(conn)? {
        CurrencySetting::Known(chosen) => Some(chosen),
        CurrencySetting::Unset | CurrencySetting::Unknown(_) => None,
    })
}

/// Re-apply the app-wide active currency from the database. Called after a sync
/// operation swaps in a different budget's data, so the adopted budget's currency
/// takes effect without a restart. A budget with no currency row leaves the current
/// choice untouched — we don't re-detect from this device's locale for data that
/// belongs to another device.
pub fn apply_active_currency(conn: &Connection) -> Result<()> {
    if let Some(chosen) = currency(conn)? {
        currency::set_active(chosen);
    }
    Ok(())
}

pub fn load_accounts(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, type, balance_cents, credit_limit_cents, available_credit_cents,
                carry_balance_cents
         FROM account
         ORDER BY
             CASE type WHEN 'checking' THEN 0 WHEN 'credit_card' THEN 1 ELSE 2 END,
             CASE type
                 WHEN 'checking' THEN balance_cents
                 WHEN 'credit_card' THEN COALESCE(credit_limit_cents, 0) - COALESCE(available_credit_cents, 0)
                 ELSE 0
             END DESC,
             name",
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

pub fn month_by_id(conn: &Connection, id: &str) -> Result<Option<Month>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, label, start_date, days_in_month
         FROM month WHERE id = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query_map([id], map_month_raw)?;
    match rows.next() {
        None => Ok(None),
        Some(raw) => Ok(Some(raw?.parse()?)),
    }
}

pub fn month_days(conn: &Connection, month_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT days_in_month FROM month WHERE id = ?1",
        [month_id],
        |r| r.get("days_in_month"),
    )
    .with_context(|| format!("month not found: {month_id}"))
}

/// Every stamped month, oldest first. Series trends use this as the time axis before
/// applying their selected range.
pub fn months(conn: &Connection) -> Result<Vec<Month>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, label, start_date, days_in_month
         FROM month ORDER BY start_date, label",
    )?;
    let rows = stmt.query_map([], map_month_raw)?;
    let mut months = Vec::new();
    for raw in rows {
        months.push(raw?.parse()?);
    }
    Ok(months)
}

pub fn load_envelopes(conn: &Connection, month_id: &str) -> Result<Vec<Envelope>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.month_id, e.series_id, e.label,
                s.label AS series_label, e.amount_cents,
                e.stamped_amount_cents, e.period_type, e.mode
         FROM envelope e
         LEFT JOIN series s ON s.id = e.series_id
         WHERE e.month_id = ?1
         ORDER BY COALESCE(s.label, e.label), e.id",
    )?;
    let rows = stmt
        .query_map([month_id], map_envelope)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn load_txns(conn: &Connection, month_id: &str) -> Result<Vec<Txn>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.month_id, t.series_id, t.envelope_id, t.account_id, t.label,
                s.label AS series_label, t.direction, t.amount_cents,
                t.stamped_amount_cents, t.settled, t.date_paid
         FROM txn t
         LEFT JOIN series s ON s.id = t.series_id
         WHERE t.month_id = ?1
         ORDER BY t.direction DESC, COALESCE(s.label, t.label), t.id",
    )?;
    let rows = stmt
        .query_map([month_id], map_txn)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn load_envelope_txns(
    conn: &Connection,
    month_id: &str,
    envelope_id: &str,
) -> Result<Vec<Txn>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.month_id, t.series_id, t.envelope_id, t.account_id, t.label,
                s.label AS series_label, t.direction, t.amount_cents,
                t.stamped_amount_cents, t.settled, t.date_paid
         FROM txn t
         LEFT JOIN series s ON s.id = t.series_id
         WHERE t.month_id = ?1 AND t.envelope_id = ?2
         ORDER BY COALESCE(s.label, t.label), t.id",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![month_id, envelope_id], map_txn)?
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
                s.id AS series_id, s.kind AS kind, s.label AS label,
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
        "SELECT id, kind, label, direction, period_type, mode
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
            "SELECT id, kind, label, direction, period_type, mode
             FROM series WHERE id = ?1",
            [series_id],
            map_series,
        )
        .optional()?;
    Ok(row)
}

/// Names of plans that currently include this series, in display order.
pub fn plan_names_for_series(conn: &Connection, series_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.name
         FROM plan p
         JOIN plan_item pi ON pi.plan_id = p.id
         WHERE pi.series_id = ?1
         ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map([series_id], |r| r.get::<_, String>("name"))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// All current plan memberships, grouped by durable series id. The Series page loads this
/// once instead of issuing one query for each visible series.
pub fn plan_names_by_series(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT pi.series_id, p.name
         FROM plan_item pi
         JOIN plan p ON p.id = pi.plan_id
         ORDER BY pi.series_id, p.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut names = HashMap::<String, Vec<String>>::new();
    for row in rows {
        let (series_id, name) = row?;
        names.entry(series_id).or_default().push(name);
    }
    Ok(names)
}

/// One series' aggregate for one stamped month. These are the compact inputs to the Series
/// chart and stats: unlike `load_txns` and `load_envelopes`, they do not pull every raw row
/// once per series.
#[derive(Clone, Debug)]
pub struct SeriesTrendAggregate {
    pub effective: Money,
    pub planned: Option<Money>,
    pub occurrence_count: usize,
    pub settled_count: usize,
}

/// Every persisted trend aggregate, nested by series then month. Transaction and envelope
/// rows have different fields, so SQLite calculates them in two grouped queries and the
/// caller receives one common shape.
pub fn series_trend_aggregates(
    conn: &Connection,
) -> Result<HashMap<String, HashMap<String, SeriesTrendAggregate>>> {
    let mut aggregates = HashMap::<String, HashMap<String, SeriesTrendAggregate>>::new();
    let mut insert = |series_id: String, month_id: String, aggregate: SeriesTrendAggregate| {
        aggregates
            .entry(series_id)
            .or_default()
            .insert(month_id, aggregate);
    };

    let mut txns = conn.prepare(
        "SELECT series_id, month_id,
                SUM(amount_cents) AS effective_cents,
                SUM(stamped_amount_cents) AS planned_cents,
                COUNT(*) AS occurrence_count,
                SUM(CASE WHEN settled THEN 1 ELSE 0 END) AS settled_count
         FROM txn
         WHERE envelope_id IS NULL AND series_id IS NOT NULL
         GROUP BY series_id, month_id",
    )?;
    let rows = txns.query_map([], |r| {
        Ok((
            r.get::<_, String>("series_id")?,
            r.get::<_, String>("month_id")?,
            SeriesTrendAggregate {
                effective: r.get("effective_cents")?,
                planned: r.get("planned_cents")?,
                occurrence_count: r.get::<_, i64>("occurrence_count")? as usize,
                settled_count: r.get::<_, i64>("settled_count")? as usize,
            },
        ))
    })?;
    for row in rows {
        let (series_id, month_id, aggregate) = row?;
        insert(series_id, month_id, aggregate);
    }

    let mut envelopes = conn.prepare(
        "SELECT series_id, month_id,
                SUM(amount_cents) AS effective_cents,
                SUM(stamped_amount_cents) AS planned_cents,
                COUNT(*) AS occurrence_count
         FROM envelope
         WHERE series_id IS NOT NULL
         GROUP BY series_id, month_id",
    )?;
    let rows = envelopes.query_map([], |r| {
        Ok((
            r.get::<_, String>("series_id")?,
            r.get::<_, String>("month_id")?,
            SeriesTrendAggregate {
                effective: r.get("effective_cents")?,
                planned: r.get("planned_cents")?,
                occurrence_count: r.get::<_, i64>("occurrence_count")? as usize,
                settled_count: 0,
            },
        ))
    })?;
    for row in rows {
        let (series_id, month_id, aggregate) = row?;
        insert(series_id, month_id, aggregate);
    }

    Ok(aggregates)
}

/// How many stamped month instances (txn + envelope rows) still carry this series id.
/// Used only to phrase the delete confirm: these are copied *soft* references that are
/// intentionally orphaned when a series is deleted (each instance is a self-contained
/// snapshot — see the `envelope` table comment in schema.sql), so they never block the
/// delete the way a live `plan_item` reference does.
pub fn series_month_usage(conn: &Connection, series_id: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM txn      WHERE series_id = ?1)
              + (SELECT COUNT(*) FROM envelope WHERE series_id = ?1)",
        [series_id],
        |r| r.get(0),
    )?;
    Ok(count)
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
        series_label: r.get("series_label")?,
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
        series_label: r.get("series_label")?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Direction, Kind, Mode, PeriodType};
    use crate::money::Money;
    use crate::{db, ops};
    use chrono::NaiveDate;

    #[test]
    fn month_rows_use_live_series_labels_with_snapshot_fallback() {
        let mut conn = db::open_in_memory().unwrap();
        let plan_id = ops::create_plan(&conn, "Normal").unwrap();
        let txn_series = ops::create_series(
            &conn,
            Kind::Transaction,
            "Electric",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let envelope_series = ops::create_series(
            &conn,
            Kind::Envelope,
            "Groceries",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Manual),
        )
        .unwrap();
        let txn_item =
            ops::add_plan_item(&conn, &plan_id, &txn_series, Money::from_dollars(90.0)).unwrap();
        let envelope_item = ops::add_plan_item(
            &conn,
            &plan_id,
            &envelope_series,
            Money::from_dollars(500.0),
        )
        .unwrap();

        let months = [
            ops::stamp(
                &mut conn,
                &plan_id,
                "2026-06",
                NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
                30,
            )
            .unwrap(),
            ops::stamp(
                &mut conn,
                &plan_id,
                "2026-07",
                NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
                31,
            )
            .unwrap(),
        ];

        ops::set_series_label(&conn, &txn_series, "Electricity").unwrap();
        ops::set_series_label(&conn, &envelope_series, "Food").unwrap();

        for month_id in &months {
            let txn = load_txns(&conn, month_id).unwrap().remove(0);
            assert_eq!(txn.label, "Electric", "stored snapshot is unchanged");
            assert_eq!(txn.series_label.as_deref(), Some("Electricity"));
            assert_eq!(txn.display_label(), "Electricity");

            let envelope = load_envelopes(&conn, month_id).unwrap().remove(0);
            assert_eq!(envelope.label, "Groceries", "stored snapshot is unchanged");
            assert_eq!(envelope.series_label.as_deref(), Some("Food"));
            assert_eq!(envelope.display_label(), "Food");
        }

        ops::delete_plan_item(&conn, &txn_item).unwrap();
        ops::delete_plan_item(&conn, &envelope_item).unwrap();
        ops::delete_series(&conn, &txn_series).unwrap();
        ops::delete_series(&conn, &envelope_series).unwrap();

        let txn = load_txns(&conn, &months[0]).unwrap().remove(0);
        assert!(txn.series_label.is_none());
        assert_eq!(txn.display_label(), "Electric");
        let envelope = load_envelopes(&conn, &months[0]).unwrap().remove(0);
        assert!(envelope.series_label.is_none());
        assert_eq!(envelope.display_label(), "Groceries");
    }

    #[test]
    fn month_rows_sort_by_the_effective_series_label() {
        let mut conn = db::open_in_memory().unwrap();
        let plan_id = ops::create_plan(&conn, "Normal").unwrap();
        let zulu = ops::create_series(
            &conn,
            Kind::Transaction,
            "Zulu",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let alpha = ops::create_series(
            &conn,
            Kind::Transaction,
            "Alpha",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        ops::add_plan_item(&conn, &plan_id, &zulu, Money::from_dollars(1.0)).unwrap();
        ops::add_plan_item(&conn, &plan_id, &alpha, Money::from_dollars(1.0)).unwrap();
        let month_id = ops::stamp(
            &mut conn,
            &plan_id,
            "2026-07",
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            31,
        )
        .unwrap();

        ops::set_series_label(&conn, &zulu, "Aaron").unwrap();

        let labels: Vec<String> = load_txns(&conn, &month_id)
            .unwrap()
            .into_iter()
            .map(|txn| txn.display_label().to_string())
            .collect();
        assert_eq!(labels, ["Aaron", "Alpha"]);
    }
}
