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
    pub funds_available: Money,     // spendable checking balances (raw, before buffers)
    pub card_debt: Money,           // total credit-card owed (= limit − available), raw
    pub income_remaining: Money,    // unsettled standalone income
    pub bills_remaining: Money,     // unsettled standalone bills
    pub envelopes_remaining: Money, // sum of every envelope's remaining
    pub carry_adjustment: Money,    // net of checking buffers (−) and card carryovers (+)
    pub whats_left: Money,          // the headline
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
        // `funds_available` and `card_debt` stay raw so the dashboard can show the real
        // cash and real debt; the carry balances land here as one already-signed term
        // (see Account::carry_adjustment for the per-type sign).
        let whats_left = funds_available - card_debt + income_remaining
            - bills_remaining
            - envelopes_remaining
            + carry_adjustment;
        WhatsLeft {
            funds_available,
            card_debt,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
            carry_adjustment,
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
            series_id: Some("s1".into()),
            label: "Groceries".into(),
            category: None,
            amount: Money::from_dollars(2000.0),
            stamped_amount: Money::from_dollars(2000.0),
            period_type: PeriodType::Monthly,
            mode: Mode::Automatic,
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
            series_id: Some("s1".into()),
            label: "Dining".into(),
            category: None,
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
        assert_eq!(envelope_remaining(&env, consumed), Money::from_dollars(240.50));
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

        let carry: Money = [&checking, &card].iter().map(|a| a.carry_adjustment()).sum();
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
