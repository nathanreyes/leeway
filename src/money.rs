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

    /// The largest user-entered amount we accept. Keeping a full month's daily rate within
    /// the signed-cent range prevents a valid input from overflowing when it is monthlyized.
    const MAX_INPUT_CENTS: u64 = i64::MAX as u64 / 31;

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
        // Strip the characters humans add for readability, then parse the decimal text
        // directly. Going through f64 would accept NaN/scientific overflow and can lose a
        // cent before we ever store the value as an integer.
        let cleaned: String = input
            .chars()
            .filter(|c| *c != '$' && *c != ',' && !c.is_whitespace())
            .collect();
        if cleaned.is_empty() {
            return None;
        }

        let (negative, unsigned) = match cleaned.as_bytes().first() {
            Some(b'-') => (true, &cleaned[1..]),
            Some(b'+') => (false, &cleaned[1..]),
            _ => (false, cleaned.as_str()),
        };
        let mut parts = unsigned.split('.');
        let whole = parts.next()?;
        let fraction = parts.next();
        if parts.next().is_some()
            || (whole.is_empty() && fraction.is_none())
            || !whole.chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }

        let whole_cents = if whole.is_empty() {
            0
        } else {
            whole.parse::<u64>().ok()?.checked_mul(100)?
        };
        let fractional_cents = match fraction {
            None | Some("") => 0,
            Some(digits) if digits.len() == 1 && digits.chars().all(|c| c.is_ascii_digit()) => {
                digits.parse::<u64>().ok()? * 10
            }
            Some(digits) if digits.len() == 2 && digits.chars().all(|c| c.is_ascii_digit()) => {
                digits.parse::<u64>().ok()?
            }
            Some(_) => return None,
        };
        let cents = whole_cents.checked_add(fractional_cents)?;
        if cents > Self::MAX_INPUT_CENTS {
            return None;
        }

        let cents = i64::try_from(cents).ok()?;
        Some(if negative {
            Money(-cents)
        } else {
            Money(cents)
        })
    }

    /// Scale by a fraction in [0.0, 1.0] and round to the nearest cent.
    /// This is exactly automatic-envelope accrual: `amount * elapsed_fraction`.
    /// We do the multiply in floating point (a fraction of a value is inherently
    /// fractional) but immediately round back to exact integer cents.
    pub fn scale(self, fraction: f64) -> Money {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Money((self.0 as f64 * fraction).round() as i64)
    }
}

// --- Operator overloading ------------------------------------------------------
// Implementing `Add`/`Sub` lets you write `a + b` and `a - b` on `Money` values.
// `type Output = Money` declares what the operation returns.

impl Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0.saturating_sub(rhs.0))
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
        let abs = self.0.unsigned_abs();
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
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    // Count from the left, inserting a comma before every group of three that
    // remains on the right.
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
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
    fn rejects_non_decimal_and_out_of_range_input() {
        for input in [
            "NaN",
            "inf",
            "1e100",
            "12.345",
            "--12",
            "$92,000,000,000,000,000",
        ] {
            assert_eq!(Money::parse_dollars(input), None, "{input}");
        }
    }

    #[test]
    fn formats_the_minimum_i64_amount() {
        assert_eq!(Money(i64::MIN).to_string(), "-$92,233,720,368,547,758.08");
    }

    #[test]
    fn accrual_rounds_to_cents() {
        // $2,000 at 17/30 of the month -> ~$1,133.33
        let consumed = Money::from_dollars(2000.0).scale(17.0 / 30.0);
        assert_eq!(consumed.to_string(), "$1,133.33");
    }
}
