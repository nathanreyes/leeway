//! The read-model the UI renders. This is the seam between the headless core and any
//! frontend: it runs the queries, applies the §4 calculations, and hands back plain
//! data. A web or desktop frontend could call `MonthView::build` and render it too.

use crate::calc::{self, WhatsLeft};
use crate::models::{Account, AccountType, Direction, Envelope, Mode, Month, Txn};
use crate::money::Money;
use crate::queries;
use anyhow::Result;
use chrono::NaiveDate;
use rusqlite::Connection;

/// One envelope with its computed state for this point in the month.
pub struct EnvelopeRow {
    pub envelope: Envelope,
    pub effective_mode: Mode,
    pub consumed: Money,
    pub remaining: Money,
}

/// Everything the dashboard needs for the current month.
pub struct MonthView {
    pub month: Month,
    pub days_elapsed: i64,
    pub elapsed_fraction: f64,
    pub whats_left: WhatsLeft,
    /// Every account, in name order — the editable ground-truth balances.
    pub accounts: Vec<Account>,
    /// Standalone income+bills (no envelope), income first — the list you toggle settled.
    pub standalone: Vec<Txn>,
    pub envelopes: Vec<EnvelopeRow>,
}

impl MonthView {
    /// Build the view for the most recent month as of `today`. `None` if nothing stamped.
    pub fn build(conn: &Connection, today: NaiveDate) -> Result<Option<MonthView>> {
        let Some(month) = queries::current_month(conn)? else {
            return Ok(None);
        };

        let default_mode = queries::default_mode(conn)?;
        let accounts = queries::load_accounts(conn)?;
        let all_txns = queries::load_txns(conn, &month.id)?;
        let envelopes_raw = queries::load_envelopes(conn, &month.id)?;

        let fraction = calc::elapsed_fraction(month.start_date, month.days_in_month, today);
        let days_elapsed = calc::days_elapsed(month.start_date, month.days_in_month, today);

        // Cash-flow roles by account type: checking is spendable, credit cards are debt.
        let funds_available: Money = accounts
            .iter()
            .filter(|a| a.account_type == AccountType::Checking)
            .map(|a| a.balance)
            .sum();
        let card_debt: Money = accounts.iter().map(|a| a.owed()).sum();

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
            let mode = calc::effective_mode(&env, default_mode);
            let env_txns: Vec<Txn> = all_txns
                .iter()
                .filter(|t| t.envelope_id.as_deref() == Some(env.id.as_str()))
                .cloned()
                .collect();
            let consumed = calc::envelope_consumed(&env, mode, &env_txns, fraction);
            let remaining = calc::envelope_remaining(&env, consumed);
            envelopes.push(EnvelopeRow { envelope: env, effective_mode: mode, consumed, remaining });
        }
        let envelopes_remaining: Money = envelopes.iter().map(|e| e.remaining).sum();

        // Sort standalone so income shows first, then bills; each group alphabetical.
        let mut standalone = standalone;
        standalone.sort_by(|a, b| {
            let dir = dir_rank(a.direction).cmp(&dir_rank(b.direction));
            dir.then_with(|| a.label.cmp(&b.label))
        });

        let whats_left = WhatsLeft::compute(
            funds_available,
            card_debt,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
        );

        Ok(Some(MonthView {
            month,
            days_elapsed,
            elapsed_fraction: fraction,
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
}
