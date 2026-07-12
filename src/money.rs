//! A tiny money type: an integer count of cents with dollar-aware display.
//!
//! Why a newtype instead of a bare `i64`? Two reasons worth internalizing as you
//! learn Rust:
//!   1. Type safety — `Money` and a plain `i64` (a day count, an id) can't be added
//!      by accident. The compiler rejects it.
//!   2. Behavior in one place — formatting ("$1,234.56"), parsing, and rounding live
//!      here, so the rest of the app never hand-rolls `/ 100.0`.

use crate::currency::{self, Currency};
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

    /// Build from a whole-and-fractional figure at a fixed 2-decimal scale, e.g.
    /// `Money::from_dollars(12.34) == Money(1234)`. Currency-neutral: it always uses
    /// a ×100 scale, which is exactly what the test suite relies on when it compares
    /// two `from_dollars` values. For currency-aware seeding of real data use
    /// [`Money::from_major`] instead.
    pub fn from_dollars(dollars: f64) -> Money {
        // `.round()` gives banker-free nearest rounding; `as i64` truncates the now-integral float.
        Money((dollars * 100.0).round() as i64)
    }

    /// Build from a whole-and-fractional figure in `currency`'s major units,
    /// scaling by its minor-unit count: `from_major(50.0, JPY) == Money(50)`,
    /// `from_major(50.0, USD) == Money(5000)`, `from_major(50.0, BHD) == Money(50000)`.
    /// Used to seed starter data in the active currency.
    pub fn from_major(major: f64, currency: Currency) -> Money {
        Money((major * currency.scale() as f64).round() as i64)
    }

    /// The raw cents. Named method (rather than `.0` everywhere) reads better at call sites.
    pub fn cents(self) -> i64 {
        self.0
    }

    /// Parse user-typed input in the **active** currency. Convenience wrapper over
    /// [`Money::parse_in`]; keeps the historic name that call sites and tests use.
    pub fn parse_dollars(input: &str) -> Option<Money> {
        Self::parse_in(input, currency::active())
    }

    /// Parse user-typed input in a given `currency`, tolerating its symbol and
    /// grouping separator: for USD "70", "$1,234.56", "-12.5" all work; for EUR
    /// "1.234,56 €"; for JPY "1,234" (no decimals). Returns `None` on anything
    /// unparseable, so the UI can reject bad input instead of storing garbage.
    pub fn parse_in(input: &str, currency: Currency) -> Option<Money> {
        // Strip the characters humans add for readability (symbol, grouping,
        // whitespace), then parse the decimal text directly. Going through f64 would
        // accept NaN/scientific overflow and can lose a minor unit before we store
        // the value as an integer. The decimal separator is preserved for the split.
        let without_symbol = input.replace(currency.symbol, "");
        let cleaned: String = without_symbol
            .chars()
            .filter(|c| *c != currency.group_sep && !c.is_whitespace())
            .collect();
        if cleaned.is_empty() {
            return None;
        }

        let (negative, unsigned) = match cleaned.as_bytes().first() {
            Some(b'-') => (true, &cleaned[1..]),
            Some(b'+') => (false, &cleaned[1..]),
            _ => (false, cleaned.as_str()),
        };
        let mut parts = unsigned.split(currency.decimal_sep);
        let whole = parts.next()?;
        let fraction = parts.next();
        if parts.next().is_some()
            || (whole.is_empty() && fraction.is_none())
            || !whole.chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }

        let scale = currency.scale() as u64;
        let whole_minor = if whole.is_empty() {
            0
        } else {
            whole.parse::<u64>().ok()?.checked_mul(scale)?
        };
        // Accept up to `minor_units` fractional digits, scaling a short fraction up
        // to the full minor-unit width (USD "5" -> 50 cents). Zero-decimal currencies
        // (JPY) accept no fractional part at all.
        let minor_units = currency.minor_units as usize;
        let fractional_minor = match fraction {
            None | Some("") => 0,
            Some(digits)
                if digits.len() <= minor_units
                    && !digits.is_empty()
                    && digits.chars().all(|c| c.is_ascii_digit()) =>
            {
                digits.parse::<u64>().ok()? * 10u64.pow((minor_units - digits.len()) as u32)
            }
            Some(_) => return None,
        };
        let minor = whole_minor.checked_add(fractional_minor)?;
        if minor > Self::MAX_INPUT_CENTS {
            return None;
        }

        let minor = i64::try_from(minor).ok()?;
        Some(if negative { Money(-minor) } else { Money(minor) })
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
// produce "$1,234.56". This is the single place amount formatting lives; it reads
// the app-wide active currency so the ~20 UI sites that print `Money` all localize
// at once.

impl Money {
    /// Format this amount in a given `currency`. `Display` calls this with the
    /// active currency; tests call it with an explicit one so they never touch the
    /// shared global.
    pub fn format_in(self, currency: Currency) -> String {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let scale = currency.scale() as u64;
        let major = abs / scale;
        let minor = abs % scale;

        // Group the whole part with the currency's separator: 1234567 -> "1,234,567".
        let major_str = group_digits(major, currency.group_sep);

        // Zero-decimal currencies (JPY) print no fractional part at all.
        let body = if currency.minor_units == 0 {
            major_str
        } else {
            format!(
                "{major_str}{}{minor:0width$}",
                currency.decimal_sep,
                width = currency.minor_units as usize
            )
        };
        currency.wrap(sign, &body)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format_in(currency::active()))
    }
}

/// Insert `sep` every three digits from the right: 1234567 -> "1,234,567". A `'\0'`
/// separator disables grouping. Kept private to this module.
fn group_digits(n: u64, sep: char) -> String {
    let digits = n.to_string();
    if sep == '\0' {
        return digits;
    }
    let mut out = String::new();
    // Count from the left, inserting a separator before every group of three that
    // remains on the right.
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(sep);
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

    // --- Multi-currency formatting/parsing --------------------------------------
    // These exercise `format_in`/`parse_in` with explicit currencies, so they never
    // mutate the shared active-currency global and stay order-independent.

    fn cur(code: &str) -> Currency {
        currency::by_code(code).unwrap()
    }

    #[test]
    fn formats_zero_decimal_currency() {
        // JPY: no fractional part; the stored integer IS the whole amount.
        let jpy = cur("JPY");
        assert_eq!(Money(1234).format_in(jpy), "¥1,234");
        assert_eq!(Money(0).format_in(jpy), "¥0");
        assert_eq!(Money(-50).format_in(jpy), "-¥50");
    }

    #[test]
    fn formats_three_decimal_currency() {
        // BHD: three fractional digits, symbol "BD".
        let bhd = cur("BHD");
        assert_eq!(Money(1234).format_in(bhd), "BD1.234");
        assert_eq!(Money(5).format_in(bhd), "BD0.005");
        assert_eq!(Money(1_234_567).format_in(bhd), "BD1,234.567");
    }

    #[test]
    fn formats_suffix_currency_with_swapped_separators() {
        // EUR: symbol after the number, '.' grouping and ',' decimal.
        let eur = cur("EUR");
        assert_eq!(Money(123456).format_in(eur), "1.234,56\u{00a0}€");
        assert_eq!(Money(-500).format_in(eur), "-5,00\u{00a0}€");
    }

    #[test]
    fn parses_in_each_currency() {
        assert_eq!(Money::parse_in("1234", cur("JPY")), Some(Money(1234)));
        assert_eq!(Money::parse_in("¥1,234", cur("JPY")), Some(Money(1234)));
        // Zero-decimal currencies reject a fractional part.
        assert_eq!(Money::parse_in("12.5", cur("JPY")), None);

        assert_eq!(Money::parse_in("1.234", cur("BHD")), Some(Money(1234)));
        assert_eq!(Money::parse_in("BD0.005", cur("BHD")), Some(Money(5)));

        assert_eq!(Money::parse_in("1.234,56\u{00a0}€", cur("EUR")), Some(Money(123456)));
        assert_eq!(Money::parse_in("-5,00 €", cur("EUR")), Some(Money(-500)));
    }

    #[test]
    fn format_then_parse_round_trips() {
        for code in ["USD", "EUR", "JPY", "BHD", "GBP", "CHF"] {
            let c = cur(code);
            for m in [Money(0), Money(5), Money(1234), Money(-1_234_567), Money(999)] {
                let printed = m.format_in(c);
                assert_eq!(Money::parse_in(&printed, c), Some(m), "{code}: {printed}");
            }
        }
    }

    #[test]
    fn from_major_scales_by_minor_units() {
        assert_eq!(Money::from_major(50.0, cur("JPY")), Money(50));
        assert_eq!(Money::from_major(50.0, currency::USD), Money(5000));
        assert_eq!(Money::from_major(50.0, cur("BHD")), Money(50000));
    }
}
