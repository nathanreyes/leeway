//! The read-model the UI renders. This is the seam between the headless core and any
//! frontend: it runs the queries, applies the §4 calculations, and hands back plain
//! data. A web or desktop frontend could call `MonthView::build` and render it too.

use crate::calc::{self, WhatsLeft};
use crate::models::{Account, AccountType, Direction, Envelope, Month, Txn};
use crate::money::Money;
use crate::queries;
use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;

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
            let env_txns: Vec<Txn> = all_txns
                .iter()
                .filter(|t| t.envelope_id.as_deref() == Some(env.id.as_str()))
                .cloned()
                .collect();
            let consumed = calc::envelope_consumed(&env, env.mode, &env_txns, fraction);
            let remaining = calc::envelope_remaining(&env, consumed);
            envelopes.push(EnvelopeRow { envelope: env, consumed, remaining });
        }
        let envelopes_remaining: Money = envelopes.iter().map(|e| e.remaining).sum();

        // Sort standalone so income shows first, then bills; each group alphabetical.
        let mut standalone = standalone;
        standalone.sort_by(|a, b| {
            let dir = dir_rank(a.direction).cmp(&dir_rank(b.direction));
            dir.then_with(|| a.label.cmp(&b.label))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Money;
    use crate::{db, ops};
    use chrono::NaiveDate;

    #[test]
    fn editing_a_balance_moves_whats_left() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_demo(&mut conn).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        let before = MonthView::build(&conn, today).unwrap().unwrap();
        assert_eq!(before.accounts.len(), 2, "seed makes checking + credit card");

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
    fn credit_card_owed_reduces_whats_left() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_demo(&mut conn).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        let before = MonthView::build(&conn, today).unwrap().unwrap();
        // Funds are checking only; the demo card owes 5000 − 4150 = 850.
        assert_eq!(before.whats_left.funds_available, Money::from_dollars(4200.0));
        assert_eq!(before.whats_left.card_debt, Money::from_dollars(850.0));
        // Regression guard: the card must SUBTRACT, not add (the old bug added it).
        assert!(before.whats_left.whats_left < before.whats_left.funds_available + before.whats_left.income_remaining);

        // Spend $100 on the card → available drops $100 → owed +$100 → what's left −$100.
        let card = before.accounts.iter().find(|a| a.name == "Credit Card").unwrap();
        let new_avail = card.available_credit.unwrap() - Money::from_dollars(100.0);
        ops::set_available_credit(&conn, &card.id, new_avail).unwrap();

        let after = MonthView::build(&conn, today).unwrap().unwrap();
        assert_eq!(after.whats_left.card_debt, before.whats_left.card_debt + Money::from_dollars(100.0));
        assert_eq!(after.whats_left.whats_left, before.whats_left.whats_left - Money::from_dollars(100.0));
    }

    #[test]
    fn off_month_excludes_account_balances() {
        use crate::ops;
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_demo(&mut conn).unwrap(); // stamps the current calendar month
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        // Stamp a clearly non-current period (next year) from the same demo plan.
        let plan_id = queries::plans(&conn).unwrap()[0].id.clone();
        let start = NaiveDate::from_ymd_opt(2027, 3, 1).unwrap();
        ops::stamp(&mut conn, &plan_id, "2027-03", start, 31).unwrap();

        let future = MonthView::build_for(&conn, today, 2027, 3).unwrap().unwrap();

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
        let current = MonthView::build_for(&conn, today, 2026, 7).unwrap().unwrap();
        assert!(current.is_current);
        assert_eq!(current.whats_left.funds_available, Money::from_dollars(4200.0));
    }

    #[test]
    fn unstamped_period_has_no_view() {
        let mut conn = db::open_in_memory().unwrap();
        ops::seed_demo(&mut conn).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        // A period nobody stamped returns None — the dashboard renders its "not stamped"
        // prompt for this and still lets you navigate away.
        assert!(MonthView::build_for(&conn, today, 2099, 1).unwrap().is_none());
    }
}
