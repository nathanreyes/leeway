//! The derived calculations from app-spec §4.
//!
//! Everything here is a *pure function*: same inputs -> same output, no database, no
//! clock, no I/O. That's what makes them trivial to unit-test (see the bottom of the
//! file) and what lets a future web/desktop frontend reuse them unchanged.

use crate::models::{Envelope, Mode, Txn};
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

/// The mode actually in effect for an envelope: its own `mode`, or the global default
/// when it left the column NULL. This is the `COALESCE(envelope.mode, setting[...])`
/// from the spec, expressed in Rust.
pub fn effective_mode(envelope: &Envelope, default_mode: Mode) -> Mode {
    envelope.mode.unwrap_or(default_mode)
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

/// The "what's left" rollup (spec §4). Each field is pre-summed by the caller so this
/// function stays a pure, obvious formula — the single source of truth for the headline
/// number the whole app exists to show.
#[derive(Clone, Copy, Debug)]
pub struct WhatsLeft {
    pub funds_available: Money,     // unprotected account balances (checking, ...)
    pub protected: Money,           // credit cards + reserve, held back
    pub income_remaining: Money,    // unsettled standalone income
    pub bills_remaining: Money,     // unsettled standalone bills
    pub envelopes_remaining: Money, // sum of every envelope's remaining
    pub whats_left: Money,          // the headline
}

impl WhatsLeft {
    pub fn compute(
        funds_available: Money,
        protected: Money,
        income_remaining: Money,
        bills_remaining: Money,
        envelopes_remaining: Money,
    ) -> WhatsLeft {
        let whats_left = funds_available - protected + income_remaining
            - bills_remaining
            - envelopes_remaining;
        WhatsLeft {
            funds_available,
            protected,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
            whats_left,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Direction, PeriodType};

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
            series_id: "s1".into(),
            label: "Groceries".into(),
            category: None,
            amount: Money::from_dollars(2000.0),
            stamped_amount: Money::from_dollars(2000.0),
            period_type: PeriodType::Monthly,
            mode: Some(Mode::Automatic),
        };
        let fraction = 17.0 / 30.0;
        let consumed = envelope_consumed(&env, Mode::Automatic, &[], fraction);
        assert_eq!(consumed, Money::from_dollars(1133.33));
        assert_eq!(envelope_remaining(&env, consumed), Money::from_dollars(866.67));
    }

    #[test]
    fn manual_envelope_sums_its_transactions() {
        let env = Envelope {
            id: "e1".into(),
            month_id: "m1".into(),
            series_id: "s1".into(),
            label: "Dining".into(),
            category: None,
            amount: Money::from_dollars(300.0),
            stamped_amount: Money::from_dollars(300.0),
            period_type: PeriodType::Monthly,
            mode: Some(Mode::Manual),
        };
        let txns = vec![
            mk_txn(Money::from_dollars(42.50)),
            mk_txn(Money::from_dollars(17.00)),
        ];
        let consumed = envelope_consumed(&env, Mode::Manual, &txns, 0.5);
        assert_eq!(consumed, Money::from_dollars(59.50));
        assert_eq!(envelope_remaining(&env, consumed), Money::from_dollars(240.50));
    }

    #[test]
    fn whats_left_formula() {
        let wl = WhatsLeft::compute(
            Money::from_dollars(5000.0), // funds
            Money::from_dollars(1200.0), // protected
            Money::from_dollars(800.0),  // income remaining
            Money::from_dollars(1500.0), // bills remaining
            Money::from_dollars(900.0),  // envelopes remaining
        );
        // 5000 - 1200 + 800 - 1500 - 900 = 2200
        assert_eq!(wl.whats_left, Money::from_dollars(2200.0));
    }

    fn mk_txn(amount: Money) -> Txn {
        Txn {
            id: "t".into(),
            month_id: "m1".into(),
            series_id: None,
            envelope_id: Some("e1".into()),
            account_id: None,
            label: "x".into(),
            category: None,
            direction: Direction::Out,
            amount,
            stamped_amount: None,
            settled: true,
            date_paid: None,
        }
    }
}
