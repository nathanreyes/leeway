//! The domain types: enums for the "coded" text columns and one struct per table/read row.
//!
//! These are plain data — no behavior beyond conversions. The calculations live in
//! `calc.rs`, the SQL in `queries.rs`/`ops.rs`. Keeping data and behavior separate is
//! a deliberate choice that keeps each file small and testable.

use crate::money::Money;
use chrono::NaiveDate;

/// Generates a string-backed enum together with the two traits SQLite needs:
/// `FromSql` (reading a TEXT column into the enum) and `ToSql` (binding the enum
/// back as TEXT). Writing this once means the five enums below stay one line each,
/// and an invalid value in the database becomes a clean error instead of a silent bug.
///
/// `macro_rules!` is Rust's declarative macro system: it pattern-matches on the tokens
/// you pass and expands to real code at compile time. `$name:ident` captures an
/// identifier, `$variant:ident => $s:literal` captures each `Variant => "text"` pair,
/// and `$(...)+` means "one or more, repeated".
macro_rules! sql_enum {
    ($name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum $name { $($variant),+ }

        impl $name {
            /// The canonical text stored in the database.
            pub fn as_str(&self) -> &'static str {
                match self { $( $name::$variant => $s ),+ }
            }
        }

        // Reading: TEXT column -> enum. Unknown text is a typed error.
        impl rusqlite::types::FromSql for $name {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                match value.as_str()? {
                    $( $s => Ok($name::$variant), )+
                    other => Err(rusqlite::types::FromSqlError::Other(
                        format!("invalid {} value: {:?}", stringify!($name), other).into(),
                    )),
                }
            }
        }

        // Writing: enum -> TEXT parameter.
        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }
    };
}

sql_enum!(Direction { In => "in", Out => "out" });
sql_enum!(Kind { Transaction => "transaction", Envelope => "envelope" });
// `Weekly` is retained only so older local databases can still be read. Active UI cycles
// between Daily and Monthly, and calculation treats Weekly as Monthly.
sql_enum!(PeriodType { Daily => "daily", Weekly => "weekly", Monthly => "monthly" });
sql_enum!(Mode { Automatic => "automatic", Manual => "manual" });
sql_enum!(ForecastMethod {
    Static => "static",
    PreviousMonth => "previous_month",
    AveragePrevious3 => "average_previous_3",
    SameMonthLastYear => "same_month_last_year",
    OverallAverage => "overall_average",
});
sql_enum!(AccountType {
    Checking => "checking",
    CreditCard => "credit_card",
});
sql_enum!(CreditCardEntryMode {
    AvailableCredit => "available_credit",
    CurrentBalance => "current_balance",
});

impl ForecastMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::PreviousMonth => "previous month",
            Self::AveragePrevious3 => "average previous 3",
            Self::SameMonthLastYear => "same month last year",
            Self::OverallAverage => "overall average",
        }
    }
}

impl CreditCardEntryMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::AvailableCredit => "Available credit",
            Self::CurrentBalance => "Current balance",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::AvailableCredit => Self::CurrentBalance,
            Self::CurrentBalance => Self::AvailableCredit,
        }
    }

    /// Convert the stored card values into the amount shown in the entry prompt.
    pub fn entered_amount(self, limit: Money, available: Money) -> Money {
        match self {
            Self::AvailableCredit => available,
            Self::CurrentBalance => limit - available,
        }
    }

    /// Convert a submitted amount back to the available-credit value stored by accounts.
    pub fn as_available_credit(self, limit: Money, entered: Money) -> Money {
        match self {
            Self::AvailableCredit => entered,
            Self::CurrentBalance => limit - entered,
        }
    }
}

// --- Active months -------------------------------------------------------------

const MONTH_NAMES: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const MONTH_ABBREVS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Which calendar months a plan item applies to. Bit 0 = January … bit 11 = December.
///
/// [`MonthSet::ALL`] is the default and what every row written before the `active_months`
/// column existed (stored NULL) reads as, so adding the column changed nothing for
/// existing plans. The mask is consumed once, at stamp time — a stamped month copies the
/// items that were active and never looks at this again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MonthSet(u16);

impl MonthSet {
    /// Every month — no seasonal restriction.
    pub const ALL: MonthSet = MonthSet(0x0FFF);

    pub fn is_all(self) -> bool {
        self == Self::ALL
    }

    /// `month` is 1-based (1 = January), matching `chrono::Datelike::month`.
    pub fn contains(self, month: u32) -> bool {
        (1..=12).contains(&month) && self.0 & (1 << (month - 1)) != 0
    }

    /// The months in this set, in calendar order, 1-based.
    pub fn months(self) -> impl Iterator<Item = u32> {
        (1..=12u32).filter(move |m| self.contains(*m))
    }

    pub fn count(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Read the stored column. NULL means "every month".
    pub fn from_db(value: Option<i64>) -> MonthSet {
        match value {
            Some(bits) => MonthSet(bits as u16 & Self::ALL.0),
            None => Self::ALL,
        }
    }

    /// Bind back to the column. `ALL` stores NULL so the column stays sparse and rows that
    /// never opted into seasonality look exactly as they did before.
    pub fn to_db(self) -> Option<i64> {
        if self.is_all() {
            None
        } else {
            Some(self.0 as i64)
        }
    }

    /// Parse what the user typed: `all`, `*`, or an empty string for every month; otherwise
    /// a comma- or space-separated list of month numbers, names, or non-wrapping ranges
    /// (`mar,jul,nov` / `3 7 11` / `jun-aug`). The error is the message shown in the status
    /// line, so it names the token that failed.
    pub fn parse(input: &str) -> Result<MonthSet, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed == "*" || trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self::ALL);
        }

        let mut bits = 0u16;
        for token in trimmed.split(|c: char| c == ',' || c.is_whitespace()) {
            if token.is_empty() {
                continue;
            }
            // A range only when the dash separates two parts; a bare leading dash is a typo.
            match token.split_once('-') {
                Some((from, to)) if !from.is_empty() && !to.is_empty() => {
                    let start = parse_month(from)?;
                    let end = parse_month(to)?;
                    if start > end {
                        return Err(format!(
                            "“{token}” runs backwards — write it as {}-{}",
                            MONTH_ABBREVS[end as usize - 1].to_lowercase(),
                            MONTH_ABBREVS[start as usize - 1].to_lowercase(),
                        ));
                    }
                    for month in start..=end {
                        bits |= 1 << (month - 1);
                    }
                }
                _ => bits |= 1 << (parse_month(token)? - 1),
            }
        }

        if bits == 0 {
            return Err("Enter at least one month, or “all”".into());
        }
        Ok(MonthSet(bits))
    }

    /// What to prefill the edit prompt with, in the same syntax [`parse`](Self::parse) reads.
    pub fn edit_string(self) -> String {
        if self.is_all() {
            return "all".into();
        }
        self.months()
            .map(|m| MONTH_ABBREVS[m as usize - 1].to_lowercase())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// A compact tag for list rows. Names them while they fit, then falls back to a count so
    /// a nine-month item can't push the amount off a narrow pane.
    pub fn short_label(self) -> String {
        if self.is_all() {
            return "all".into();
        }
        if self.count() <= 3 {
            return self
                .months()
                .map(|m| MONTH_ABBREVS[m as usize - 1])
                .collect::<Vec<_>>()
                .join(", ");
        }
        format!("{} months", self.count())
    }
}

/// One month token: a number 1–12, or any prefix of a month name that names exactly one
/// month (`mar`, `sept`, `September`). A prefix shared by two months — `ju`, `ma` — is an
/// error rather than a guess, which is a better answer than an arbitrary minimum length.
fn parse_month(token: &str) -> Result<u32, String> {
    let token = token.trim();
    if let Ok(number) = token.parse::<u32>() {
        return if (1..=12).contains(&number) {
            Ok(number)
        } else {
            Err(format!("“{token}” isn't a month — use 1–12 or a name"))
        };
    }

    let lowered = token.to_lowercase();
    let mut matches = MONTH_NAMES
        .iter()
        .enumerate()
        .filter(|(_, name)| name.starts_with(&lowered));
    match (matches.next(), matches.next()) {
        (Some((index, _)), None) => Ok(index as u32 + 1),
        (Some(_), Some(_)) => Err(format!("“{token}” matches more than one month")),
        _ => Err(format!("Couldn't read “{token}” as a month")),
    }
}

// --- Money <-> SQLite ----------------------------------------------------------
// `Money` wraps an i64 (cents), and the DB stores those cents as INTEGER, so the
// conversions just pass the i64 through. We keep these impls here (not in money.rs)
// so money.rs stays free of any database dependency.
impl rusqlite::types::FromSql for Money {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Money(value.as_i64()?))
    }
}
impl rusqlite::ToSql for Money {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0))
    }
}

// --- Table/read structs --------------------------------------------------------
// One struct per row shape. `Option<T>` mirrors a nullable column or an optional joined
// value such as the current canonical series label. Note `account_type` rather than `type`
// (a reserved word), and `NaiveDate` for the parsed start date.

/// A cash-flow account. `balance` is the spendable ground truth for a **checking**
/// account. A **credit card** instead carries `credit_limit` + `available_credit` (both
/// entered by hand); its debt is `owed()` and its `balance` is unused (0).
#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub account_type: AccountType,
    pub balance: Money,
    pub credit_limit: Option<Money>,
    pub available_credit: Option<Money>,
    /// One amount, meaning set by `account_type` (`None` = not set, treated as 0):
    ///   - checking:    a buffer the user wants to keep parked in the account.
    ///   - credit card: a balance the user is willing to carry into next month.
    ///
    /// The math effect lives in `carry_adjustment()`, which knows the sign per type.
    pub carry_balance: Option<Money>,
}

impl Account {
    /// What a credit card owes: `limit − available`. Can be negative (a statement credit,
    /// i.e. the card owes you). Returns `ZERO` for non-card accounts and unset fields.
    pub fn owed(&self) -> Money {
        match self.account_type {
            AccountType::CreditCard => {
                self.credit_limit.unwrap_or(Money::ZERO)
                    - self.available_credit.unwrap_or(Money::ZERO)
            }
            AccountType::Checking => Money::ZERO,
        }
    }

    /// The signed effect this account's carry balance has on "what's left". This is the
    /// ONE place the buffer/carryover sign asymmetry lives — the reason a single stored
    /// column is safe:
    ///   - Checking: the buffer is cash you won't spend, so hold it back → NEGATIVE.
    ///   - Credit card: the tolerated balance is debt you won't pay this month, so it
    ///     cancels that much of `owed()`'s drag → POSITIVE.
    ///
    /// `None` carry (the default) means no adjustment.
    pub fn carry_adjustment(&self) -> Money {
        let carry = self.carry_balance.unwrap_or(Money::ZERO);
        match self.account_type {
            AccountType::Checking => Money::ZERO - carry, // reserve cash
            AccountType::CreditCard => carry,             // forgive deferred debt
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub id: String,
    pub name: String,
}

/// A first-class recurring item — WHAT a bill/paycheck/envelope is, independent of any
/// plan. Its `id` is the durable series identity that instances carry as `series_id` and
/// that trends group by. One series can appear in many plans; editing these fields
/// affects every plan that uses it.
#[derive(Clone, Debug)]
pub struct Series {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    pub direction: Option<Direction>,    // transactions
    pub period_type: Option<PeriodType>, // envelopes
    pub mode: Option<Mode>, // Some for envelopes (frozen at creation); None for transactions
}

/// A series' membership in one plan: the join of `plan_item` and `series`. `item_id` is
/// the plan_item row (per-plan); `amount` is this plan's budgeted figure; `series` is the
/// shared definition. This is the read-model the editor and stamping work with.
#[derive(Clone, Debug)]
pub struct PlanEntry {
    pub item_id: String,
    pub plan_id: String,
    pub amount: Money,
    /// The historical source to try when this item is stamped. Static always works; a
    /// historical method falls back to `amount` when its required observations are absent.
    pub forecast_method: ForecastMethod,
    /// The months this plan stamps the item in — [`MonthSet::ALL`] unless the user narrowed
    /// it. Per-plan like `amount`, so one plan can carry birthday gifts in March/July while
    /// another drops them entirely.
    pub active_months: MonthSet,
    pub series: Series,
}

#[derive(Clone, Debug)]
pub struct Month {
    pub id: String,
    pub plan_id: Option<String>,
    pub label: String,
    pub start_date: NaiveDate,
    pub days_in_month: i64,
}

#[derive(Clone, Debug)]
pub struct Envelope {
    pub id: String,
    pub month_id: String,
    /// The durable series identity this instance was stamped from, or `None` for an
    /// **ad-hoc** envelope added straight into the month (no plan behind it). Same meaning
    /// as `Txn::series_id`: `Some` = plan-derived, `None` = hand-entered.
    pub series_id: Option<String>,
    /// Label copied onto this month instance at creation/stamp time. Month-facing queries
    /// also load `series_label`; use `display_label()` for UI so a surviving series owns
    /// the canonical name while this value remains the deletion/legacy fallback.
    pub label: String,
    pub series_label: Option<String>,
    pub amount: Money,
    pub stamped_amount: Money,
    pub period_type: PeriodType,
    pub mode: Mode, // frozen at stamp time; never re-resolved against the global default
}

impl Envelope {
    pub fn display_label(&self) -> &str {
        self.series_label.as_deref().unwrap_or(&self.label)
    }
}

#[derive(Clone, Debug)]
pub struct Txn {
    pub id: String,
    pub month_id: String,
    pub series_id: Option<String>,
    pub envelope_id: Option<String>,
    pub account_id: Option<String>,
    /// Label stored on this particular transaction. For a series-backed top-level budget
    /// row, `series_label` supplies the live canonical display name. For legacy one-offs
    /// and envelope spending it is `None`, so this stored label remains the display name.
    pub label: String,
    pub series_label: Option<String>,
    pub direction: Direction,
    pub amount: Money,
    pub stamped_amount: Option<Money>,
    pub settled: bool,
    pub date_paid: Option<String>,
}

impl Txn {
    pub fn display_label(&self) -> &str {
        self.series_label.as_deref().unwrap_or(&self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn months_of(input: &str) -> Vec<u32> {
        MonthSet::parse(input)
            .expect("should parse")
            .months()
            .collect()
    }

    #[test]
    fn month_names_numbers_and_ranges_all_parse() {
        assert_eq!(months_of("mar,jul,nov"), vec![3, 7, 11]);
        assert_eq!(months_of("3, 7, 11"), vec![3, 7, 11]);
        assert_eq!(months_of("March July November"), vec![3, 7, 11]);
        assert_eq!(months_of("sept"), vec![9]);
        assert_eq!(months_of("jun-aug"), vec![6, 7, 8]);
        assert_eq!(months_of("6-8, dec"), vec![6, 7, 8, 12]);
    }

    #[test]
    fn months_come_back_in_calendar_order_however_they_were_typed() {
        assert_eq!(months_of("nov,mar,jul"), vec![3, 7, 11]);
        // Overlapping input is a set, not a tally.
        assert_eq!(months_of("mar,3,march"), vec![3]);
    }

    #[test]
    fn every_month_has_several_spellings_and_all_store_null() {
        for input in ["all", "ALL", "*", "", "   "] {
            assert!(MonthSet::parse(input).unwrap().is_all(), "{input:?}");
        }
        // A list naming all twelve is the same set, so it stores NULL too.
        assert_eq!(MonthSet::parse("jan-dec").unwrap(), MonthSet::ALL);
        assert_eq!(MonthSet::ALL.to_db(), None);
        assert_eq!(MonthSet::from_db(None), MonthSet::ALL);
    }

    #[test]
    fn a_partial_set_round_trips_through_the_column() {
        let months = MonthSet::parse("mar,jul,nov").unwrap();
        assert_eq!(MonthSet::from_db(months.to_db()), months);
        assert!(months.contains(3) && months.contains(7) && months.contains(11));
        assert!(!months.contains(4));
        // Out-of-range input is simply not a member; it never panics.
        assert!(!months.contains(0) && !months.contains(13));
    }

    #[test]
    fn bad_input_names_the_token_that_failed() {
        for input in ["nope", "13", "0", "ju", "mar,zzz"] {
            assert!(
                MonthSet::parse(input).is_err(),
                "{input:?} should be rejected"
            );
        }
        // "ju" is short for both June and July, so it's rejected as ambiguous rather than
        // silently resolved to whichever comes first. "jun" and "jul" separate them.
        assert!(MonthSet::parse("ju").unwrap_err().contains("more than one"));
        assert_eq!(months_of("jun"), vec![6]);
        assert_eq!(months_of("jul"), vec![7]);
        assert!(
            MonthSet::parse("aug-mar")
                .unwrap_err()
                .contains("backwards")
        );
    }

    #[test]
    fn labels_name_the_months_while_they_fit_then_count_them() {
        let three = MonthSet::parse("mar,jul,nov").unwrap();
        assert_eq!(three.short_label(), "Mar, Jul, Nov");
        assert_eq!(three.edit_string(), "mar,jul,nov");
        // The prompt prefill is always something parse() reads back unchanged.
        assert_eq!(MonthSet::parse(&three.edit_string()).unwrap(), three);

        let many = MonthSet::parse("jan-apr").unwrap();
        assert_eq!(many.short_label(), "4 months");
        assert_eq!(MonthSet::parse(&many.edit_string()).unwrap(), many);

        assert_eq!(MonthSet::ALL.short_label(), "all");
        assert_eq!(MonthSet::ALL.edit_string(), "all");
    }
}
