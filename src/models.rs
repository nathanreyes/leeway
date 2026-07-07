//! The domain types: enums for the "coded" text columns and one struct per table.
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
sql_enum!(PeriodType { Daily => "daily", Weekly => "weekly", Monthly => "monthly" });
sql_enum!(Mode { Automatic => "automatic", Manual => "manual" });
sql_enum!(AccountType {
    Checking => "checking",
    CreditCard => "credit_card",
});

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

// --- Table structs -------------------------------------------------------------
// One struct per row shape. `Option<T>` mirrors a nullable column. Note `account_type`
// rather than `type` (a reserved word), and `NaiveDate` for the parsed start date.

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
}

impl Account {
    /// What a credit card owes: `limit − available`. Can be negative (a statement credit,
    /// i.e. the card owes you). Returns `ZERO` for non-card accounts and unset fields.
    pub fn owed(&self) -> Money {
        match self.account_type {
            AccountType::CreditCard => {
                self.credit_limit.unwrap_or(Money::ZERO) - self.available_credit.unwrap_or(Money::ZERO)
            }
            AccountType::Checking => Money::ZERO,
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
    pub category: Option<String>,
    pub direction: Option<Direction>,   // transactions
    pub period_type: Option<PeriodType>, // envelopes
    pub mode: Option<Mode>,              // envelopes (None = inherit global default)
}

/// A series' membership in one plan: the join of `plan_item` and `series`. `item_id` is
/// the plan_item row (per-plan); `amount` is this plan's budgeted figure; `series` is the
/// shared definition. This is the read-model the editor and stamping work with.
#[derive(Clone, Debug)]
pub struct PlanEntry {
    pub item_id: String,
    pub plan_id: String,
    pub amount: Money,
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
    pub series_id: String,
    pub label: String,
    pub category: Option<String>,
    pub amount: Money,
    pub stamped_amount: Money,
    pub period_type: PeriodType,
    pub mode: Option<Mode>,
}

#[derive(Clone, Debug)]
pub struct Txn {
    pub id: String,
    pub month_id: String,
    pub series_id: Option<String>,
    pub envelope_id: Option<String>,
    pub account_id: Option<String>,
    pub label: String,
    pub category: Option<String>,
    pub direction: Direction,
    pub amount: Money,
    pub stamped_amount: Option<Money>,
    pub settled: bool,
    pub date_paid: Option<String>,
}
