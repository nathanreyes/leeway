//! The app-wide display currency.
//!
//! Leeway stores every amount as an integer count of a currency's *minor units*
//! (see `money.rs`). The currency itself is a single, app-wide choice — not a
//! per-transaction tag and not converted between (there is no FX). This module
//! owns:
//!
//!   1. The `Currency` value type and the curated table of ones we know how to
//!      format (`CURRENCIES`).
//!   2. The process-global "active" currency the `Money` `Display`/parse edge
//!      reads. It is set once at startup from the `setting` table (or, on a fresh
//!      budget, detected from the OS locale) and again whenever a synced budget is
//!      adopted or the user picks a different currency.
//!
//! Keeping formatting *conventions* (symbol, minor-unit digits, decimal and
//! grouping separators, symbol placement) on the currency lets the same integer
//! render correctly whether it is dollars, yen (no decimals), or dinars (three).

use std::sync::RwLock;

/// A currency and everything needed to render and parse amounts in it.
///
/// `Copy` because every field is a `&'static str`, `u32`, `char`, or `bool`, so
/// `active()` can hand back a cheap owned value rather than a lock guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Currency {
    /// ISO 4217 code, e.g. "USD", "JPY", "BHD". This is what we persist.
    pub code: &'static str,
    /// The symbol shown next to amounts, e.g. "$", "€", "¥".
    pub symbol: &'static str,
    /// Number of fractional digits: 0 (JPY), 2 (USD/EUR), 3 (BHD/KWD).
    pub minor_units: u32,
    /// Character between the whole and fractional parts, e.g. '.' or ','.
    pub decimal_sep: char,
    /// Thousands-grouping character, e.g. ',' '.' '\'' ' '. `'\0'` means no grouping.
    pub group_sep: char,
    /// Whether the symbol precedes the number ("$1") or follows it ("1 €").
    pub symbol_prefix: bool,
}

impl Currency {
    /// The scale factor between major and minor units: 10^minor_units.
    /// e.g. 100 for USD, 1 for JPY, 1000 for BHD.
    pub fn scale(&self) -> i64 {
        10i64.pow(self.minor_units)
    }

    /// Wrap an already-formatted number `body` (and its leading `sign`) with the
    /// currency symbol on the correct side. Prefix currencies hug the number
    /// ("-$12.34"); suffix currencies get a thin space ("-12,34 €").
    pub fn wrap(&self, sign: &str, body: &str) -> String {
        if self.symbol_prefix {
            format!("{sign}{}{body}", self.symbol)
        } else {
            format!("{sign}{body}\u{00a0}{}", self.symbol)
        }
    }
}

/// United States dollar — the default until a budget says otherwise. Chosen as the
/// default so the existing test suite (which asserts `$`/`.`/`,`, 2 decimals) keeps
/// passing without touching the global.
pub const USD: Currency = Currency {
    code: "USD",
    symbol: "$",
    minor_units: 2,
    decimal_sep: '.',
    group_sep: ',',
    symbol_prefix: true,
};

/// The currencies Leeway knows how to format, spanning all three minor-unit cases
/// (0 / 2 / 3 decimals). Add rows here to support more — nothing else needs to
/// change. Kept curated (rather than a full ISO 4217 crate) so the separator and
/// placement conventions are explicit and reviewable.
pub const CURRENCIES: &[Currency] = &[
    USD,
    Currency { code: "EUR", symbol: "€", minor_units: 2, decimal_sep: ',', group_sep: '.', symbol_prefix: false },
    Currency { code: "GBP", symbol: "£", minor_units: 2, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "JPY", symbol: "¥", minor_units: 0, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "CNY", symbol: "¥", minor_units: 2, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "INR", symbol: "₹", minor_units: 2, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "CHF", symbol: "CHF", minor_units: 2, decimal_sep: '.', group_sep: '\'', symbol_prefix: true },
    Currency { code: "CAD", symbol: "$", minor_units: 2, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "AUD", symbol: "$", minor_units: 2, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "BHD", symbol: "BD", minor_units: 3, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
    Currency { code: "KWD", symbol: "KD", minor_units: 3, decimal_sep: '.', group_sep: ',', symbol_prefix: true },
];

/// Look up a currency by its ISO code. Returns `None` for codes not in the table.
pub fn by_code(code: &str) -> Option<Currency> {
    CURRENCIES.iter().copied().find(|c| c.code == code)
}

// The active currency. `RwLock` (not a plain `static mut`) so reads are safe from
// the render loop while the settings screen may write. Initialized to USD; startup
// overwrites it with the persisted / detected choice before the first frame.
static ACTIVE: RwLock<Currency> = RwLock::new(USD);

/// The currency amounts are currently displayed and parsed in.
pub fn active() -> Currency {
    *ACTIVE.read().expect("currency lock poisoned")
}

/// Set the app-wide currency. Called at startup, after adopting a synced budget,
/// and when the user picks a different currency. Formatting is a pure read of this
/// value, so a change takes effect on the next render with no other bookkeeping.
pub fn set_active(currency: Currency) {
    *ACTIVE.write().expect("currency lock poisoned") = currency;
}

/// Best-effort default from the OS locale, e.g. `"en-US"` → USD, `"ja-JP"` → JPY.
/// Used only to seed a brand-new budget. Falls back to USD when the locale is
/// missing, has no region, or names a currency we don't have in the table.
pub fn detect_from_locale() -> Currency {
    sys_locale::get_locale()
        .as_deref()
        .and_then(region_of)
        .and_then(currency_for_region)
        .unwrap_or(USD)
}

/// Pull the region subtag out of a BCP-47/POSIX locale string: the part after the
/// language, e.g. "US" from "en-US", "en_US.UTF-8", or "en-Latn-US".
fn region_of(locale: &str) -> Option<String> {
    // Drop any POSIX charset/modifier suffix (".UTF-8", "@euro").
    let base = locale
        .split(['.', '@'])
        .next()
        .unwrap_or(locale);
    // Split language[-script]-REGION on '-' or '_' and take the first 2-letter,
    // uppercased subtag after the first component.
    base.split(['-', '_'])
        .skip(1)
        .find(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|region| region.to_ascii_uppercase())
}

/// Map an ISO 3166 region code to the currency we'd default it to. Only regions
/// whose currency is in `CURRENCIES` are listed; everything else falls back to USD.
fn currency_for_region(region: String) -> Option<Currency> {
    let code = match region.as_str() {
        "US" => "USD",
        "GB" => "GBP",
        "JP" => "JPY",
        "CN" => "CNY",
        "IN" => "INR",
        "CH" | "LI" => "CHF",
        "CA" => "CAD",
        "AU" => "AUD",
        "BH" => "BHD",
        "KW" => "KWD",
        // Euro-area members we can reasonably default to EUR.
        "IE" | "DE" | "FR" | "ES" | "IT" | "NL" | "AT" | "BE" | "PT" | "FI" | "GR"
        | "SK" | "SI" | "LT" | "LV" | "EE" | "LU" | "CY" | "MT" => "EUR",
        _ => return None,
    };
    by_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_matches_minor_units() {
        assert_eq!(USD.scale(), 100);
        assert_eq!(by_code("JPY").unwrap().scale(), 1);
        assert_eq!(by_code("BHD").unwrap().scale(), 1000);
    }

    #[test]
    fn by_code_known_and_unknown() {
        assert_eq!(by_code("EUR").unwrap().code, "EUR");
        assert!(by_code("XYZ").is_none());
    }

    #[test]
    fn region_extraction() {
        assert_eq!(region_of("en-US").as_deref(), Some("US"));
        assert_eq!(region_of("en_US.UTF-8").as_deref(), Some("US"));
        assert_eq!(region_of("ja-JP").as_deref(), Some("JP"));
        assert_eq!(region_of("zh-Hans-CN").as_deref(), Some("CN"));
        assert_eq!(region_of("en"), None);
    }

    #[test]
    fn region_to_currency_with_fallback() {
        assert_eq!(currency_for_region("DE".into()).unwrap().code, "EUR");
        assert_eq!(currency_for_region("JP".into()).unwrap().code, "JPY");
        assert!(currency_for_region("ZZ".into()).is_none());
    }
}
