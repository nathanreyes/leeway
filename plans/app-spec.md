# Budgeting App — Design Spec

A single-user, local-first budgeting app whose job is to answer one question all month long: **"what's left?"** It is a *forecasting* tool, not an accounting ledger — most spending is never entered transaction-by-transaction; it's reserved and drawn down. Low daily friction is a first-class design goal ("lazy-first").

---

## 1. Philosophy & guardrails

- The **current checking balance is ground truth**, entered manually. The app never reconciles individual transactions against it.
- Known income and bills are **events** you toggle as settled — you usually don't care *when*, only *whether*.
- Spending categories (groceries, dining) are **envelopes** that bleed down over the month on their own.
- The app keeps every month it has ever stamped, so it can show **trends** ("what was electric this time last year?").
- Anything that would force routine data entry into the main loop is suspect. Entry is optional enrichment, never a requirement.

---

## 2. Core concepts

Two primitives, cleanly related:

- **Transaction** — an atomic money event: amount, direction (in/out), a settled flag. Paychecks and known bills *are* transactions. A transaction may stand alone (a bill, a paycheck) or belong to a manual envelope (a grocery purchase).
- **Envelope** — a budgeted pool with a consumption *mode*. An **automatic** envelope accrues over time; a **manual** envelope is consumed by the transactions inside it.

And a separation across time:

- **Plan** — a reusable *template*: a named set of recurring transaction- and envelope-definitions. You can keep several (normal month, tight month, summer-with-the-kids) and choose which to stamp.
- **Stamping** — at the start of a month you stamp a plan, which **copies** its items into concrete instances for that month. The link to the plan is then **broken**: the month is an independent snapshot. Editing the plan afterward never reaches back into a stamped month.

---

## 3. Data model (SQLite)

```sql
-- Real-world balances. Manually entered ground truth, carried across months.
CREATE TABLE account (
    id        TEXT PRIMARY KEY,            -- UUID
    name      TEXT NOT NULL,
    type      TEXT NOT NULL,               -- 'checking' | 'credit_card' | 'reserve' | 'investment'
    balance   REAL NOT NULL,
    protected INTEGER NOT NULL DEFAULT 0   -- 1 = held back from funds (credit cards, reserve)
);

-- Named templates.
CREATE TABLE plan (
    id   TEXT PRIMARY KEY,                 -- UUID
    name TEXT NOT NULL
);

-- Recurring definitions inside a plan.
-- plan_item.id is the PERMANENT SERIES IDENTITY: minted once, never reused or recomputed,
-- copied onto every instance it stamps. It is NOT derived from the label, so renames are free.
CREATE TABLE plan_item (
    id          TEXT PRIMARY KEY,          -- UUID — the durable series id
    plan_id     TEXT NOT NULL REFERENCES plan(id),
    kind        TEXT NOT NULL,             -- 'transaction' | 'envelope'
    label       TEXT NOT NULL,             -- cosmetic only; safe to edit
    slug        TEXT,                      -- optional, for display/search ONLY — never used to match
    category    TEXT,
    direction   TEXT,                      -- 'in' | 'out'  (transactions)
    amount      REAL NOT NULL,             -- default / budgeted amount
    period_type TEXT,                      -- envelopes: 'daily' | 'weekly' | 'monthly'
    mode        TEXT                       -- envelopes: 'automatic' | 'manual' | NULL = inherit global default
);

-- A stamped period.
CREATE TABLE month (
    id            TEXT PRIMARY KEY,        -- UUID
    plan_id       TEXT,                    -- which plan was stamped (record only; snapshot is independent)
    label         TEXT NOT NULL,           -- e.g. '2026-06'
    start_date    TEXT NOT NULL,
    days_in_month INTEGER NOT NULL
);

-- Envelope instance (one per stamped envelope per month).
CREATE TABLE envelope (
    id             TEXT PRIMARY KEY,       -- UUID
    month_id       TEXT NOT NULL REFERENCES month(id),
    series_id      TEXT NOT NULL,          -- copied plan_item.id (plain value, NOT a live FK)
    label          TEXT NOT NULL,
    category       TEXT,
    amount         REAL NOT NULL,          -- this month's budget (editable)
    stamped_amount REAL NOT NULL,          -- immutable snapshot, used by "revert to planned"
    period_type    TEXT NOT NULL,          -- 'daily' | 'weekly' | 'monthly'
    mode           TEXT                    -- 'automatic' | 'manual' | NULL = inherit global default
);

-- The atomic money event. Standalone (bill/income) or attached to a manual envelope.
CREATE TABLE txn (
    id             TEXT PRIMARY KEY,       -- UUID
    month_id       TEXT NOT NULL REFERENCES month(id),   -- the PERIOD = the trend time axis
    series_id      TEXT,                   -- copied plan_item.id; NULL for one-offs (no series)
    envelope_id    TEXT REFERENCES envelope(id),         -- NULL = standalone (bill/income)
    account_id     TEXT REFERENCES account(id),
    label          TEXT NOT NULL,
    category       TEXT,
    direction      TEXT NOT NULL,          -- 'in' | 'out'
    amount         REAL NOT NULL,          -- forecast input while unsettled; historical actual once settled
    stamped_amount REAL,                   -- immutable snapshot for revert (NULL for one-offs)
    settled        INTEGER NOT NULL DEFAULT 0,  -- THE driver of "what's left"
    date_paid      TEXT                    -- optional metadata; never required
);

-- Global settings, e.g. the default envelope mode.
CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL                    -- e.g. ('default_envelope_mode', 'automatic')
);
```

Key field notes:

- **`amount` is NOT NULL everywhere** and is seeded from the plan at stamp time, so it is never empty. That makes "marking paid requires an amount" true by construction — there is no null to chase.
- **`stamped_amount`** is the frozen value captured at stamp time. The plan's budget figure is never lost: budget-vs-actual for any month is `amount` vs `stamped_amount`.
- **`series_id`** is a *copied* value, not a foreign key. The relationship to the plan is severed (no cascade, plan can change or be deleted), but the identifier survives so trends can group.
- **Effective envelope mode** = `COALESCE(envelope.mode, setting['default_envelope_mode'])`. Setting `mode` explicitly is the per-envelope override.

---

## 4. Derived calculations

### Time elapsed (drives automatic accrual)

```
days_elapsed    = clamp(today - month.start_date, 0, days_in_month)
elapsed_fraction = days_elapsed / days_in_month        -- linear over the month
```

`period_type` (daily/weekly/monthly) is how the user *enters and reads* the target; accrual itself is linear by day. (A $2,000 monthly grocery envelope at ~17/30 of the month shows ~$1,133 consumed — matching the source spreadsheet.)

### Consumption & remaining

```
envelope.consumed =
    automatic : amount * elapsed_fraction
    manual    : SUM(txn.amount WHERE txn.envelope_id = envelope.id)

envelope.remaining = amount - consumed

txn.remaining (standalone) = settled ? 0 : amount
```

Note: on an automatic envelope you may still record transactions — they are kept purely as a record and do **not** affect `remaining`. (A "confirmed" flag could later distinguish a real figure from an untouched default, but it's deliberately omitted for now.)

### The "what's left" rollup

```
funds_available       = SUM(account.balance WHERE protected = 0)            -- checking, etc.
protected             = SUM(account.balance WHERE protected = 1)            -- credit cards + reserve
income_remaining      = SUM(txn.amount  WHERE direction='in'  AND settled=0 AND envelope_id IS NULL)
bills_remaining       = SUM(txn.amount  WHERE direction='out' AND settled=0 AND envelope_id IS NULL)
envelopes_remaining   = SUM(envelope.remaining)

whats_left = funds_available
           - protected
           + income_remaining
           - bills_remaining
           - envelopes_remaining
```

As the month progresses, automatic envelopes' `remaining` shrinks (commitments "release"), while the real checking balance falls with actual spending. The gap between them is your true over/under versus plan — surfaced only if you ever want it.

---

## 5. Key operations

### Stamp a plan onto a new month

```
stamp(plan, start_date, days_in_month):
    m = INSERT month(plan_id=plan.id, label, start_date, days_in_month)
    # accounts persist; balances carry forward and are updated by hand as needed
    for item in plan.items:
        if item.kind == 'envelope':
            INSERT envelope(month_id=m, series_id=item.id,
                            amount=item.amount, stamped_amount=item.amount,
                            period_type=item.period_type, mode=item.mode,
                            label=item.label, category=item.category)
        else:  # transaction
            INSERT txn(month_id=m, series_id=item.id,
                       amount=item.amount, stamped_amount=item.amount,
                       direction=item.direction, settled=0,
                       label=item.label, category=item.category)
    # link is now broken: the month is a self-contained snapshot
```

### Mark paid / un-mark

```
mark_paid(txn, actual):        # `actual` is prefilled with txn.amount; editable
    txn.amount  = actual       # NOT NULL guaranteed
    txn.settled = 1
    txn.date_paid = optional

unmark_paid(txn):
    txn.settled = 0
    prompt "Revert to the planned value?":
        yes -> txn.amount = txn.stamped_amount
        no  -> keep txn.amount   # default: never silently lose a real figure
```

### Trends

```sql
SELECT m.label AS period, t.amount
FROM txn t
JOIN month m ON m.id = t.month_id
WHERE t.series_id = :series_id
ORDER BY m.start_date;
```

Group by the copied `series_id`, never by name. Compare `amount` to `stamped_amount` for budget-vs-actual. Lazy use rides the stamped defaults and still produces a continuous line; correcting amounts at payment upgrades it to a true cost trend.

---

## 6. Storage & platform

- **SQLite, local-first.** The data is small (thousands of rows over years) but you want to *query across time* for trends, which is exactly SQLite's strength. Recompute the current month on open; query history when charting.
- **UUIDs over autoincrement**, so the file stays portable and a future multi-device sync won't collide.
- The computation-heavy, keyboard-friendly daily loop (open → glance at "what's left" → toggle a bill, update a balance) suits a **TUI** (lazygit-style) especially well, but the core is platform-agnostic — a web or mobile frontend can sit on the same schema later.

---

## 7. Deferred (intentionally not built yet)

- **Plan-diff indicators & restamp** — flagging where the current month drifts from a selected plan, with a one-click restamp. Enabled for free by the design: match each instance's `series_id` against the current plan's `plan_item.id`. No data changes needed to add it later.
- **"Confirmed amount" flag** for trend honesty on automatic envelopes.
- **Sub-monthly forecasting** (week-by-week) using the optional `date_paid`.
