//! A tiny money type: an integer count of cents with dollar-aware display.
//!
//! Why a newtype instead of a bare `i64`? Two reasons worth internalizing as you
//! learn Rust:
//!   1. Type safety — `Money` and a plain `i64` (a day count, an id) can't be added
//!      by accident. The compiler rejects it.
//!   2. Behavior in one place — formatting ("$1,234.56"), parsing, and rounding live
//!      here, so the rest of the app never hand-rolls `/ 100.0`.

use std::fmt;
use std::ops::{Add, Sub};

/// An exact monetary amount, stored as a signed number of cents.
///
/// `#[derive(...)]` asks the compiler to auto-generate common trait impls:
///  - `Clone, Copy`  — `Money` is a single `i64`, so it's cheap to copy by value
///    (no need to pass references around).
///  - `PartialEq, Eq` — `==` comparisons.
///  - `PartialOrd, Ord` — `<`, `>`, sorting.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Money(pub i64); // the inner i64 is `pub` so `Money(1234)` and `.0` work within the crate

impl Money {
    /// Zero dollars — handy as a starting accumulator.
    pub const ZERO: Money = Money(0);

    /// Build from a whole-and-fractional dollar figure, e.g. `Money::from_dollars(12.34)`.
    /// Rounds to the nearest cent. Used when seeding demo data or accepting typed input.
    pub fn from_dollars(dollars: f64) -> Money {
        // `.round()` gives banker-free nearest rounding; `as i64` truncates the now-integral float.
        Money((dollars * 100.0).round() as i64)
    }

    /// The raw cents. Named method (rather than `.0` everywhere) reads better at call sites.
    pub fn cents(self) -> i64 {
        self.0
    }

    /// Parse user-typed dollars into `Money`, tolerating `$` and thousands commas:
    /// "70", "$1,234.56", "-12.5" all work. Returns `None` on anything unparseable, so
    /// the UI can reject bad input instead of silently storing garbage.
    pub fn parse_dollars(input: &str) -> Option<Money> {
        // Strip the characters humans add for readability, then lean on f64 parsing.
        let cleaned: String = input
            .chars()
            .filter(|c| *c != '$' && *c != ',' && !c.is_whitespace())
            .collect();
        if cleaned.is_empty() {
            return None;
        }
        cleaned.parse::<f64>().ok().map(Money::from_dollars)
    }

    /// Scale by a fraction in [0.0, 1.0] and round to the nearest cent.
    /// This is exactly automatic-envelope accrual: `amount * elapsed_fraction`.
    /// We do the multiply in floating point (a fraction of a value is inherently
    /// fractional) but immediately round back to exact integer cents.
    pub fn scale(self, fraction: f64) -> Money {
        Money((self.0 as f64 * fraction).round() as i64)
    }
}

// --- Operator overloading ------------------------------------------------------
// Implementing `Add`/`Sub` lets you write `a + b` and `a - b` on `Money` values.
// `type Output = Money` declares what the operation returns.

impl Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0 + rhs.0)
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0 - rhs.0)
    }
}

// Lets `.sum()` work over an iterator of `Money` (used in the rollup).
impl std::iter::Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        iter.fold(Money::ZERO, |acc, m| acc + m)
    }
}

// --- Display -------------------------------------------------------------------
// Implementing `Display` is what makes `format!("{}", money)` and `.to_string()`
// produce "$1,234.56". This is the single place dollars-and-cents formatting lives.

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.0 < 0;
        let abs = self.0.abs();
        let dollars = abs / 100;
        let cents = abs % 100;

        // Group the dollar part with thousands separators: 1234567 -> "1,234,567".
        let dollar_str = group_thousands(dollars);

        // `{:02}` zero-pads cents to two digits (so 5 cents prints as "05").
        write!(
            f,
            "{}${}.{:02}",
            if negative { "-" } else { "" },
            dollar_str,
            cents
        )
    }
}

/// Insert commas every three digits from the right. Kept private to this module.
fn group_thousands(n: i64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    // Count from the left, inserting a comma before every group of three that
    // remains on the right.
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// --- Tests ---------------------------------------------------------------------
// `cargo test` runs these. They double as executable documentation of the edge cases.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dollars_and_cents() {
        assert_eq!(Money(1234).to_string(), "$12.34");
        assert_eq!(Money(5).to_string(), "$0.05");
        assert_eq!(Money(0).to_string(), "$0.00");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(Money(123456789).to_string(), "$1,234,567.89");
    }

    #[test]
    fn handles_negatives() {
        assert_eq!(Money(-1234).to_string(), "-$12.34");
    }

    #[test]
    fn parses_typed_dollars() {
        assert_eq!(Money::parse_dollars("70"), Some(Money(7000)));
        assert_eq!(Money::parse_dollars("$1,234.56"), Some(Money(123456)));
        assert_eq!(Money::parse_dollars(" -12.5 "), Some(Money(-1250)));
        assert_eq!(Money::parse_dollars(""), None);
        assert_eq!(Money::parse_dollars("abc"), None);
    }

    #[test]
    fn accrual_rounds_to_cents() {
        // $2,000 at 17/30 of the month -> ~$1,133.33
        let consumed = Money::from_dollars(2000.0).scale(17.0 / 30.0);
        assert_eq!(consumed.to_string(), "$1,133.33");
    }
}
