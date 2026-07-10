//! Small, self-contained animation state for the Dashboard summary.
//!
//! The rest of the UI is still immediate-mode: each loop builds a fresh `MonthView` from
//! SQLite and renders it. This module remembers only the previous Summary display values and
//! any in-flight tweens, so the renderer can make changed numbers roll and glow without
//! making the read-model stateful.

use leeway::calc::WhatsLeft;
use ratatui::style::{Color, Style};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const ANIM_DURATION: Duration = Duration::from_millis(550);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SummaryTerm {
    Funds,
    Buffer,
    CardDebt,
    Carry,
    IncomeLeft,
    BillsLeft,
    Envelopes,
    WhatsLeft,
}

const TERMS: [SummaryTerm; 8] = [
    SummaryTerm::Funds,
    SummaryTerm::Buffer,
    SummaryTerm::CardDebt,
    SummaryTerm::Carry,
    SummaryTerm::IncomeLeft,
    SummaryTerm::BillsLeft,
    SummaryTerm::Envelopes,
    SummaryTerm::WhatsLeft,
];

#[derive(Clone, Copy, Debug)]
struct Anim {
    from: i64,
    to: i64,
    start: Instant,
}

#[derive(Default)]
pub struct SummaryAnimations {
    prev: HashMap<SummaryTerm, i64>,
    active: HashMap<SummaryTerm, Anim>,
    last_period: Option<(i32, u32, bool)>,
}

impl SummaryAnimations {
    pub fn new() -> SummaryAnimations {
        SummaryAnimations::default()
    }

    pub fn sync(
        &mut self,
        wl: Option<&WhatsLeft>,
        is_current: bool,
        period: (i32, u32),
        now: Instant,
    ) {
        let Some(wl) = wl else {
            self.prev.clear();
            self.active.clear();
            self.last_period = None;
            return;
        };

        let period_key = (period.0, period.1, is_current);
        if self.last_period != Some(period_key) {
            self.prev = current_values(wl);
            self.active.clear();
            self.last_period = Some(period_key);
            return;
        }

        for term in TERMS {
            let current = display_cents(term, wl);
            let previous = self.prev.get(&term).copied().unwrap_or(current);
            if current != previous {
                let from = self.displayed_cents(term, now).unwrap_or(previous);
                self.active.insert(
                    term,
                    Anim {
                        from,
                        to: current,
                        start: now,
                    },
                );
                self.prev.insert(term, current);
            }
        }

        self.active
            .retain(|_, anim| now.saturating_duration_since(anim.start) < ANIM_DURATION);
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.active
            .values()
            .any(|anim| now.saturating_duration_since(anim.start) < ANIM_DURATION)
    }

    pub fn render(
        &self,
        term: SummaryTerm,
        base_cents: i64,
        color: Color,
        now: Instant,
    ) -> (i64, Style) {
        let Some(anim) = self.active.get(&term) else {
            return (base_cents, Style::default().fg(color));
        };
        let elapsed = now.saturating_duration_since(anim.start);
        if elapsed >= ANIM_DURATION {
            return (base_cents, Style::default().fg(color));
        }

        let t = elapsed.as_secs_f64() / ANIM_DURATION.as_secs_f64();
        let p = ease_out_cubic(t);
        let cents = anim.from + ((anim.to - anim.from) as f64 * p).round() as i64;

        let g = glow(t);
        let term_color = term_rgb(color);
        // Terminal cells do not support alpha blending, so "fade to transparent" means
        // holding a tinted background only while the glow is visibly strong, then clearing
        // the background instead of blending toward black.
        let style = if g > 0.58 {
            Style::default()
                .fg(Color::Black)
                .bg(rgb_color(term_color, 1.0))
        } else if g > 0.34 {
            Style::default()
                .fg(Color::Black)
                .bg(rgb_color(term_color, 0.68))
        } else if g > 0.16 {
            Style::default()
                .fg(Color::White)
                .bg(rgb_color(term_color, 0.38))
        } else {
            Style::default().fg(color)
        };
        (cents, style)
    }

    fn displayed_cents(&self, term: SummaryTerm, now: Instant) -> Option<i64> {
        let anim = self.active.get(&term)?;
        let elapsed = now.saturating_duration_since(anim.start);
        if elapsed >= ANIM_DURATION {
            return Some(anim.to);
        }
        let t = elapsed.as_secs_f64() / ANIM_DURATION.as_secs_f64();
        let p = ease_out_cubic(t);
        Some(anim.from + ((anim.to - anim.from) as f64 * p).round() as i64)
    }
}

pub fn display_cents(term: SummaryTerm, wl: &WhatsLeft) -> i64 {
    match term {
        SummaryTerm::Funds => wl.funds_available.cents(),
        SummaryTerm::Buffer => -wl.checking_buffer.cents(),
        SummaryTerm::CardDebt => -wl.card_debt.cents(),
        SummaryTerm::Carry => wl.card_carry.cents(),
        SummaryTerm::IncomeLeft => wl.income_remaining.cents(),
        SummaryTerm::BillsLeft => -wl.bills_remaining.cents(),
        SummaryTerm::Envelopes => -wl.envelopes_remaining.cents(),
        SummaryTerm::WhatsLeft => wl.whats_left.cents(),
    }
}

fn current_values(wl: &WhatsLeft) -> HashMap<SummaryTerm, i64> {
    TERMS
        .into_iter()
        .map(|term| (term, display_cents(term, wl)))
        .collect()
}

fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn glow(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    (1.0 - t).powi(2)
}

fn term_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Cyan => (0, 220, 255),
        Color::Yellow => (255, 210, 70),
        Color::Red => (255, 85, 85),
        Color::Green => (80, 220, 120),
        Color::Magenta => (255, 90, 220),
        Color::White => (255, 255, 255),
        Color::Gray => (180, 180, 180),
        Color::DarkGray => (90, 90, 90),
        Color::Black => (0, 0, 0),
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (255, 255, 255),
    }
}

fn rgb_color(rgb: (u8, u8, u8), strength: f64) -> Color {
    let channel = |value: u8| -> u8 { (value as f64 * strength).round().clamp(0.0, 255.0) as u8 };
    Color::Rgb(channel(rgb.0), channel(rgb.1), channel(rgb.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leeway::money::Money;

    fn wl(funds: i64, income: i64, bills: i64) -> WhatsLeft {
        WhatsLeft::compute_with_carry_parts(
            Money(funds),
            Money::ZERO,
            Money(income),
            Money(bills),
            Money::ZERO,
            Money::ZERO,
            Money::ZERO,
        )
    }

    #[test]
    fn first_sync_sets_a_baseline_without_animating() {
        let mut anims = SummaryAnimations::new();
        let now = Instant::now();
        anims.sync(Some(&wl(10_000, 20_000, 5_000)), true, (2026, 7), now);

        assert!(!anims.is_animating(now));
        assert_eq!(
            anims
                .render(SummaryTerm::WhatsLeft, 25_000, Color::Green, now)
                .0,
            25_000
        );
    }

    #[test]
    fn same_period_value_change_animates_from_previous_value() {
        let mut anims = SummaryAnimations::new();
        let now = Instant::now();
        anims.sync(Some(&wl(10_000, 20_000, 5_000)), true, (2026, 7), now);
        anims.sync(
            Some(&wl(10_000, 20_000, 3_000)),
            true,
            (2026, 7),
            now + Duration::from_millis(10),
        );

        assert!(anims.is_animating(now + Duration::from_millis(10)));
        assert_eq!(
            anims
                .render(
                    SummaryTerm::WhatsLeft,
                    27_000,
                    Color::Green,
                    now + Duration::from_millis(10),
                )
                .0,
            25_000
        );
    }

    #[test]
    fn period_change_sets_a_new_baseline_without_flashing() {
        let mut anims = SummaryAnimations::new();
        let now = Instant::now();
        anims.sync(Some(&wl(10_000, 20_000, 5_000)), true, (2026, 7), now);
        anims.sync(
            Some(&wl(20_000, 20_000, 5_000)),
            true,
            (2026, 8),
            now + Duration::from_millis(10),
        );

        assert!(!anims.is_animating(now + Duration::from_millis(10)));
        assert_eq!(
            anims
                .render(
                    SummaryTerm::WhatsLeft,
                    35_000,
                    Color::Green,
                    now + Duration::from_millis(10),
                )
                .0,
            35_000
        );
    }
}
