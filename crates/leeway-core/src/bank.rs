//! Bank sync: balances and month items from a bank feed.
//!
//! The apps fetch the feed (from SimpleFIN) and hand it over already parsed. Sync touches
//! only what the budget already knows: linked accounts take the bank's balance, and the
//! current month's standalone income and bills settle from the transactions that cleared.
//! Envelope spending stays manual. A charge that recurs month after month with no plan item
//! to match it comes back as a suggestion instead.

use crate::models::{AccountType, Direction, ForecastMethod};
use crate::money::Money;
use crate::{ops, queries};
use anyhow::{Context, Result};
use chrono::{Datelike, Duration, NaiveDate};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};

/// How far a bank amount may stray from what's due and still settle the row, in percent.
const AMOUNT_TOLERANCE_PERCENT: i64 = 15;
/// The last days of a month whose payments may belong to the next one: rent paid early, or a
/// paycheck that came before a weekend payday. Sync asks before using one there, and the
/// review offers them when picking by hand.
const EARLY_DAYS: i64 = 7;
/// How far back suggestions look.
const SUGGESTION_DAYS: i64 = 100;

/// Words banks add to descriptions that say nothing about who was paid.
const NOISE: &[&str] = &[
    "ach",
    "autopay",
    "bill",
    "card",
    "ccd",
    "checkcard",
    "co",
    "com",
    "debit",
    "des",
    "id",
    "indn",
    "online",
    "payment",
    "pmt",
    "pos",
    "ppd",
    "purchase",
    "recurring",
    "visa",
    "web",
    "www",
];

/// An account in the feed.
#[derive(Clone, Debug)]
pub struct FeedAccount {
    pub id: String,
    pub name: String,
    /// The bank's name.
    pub org: String,
    /// What the bank calls the balance. Card debt is negative.
    pub balance: Money,
    /// The balance less holds, when the bank gives one.
    pub available: Option<Money>,
}

/// A transaction in the feed.
#[derive(Clone, Debug)]
pub struct FeedTxn {
    pub account_id: String,
    /// Unique within its account only.
    pub id: String,
    pub date: NaiveDate,
    /// Negative for money out.
    pub amount: Money,
    pub description: String,
    pub pending: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Feed {
    pub accounts: Vec<FeedAccount>,
    pub txns: Vec<FeedTxn>,
}

/// A linked account whose figure sync changed: checking's balance, or what a card owes.
#[derive(Clone, Debug, PartialEq)]
pub struct BalanceChange {
    pub account_id: String,
    pub name: String,
    pub account_type: AccountType,
    pub before: Money,
    pub after: Money,
}

/// A month row sync settled.
#[derive(Clone, Debug, PartialEq)]
pub struct Settled {
    pub txn_id: String,
    pub bank_txn_id: String,
    pub amount: Money,
    pub date: String,
    pub description: String,
    /// It was settled by hand before, and took the bank's amount and date.
    pub was_settled: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub balances: Vec<BalanceChange>,
    pub settled: Vec<Settled>,
    /// Month rows that more than one bank transaction could settle; the user picks.
    pub ambiguous: Vec<String>,
    /// Payments at the end of a month whose item was already paid, which may be next month's.
    pub early: Vec<EarlyPayment>,
}

/// A payment dated late in one month, matching an item that month already paid and an open
/// one in the next. Sync doesn't settle it; it asks.
#[derive(Clone, Debug, PartialEq)]
pub struct EarlyPayment {
    pub bank: BankTxn,
    /// The next month's row it would settle.
    pub txn_id: String,
    pub label: String,
    /// The month it's dated in, and the one it would count for, `YYYY-MM`.
    pub paid_in: String,
    pub counts_for: String,
}

/// A stored bank transaction, as the review lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct BankTxn {
    pub id: String,
    pub date: String,
    /// Negative for money out.
    pub amount: Money,
    pub description: String,
    pub pending: bool,
}

/// Money that comes in or goes out on a schedule, monthly or twice a month, with no plan item
/// to match it.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// The cleaned description, which is also the match text it would get.
    pub key: String,
    pub label: String,
    pub direction: Direction,
    /// One payment's amount, positive: the middle of the last three, so one odd month
    /// doesn't set it.
    pub amount: Money,
    /// Payments in a typical month: 1, or 2 for twice a month or every two weeks.
    pub payments: u32,
    /// The months it was seen in, `YYYY-MM`, oldest first.
    pub months: Vec<String>,
}

// --- Linking -------------------------------------------------------------------------

/// Sync `account_id` from the feed account `bank_account_id`, or stop syncing it with `None`.
/// A feed account syncs into one account at most, so any other account linked to it lets go.
pub fn link_account(
    conn: &Connection,
    account_id: &str,
    bank_account_id: Option<&str>,
) -> Result<()> {
    if let Some(bank) = bank_account_id {
        conn.execute(
            "UPDATE account SET bank_account_id = NULL WHERE bank_account_id = ?1 AND id != ?2",
            params![bank, account_id],
        )?;
    }
    conn.execute(
        "UPDATE account SET bank_account_id = ?1 WHERE id = ?2",
        params![bank_account_id, account_id],
    )?;
    Ok(())
}

/// Each linked account's feed account id, by account id.
pub fn links(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT id, bank_account_id FROM account WHERE bank_account_id IS NOT NULL")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Stop syncing: unlink every account and forget the bank transactions no row holds. Rows
/// sync settled stay settled.
pub fn unlink_all(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE account SET bank_account_id = NULL WHERE bank_account_id IS NOT NULL",
        [],
    )?;
    conn.execute(
        "DELETE FROM bank_txn
         WHERE id NOT IN (SELECT bank_txn_id FROM txn WHERE bank_txn_id IS NOT NULL)",
        [],
    )?;
    Ok(())
}

// --- Applying a feed -----------------------------------------------------------------

/// Apply `feed` as of `today`: update linked balances, store the transactions, and settle
/// last month and the month containing `today`, each from the transactions dated in it. Last
/// month goes first, so a payment that cleared on its last day settles it rather than this
/// one. All or nothing.
pub fn apply_feed(conn: &mut Connection, feed: &Feed, today: NaiveDate) -> Result<Report> {
    let tx = conn.transaction()?;
    let links = links(&tx)?;
    let balances = update_balances(&tx, feed, &links)?;
    store_txns(&tx, feed, &links)?;
    let this = month_label(today);
    let last = month_label(first_of(today) - Duration::days(1));
    let next = month_label(first_of(today) + Duration::days(31));
    let mut settled = Vec::new();
    let mut ambiguous = Vec::new();
    for label in [&last, &this] {
        if let Some(month) = queries::month_by_label(&tx, label)? {
            let (s, a) = settle_month(&tx, &month)?;
            settled.extend(s);
            ambiguous.extend(a);
        }
    }
    let mut early = Vec::new();
    for (paid_in, counts_for) in [(&last, &this), (&this, &next)] {
        if let (Some(paid), Some(counts)) = (
            queries::month_by_label(&tx, paid_in)?,
            queries::month_by_label(&tx, counts_for)?,
        ) {
            early.extend(early_payments(&tx, &paid, &counts)?);
        }
    }
    tx.commit()?;
    Ok(Report {
        balances,
        settled,
        ambiguous,
        early,
    })
}

fn month_label(day: NaiveDate) -> String {
    format!("{:04}-{:02}", day.year(), day.month())
}

fn first_of(day: NaiveDate) -> NaiveDate {
    day.with_day(1).expect("every month has a first")
}

fn update_balances(
    conn: &Connection,
    feed: &Feed,
    links: &HashMap<String, String>,
) -> Result<Vec<BalanceChange>> {
    let by_id: HashMap<&str, &FeedAccount> =
        feed.accounts.iter().map(|a| (a.id.as_str(), a)).collect();
    let mut changes = Vec::new();
    for account in queries::load_accounts(conn)? {
        let Some(fed) = links.get(&account.id).and_then(|b| by_id.get(b.as_str())) else {
            continue;
        };
        match account.account_type {
            AccountType::Checking => {
                let after = fed.available.unwrap_or(fed.balance);
                if after != account.balance {
                    ops::set_balance(conn, &account.id, after)?;
                    changes.push(BalanceChange {
                        account_id: account.id.clone(),
                        name: account.name.clone(),
                        account_type: account.account_type,
                        before: account.balance,
                        after,
                    });
                }
            }
            AccountType::CreditCard => {
                // Card debt comes as a negative balance; a card in credit, positive.
                let owed = Money(-fed.balance.0);
                let before = account.owed();
                if owed != before {
                    let limit = account.credit_limit.unwrap_or(owed);
                    if account.credit_limit.is_none() {
                        ops::set_credit_limit(conn, &account.id, limit)?;
                    }
                    ops::set_available_credit(conn, &account.id, limit - owed)?;
                    changes.push(BalanceChange {
                        account_id: account.id.clone(),
                        name: account.name.clone(),
                        account_type: account.account_type,
                        before,
                        after: owed,
                    });
                }
            }
        }
    }
    Ok(changes)
}

fn bank_txn_key(account_id: &str, txn_id: &str) -> String {
    format!("{account_id}/{txn_id}")
}

/// Upsert the feed's transactions for linked accounts. Pending ones come and go, and often
/// post under a new id, so each sync replaces the pending ones it has.
fn store_txns(conn: &Connection, feed: &Feed, links: &HashMap<String, String>) -> Result<()> {
    let linked: HashSet<&str> = links.values().map(String::as_str).collect();
    for account in feed
        .accounts
        .iter()
        .filter(|a| linked.contains(a.id.as_str()))
    {
        conn.execute(
            "DELETE FROM bank_txn
             WHERE bank_account_id = ?1 AND pending = 1
               AND id NOT IN (SELECT bank_txn_id FROM txn WHERE bank_txn_id IS NOT NULL)",
            [&account.id],
        )?;
    }
    let mut upsert = conn.prepare(
        "INSERT INTO bank_txn (id, bank_account_id, posted, amount_cents, description, pending)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (id) DO UPDATE SET
             posted = excluded.posted, amount_cents = excluded.amount_cents,
             description = excluded.description, pending = excluded.pending
         WHERE posted != excluded.posted OR amount_cents != excluded.amount_cents
            OR description != excluded.description OR pending != excluded.pending",
    )?;
    for t in feed
        .txns
        .iter()
        .filter(|t| linked.contains(t.account_id.as_str()))
    {
        upsert.execute(params![
            bank_txn_key(&t.account_id, &t.id),
            t.account_id,
            t.date.to_string(),
            t.amount,
            t.description,
            t.pending,
        ])?;
    }
    Ok(())
}

/// The first and last day of `month`: sync settles its rows only from transactions dated in it.
fn dates(month: &crate::models::Month) -> (NaiveDate, NaiveDate) {
    (
        month.start_date,
        month.start_date + Duration::days(month.days_in_month - 1),
    )
}

/// What the review offers when picking by hand: the month, and the last days of the one
/// before, for a payment made early.
fn pick_window(month: &crate::models::Month) -> (NaiveDate, NaiveDate) {
    let (from, to) = dates(month);
    (from - Duration::days(EARLY_DAYS), to)
}

/// Posted, not ignored, from a linked account, held by no row, and dated in the window.
fn free_txns(
    conn: &Connection,
    from: NaiveDate,
    to: NaiveDate,
    pending: bool,
) -> Result<Vec<BankTxn>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.posted, b.amount_cents, b.description, b.pending
         FROM bank_txn b
         WHERE b.ignored = 0 AND (?3 OR b.pending = 0)
           AND b.posted BETWEEN ?1 AND ?2
           AND b.bank_account_id IN (SELECT bank_account_id FROM account WHERE bank_account_id IS NOT NULL)
           AND NOT EXISTS (SELECT 1 FROM txn t WHERE t.bank_txn_id = b.id)
         ORDER BY b.posted, b.id",
    )?;
    let rows = stmt
        .query_map(
            params![from.to_string(), to.to_string(), pending],
            map_bank_txn,
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn map_bank_txn(r: &rusqlite::Row) -> rusqlite::Result<BankTxn> {
    Ok(BankTxn {
        id: r.get(0)?,
        date: r.get(1)?,
        amount: r.get(2)?,
        description: r.get(3)?,
        pending: r.get(4)?,
    })
}

/// A month row sync may settle.
struct Row {
    id: String,
    label: String,
    group: String,
    match_text: String,
    direction: Direction,
    due: Money,
    any_amount: bool,
    settled: bool,
}

fn settle_month(
    conn: &Connection,
    month: &crate::models::Month,
) -> Result<(Vec<Settled>, Vec<String>)> {
    let rows = open_rows(conn, month)?;
    let (from, to) = dates(month);
    let pool = free_txns(conn, from, to, false)?;

    // Each group of rows (one series, or one hand-added label) and the pool indexes it could take.
    let mut groups: Vec<(Vec<&Row>, Vec<usize>)> = Vec::new();
    for row in &rows {
        match groups.iter_mut().find(|(rs, _)| rs[0].group == row.group) {
            Some((rs, _)) => rs.push(row),
            None => groups.push((vec![row], Vec::new())),
        }
    }
    for (rs, cands) in &mut groups {
        let first = rs[0];
        *cands = pool
            .iter()
            .enumerate()
            .filter(|(_, b)| matches(first, b))
            .map(|(i, _)| i)
            .collect();
    }
    // A transaction two groups could take settles neither; the user picks.
    let mut takers: HashMap<usize, usize> = HashMap::new();
    for (_, cands) in &groups {
        for &i in cands {
            *takers.entry(i).or_default() += 1;
        }
    }

    let mut settled = Vec::new();
    let mut ambiguous = Vec::new();
    for (mut rs, cands) in groups {
        let shared = cands.iter().any(|i| takers[i] > 1);
        let cands: Vec<usize> = cands.into_iter().filter(|i| takers[i] == 1).collect();
        if (shared && cands.is_empty()) || cands.len() > rs.len() {
            ambiguous.extend(rs.iter().filter(|r| !r.settled).map(|r| r.id.clone()));
            continue;
        }
        // Rows settled by hand were most likely paid first.
        rs.sort_by_key(|r| !r.settled);
        for (row, i) in rs.into_iter().zip(cands) {
            let bank = &pool[i];
            settle(conn, &row.id, bank)?;
            settled.push(Settled {
                txn_id: row.id.clone(),
                bank_txn_id: bank.id.clone(),
                amount: Money(bank.amount.0.abs()),
                date: bank.date.clone(),
                description: bank.description.clone(),
                was_settled: row.settled,
            });
        }
    }
    Ok((settled, ambiguous))
}

/// Payments in the last days of `paid` that no row took, matching an item `paid` already
/// settled from the bank and an open row in `next`. A payment two items could take is left
/// out, and so is a second payment for the same row.
fn early_payments(
    conn: &Connection,
    paid: &crate::models::Month,
    next: &crate::models::Month,
) -> Result<Vec<EarlyPayment>> {
    let (_, last) = dates(paid);
    let pool = free_txns(conn, last - Duration::days(EARLY_DAYS - 1), last, false)?;
    // Items `paid` still has a row for that the bank hasn't settled could take the payment
    // themselves, so they don't count as paid.
    let waiting: HashSet<String> = open_rows(conn, paid)?
        .into_iter()
        .map(|r| r.group)
        .collect();
    let mut stmt = conn.prepare(
        "SELECT series_id, label FROM txn
         WHERE month_id = ?1 AND envelope_id IS NULL AND bank_txn_id IS NOT NULL",
    )?;
    let held: HashSet<String> = stmt
        .query_map([&paid.id], |r| {
            Ok(group_key(
                r.get::<_, Option<String>>(0)?.as_deref(),
                &r.get::<_, String>(1)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let open: Vec<Row> = open_rows(conn, next)?
        .into_iter()
        .filter(|r| !r.settled && held.contains(&r.group) && !waiting.contains(&r.group))
        .collect();
    let mut early = Vec::new();
    let mut taken = HashSet::new();
    for bank in pool {
        let fits: Vec<&Row> = open.iter().filter(|r| matches(r, &bank)).collect();
        let Some(row) = fits.first() else { continue };
        if fits.iter().any(|r| r.group != row.group) || !taken.insert(row.group.clone()) {
            continue;
        }
        early.push(EarlyPayment {
            txn_id: row.id.clone(),
            label: row.label.clone(),
            paid_in: paid.label.clone(),
            counts_for: next.label.clone(),
            bank,
        });
    }
    Ok(early)
}

/// Rows of one series, or hand-added rows sharing a label, are one item.
fn group_key(series_id: Option<&str>, label: &str) -> String {
    match series_id {
        Some(s) => format!("series:{s}"),
        None => format!("label:{}", label.to_lowercase()),
    }
}

/// The month's standalone rows no bank transaction holds yet, settled or not.
fn open_rows(conn: &Connection, month: &crate::models::Month) -> Result<Vec<Row>> {
    let held: HashSet<String> = conn
        .prepare("SELECT id FROM txn WHERE month_id = ?1 AND bank_txn_id IS NOT NULL")?
        .query_map([&month.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let matches = match_texts(conn)?;
    let any_amount: HashSet<String> = match &month.plan_id {
        Some(plan) => queries::load_plan_entries(conn, plan)?
            .into_iter()
            .filter(|e| e.forecast_method != ForecastMethod::Static)
            .map(|e| e.series.id)
            .collect(),
        None => HashSet::new(),
    };
    Ok(queries::load_txns(conn, &month.id)?
        .into_iter()
        .filter(|t| t.envelope_id.is_none() && !held.contains(&t.id))
        .map(|t| {
            let label = t.series_label.clone().unwrap_or_else(|| t.label.clone());
            let match_text = t
                .series_id
                .as_ref()
                .and_then(|s| matches.get(s).cloned())
                .unwrap_or(label);
            Row {
                group: group_key(t.series_id.as_deref(), &t.label),
                label: t.label.clone(),
                any_amount: t.series_id.as_ref().is_some_and(|s| any_amount.contains(s)),
                due: t.stamped_amount.unwrap_or(t.amount),
                match_text,
                direction: t.direction,
                settled: t.settled,
                id: t.id,
            }
        })
        .collect())
}

/// Each series' saved match text, by series id.
fn match_texts(conn: &Connection) -> Result<HashMap<String, String>> {
    let rows = conn
        .prepare(
            "SELECT id, bank_match FROM series WHERE bank_match IS NOT NULL AND bank_match != ''",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn matches(row: &Row, bank: &BankTxn) -> bool {
    let right_way = match row.direction {
        Direction::Out => bank.amount.0 < 0,
        Direction::In => bank.amount.0 > 0,
    };
    right_way
        && names(&bank.description, &row.match_text)
        && (row.any_amount || close(row.due, bank.amount))
}

/// Whether `description` names `text`: its cleaned words appear, in order, in the
/// description's cleaned words.
pub fn names(description: &str, text: &str) -> bool {
    let text = clean(text);
    !text.is_empty() && format!(" {} ", clean(description)).contains(&format!(" {text} "))
}

/// Whether `bank` is near enough to `due`. A row due $0 hasn't been given an amount, so any
/// amount will do.
fn close(due: Money, bank: Money) -> bool {
    let (due, bank) = (due.0.abs(), bank.0.abs());
    due == 0 || (bank - due).abs() * 100 <= due * AMOUNT_TOLERANCE_PERCENT
}

/// A description reduced to the words that name who was paid: lowercase, no punctuation,
/// no words with digits (dates, store numbers, references), no bank noise words.
pub fn clean(description: &str) -> String {
    let lower = description.to_lowercase();
    let spaced: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let words: Vec<&str> = spaced
        .split_whitespace()
        .filter(|w| !w.chars().any(|c| c.is_ascii_digit()))
        .collect();
    let kept: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| !NOISE.contains(w))
        .collect();
    // A description of nothing but noise ("ONLINE PAYMENT") still names something.
    if kept.is_empty() { words } else { kept }.join(" ")
}

fn settle(conn: &Connection, txn_id: &str, bank: &BankTxn) -> Result<()> {
    conn.execute(
        "UPDATE txn SET amount_cents = ?1, settled = 1, date_paid = ?2, bank_txn_id = ?3 WHERE id = ?4",
        params![Money(bank.amount.0.abs()), bank.date, bank.id, txn_id],
    )?;
    Ok(())
}

// --- Review ----------------------------------------------------------------------------

/// The bank transactions month `month_id` could still use, pending ones included, oldest
/// first.
pub fn free_txns_for_month(conn: &Connection, month_id: &str) -> Result<Vec<BankTxn>> {
    let month = queries::month_by_id(conn, month_id)?.context("month not found")?;
    let (from, to) = pick_window(&month);
    free_txns(conn, from, to, true)
}

/// The bank transaction that settled each of month `month_id`'s rows, by row id.
pub fn settled_by_bank(conn: &Connection, month_id: &str) -> Result<HashMap<String, BankTxn>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, b.id, b.posted, b.amount_cents, b.description, b.pending
         FROM txn t JOIN bank_txn b ON b.id = t.bank_txn_id
         WHERE t.month_id = ?1",
    )?;
    let rows = stmt
        .query_map([month_id], |r| {
            Ok((
                r.get(0)?,
                BankTxn {
                    id: r.get(1)?,
                    date: r.get(2)?,
                    amount: r.get(3)?,
                    description: r.get(4)?,
                    pending: r.get(5)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Settle row `txn_id` from bank transaction `bank_txn_id`, the user's pick. With
/// `save_match`, the row's series keeps the description as its match text, so later syncs
/// find it.
pub fn settle_from_bank(
    conn: &Connection,
    txn_id: &str,
    bank_txn_id: &str,
    save_match: bool,
) -> Result<()> {
    let bank = conn
        .query_row(
            "SELECT id, posted, amount_cents, description, pending FROM bank_txn WHERE id = ?1",
            [bank_txn_id],
            map_bank_txn,
        )
        .optional()?
        .context("bank transaction not found")?;
    conn.execute(
        "UPDATE txn SET bank_txn_id = NULL WHERE bank_txn_id = ?1",
        [bank_txn_id],
    )?;
    conn.execute(
        "UPDATE bank_txn SET ignored = 0 WHERE id = ?1",
        [bank_txn_id],
    )?;
    settle(conn, txn_id, &bank)?;
    if save_match {
        let series: Option<String> =
            conn.query_row("SELECT series_id FROM txn WHERE id = ?1", [txn_id], |r| {
                r.get(0)
            })?;
        if let Some(series) = series {
            set_match_text(conn, &series, Some(&clean(&bank.description)))?;
        }
    }
    Ok(())
}

/// Leave bank transaction `bank_txn_id` alone from now on: the answer to an early payment that
/// isn't next month's.
pub fn ignore_txn(conn: &Connection, bank_txn_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE bank_txn SET ignored = 1 WHERE id = ?1",
        [bank_txn_id],
    )?;
    Ok(())
}

/// Undo sync settling row `txn_id`: it goes back to open at its stamped amount, and its bank
/// transaction is ignored from now on.
pub fn undo_settle(conn: &Connection, txn_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE bank_txn SET ignored = 1 WHERE id = (SELECT bank_txn_id FROM txn WHERE id = ?1)",
        [txn_id],
    )?;
    conn.execute("UPDATE txn SET bank_txn_id = NULL WHERE id = ?1", [txn_id])?;
    ops::unmark_paid(conn, txn_id, true)
}

/// Series `series_id`'s match text; `None` means its label.
pub fn match_text(conn: &Connection, series_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT bank_match FROM series WHERE id = ?1",
            [series_id],
            |r| r.get::<_, Option<String>>(0),
        )?
        .filter(|t| !t.trim().is_empty()))
}

pub fn set_match_text(conn: &Connection, series_id: &str, text: Option<&str>) -> Result<()> {
    let text = text.map(str::trim).filter(|t| !t.is_empty());
    conn.execute(
        "UPDATE series SET bank_match = ?1 WHERE id = ?2",
        params![text, series_id],
    )?;
    Ok(())
}

// --- Suggestions -----------------------------------------------------------------------

/// Income and charges from the last few months that recur on a schedule and that no series in
/// use names. Income first, then charges, each largest first.
pub fn suggestions(conn: &Connection, today: NaiveDate) -> Result<Vec<Suggestion>> {
    let from = today - Duration::days(SUGGESTION_DAYS);
    // A series no plan gives an amount and no month has used, such as a $0 starter item,
    // doesn't hide anything.
    let mut texts: Vec<String> = conn
        .prepare(
            "SELECT s.label FROM series s
             WHERE EXISTS (SELECT 1 FROM plan_item p
                           WHERE p.series_id = s.id AND p.amount_cents != 0)
                OR EXISTS (SELECT 1 FROM txn t
                           WHERE t.series_id = s.id AND (t.amount_cents != 0 OR t.settled))",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    texts.extend(match_texts(conn)?.into_values());
    let dismissed: HashSet<String> = conn
        .prepare("SELECT key FROM bank_dismissal")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut groups: HashMap<(String, bool), Vec<BankTxn>> = HashMap::new();
    for t in free_txns(conn, from, today, false)? {
        let key = clean(&t.description);
        if t.amount.0 != 0
            && !key.is_empty()
            && !dismissed.contains(&key)
            && !texts.iter().any(|text| names(&t.description, text))
        {
            groups.entry((key, t.amount.0 > 0)).or_default().push(t);
        }
    }
    let mut found: Vec<Suggestion> = groups
        .into_iter()
        .filter_map(|((key, income), txns)| recurring(key, income, txns))
        .collect();
    found.sort_by(|a, b| {
        (b.direction == Direction::In)
            .cmp(&(a.direction == Direction::In))
            .then((b.amount.0 * b.payments as i64).cmp(&(a.amount.0 * a.payments as i64)))
            .then(a.key.cmp(&b.key))
    });
    Ok(found)
}

/// A suggestion when `txns` (oldest first) come on a schedule at much the same amount: about
/// a month apart in two or more months, or about two weeks apart at least four times.
fn recurring(key: String, income: bool, txns: Vec<BankTxn>) -> Option<Suggestion> {
    let dates: Vec<NaiveDate> = txns.iter().filter_map(|t| t.date.parse().ok()).collect();
    if dates.len() != txns.len() || dates.len() < 2 {
        return None;
    }
    let gaps: Vec<i64> = dates.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
    let payments = if gaps.iter().all(|g| (20..=40).contains(g)) {
        1
    } else if dates.len() >= 4 && gaps.iter().all(|g| (10..=20).contains(g)) {
        2
    } else {
        return None;
    };
    let mut recent: Vec<i64> = txns
        .iter()
        .rev()
        .take(3)
        .map(|t| t.amount.0.abs())
        .collect();
    recent.sort_unstable();
    let amount = Money(recent[recent.len() / 2]);
    if !txns.iter().all(|t| close(amount, t.amount)) {
        return None;
    }
    let mut months: Vec<String> = dates
        .iter()
        .map(|d| d.format("%Y-%m").to_string())
        .collect();
    months.dedup();
    Some(Suggestion {
        label: title_case(&key),
        direction: if income {
            Direction::In
        } else {
            Direction::Out
        },
        amount,
        payments,
        months,
        key,
    })
}

fn title_case(text: &str) -> String {
    text.split(' ')
        .map(|w| {
            let mut chars = w.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// Stop suggesting the charges cleaned to `key`.
pub fn dismiss(conn: &Connection, key: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO bank_dismissal (key) VALUES (?1)",
        [key],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::{Kind, Mode, PeriodType};
    use crate::ops::{add_plan_item, create_plan, create_series, stamp};

    struct Fixture {
        conn: Connection,
        checking: String,
        card: String,
        month: String,
        rent: String,
        pay: String,
        electric: String,
        pay_series: String,
        plan: String,
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
    }

    fn row_by_label(conn: &Connection, month: &str, label: &str) -> String {
        queries::load_txns(conn, month)
            .unwrap()
            .into_iter()
            .find(|t| t.series_label.as_deref() == Some(label))
            .unwrap()
            .id
    }

    /// October 2026: rent $1,000, a $2,000 paycheck, electric drawn from its 3-month
    /// average, and a Dining envelope. Checking and a card, both linked.
    fn fixture() -> Fixture {
        let mut conn = db::open_in_memory().unwrap();
        let plan = create_plan(&conn, "P").unwrap();
        let series = |label: &str, direction: Direction, dollars: f64| {
            let id = create_series(&conn, Kind::Transaction, label, Some(direction), None, None)
                .unwrap();
            let item = add_plan_item(&conn, &plan, &id, Money::from_dollars(dollars)).unwrap();
            (id, item)
        };
        series("Rent", Direction::Out, 1000.0);
        let (pay_series, _) = series("Paycheck", Direction::In, 2000.0);
        let (_, electric_item) = series("Electric", Direction::Out, 140.0);
        ops::set_item_forecast_method(&conn, &electric_item, ForecastMethod::AveragePrevious3)
            .unwrap();
        let dining = create_series(
            &conn,
            Kind::Envelope,
            "Dining",
            None,
            Some(PeriodType::Monthly),
            Some(Mode::Manual),
        )
        .unwrap();
        add_plan_item(&conn, &plan, &dining, Money::from_dollars(300.0)).unwrap();
        let start = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let month = stamp(&mut conn, &plan, "2026-10", start, 31).unwrap();
        let checking = ops::create_checking_account(&conn, "Checking", Money(0)).unwrap();
        let card =
            ops::create_credit_card_account(&conn, "Card", Money(500_000), Money(500_000)).unwrap();
        link_account(&conn, &checking, Some("chk")).unwrap();
        link_account(&conn, &card, Some("cc")).unwrap();
        Fixture {
            rent: row_by_label(&conn, &month, "Rent"),
            pay: row_by_label(&conn, &month, "Paycheck"),
            electric: row_by_label(&conn, &month, "Electric"),
            conn,
            checking,
            card,
            month,
            pay_series,
            plan,
        }
    }

    /// Stamp November 2026 from the fixture's plan.
    fn november(f: &mut Fixture) -> String {
        let start = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        stamp(&mut f.conn, &f.plan, "2026-11", start, 30).unwrap()
    }

    fn day(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn account(id: &str, balance: i64, available: Option<i64>) -> FeedAccount {
        FeedAccount {
            id: id.into(),
            name: id.into(),
            org: "Bank".into(),
            balance: Money(balance),
            available: available.map(Money),
        }
    }

    fn txn(account: &str, id: &str, date: &str, cents: i64, description: &str) -> FeedTxn {
        FeedTxn {
            account_id: account.into(),
            id: id.into(),
            date: date.parse().unwrap(),
            amount: Money(cents),
            description: description.into(),
            pending: false,
        }
    }

    fn feed(txns: Vec<FeedTxn>) -> Feed {
        Feed {
            accounts: vec![
                account("chk", 250_000, Some(240_000)),
                account("cc", -61_200, None),
            ],
            txns,
        }
    }

    fn row(conn: &Connection, id: &str) -> (bool, i64, Option<String>) {
        conn.query_row(
            "SELECT settled, amount_cents, date_paid FROM txn WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn descriptions_clean_down_to_who_was_paid() {
        assert_eq!(clean("DUKE ENERGY PAYMENT 10/03"), "duke energy");
        assert_eq!(clean("NETFLIX.COM"), "netflix");
        assert_eq!(clean("TRADER JOES #552"), "trader joes");
        assert_eq!(clean("ONLINE PAYMENT"), "online payment");
        assert!(names("ACH RENT PMT 1000.00", "Rent"));
        assert!(!names("PARENT TEACHER ASSN", "Rent"));
        assert!(!names("ANYTHING", ""));
    }

    #[test]
    fn linked_accounts_take_the_bank_balance() {
        let mut f = fixture();
        let unlinked = ops::create_checking_account(&f.conn, "Savings", Money(9_900)).unwrap();
        let report = apply_feed(&mut f.conn, &feed(vec![]), today()).unwrap();

        let accounts = queries::load_accounts(&f.conn).unwrap();
        let get = |id: &str| accounts.iter().find(|a| a.id == id).unwrap().clone();
        assert_eq!(
            get(&f.checking).balance,
            Money(240_000),
            "available wins over balance"
        );
        assert_eq!(
            get(&f.card).owed(),
            Money(61_200),
            "negative balance is owed"
        );
        assert_eq!(
            get(&f.card).credit_limit,
            Some(Money(500_000)),
            "limit stays"
        );
        assert_eq!(get(&unlinked).balance, Money(9_900));
        assert_eq!(report.balances.len(), 2);
        assert_eq!(report.balances[1].before, Money(0));
        assert_eq!(report.balances[1].after, Money(61_200));

        let again = apply_feed(&mut f.conn, &feed(vec![]), today()).unwrap();
        assert!(again.balances.is_empty(), "nothing changed the second time");
    }

    #[test]
    fn cleared_items_settle_at_the_bank_amount() {
        let mut f = fixture();
        set_match_text(&f.conn, &f.pay_series, Some("Acme Payroll")).unwrap();
        let mut pending = txn("chk", "p1", "2026-10-14", -14_000, "DUKE ENERGY");
        pending.pending = true;
        let report = apply_feed(
            &mut f.conn,
            &feed(vec![
                txn("chk", "1", "2026-09-28", -100_000, "ACH RENT PMT"),
                txn("chk", "2", "2026-10-01", 205_255, "ACME PAYROLL PPD ID 99"),
                txn("chk", "5", "2026-10-02", -100_000, "ACH RENT PMT"),
                txn("chk", "3", "2026-10-05", -5_000, "DINING OUT"),
                txn("unlinked", "4", "2026-10-05", -100_000, "RENT"),
                pending,
            ]),
            today(),
        )
        .unwrap();

        assert_eq!(
            row(&f.conn, &f.rent),
            (true, 100_000, Some("2026-10-02".into())),
            "September's rent payment stays out of October"
        );
        assert_eq!(
            row(&f.conn, &f.pay),
            (true, 205_255, Some("2026-10-01".into()))
        );
        assert!(!row(&f.conn, &f.electric).0, "pending settles nothing");
        assert_eq!(report.settled.len(), 2);
        assert!(report.ambiguous.is_empty());

        let again = apply_feed(&mut f.conn, &feed(vec![]), today()).unwrap();
        assert!(again.settled.is_empty(), "a transaction settles once");
        let free = free_txns_for_month(&f.conn, &f.month).unwrap();
        let ids: Vec<&str> = free.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["chk/1", "chk/3"],
            "picking by hand offers the last days of September; the pending charge went with the feed that dropped it"
        );
    }

    #[test]
    fn amounts_must_be_close_unless_the_source_varies() {
        let mut f = fixture();
        apply_feed(
            &mut f.conn,
            &feed(vec![
                txn("chk", "1", "2026-10-02", -130_000, "RENT"),
                txn("chk", "2", "2026-10-03", -31_000, "ELECTRIC CO"),
            ]),
            today(),
        )
        .unwrap();
        assert!(!row(&f.conn, &f.rent).0, "30% over rent");
        assert_eq!(row(&f.conn, &f.electric).1, 31_000, "electric varies");
    }

    #[test]
    fn a_row_settled_by_hand_takes_the_bank_amount() {
        let mut f = fixture();
        ops::mark_paid(&f.conn, &f.rent, Money(100_000), None).unwrap();
        let report = apply_feed(
            &mut f.conn,
            &feed(vec![txn("chk", "1", "2026-10-02", -101_500, "RENT")]),
            today(),
        )
        .unwrap();
        assert_eq!(
            row(&f.conn, &f.rent),
            (true, 101_500, Some("2026-10-02".into()))
        );
        assert!(report.settled[0].was_settled);
    }

    #[test]
    fn two_matches_wait_for_a_pick_that_teaches_the_match() {
        let mut f = fixture();
        let report = apply_feed(
            &mut f.conn,
            &feed(vec![
                txn("chk", "1", "2026-10-01", 200_000, "ACME CORP PAYROLL"),
                txn("chk", "2", "2026-10-02", 200_000, "ACME CORP PAYROLL"),
            ]),
            today(),
        )
        .unwrap();
        assert!(report.settled.is_empty(), "Paycheck names neither");
        set_match_text(&f.conn, &f.pay_series, Some("acme corp payroll")).unwrap();
        let report = apply_feed(&mut f.conn, &feed(vec![]), today()).unwrap();
        assert_eq!(
            report.ambiguous,
            vec![f.pay.clone()],
            "two candidates for one row"
        );

        set_match_text(&f.conn, &f.pay_series, None).unwrap();
        settle_from_bank(&f.conn, &f.pay, "chk/2", true).unwrap();
        assert_eq!(
            row(&f.conn, &f.pay),
            (true, 200_000, Some("2026-10-02".into()))
        );
        assert_eq!(
            match_text(&f.conn, &f.pay_series).unwrap().as_deref(),
            Some("acme corp payroll")
        );
    }

    #[test]
    fn an_item_paid_twice_takes_its_matches_in_date_order() {
        let mut f = fixture();
        let txns = queries::load_txns(&f.conn, &f.month).unwrap();
        let rent = txns.iter().find(|t| t.id == f.rent).unwrap();
        let second = ops::add_series_txn_instance(
            &f.conn,
            &f.month,
            rent.series_id.as_ref().unwrap(),
            Money(100_000),
        )
        .unwrap();
        ops::mark_paid(&f.conn, &f.rent, Money(100_000), None).unwrap();
        apply_feed(
            &mut f.conn,
            &feed(vec![
                txn("chk", "2", "2026-10-15", -100_000, "RENT"),
                txn("chk", "1", "2026-10-01", -100_000, "RENT"),
            ]),
            today(),
        )
        .unwrap();
        assert_eq!(
            row(&f.conn, &f.rent).2.as_deref(),
            Some("2026-10-01"),
            "settled first, paid first"
        );
        assert_eq!(row(&f.conn, &second).2.as_deref(), Some("2026-10-15"));
    }

    #[test]
    fn undo_keeps_a_transaction_from_settling_again() {
        let mut f = fixture();
        apply_feed(
            &mut f.conn,
            &feed(vec![txn("chk", "1", "2026-10-02", -100_000, "RENT")]),
            today(),
        )
        .unwrap();
        undo_settle(&f.conn, &f.rent).unwrap();
        assert_eq!(row(&f.conn, &f.rent), (false, 100_000, None));
        let report = apply_feed(
            &mut f.conn,
            &feed(vec![txn("chk", "1", "2026-10-02", -100_000, "RENT")]),
            today(),
        )
        .unwrap();
        assert!(report.settled.is_empty());
        assert!(free_txns_for_month(&f.conn, &f.month).unwrap().is_empty());
    }

    #[test]
    fn monthly_charges_no_series_names_become_suggestions() {
        let mut f = fixture();
        let mut txns = vec![
            txn("chk", "n1", "2026-08-12", -1_549, "NETFLIX.COM 866-579"),
            txn("chk", "n2", "2026-09-12", -1_549, "NETFLIX.COM 866-579"),
            txn("chk", "n3", "2026-10-12", -1_599, "NETFLIX.COM 866-579"),
            txn("chk", "g1", "2026-08-20", -1_000, "GYM"),
            txn("chk", "g2", "2026-09-20", -1_000, "GYM"),
            txn("chk", "r1", "2026-08-01", -100_000, "RENT"),
            txn("chk", "r2", "2026-09-01", -100_000, "RENT"),
            txn("chk", "o1", "2026-08-05", -999, "ONE OFF SHOP"),
        ];
        for (i, day) in ["2026-08-03", "2026-08-10", "2026-09-03", "2026-10-03"]
            .iter()
            .enumerate()
        {
            txns.push(txn(
                "chk",
                &format!("t{i}"),
                day,
                -6_000,
                "TRADER JOES #552",
            ));
        }
        apply_feed(&mut f.conn, &feed(txns), today()).unwrap();
        dismiss(&f.conn, "gym").unwrap();

        let found = suggestions(&f.conn, today()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "netflix");
        assert_eq!(found[0].label, "Netflix");
        assert_eq!(found[0].direction, Direction::Out);
        assert_eq!(
            found[0].amount,
            Money(1_549),
            "the middle of the last three"
        );
        assert_eq!(found[0].payments, 1);
        assert_eq!(found[0].months, vec!["2026-08", "2026-09", "2026-10"]);
    }

    #[test]
    fn unlinking_forgets_the_feed_but_keeps_what_settled() {
        let mut f = fixture();
        apply_feed(
            &mut f.conn,
            &feed(vec![
                txn("chk", "1", "2026-10-02", -100_000, "RENT"),
                txn("chk", "2", "2026-10-03", -2_000, "SHOP"),
            ]),
            today(),
        )
        .unwrap();
        unlink_all(&f.conn).unwrap();
        assert!(links(&f.conn).unwrap().is_empty());
        assert!(row(&f.conn, &f.rent).0);
        assert_eq!(settled_by_bank(&f.conn, &f.month).unwrap().len(), 1);
        let left: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM bank_txn", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn linking_a_feed_account_again_moves_it() {
        let f = fixture();
        link_account(&f.conn, &f.card, Some("chk")).unwrap();
        let links = links(&f.conn).unwrap();
        assert_eq!(links.get(&f.card).map(String::as_str), Some("chk"));
        assert!(!links.contains_key(&f.checking));
    }

    #[test]
    fn paychecks_twice_a_month_become_income_with_two_payments() {
        let mut f = fixture();
        let mut txns = Vec::new();
        for (i, day) in [
            "2026-08-01",
            "2026-08-15",
            "2026-09-01",
            "2026-09-15",
            "2026-10-01",
            "2026-10-15",
        ]
        .iter()
        .enumerate()
        {
            txns.push(txn("chk", &format!("p{i}"), day, 256_448, "Pay day!"));
        }
        // Paid every two weeks, on Fridays: three in September.
        for (i, day) in [
            "2026-08-07",
            "2026-08-21",
            "2026-09-04",
            "2026-09-18",
            "2026-10-02",
        ]
        .iter()
        .enumerate()
        {
            txns.push(txn("chk", &format!("s{i}"), day, 80_000, "SIDE GIG LLC"));
        }
        apply_feed(&mut f.conn, &feed(txns), today()).unwrap();

        let found = suggestions(&f.conn, today()).unwrap();
        let keys: Vec<&str> = found.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["pay day", "side gig llc"],
            "income, larger first"
        );
        assert!(
            found
                .iter()
                .all(|s| s.direction == Direction::In && s.payments == 2)
        );
        assert_eq!(found[0].months, vec!["2026-08", "2026-09", "2026-10"]);
    }

    #[test]
    fn a_series_nobody_uses_hides_nothing() {
        let mut f = fixture();
        // "Electric" is in the plan but at $0 and never paid, like a starter placeholder.
        let plan: String = f
            .conn
            .query_row("SELECT id FROM plan", [], |r| r.get(0))
            .unwrap();
        let unused = create_series(
            &f.conn,
            Kind::Transaction,
            "Water",
            Some(Direction::Out),
            None,
            None,
        )
        .unwrap();
        add_plan_item(&f.conn, &plan, &unused, Money(0)).unwrap();
        apply_feed(
            &mut f.conn,
            &feed(vec![
                txn("chk", "1", "2026-09-10", -4_000, "CITY WATER"),
                txn("chk", "2", "2026-10-10", -4_200, "CITY WATER"),
            ]),
            today(),
        )
        .unwrap();
        let found = suggestions(&f.conn, today()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "city water");
    }

    #[test]
    fn a_payment_settles_the_month_it_is_dated_in() {
        let mut f = fixture();
        let nov = november(&mut f);
        let nov_pay = row_by_label(&f.conn, &nov, "Paycheck");
        // October's paycheck was marked paid by hand before bank sync saw it.
        ops::mark_paid(&f.conn, &f.pay, Money(200_000), None).unwrap();
        let report = apply_feed(
            &mut f.conn,
            &feed(vec![txn("chk", "1", "2026-10-30", 200_000, "PAYCHECK")]),
            day("2026-11-03"),
        )
        .unwrap();
        assert_eq!(
            row(&f.conn, &f.pay),
            (true, 200_000, Some("2026-10-30".into())),
            "last month goes first"
        );
        assert!(
            !row(&f.conn, &nov_pay).0,
            "November's paycheck waits for its own"
        );
        assert_eq!(report.settled.len(), 1);
        assert!(
            report.early.is_empty(),
            "October had nothing paid from the bank before it"
        );
    }

    #[test]
    fn a_late_payment_for_an_item_already_paid_is_asked_about() {
        let mut f = fixture();
        let nov = november(&mut f);
        let nov_rent = row_by_label(&f.conn, &nov, "Rent");
        // October's rent cleared early in the month; another payment came at its end.
        let first = feed(vec![txn("chk", "1", "2026-10-02", -100_000, "RENT")]);
        apply_feed(&mut f.conn, &first, day("2026-10-03")).unwrap();
        let late = feed(vec![txn("chk", "2", "2026-10-28", -100_000, "RENT")]);
        let report = apply_feed(&mut f.conn, &late, day("2026-10-29")).unwrap();
        assert_eq!(row(&f.conn, &f.rent).2, Some("2026-10-02".into()));
        assert!(!row(&f.conn, &nov_rent).0, "never settled without asking");
        assert_eq!(report.early.len(), 1);
        let early = &report.early[0];
        assert_eq!(
            (
                early.txn_id.as_str(),
                early.bank.id.as_str(),
                early.label.as_str()
            ),
            (nov_rent.as_str(), "chk/2", "Rent")
        );
        assert_eq!(
            (early.paid_in.as_str(), early.counts_for.as_str()),
            ("2026-10", "2026-11")
        );

        // Into November, it's still last month's payment and still asked about.
        let report = apply_feed(&mut f.conn, &feed(vec![]), day("2026-11-02")).unwrap();
        assert_eq!(report.early.len(), 1);

        settle_from_bank(&f.conn, &nov_rent, "chk/2", false).unwrap();
        assert_eq!(
            row(&f.conn, &nov_rent),
            (true, 100_000, Some("2026-10-28".into()))
        );
        let report = apply_feed(&mut f.conn, &feed(vec![]), day("2026-11-02")).unwrap();
        assert!(report.early.is_empty(), "answered");
    }

    #[test]
    fn a_late_payment_that_isnt_next_months_can_be_left_alone() {
        let mut f = fixture();
        november(&mut f);
        let first = feed(vec![txn("chk", "1", "2026-10-02", -100_000, "RENT")]);
        apply_feed(&mut f.conn, &first, day("2026-10-03")).unwrap();
        let late = feed(vec![txn("chk", "2", "2026-10-28", -100_000, "RENT")]);
        let report = apply_feed(&mut f.conn, &late, day("2026-10-29")).unwrap();
        ignore_txn(&f.conn, &report.early[0].bank.id).unwrap();
        let report = apply_feed(&mut f.conn, &feed(vec![]), day("2026-10-30")).unwrap();
        assert!(report.early.is_empty());
    }

    #[test]
    fn a_late_payment_is_not_asked_about_while_its_own_month_could_take_it() {
        let mut f = fixture();
        november(&mut f);
        // October's rent isn't paid, so the late payment settles October.
        let report = apply_feed(
            &mut f.conn,
            &feed(vec![txn("chk", "1", "2026-10-28", -100_000, "RENT")]),
            day("2026-10-29"),
        )
        .unwrap();
        assert!(row(&f.conn, &f.rent).0);
        assert!(report.early.is_empty());
    }
}
