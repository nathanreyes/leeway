//! Resolve a plan item's saved amount source for one target month.
//!
//! Forecasts are stamp-time suggestions, not live links. This module reads only months
//! before the target, returns every method that has enough data, and always includes the
//! plan item's static amount as a safe fallback.

use crate::calc;
use crate::models::{ForecastMethod, Kind, Mode, PeriodType, PlanEntry};
use crate::money::Money;
use crate::queries;
use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForecastOption {
    pub method: ForecastMethod,
    /// The concrete amount that will land in the stamped month. Daily envelope rates have
    /// already been expanded across the target month's day count.
    pub amount: Money,
}

#[derive(Clone, Debug)]
pub struct ResolvedPlanEntry {
    pub entry: PlanEntry,
    pub options: Vec<ForecastOption>,
    pub amount: Money,
    pub used_method: ForecastMethod,
}

impl ResolvedPlanEntry {
    pub fn option(&self, method: ForecastMethod) -> Option<ForecastOption> {
        self.options
            .iter()
            .copied()
            .find(|option| option.method == method)
    }
}

#[derive(Clone, Copy, Debug)]
struct Observation {
    month: NaiveDate,
    days_in_month: i64,
    amount: Money,
}

/// Resolve every plan entry active in `target`. Repeated occurrences of one series stay
/// static because history is a monthly series total and applying it to each occurrence
/// would duplicate that total.
pub fn resolve_plan_entries(
    conn: &Connection,
    plan_id: &str,
    target: NaiveDate,
    target_days: i64,
) -> Result<Vec<ResolvedPlanEntry>> {
    let entries: Vec<PlanEntry> = queries::load_plan_entries(conn, plan_id)?
        .into_iter()
        .filter(|entry| entry.active_months.contains(target.month()))
        .collect();
    let mut occurrences = HashMap::<String, usize>::new();
    for entry in &entries {
        *occurrences.entry(entry.series.id.clone()).or_default() += 1;
    }

    entries
        .into_iter()
        .map(|entry| {
            let unique = occurrences.get(&entry.series.id) == Some(&1);
            resolve_entry(conn, entry, target, target_days, unique)
        })
        .collect()
}

fn resolve_entry(
    conn: &Connection,
    entry: PlanEntry,
    target: NaiveDate,
    target_days: i64,
    unique: bool,
) -> Result<ResolvedPlanEntry> {
    let static_amount = match entry.series.kind {
        Kind::Envelope => calc::monthlyized_envelope_amount(
            entry.amount,
            entry.series.period_type.unwrap_or(PeriodType::Monthly),
            target_days,
        ),
        Kind::Transaction => entry.amount,
    };
    let mut options = vec![ForecastOption {
        method: ForecastMethod::Static,
        amount: static_amount,
    }];

    // Automatic envelopes have no tracked actual. Repeated series occurrences have no
    // durable per-occurrence identity in stamped months. Both cases stay static.
    let supports_history = unique
        && match entry.series.kind {
            Kind::Transaction => true,
            Kind::Envelope => entry.series.mode == Some(Mode::Manual),
        };
    if supports_history {
        let observations = load_observations(conn, &entry, target)?;
        add_historical_options(
            &mut options,
            &observations,
            target,
            target_days,
            entry.series.period_type,
        );
    }

    let selected = options
        .iter()
        .find(|option| option.method == entry.forecast_method)
        .copied()
        .unwrap_or(options[0]);
    Ok(ResolvedPlanEntry {
        entry,
        options,
        amount: selected.amount,
        used_method: selected.method,
    })
}

fn load_observations(
    conn: &Connection,
    entry: &PlanEntry,
    target: NaiveDate,
) -> Result<Vec<Observation>> {
    let target_text = target.format("%Y-%m-%d").to_string();
    let sql = match entry.series.kind {
        Kind::Transaction => {
            "SELECT m.start_date, m.days_in_month, SUM(t.amount_cents) AS amount_cents
             FROM txn t
             JOIN month m ON m.id = t.month_id
             WHERE t.series_id = ?1 AND t.envelope_id IS NULL AND m.start_date < ?2
             GROUP BY m.id, m.start_date, m.days_in_month
             HAVING COUNT(*) = SUM(CASE WHEN t.settled THEN 1 ELSE 0 END)
             ORDER BY m.start_date"
        }
        Kind::Envelope => {
            "SELECT m.start_date, m.days_in_month, SUM(t.amount_cents) AS amount_cents
             FROM envelope e
             JOIN month m ON m.id = e.month_id
             JOIN txn t ON t.envelope_id = e.id AND t.month_id = e.month_id
             WHERE e.series_id = ?1 AND e.mode = 'manual' AND m.start_date < ?2
             GROUP BY m.id, m.start_date, m.days_in_month
             ORDER BY m.start_date"
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![entry.series.id, target_text], |row| {
        Ok((
            row.get::<_, String>("start_date")?,
            row.get::<_, i64>("days_in_month")?,
            row.get::<_, Money>("amount_cents")?,
        ))
    })?;
    let mut observations = Vec::new();
    for row in rows {
        let (month, days_in_month, amount) = row?;
        observations.push(Observation {
            month: NaiveDate::parse_from_str(&month, "%Y-%m-%d")
                .with_context(|| format!("parsing forecast month {month}"))?,
            days_in_month,
            amount,
        });
    }
    Ok(observations)
}

fn add_historical_options(
    options: &mut Vec<ForecastOption>,
    observations: &[Observation],
    target: NaiveDate,
    target_days: i64,
    period: Option<PeriodType>,
) {
    let amount_for =
        |observation: Observation| normalize_for_target(observation, target_days, period);
    let find = |month: NaiveDate| observations.iter().copied().find(|o| o.month == month);

    if let Some(observation) = find(shift_month(target, -1)) {
        options.push(ForecastOption {
            method: ForecastMethod::PreviousMonth,
            amount: amount_for(observation),
        });
    }

    let previous_three: Option<Vec<Money>> = (1..=3)
        .map(|offset| find(shift_month(target, -offset)).map(amount_for))
        .collect();
    if let Some(amounts) = previous_three {
        options.push(ForecastOption {
            method: ForecastMethod::AveragePrevious3,
            amount: average(&amounts),
        });
    }

    if let Some(last_year) = target.with_year(target.year() - 1).and_then(find) {
        options.push(ForecastOption {
            method: ForecastMethod::SameMonthLastYear,
            amount: amount_for(last_year),
        });
    }

    if !observations.is_empty() {
        let amounts: Vec<Money> = observations.iter().copied().map(amount_for).collect();
        options.push(ForecastOption {
            method: ForecastMethod::OverallAverage,
            amount: average(&amounts),
        });
    }
}

fn normalize_for_target(
    observation: Observation,
    target_days: i64,
    period: Option<PeriodType>,
) -> Money {
    if calc::active_period(period.unwrap_or(PeriodType::Monthly)) != PeriodType::Daily
        || observation.days_in_month <= 0
    {
        return observation.amount;
    }
    ratio_money(
        observation.amount,
        target_days.max(0),
        observation.days_in_month,
    )
}

fn average(amounts: &[Money]) -> Money {
    let total: i128 = amounts.iter().map(|amount| amount.cents() as i128).sum();
    Money(round_ratio(total, amounts.len() as i128))
}

fn ratio_money(amount: Money, numerator: i64, denominator: i64) -> Money {
    Money(round_ratio(
        amount.cents() as i128 * numerator as i128,
        denominator as i128,
    ))
}

/// Integer division rounded to the nearest cent, with halves away from zero.
fn round_ratio(numerator: i128, denominator: i128) -> i64 {
    if denominator <= 0 {
        return 0;
    }
    let adjustment = denominator / 2;
    let rounded = if numerator < 0 {
        (numerator - adjustment) / denominator
    } else {
        (numerator + adjustment) / denominator
    };
    rounded.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn shift_month(date: NaiveDate, delta: i32) -> NaiveDate {
    let absolute = date.year() * 12 + date.month0() as i32 + delta;
    let year = absolute.div_euclid(12);
    let month = absolute.rem_euclid(12) as u32 + 1;
    NaiveDate::from_ymd_opt(year, month, 1).expect("shifted month is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::{Direction, Kind};
    use crate::ops;

    fn transaction_plan(conn: &Connection, static_amount: Money) -> (String, String, String) {
        let plan = ops::create_plan(conn, "Plan").unwrap();
        let series = ops::create_series(
            conn,
            Kind::Transaction,
            "Power",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let item = ops::add_plan_item(conn, &plan, &series, static_amount).unwrap();
        (plan, series, item)
    }

    fn stamp_and_settle(conn: &mut Connection, plan: &str, year: i32, month: u32, actual: Money) {
        let start = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
        let label = format!("{year:04}-{month:02}");
        let month_id =
            ops::stamp(conn, plan, &label, start, ops::days_in_month(year, month)).unwrap();
        let txn = queries::load_txns(conn, &month_id).unwrap().remove(0);
        ops::mark_paid(conn, &txn.id, actual, None).unwrap();
    }

    #[test]
    fn averages_round_to_nearest_cent() {
        assert_eq!(average(&[Money(100), Money(101)]), Money(101));
        assert_eq!(average(&[Money(-100), Money(-101)]), Money(-101));
    }

    #[test]
    fn month_shift_crosses_year_boundaries() {
        let january = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(
            shift_month(january, -1),
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()
        );
    }

    #[test]
    fn previous_month_uses_settled_actual_and_is_remembered() {
        let mut conn = db::open_in_memory().unwrap();
        let (plan, _, item) = transaction_plan(&conn, Money(10_000));
        stamp_and_settle(&mut conn, &plan, 2026, 1, Money(12_345));
        ops::set_item_forecast_method(&conn, &item, ForecastMethod::PreviousMonth).unwrap();

        let target = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let resolved = resolve_plan_entries(&conn, &plan, target, 28).unwrap();
        assert_eq!(resolved[0].amount, Money(12_345));
        assert_eq!(resolved[0].used_method, ForecastMethod::PreviousMonth);
        assert_eq!(
            resolved[0].entry.forecast_method,
            ForecastMethod::PreviousMonth
        );
    }

    #[test]
    fn unsettled_history_falls_back_without_erasing_preference() {
        let mut conn = db::open_in_memory().unwrap();
        let (plan, _, item) = transaction_plan(&conn, Money(10_000));
        let january = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        ops::stamp(&mut conn, &plan, "2026-01", january, 31).unwrap();
        ops::set_item_forecast_method(&conn, &item, ForecastMethod::PreviousMonth).unwrap();

        let target = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let resolved = resolve_plan_entries(&conn, &plan, target, 28).unwrap();
        assert_eq!(resolved[0].amount, Money(10_000));
        assert_eq!(resolved[0].used_method, ForecastMethod::Static);
        assert_eq!(
            resolved[0].entry.forecast_method,
            ForecastMethod::PreviousMonth
        );
    }

    #[test]
    fn previous_three_requires_each_calendar_month() {
        let mut conn = db::open_in_memory().unwrap();
        let (plan, _, item) = transaction_plan(&conn, Money(10_000));
        stamp_and_settle(&mut conn, &plan, 2026, 1, Money(10_000));
        stamp_and_settle(&mut conn, &plan, 2026, 2, Money(20_000));
        stamp_and_settle(&mut conn, &plan, 2026, 3, Money(30_000));
        ops::set_item_forecast_method(&conn, &item, ForecastMethod::AveragePrevious3).unwrap();

        let target = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let resolved = resolve_plan_entries(&conn, &plan, target, 30).unwrap();
        assert_eq!(resolved[0].amount, Money(20_000));
        assert_eq!(resolved[0].used_method, ForecastMethod::AveragePrevious3);
    }

    #[test]
    fn last_year_and_overall_average_use_only_prior_observations() {
        let mut conn = db::open_in_memory().unwrap();
        let (plan, _, _) = transaction_plan(&conn, Money(10_000));
        stamp_and_settle(&mut conn, &plan, 2025, 4, Money(12_000));
        stamp_and_settle(&mut conn, &plan, 2026, 3, Money(18_000));

        let target = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let resolved = resolve_plan_entries(&conn, &plan, target, 30).unwrap();
        assert_eq!(
            resolved[0]
                .option(ForecastMethod::SameMonthLastYear)
                .unwrap()
                .amount,
            Money(12_000)
        );
        assert_eq!(
            resolved[0]
                .option(ForecastMethod::OverallAverage)
                .unwrap()
                .amount,
            Money(15_000)
        );
    }

    #[test]
    fn repeated_plan_occurrences_offer_only_static_amounts() {
        let mut conn = db::open_in_memory().unwrap();
        let (plan, series, _) = transaction_plan(&conn, Money(10_000));
        ops::add_plan_item(&conn, &plan, &series, Money(20_000)).unwrap();
        stamp_and_settle(&mut conn, &plan, 2026, 1, Money(30_000));

        let target = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let resolved = resolve_plan_entries(&conn, &plan, target, 28).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|row| row.options.len() == 1));
        assert_eq!(
            resolved.iter().map(|row| row.amount).sum::<Money>(),
            Money(30_000)
        );
    }

    #[test]
    fn automatic_envelopes_offer_only_static() {
        let conn = db::open_in_memory().unwrap();
        let plan = ops::create_plan(&conn, "Plan").unwrap();
        let series = ops::create_series(
            &conn,
            Kind::Envelope,
            "Groceries",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Automatic),
        )
        .unwrap();
        let item = ops::add_plan_item(&conn, &plan, &series, Money(70_000)).unwrap();
        ops::set_item_forecast_method(&conn, &item, ForecastMethod::OverallAverage).unwrap();

        let target = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let resolved = resolve_plan_entries(&conn, &plan, target, 30).unwrap();
        assert_eq!(resolved[0].options.len(), 1);
        assert_eq!(resolved[0].used_method, ForecastMethod::Static);
        assert_eq!(resolved[0].amount, Money(70_000));
    }
}
