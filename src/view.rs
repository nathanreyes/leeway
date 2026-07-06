//! The read-model the UI renders. This is the seam between the headless core and any
//! frontend: it runs the queries, applies the §4 calculations, and hands back plain
//! data. A web or desktop frontend could call `MonthView::build` and render it too.

use crate::calc::{self, WhatsLeft};
use crate::models::{Direction, Envelope, Mode, Month, Txn};
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

        // Accounts split into spendable vs held-back.
        let funds_available: Money = accounts.iter().filter(|a| !a.protected).map(|a| a.balance).sum();
        let protected: Money = accounts.iter().filter(|a| a.protected).map(|a| a.balance).sum();

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
            protected,
            income_remaining,
            bills_remaining,
            envelopes_remaining,
        );

        Ok(Some(MonthView {
            month,
            days_elapsed,
            elapsed_fraction: fraction,
            whats_left,
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
