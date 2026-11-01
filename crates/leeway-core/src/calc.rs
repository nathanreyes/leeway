//! The derived calculations from app-spec §4.
//!
//! Everything here is a *pure function*: same inputs -> same output, no database, no
//! clock, no I/O. That's what makes them trivial to unit-test (see the bottom of the
//! file) and what lets a future web/desktop frontend reuse them unchanged.

use crate::models::{Direction, Envelope, Kind, Mode, MonthSet, PeriodType, PlanEntry, Txn};
use crate::money::Money;
use chrono::NaiveDate;

/// Whole days elapsed since the month began, clamped to `[0, days_in_month]`.
/// Clamping matters: before the month starts it's 0; after it ends it's the full month,
/// so a stale "current month" never accrues past 100%.
pub fn days_elapsed(start_date: NaiveDate, days_in_month: i64, today: NaiveDate) -> i64 {
    let raw = (today - start_date).num_days();
    raw.clamp(0, days_in_month)
}

/// Fraction of the month elapsed, linear by day, in `[0.0, 1.0]`.
pub fn elapsed_fraction(start_date: NaiveDate, days_in_month: i64, today: NaiveDate) -> f64 {
    if days_in_month <= 0 {
        return 0.0; // guard against a malformed month; avoids divide-by-zero
    }
    days_elapsed(start_date, days_in_month, today) as f64 / days_in_month as f64
}

/// The active period set. `Weekly` is retained only so old databases can still be read; the
/// app no longer creates it and treats legacy weekly rows as monthly.
pub fn active_period(period: PeriodType) -> PeriodType {
    match period {
        PeriodType::Daily => PeriodType::Daily,
        PeriodType::Weekly | PeriodType::Monthly => PeriodType::Monthly,
    }
}

/// Convert an entered envelope amount into the concrete budget for one stamped month.
/// Monthly values are already monthly totals; daily values are rates multiplied by that
/// month's actual day count.
pub fn monthlyized_envelope_amount(amount: Money, period: PeriodType, days_in_month: i64) -> Money {
    match active_period(period) {
        PeriodType::Daily => Money(amount.cents().saturating_mul(days_in_month.max(0))),
        PeriodType::Monthly | PeriodType::Weekly => amount,
    }
}

/// Convert a concrete monthly envelope budget back into the amount the user edits for the
/// selected period. Daily rates round to the nearest cent for display and editing.
pub fn envelope_period_amount(
    monthly_amount: Money,
    period: PeriodType,
    days_in_month: i64,
) -> Money {
    match active_period(period) {
        PeriodType::Daily if days_in_month > 0 => monthly_amount.scale(1.0 / days_in_month as f64),
        PeriodType::Daily => Money::ZERO,
        PeriodType::Monthly | PeriodType::Weekly => monthly_amount,
    }
}

/// How much of an envelope is consumed so far.
///
/// - **automatic**: accrues linearly with time — `amount * elapsed_fraction`.
/// - **manual**: the sum of the transactions filed inside it.
///
/// `envelope_txns` should be only the txns whose `envelope_id` is this envelope; the
/// caller (queries/view layer) is responsible for that filtering.
pub fn envelope_consumed(
    envelope: &Envelope,
    mode: Mode,
    envelope_txns: &[Txn],
    elapsed_fraction: f64,
) -> Money {
    match mode {
        Mode::Automatic => envelope.amount.scale(elapsed_fraction),
        Mode::Manual => envelope_txns.iter().map(|t| t.amount).sum(),
    }
}

/// What remains in an envelope: `amount - consumed`. Can go negative (overspent),
/// which is meaningful, so we don't clamp it.
pub fn envelope_remaining(envelope: &Envelope, consumed: Money) -> Money {
    envelope.amount - consumed
}

/// Remaining on a standalone transaction (a bill or paycheck): the full amount while
/// unsettled, zero once settled. Settling is what "releases" it from the forecast.
pub fn txn_remaining(txn: &Txn) -> Money {
    if txn.settled { Money::ZERO } else { txn.amount }
}

/// The normalized month length used to compare reusable plans before they are stamped
/// onto a concrete calendar month. Monthly envelope amounts pass through unchanged;
/// daily rates are projected across this many days.
pub const PLAN_PROJECTION_DAYS: i64 = 30;

/// A plan item that doesn't run every month, held out of the totals and listed on its own.
/// `net` is signed the way it hits the bottom line: income positive, bills and envelopes
/// negative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeasonalItem {
    pub label: String,
    pub net: Money,
    pub months: MonthSet,
}

/// A plan-only cash-flow projection. Unlike [`WhatsLeft`], this deliberately has no
/// account or settlement terms: a reusable plan describes commitments, not live money.
///
/// The four totals describe a **typical** month — they count only the items that run every
/// month. Seasonal items are reported separately in `seasonal` instead of being averaged
/// in, so the headline stays the number you plan against and the exceptions stay visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanProjection {
    pub income: Money,
    pub expenses: Money,
    pub envelopes: Money,
    pub whats_left: Money,
    pub seasonal: Vec<SeasonalItem>,
}

/// Sum a plan into the scenario shown on Plan Details.
///
/// A plan has no target calendar month yet, so daily envelope rates use the documented
/// 30-day assumption. Legacy weekly envelopes continue to behave as monthly values,
/// matching stamping behavior.
pub fn project_plan(entries: &[PlanEntry]) -> PlanProjection {
    let (every_month, seasonal): (Vec<&PlanEntry>, Vec<&PlanEntry>) = entries
        .iter()
        .partition(|entry| entry.active_months.is_all());

    let income: Money = every_month
        .iter()
        .filter(|entry| {
            entry.series.kind == Kind::Transaction && entry.series.direction == Some(Direction::In)
        })
        .map(|entry| entry.amount)
        .sum();
    let expenses: Money = every_month
        .iter()
        .filter(|entry| {
            entry.series.kind == Kind::Transaction && entry.series.direction == Some(Direction::Out)
        })
        .map(|entry| entry.amount)
        .sum();
    let envelopes: Money = every_month
        .iter()
        .filter(|entry| entry.series.kind == Kind::Envelope)
        .map(|entry| projected_monthly_amount(entry))
        .sum();

    PlanProjection {
        income,
        expenses,
        envelopes,
        whats_left: income - expenses - envelopes,
        seasonal: seasonal
            .into_iter()
            .map(|entry| SeasonalItem {
                label: entry.series.label.clone(),
                net: signed_projected_amount(entry),
                months: entry.active_months,
            })
            .collect(),
    }
}

/// One entry's contribution to a month, before direction: envelopes projected across the
/// 30-day assumption, transactions taken as entered.
fn projected_monthly_amount(entry: &PlanEntry) -> Money {
    match entry.series.kind {
        Kind::Envelope => monthlyized_envelope_amount(
            entry.amount,
            entry.series.period_type.unwrap_or(PeriodType::Monthly),
            PLAN_PROJECTION_DAYS,
        ),
        Kind::Transaction => entry.amount,
    }
}

/// The same figure signed by its effect on what's left: incoming money adds, everything
/// else subtracts.
fn signed_projected_amount(entry: &PlanEntry) -> Money {
    let amount = projected_monthly_amount(entry);
    if entry.series.kind == Kind::Transaction && entry.series.direction == Some(Direction::In) {
        amount
    } else {
        Money::ZERO - amount
    }
}

/// The "what's left" rollup (spec §4). Each field is pre-summed by the caller so this
/// function stays a pure, obvious formula — the single source of truth for the headline
/// number the whole app exists to show.
#[derive(Clone, Copy, Debug)]
pub struct WhatsLeft {
    pub funds_available: Money, // spendable checking balances (raw, before buffers)
    pub card_debt: Money,       // total credit-card owed (= limit − available), raw
    pub income_remaining: Money, // unsettled standalone income
    pub bills_remaining: Money, // unsettled standalone bills
    pub envelopes_remaining: Money, // sum of every envelope's remaining
    pub checking_buffer: Money, // checking carry balances held back from spendable funds
    pub card_carry: Money,      // credit-card carry balances not paid this month
    pub carry_adjustment: Money, // net of checking buffers (−) and card carryovers (+)
    pub whats_left: Money,      // the headline
}

impl WhatsLeft {
    pub fn compute(
        funds_available: Money,
        card_debt: Money,
        income_remaining: Money,
        bills_remaining: Money,
        envelopes_remaining: Money,
        carry_adjustment: Money,
    ) -> WhatsLeft {
        WhatsLeft::compute_with_carry_parts(
            funds_available,
            card_debt,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
            Money::ZERO,
            carry_adjustment,
        )
    }

    pub fn compute_with_carry_parts(
        funds_available: Money,
        card_debt: Money,
        income_remaining: Money,
        bills_remaining: Money,
        envelopes_remaining: Money,
        checking_buffer: Money,
        card_carry: Money,
    ) -> WhatsLeft {
        let carry_adjustment = card_carry - checking_buffer;
        // `funds_available` and `card_debt` stay raw so the dashboard can show the real
        // cash and real debt; the carry balances land here as one already-signed term
        // (see Account::carry_adjustment for the per-type sign).
        let whats_left =
            funds_available - card_debt + income_remaining - bills_remaining - envelopes_remaining
                + carry_adjustment;
        WhatsLeft {
            funds_available,
            card_debt,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
            checking_buffer,
            card_carry,
            carry_adjustment,
            whats_left,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Direction, PeriodType, Series};

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn elapsed_fraction_is_linear_and_clamped() {
        let start = ymd(2026, 6, 1);
        // 17 days into a 30-day month -> 17/30
        assert!((elapsed_fraction(start, 30, ymd(2026, 6, 18)) - 17.0 / 30.0).abs() < 1e-9);
        // before the start -> 0
        assert_eq!(elapsed_fraction(start, 30, ymd(2026, 5, 20)), 0.0);
        // long after the end -> capped at 1.0
        assert_eq!(elapsed_fraction(start, 30, ymd(2026, 9, 1)), 1.0);
    }

    #[test]
    fn automatic_envelope_accrues_with_time() {
        // The spec's worked example: a $2,000 monthly grocery envelope at ~17/30
        // should show ~$1,133 consumed.
        let env = Envelope {
            id: "e1".into(),
            month_id: "m1".into(),
            series_id: Some("s1".into()),
            label: "Groceries".into(),
            series_label: None,
            amount: Money::from_dollars(2000.0),
            stamped_amount: Money::from_dollars(2000.0),
            period_type: PeriodType::Monthly,
            mode: Mode::Automatic,
        };
        let fraction = 17.0 / 30.0;
        let consumed = envelope_consumed(&env, Mode::Automatic, &[], fraction);
        assert_eq!(consumed, Money::from_dollars(1133.33));
        assert_eq!(
            envelope_remaining(&env, consumed),
            Money::from_dollars(866.67)
        );
    }

    #[test]
    fn manual_envelope_sums_its_transactions() {
        let env = Envelope {
            id: "e1".into(),
            month_id: "m1".into(),
            series_id: Some("s1".into()),
            label: "Dining".into(),
            series_label: None,
            amount: Money::from_dollars(300.0),
            stamped_amount: Money::from_dollars(300.0),
            period_type: PeriodType::Monthly,
            mode: Mode::Manual,
        };
        let txns = vec![
            mk_txn(Money::from_dollars(42.50)),
            mk_txn(Money::from_dollars(17.00)),
        ];
        let consumed = envelope_consumed(&env, Mode::Manual, &txns, 0.5);
        assert_eq!(consumed, Money::from_dollars(59.50));
        assert_eq!(
            envelope_remaining(&env, consumed),
            Money::from_dollars(240.50)
        );
    }

    #[test]
    fn whats_left_formula() {
        let wl = WhatsLeft::compute(
            Money::from_dollars(5000.0), // funds
            Money::from_dollars(1200.0), // card debt
            Money::from_dollars(800.0),  // income remaining
            Money::from_dollars(1500.0), // bills remaining
            Money::from_dollars(900.0),  // envelopes remaining
            Money::ZERO,                 // carry adjustment
        );
        // 5000 - 1200 + 800 - 1500 - 900 + 0 = 2200
        assert_eq!(wl.whats_left, Money::from_dollars(2200.0));
    }

    #[test]
    fn whats_left_formula_with_split_buffer_and_carry() {
        let wl = WhatsLeft::compute_with_carry_parts(
            Money::from_dollars(5000.0), // funds
            Money::from_dollars(1200.0), // card debt
            Money::from_dollars(800.0),  // income remaining
            Money::from_dollars(1500.0), // bills remaining
            Money::from_dollars(900.0),  // envelopes remaining
            Money::from_dollars(500.0),  // checking buffer
            Money::from_dollars(300.0),  // card carry
        );
        // 5000 - 1200 + 800 - 1500 - 900 - 500 + 300 = 2000
        assert_eq!(wl.checking_buffer, Money::from_dollars(500.0));
        assert_eq!(wl.card_carry, Money::from_dollars(300.0));
        assert_eq!(wl.carry_adjustment, Money::from_dollars(-200.0));
        assert_eq!(wl.whats_left, Money::from_dollars(2000.0));
    }

    #[test]
    fn carry_adjustment_moves_the_headline_both_ways() {
        use crate::models::{Account, AccountType};

        // A checking buffer is a hold-back → subtracts from what's left.
        let checking = Account {
            id: "c".into(),
            name: "Checking".into(),
            account_type: AccountType::Checking,
            balance: Money::from_dollars(3000.0),
            credit_limit: None,
            available_credit: None,
            carry_balance: Some(Money::from_dollars(500.0)),
        };
        // A card carryover is debt you won't pay now → adds back to what's left.
        let card = Account {
            id: "k".into(),
            name: "Card".into(),
            account_type: AccountType::CreditCard,
            balance: Money::ZERO,
            credit_limit: Some(Money::from_dollars(5000.0)),
            available_credit: Some(Money::from_dollars(4000.0)),
            carry_balance: Some(Money::from_dollars(300.0)),
        };
        assert_eq!(checking.carry_adjustment(), Money::from_dollars(-500.0));
        assert_eq!(card.carry_adjustment(), Money::from_dollars(300.0));

        let carry: Money = [&checking, &card]
            .iter()
            .map(|a| a.carry_adjustment())
            .sum();
        assert_eq!(carry, Money::from_dollars(-200.0)); // −500 + 300

        let base = WhatsLeft::compute(
            Money::from_dollars(3000.0),
            card.owed(), // 1000
            Money::ZERO,
            Money::ZERO,
            Money::ZERO,
            Money::ZERO,
        );
        let with_carry = WhatsLeft::compute(
            Money::from_dollars(3000.0),
            card.owed(),
            Money::ZERO,
            Money::ZERO,
            Money::ZERO,
            carry,
        );
        assert_eq!(
            with_carry.whats_left,
            base.whats_left - Money::from_dollars(200.0)
        );
    }

    #[test]
    fn daily_envelope_amount_monthlyizes_by_days_in_month() {
        assert_eq!(
            monthlyized_envelope_amount(Money::from_dollars(15.0), PeriodType::Daily, 30),
            Money::from_dollars(450.0)
        );
        assert_eq!(
            monthlyized_envelope_amount(Money::from_dollars(15.0), PeriodType::Daily, 31),
            Money::from_dollars(465.0)
        );
    }

    #[test]
    fn weekly_period_is_legacy_and_behaves_as_monthly() {
        assert_eq!(active_period(PeriodType::Weekly), PeriodType::Monthly);
        assert_eq!(
            monthlyized_envelope_amount(Money::from_dollars(80.0), PeriodType::Weekly, 31),
            Money::from_dollars(80.0)
        );
    }

    #[test]
    fn plan_projection_classifies_income_and_expenses() {
        let entries = vec![
            plan_entry(
                Kind::Transaction,
                Some(Direction::In),
                None,
                Money::from_dollars(6000.0),
            ),
            plan_entry(
                Kind::Transaction,
                Some(Direction::Out),
                None,
                Money::from_dollars(2200.0),
            ),
        ];

        let projection = project_plan(&entries);

        assert_eq!(projection.income, Money::from_dollars(6000.0));
        assert_eq!(projection.expenses, Money::from_dollars(2200.0));
        assert_eq!(projection.envelopes, Money::ZERO);
        assert_eq!(projection.whats_left, Money::from_dollars(3800.0));
    }

    #[test]
    fn plan_projection_uses_monthly_and_thirty_day_envelopes() {
        let entries = vec![
            plan_entry(
                Kind::Envelope,
                None,
                Some(PeriodType::Monthly),
                Money::from_dollars(500.0),
            ),
            plan_entry(
                Kind::Envelope,
                None,
                Some(PeriodType::Daily),
                Money::from_dollars(12.0),
            ),
        ];

        let projection = project_plan(&entries);

        assert_eq!(projection.envelopes, Money::from_dollars(860.0));
        assert_eq!(projection.whats_left, Money::from_dollars(-860.0));
    }

    #[test]
    fn empty_plan_projection_is_zero() {
        assert_eq!(
            project_plan(&[]),
            PlanProjection {
                income: Money::ZERO,
                expenses: Money::ZERO,
                envelopes: Money::ZERO,
                whats_left: Money::ZERO,
                seasonal: Vec::new(),
            }
        );
    }

    #[test]
    fn plan_projection_holds_seasonal_items_out_of_the_totals() {
        let entries = vec![
            plan_entry(
                Kind::Transaction,
                Some(Direction::In),
                None,
                Money::from_dollars(3000.0),
            ),
            plan_entry(
                Kind::Envelope,
                None,
                Some(PeriodType::Monthly),
                Money::from_dollars(600.0),
            ),
            seasonal_entry(
                Kind::Envelope,
                None,
                Money::from_dollars(120.0),
                "Kid gifts",
                "mar,jul,nov",
            ),
            seasonal_entry(
                Kind::Transaction,
                Some(Direction::In),
                Money::from_dollars(500.0),
                "Bonus",
                "dec",
            ),
        ];

        let projection = project_plan(&entries);

        // The headline describes an ordinary month: neither seasonal item is folded in.
        assert_eq!(projection.income, Money::from_dollars(3000.0));
        assert_eq!(projection.envelopes, Money::from_dollars(600.0));
        assert_eq!(projection.whats_left, Money::from_dollars(2400.0));

        // Each is reported on its own, signed by how it would hit the bottom line.
        assert_eq!(
            projection.seasonal,
            vec![
                SeasonalItem {
                    label: "Kid gifts".into(),
                    net: Money::from_dollars(-120.0),
                    months: MonthSet::parse("mar,jul,nov").unwrap(),
                },
                SeasonalItem {
                    label: "Bonus".into(),
                    net: Money::from_dollars(500.0),
                    months: MonthSet::parse("dec").unwrap(),
                },
            ]
        );
    }

    #[test]
    fn seasonal_daily_envelope_uses_the_same_thirty_day_projection() {
        let entries = vec![seasonal_entry(
            Kind::Envelope,
            None,
            Money::from_dollars(12.0),
            "Camp snacks",
            "jun-aug",
        )];
        let mut entries = entries;
        entries[0].series.period_type = Some(PeriodType::Daily);

        let projection = project_plan(&entries);

        assert_eq!(projection.envelopes, Money::ZERO);
        assert_eq!(projection.seasonal[0].net, Money::from_dollars(-360.0));
    }

    fn plan_entry(
        kind: Kind,
        direction: Option<Direction>,
        period_type: Option<PeriodType>,
        amount: Money,
    ) -> PlanEntry {
        PlanEntry {
            item_id: "item".into(),
            plan_id: "plan".into(),
            amount,
            forecast_method: crate::models::ForecastMethod::Static,
            active_months: MonthSet::ALL,
            series: Series {
                id: "series".into(),
                kind,
                label: "Test".into(),
                direction,
                period_type,
                mode: (kind == Kind::Envelope).then_some(Mode::Automatic),
            },
        }
    }

    /// A plan entry that only runs in some months, with a label so the projection's
    /// seasonal list can be told apart from the always-on ones.
    fn seasonal_entry(
        kind: Kind,
        direction: Option<Direction>,
        amount: Money,
        label: &str,
        months: &str,
    ) -> PlanEntry {
        let mut entry = plan_entry(
            kind,
            direction,
            (kind == Kind::Envelope).then_some(PeriodType::Monthly),
            amount,
        );
        entry.series.label = label.into();
        entry.active_months = MonthSet::parse(months).expect("test months should parse");
        entry
    }

    fn mk_txn(amount: Money) -> Txn {
        Txn {
            id: "t".into(),
            month_id: "m1".into(),
            series_id: None,
            envelope_id: Some("e1".into()),
            account_id: None,
            label: "x".into(),
            series_label: None,
            direction: Direction::Out,
            amount,
            stamped_amount: None,
            settled: true,
            date_paid: None,
        }
    }
}
