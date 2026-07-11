//! The read-model the UI renders. This is the seam between the headless core and any
//! frontend: it runs the queries, applies the §4 calculations, and hands back plain
//! data. A web or desktop frontend could call `MonthView::build` and render it too.

use crate::calc::{self, WhatsLeft};
use crate::models::{Account, AccountType, Direction, Envelope, Kind, Month, Series, Txn};
use crate::money::Money;
use crate::queries;
use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;
use std::collections::HashMap;

/// One envelope with its computed state for this point in the month.
pub struct EnvelopeRow {
    pub envelope: Envelope,
    pub consumed: Money,
    pub remaining: Money,
}

/// Everything the dashboard needs for one viewed month.
pub struct MonthView {
    pub month: Month,
    pub days_elapsed: i64,
    pub elapsed_fraction: f64,
    /// Whether the viewed period is the real calendar month. It drives two things: the live
    /// account balances only feed "what's left" when true (spec: off-month is a plain
    /// balance of that month's transactions + envelopes), and the header labels it as
    /// current vs. past/upcoming.
    pub is_current: bool,
    pub whats_left: WhatsLeft,
    /// Every account, in name order — the editable ground-truth balances.
    pub accounts: Vec<Account>,
    /// Standalone income+bills (no envelope), income first — the list you toggle settled.
    pub standalone: Vec<Txn>,
    pub envelopes: Vec<EnvelopeRow>,
}

impl MonthView {
    /// Build the view for the calendar month containing `today`. `None` if it isn't stamped.
    /// A thin wrapper over `build_for` kept for callers (and tests) that just want "now".
    pub fn build(conn: &Connection, today: NaiveDate) -> Result<Option<MonthView>> {
        Self::build_for(conn, today, today.year(), today.month())
    }

    /// Build the view for a specific `year`-`month` period. Returns `None` when that period
    /// has no stamped month — the dashboard renders a "not stamped" prompt in that case and
    /// still lets you keep navigating.
    pub fn build_for(
        conn: &Connection,
        today: NaiveDate,
        year: i32,
        month: u32,
    ) -> Result<Option<MonthView>> {
        let label = format!("{year:04}-{month:02}");
        let Some(month_row) = queries::month_by_label(conn, &label)? else {
            return Ok(None);
        };

        // The one calendar month we're actually living in. Only then are the real-world
        // account balances part of the headline; past/future months are self-contained.
        let is_current = year == today.year() && month == today.month();

        let accounts = queries::load_accounts(conn)?;
        let all_txns = queries::load_txns(conn, &month_row.id)?;
        let envelopes_raw = queries::load_envelopes(conn, &month_row.id)?;

        // Manual envelope spending is needed only as one total per envelope. Group it once
        // rather than cloning and filtering the entire transaction list for every envelope.
        let mut spending_by_envelope = HashMap::<&str, Money>::new();
        for txn in &all_txns {
            if let Some(envelope_id) = txn.envelope_id.as_deref() {
                let total = spending_by_envelope
                    .entry(envelope_id)
                    .or_insert(Money::ZERO);
                *total = *total + txn.amount;
            }
        }

        let fraction = calc::elapsed_fraction(month_row.start_date, month_row.days_in_month, today);
        let days_elapsed = calc::days_elapsed(month_row.start_date, month_row.days_in_month, today);

        // Cash-flow roles by account type: checking is spendable, credit cards are debt, and
        // carry balances net out (buffers −, card carryovers +). These three account-derived
        // terms only apply to the current month — off-month we zero them so "what's left" is
        // purely income − bills − envelopes for that period.
        let (funds_available, card_debt, checking_buffer, card_carry) = if is_current {
            let funds: Money = accounts
                .iter()
                .filter(|a| a.account_type == AccountType::Checking)
                .map(|a| a.balance)
                .sum();
            let debt: Money = accounts.iter().map(|a| a.owed()).sum();
            let buffer: Money = accounts
                .iter()
                .filter(|a| a.account_type == AccountType::Checking)
                .map(|a| a.carry_balance.unwrap_or(Money::ZERO))
                .sum();
            let carry: Money = accounts
                .iter()
                .filter(|a| a.account_type == AccountType::CreditCard)
                .map(|a| a.carry_balance.unwrap_or(Money::ZERO))
                .sum();
            (funds, debt, buffer, carry)
        } else {
            (Money::ZERO, Money::ZERO, Money::ZERO, Money::ZERO)
        };

        // Standalone transactions (envelope_id IS NULL) drive income/bills remaining.
        let standalone: Vec<Txn> = all_txns
            .iter()
            .filter(|t| t.envelope_id.is_none())
            .cloned()
            .collect();

        let income_remaining: Money = standalone
            .iter()
            .filter(|t| t.direction == Direction::In && !t.settled)
            .map(|t| t.amount)
            .sum();
        let bills_remaining: Money = standalone
            .iter()
            .filter(|t| t.direction == Direction::Out && !t.settled)
            .map(|t| t.amount)
            .sum();

        // Each envelope's remaining, using its own transactions when manual.
        let mut envelopes = Vec::new();
        for env in envelopes_raw {
            // `env.mode` is the frozen snapshot from stamp time — no global re-resolution.
            let consumed = match env.mode {
                crate::models::Mode::Automatic => {
                    calc::envelope_consumed(&env, env.mode, &[], fraction)
                }
                crate::models::Mode::Manual => spending_by_envelope
                    .get(env.id.as_str())
                    .copied()
                    .unwrap_or(Money::ZERO),
            };
            let remaining = calc::envelope_remaining(&env, consumed);
            envelopes.push(EnvelopeRow {
                envelope: env,
                consumed,
                remaining,
            });
        }
        let envelopes_remaining: Money = envelopes.iter().map(|e| e.remaining).sum();

        // Sort standalone so income shows first, then bills; each group alphabetical.
        let mut standalone = standalone;
        standalone.sort_by(|a, b| {
            let dir = dir_rank(a.direction).cmp(&dir_rank(b.direction));
            dir.then_with(|| a.display_label().cmp(b.display_label()))
        });

        let whats_left = WhatsLeft::compute_with_carry_parts(
            funds_available,
            card_debt,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
            checking_buffer,
            card_carry,
        );

        Ok(Some(MonthView {
            month: month_row,
            days_elapsed,
            elapsed_fraction: fraction,
            is_current,
            whats_left,
            accounts, // moved in after its balances were summed above
            standalone,
            envelopes,
        }))
    }
}

fn dir_rank(d: Direction) -> u8 {
    match d {
        Direction::In => 0,
        Direction::Out => 1,
    }
}

/// The date window used by the Series screen. The selected range scopes both the chart and
/// every stat unless a future stat explicitly says otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeriesTimeRange {
    Last12Stamped,
    ThisYear,
    LastYear,
    AllHistory,
}

impl SeriesTimeRange {
    pub fn label(self, today: NaiveDate) -> String {
        match self {
            SeriesTimeRange::Last12Stamped => "Last 12 stamped months".into(),
            SeriesTimeRange::ThisYear => format!("This year ({})", today.year()),
            SeriesTimeRange::LastYear => format!("Last year ({})", today.year() - 1),
            SeriesTimeRange::AllHistory => "All history".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SeriesGroup {
    Income,
    Expenses,
    Envelopes,
}

impl SeriesGroup {
    pub fn for_series(series: &Series) -> SeriesGroup {
        match series.kind {
            Kind::Envelope => SeriesGroup::Envelopes,
            Kind::Transaction => match series.direction {
                Some(Direction::In) => SeriesGroup::Income,
                _ => SeriesGroup::Expenses,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SeriesGroup::Income => "Income",
            SeriesGroup::Expenses => "Expenses",
            SeriesGroup::Envelopes => "Envelopes",
        }
    }
}

/// One month on the selected series chart. `None` means the month is on the x-axis but the
/// series had no row in that month, so charts render a gap instead of a fake zero.
#[derive(Clone, Debug)]
pub struct SeriesTrendPoint {
    pub month_label: String,
    pub effective: Option<Money>,
    pub planned: Option<Money>,
    pub occurrence_count: usize,
    pub settled_count: usize,
    pub unsettled_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SeriesStats {
    pub latest: Option<Money>,
    pub min: Option<Money>,
    pub max: Option<Money>,
    pub avg: Option<Money>,
    pub planned_avg: Option<Money>,
    pub avg_delta: Option<Money>,
    pub occurrence_count: usize,
}

#[derive(Clone, Debug)]
pub struct SeriesCurrentSummary {
    pub month_label: String,
    pub amount: Money,
    pub occurrence_count: usize,
    pub settled_count: usize,
    pub unsettled_count: usize,
}

#[derive(Clone, Debug)]
pub struct SeriesDetailView {
    pub series: Series,
    pub group: SeriesGroup,
    pub plan_names: Vec<String>,
    pub points: Vec<SeriesTrendPoint>,
    pub stats: SeriesStats,
    pub current: Option<SeriesCurrentSummary>,
}

pub struct SeriesPageView {
    pub range: SeriesTimeRange,
    pub range_label: String,
    pub details: Vec<SeriesDetailView>,
}

impl SeriesPageView {
    pub fn build(
        conn: &Connection,
        today: NaiveDate,
        range: SeriesTimeRange,
    ) -> Result<SeriesPageView> {
        let all_months = queries::months(conn)?;
        let axis = months_for_range(&all_months, today, range);
        let current_label = format!("{:04}-{:02}", today.year(), today.month());
        let current_month = all_months.iter().find(|month| month.label == current_label);
        let aggregates = queries::series_trend_aggregates(conn)?;
        let mut plan_names = queries::plan_names_by_series(conn)?;

        let mut details = Vec::new();
        for series in queries::list_series(conn)? {
            let points = trend_points(&series, &axis, &aggregates);
            let current =
                current_month.and_then(|month| current_summary(&series, month, &aggregates));
            details.push(SeriesDetailView {
                group: SeriesGroup::for_series(&series),
                plan_names: plan_names.remove(&series.id).unwrap_or_default(),
                stats: stats_for_points(&points),
                points,
                current,
                series,
            });
        }

        details.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| a.series.label.cmp(&b.series.label))
                .then_with(|| a.series.id.cmp(&b.series.id))
        });

        Ok(SeriesPageView {
            range,
            range_label: range.label(today),
            details,
        })
    }
}

fn months_for_range(months: &[Month], today: NaiveDate, range: SeriesTimeRange) -> Vec<Month> {
    match range {
        SeriesTimeRange::Last12Stamped => {
            let start = months.len().saturating_sub(12);
            months[start..].to_vec()
        }
        SeriesTimeRange::ThisYear => months
            .iter()
            .filter(|m| m.start_date.year() == today.year())
            .cloned()
            .collect(),
        SeriesTimeRange::LastYear => months
            .iter()
            .filter(|m| m.start_date.year() == today.year() - 1)
            .cloned()
            .collect(),
        SeriesTimeRange::AllHistory => months.to_vec(),
    }
}

fn trend_points(
    series: &Series,
    months: &[Month],
    aggregates: &std::collections::HashMap<
        String,
        std::collections::HashMap<String, queries::SeriesTrendAggregate>,
    >,
) -> Vec<SeriesTrendPoint> {
    months
        .iter()
        .map(|month| trend_point(series, month, aggregates))
        .collect()
}

fn trend_point(
    series: &Series,
    month: &Month,
    aggregates: &std::collections::HashMap<
        String,
        std::collections::HashMap<String, queries::SeriesTrendAggregate>,
    >,
) -> SeriesTrendPoint {
    let aggregate = aggregates
        .get(&series.id)
        .and_then(|by_month| by_month.get(&month.id));
    let occurrence_count = aggregate.map(|value| value.occurrence_count).unwrap_or(0);
    let settled_count = aggregate.map(|value| value.settled_count).unwrap_or(0);

    SeriesTrendPoint {
        month_label: month.label.clone(),
        effective: aggregate.map(|value| value.effective),
        planned: aggregate.and_then(|value| value.planned),
        occurrence_count,
        settled_count,
        unsettled_count: occurrence_count.saturating_sub(settled_count),
    }
}

fn current_summary(
    series: &Series,
    month: &Month,
    aggregates: &std::collections::HashMap<
        String,
        std::collections::HashMap<String, queries::SeriesTrendAggregate>,
    >,
) -> Option<SeriesCurrentSummary> {
    let point = trend_point(series, month, aggregates);
    point.effective.map(|amount| SeriesCurrentSummary {
        month_label: point.month_label,
        amount,
        occurrence_count: point.occurrence_count,
        settled_count: point.settled_count,
        unsettled_count: point.unsettled_count,
    })
}

fn stats_for_points(points: &[SeriesTrendPoint]) -> SeriesStats {
    let values: Vec<Money> = points.iter().filter_map(|p| p.effective).collect();
    let planned_values: Vec<Money> = points.iter().filter_map(|p| p.planned).collect();
    let paired_values: Vec<(Money, Money)> = points
        .iter()
        .filter_map(|p| Some((p.effective?, p.planned?)))
        .collect();
    let occurrence_count = points.iter().map(|p| p.occurrence_count).sum();

    SeriesStats {
        latest: values.last().copied(),
        min: values.iter().copied().min(),
        max: values.iter().copied().max(),
        avg: average_money(&values),
        planned_avg: average_money(&planned_values),
        avg_delta: avg_delta(&paired_values),
        occurrence_count,
    }
}

fn average_money(values: &[Money]) -> Option<Money> {
    if values.is_empty() {
        None
    } else {
        let sum: i64 = values.iter().map(|m| m.cents()).sum();
        Some(Money((sum as f64 / values.len() as f64).round() as i64))
    }
}

fn avg_delta(values: &[(Money, Money)]) -> Option<Money> {
    if values.is_empty() {
        None
    } else {
        let effective: Vec<Money> = values.iter().map(|(effective, _)| *effective).collect();
        let planned: Vec<Money> = values.iter().map(|(_, planned)| *planned).collect();
        Some(average_money(&effective)? - average_money(&planned)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Mode, PeriodType};
    use crate::money::Money;
    use crate::{db, ops};
    use chrono::{Local, NaiveDate};

    #[test]
    fn editing_a_balance_moves_whats_left() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_starter(&mut conn).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        let before = MonthView::build(&conn, today).unwrap().unwrap();
        assert_eq!(
            before.accounts.len(),
            2,
            "seed makes checking + credit card"
        );

        // Bump the (unprotected) checking account by exactly $500.
        let checking = before
            .accounts
            .iter()
            .find(|a| a.name == "Checking")
            .unwrap();
        let new_balance = checking.balance + Money::from_dollars(500.0);
        ops::set_balance(&conn, &checking.id, new_balance).unwrap();

        let after = MonthView::build(&conn, today).unwrap().unwrap();
        // Funds and the headline both rise by the same $500; nothing else shifts.
        assert_eq!(
            after.whats_left.funds_available,
            before.whats_left.funds_available + Money::from_dollars(500.0)
        );
        assert_eq!(
            after.whats_left.whats_left,
            before.whats_left.whats_left + Money::from_dollars(500.0)
        );
    }

    #[test]
    fn automatic_envelope_transactions_do_not_change_whats_left() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_starter(&mut conn).unwrap();
        let today = Local::now().date_naive();
        let month = queries::current_month(&conn).unwrap().unwrap();
        let envelope_id = ops::add_oneoff_envelope(
            &conn,
            &month.id,
            "Trip",
            Money::from_dollars(200.0),
            PeriodType::Monthly,
            Mode::Automatic,
        )
        .unwrap();

        let before = MonthView::build(&conn, today).unwrap().unwrap();
        let before_trip = before
            .envelopes
            .iter()
            .find(|row| row.envelope.id == envelope_id)
            .unwrap();
        ops::add_envelope_spending(
            &conn,
            &month.id,
            &envelope_id,
            "Flight",
            Money::from_dollars(75.0),
        )
        .unwrap();

        let after = MonthView::build(&conn, today).unwrap().unwrap();
        let after_trip = after
            .envelopes
            .iter()
            .find(|row| row.envelope.id == envelope_id)
            .unwrap();
        assert_eq!(after.whats_left.whats_left, before.whats_left.whats_left);
        assert_eq!(after_trip.consumed, before_trip.consumed);
        assert_eq!(after_trip.remaining, before_trip.remaining);
    }

    #[test]
    fn credit_card_owed_reduces_whats_left() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_starter(&mut conn).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        // Fund the checking account and draw the card down so it carries a balance.
        for acct in queries::load_accounts(&conn).unwrap() {
            match acct.name.as_str() {
                "Checking" => {
                    ops::set_balance(&conn, &acct.id, Money::from_dollars(4200.0)).unwrap()
                }
                "Credit Card" => {
                    ops::set_available_credit(&conn, &acct.id, Money::from_dollars(4150.0)).unwrap()
                }
                _ => {}
            }
        }

        let before = MonthView::build(&conn, today).unwrap().unwrap();
        // Funds are checking only; the card owes 5000 − 4150 = 850.
        assert_eq!(
            before.whats_left.funds_available,
            Money::from_dollars(4200.0)
        );
        assert_eq!(before.whats_left.card_debt, Money::from_dollars(850.0));
        // Regression guard: the card must SUBTRACT, not add (the old bug added it).
        assert!(
            before.whats_left.whats_left
                < before.whats_left.funds_available + before.whats_left.income_remaining
        );

        // Spend $100 on the card → available drops $100 → owed +$100 → what's left −$100.
        let card = before
            .accounts
            .iter()
            .find(|a| a.name == "Credit Card")
            .unwrap();
        let new_avail = card.available_credit.unwrap() - Money::from_dollars(100.0);
        ops::set_available_credit(&conn, &card.id, new_avail).unwrap();

        let after = MonthView::build(&conn, today).unwrap().unwrap();
        assert_eq!(
            after.whats_left.card_debt,
            before.whats_left.card_debt + Money::from_dollars(100.0)
        );
        assert_eq!(
            after.whats_left.whats_left,
            before.whats_left.whats_left - Money::from_dollars(100.0)
        );
    }

    #[test]
    fn off_month_excludes_account_balances() {
        use crate::ops;
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_starter(&mut conn).unwrap(); // stamps the current calendar month
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        // Fund the checking account so the current month has a balance to fold in.
        let checking = queries::load_accounts(&conn)
            .unwrap()
            .into_iter()
            .find(|a| a.name == "Checking")
            .unwrap();
        ops::set_balance(&conn, &checking.id, Money::from_dollars(4200.0)).unwrap();

        // Stamp a clearly non-current period (next year) from the same starter plan.
        let plan_id = queries::plans(&conn).unwrap()[0].id.clone();
        let start = NaiveDate::from_ymd_opt(2027, 3, 1).unwrap();
        ops::stamp(&mut conn, &plan_id, "2027-03", start, 31).unwrap();

        let future = MonthView::build_for(&conn, today, 2027, 3)
            .unwrap()
            .unwrap();

        // Off-month: the three account-derived terms drop out entirely...
        assert!(!future.is_current);
        assert_eq!(future.whats_left.funds_available, Money::ZERO);
        assert_eq!(future.whats_left.card_debt, Money::ZERO);
        assert_eq!(future.whats_left.carry_adjustment, Money::ZERO);
        // ...leaving a plain income − bills − envelopes balance.
        let wl = &future.whats_left;
        assert_eq!(
            wl.whats_left,
            wl.income_remaining - wl.bills_remaining - wl.envelopes_remaining
        );

        // The current month, by contrast, still folds the checking balance in.
        let current = MonthView::build_for(&conn, today, 2026, 7)
            .unwrap()
            .unwrap();
        assert!(current.is_current);
        assert_eq!(
            current.whats_left.funds_available,
            Money::from_dollars(4200.0)
        );
    }

    #[test]
    fn unstamped_period_has_no_view() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_starter(&mut conn).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        // A period nobody stamped returns None — the dashboard renders its "not stamped"
        // prompt for this and still lets you navigate away.
        assert!(
            MonthView::build_for(&conn, today, 2099, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn series_stats_total_repeated_monthly_occurrences() {
        use crate::models::{Direction, Kind};

        let mut conn = db::open_in_memory().unwrap();
        let plan_id = ops::create_plan(&conn, "Normal").unwrap();
        let paycheck = ops::create_series(
            &conn,
            Kind::Transaction,
            "Paycheck",
            Some(Direction::In),
            None,
            None,
        )
        .unwrap();
        let first =
            ops::add_plan_item(&conn, &plan_id, &paycheck, Money::from_dollars(100.0)).unwrap();
        let second =
            ops::add_plan_item(&conn, &plan_id, &paycheck, Money::from_dollars(200.0)).unwrap();

        stamp_month(&mut conn, &plan_id, 2026, 1);
        ops::set_item_amount(&conn, &first, Money::from_dollars(150.0)).unwrap();
        ops::set_item_amount(&conn, &second, Money::from_dollars(250.0)).unwrap();
        stamp_month(&mut conn, &plan_id, 2026, 2);

        let view = SeriesPageView::build(
            &conn,
            NaiveDate::from_ymd_opt(2026, 2, 15).unwrap(),
            SeriesTimeRange::AllHistory,
        )
        .unwrap();
        let detail = view
            .details
            .iter()
            .find(|detail| detail.series.id == paycheck)
            .unwrap();

        assert_eq!(detail.points.len(), 2);
        assert_eq!(detail.points[0].effective, Some(Money::from_dollars(300.0)));
        assert_eq!(detail.points[0].occurrence_count, 2);
        assert_eq!(detail.points[1].effective, Some(Money::from_dollars(400.0)));
        assert_eq!(detail.points[1].occurrence_count, 2);
        assert_eq!(detail.stats.latest, Some(Money::from_dollars(400.0)));
        assert_eq!(detail.stats.min, Some(Money::from_dollars(300.0)));
        assert_eq!(detail.stats.max, Some(Money::from_dollars(400.0)));
        assert_eq!(detail.stats.avg, Some(Money::from_dollars(350.0)));
        assert_eq!(detail.stats.occurrence_count, 4);
    }

    #[test]
    fn series_stats_follow_the_selected_time_range() {
        use crate::models::{Direction, Kind};

        let mut conn = db::open_in_memory().unwrap();
        let plan_id = ops::create_plan(&conn, "Normal").unwrap();
        let rent = ops::create_series(
            &conn,
            Kind::Transaction,
            "Rent",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        let item = ops::add_plan_item(&conn, &plan_id, &rent, Money::from_dollars(100.0)).unwrap();

        stamp_month(&mut conn, &plan_id, 2025, 1);
        ops::set_item_amount(&conn, &item, Money::from_dollars(300.0)).unwrap();
        stamp_month(&mut conn, &plan_id, 2026, 1);

        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        let all = SeriesPageView::build(&conn, today, SeriesTimeRange::AllHistory).unwrap();
        let this_year = SeriesPageView::build(&conn, today, SeriesTimeRange::ThisYear).unwrap();

        let all_rent = all
            .details
            .iter()
            .find(|detail| detail.series.id == rent)
            .unwrap();
        let this_year_rent = this_year
            .details
            .iter()
            .find(|detail| detail.series.id == rent)
            .unwrap();

        assert_eq!(all_rent.stats.avg, Some(Money::from_dollars(200.0)));
        assert_eq!(this_year_rent.points.len(), 1);
        assert_eq!(this_year_rent.points[0].month_label, "2026-01");
        assert_eq!(this_year_rent.stats.avg, Some(Money::from_dollars(300.0)));
        assert_eq!(
            this_year_rent.stats.latest,
            Some(Money::from_dollars(300.0))
        );
    }

    fn stamp_month(conn: &mut rusqlite::Connection, plan_id: &str, year: i32, month: u32) {
        let start = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
        let label = format!("{year:04}-{month:02}");
        ops::stamp(
            conn,
            plan_id,
            &label,
            start,
            ops::days_in_month(year, month),
        )
        .unwrap();
    }
}
