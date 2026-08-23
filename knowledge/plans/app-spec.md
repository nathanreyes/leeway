# Budgeting App — Design Spec

A single-user, local-first budgeting app whose job is to answer one question all month long: **"what's left?"** It is a *forecasting* tool, not an accounting ledger — most spending is never entered transaction-by-transaction; it's reserved and drawn down. Low daily friction is a first-class design goal ("lazy-first").

---

## 1. Philosophy & guardrails

- The **current checking balance is ground truth**, entered manually. The app never reconciles individual transactions against it.
- Known income and bills are **events** you toggle as settled — you usually don't care *when*, only *whether*.
- Spending pools (groceries, dining) are **envelopes** that bleed down over the month on their own.
- The app keeps every month it has ever stamped, so it can show **trends** ("what was electric this time last year?").
- Anything that would force routine data entry into the main loop is suspect. Entry is optional enrichment, never a requirement.
- **Cash flow, not net worth.** Accounts are only checking (spendable) and credit cards (a liability = `credit_limit − available_credit`). Saving toward a goal is modeled as a monthly commitment (an envelope or a set-aside bill that lowers "what's left"); tracking accumulated goal *balances* is out of scope.

---

## 2. Core concepts

Three primitives, cleanly related:

- **Transaction** — an atomic money event: amount, direction (in/out), a settled flag. Paychecks and known bills *are* transactions. A transaction may stand alone (a bill, a paycheck) or belong to a manual envelope (a grocery purchase).
- **Envelope** — a budgeted pool with a consumption *mode*. An **automatic** envelope accrues over time; a **manual** envelope is consumed by the transactions inside it.
- **Series** — a first-class, durable definition of a recurring item (Rent, Groceries): its kind, label, and coded fields (direction / period / mode). A series is the **permanent identity** that stamped instances carry as `series_id` and that trends group by. One series can appear in many plans, so trends connect even when you switch which plan you stamp.

And a separation across time:

- **Plan** — a reusable *template*: a named set of plan-items, each referencing a **series** and carrying that plan's budgeted *amount*, *active months*, and saved amount source. The series says *what* the item is; the plan says *how much* and *when*. An item runs every month unless the plan narrows it, which is how yearly costs — birthday gifts, school fees, an annual premium — live in the baseline instead of in the user's memory. You can keep several plans (normal month, tight month, summer-with-the-kids) that share series, and choose which to stamp.
- **Stamping** — at the start of a month you stamp a plan. A review shows each concrete amount and lets you choose a static or available historical source. The app remembers that source on the plan item. Confirming copies the items into concrete instances for that month, skipping any item whose active months exclude that month. The link to the plan is then **broken**: the month is an independent snapshot. Editing the plan afterward never reaches back into a stamped month. A month may be **restamped** with any plan — **merge** (additive; refresh unsettled instances, protect settled ones) or **replace** (clean slate; keeps hand-entered data only if you ask). Matching is by shared `series_id`.

---

## 3. Data model (SQLite)

```sql
-- Cash-flow accounts. Manually entered ground truth, carried across months. Only two
-- roles, derived from `type`: checking (spendable) and credit_card (a liability). Reserve/
-- investment and net-worth tracking are intentionally out of scope.
CREATE TABLE account (
    id               TEXT PRIMARY KEY,     -- UUID
    name             TEXT NOT NULL,
    type             TEXT NOT NULL,        -- 'checking' | 'credit_card'
    balance          REAL NOT NULL,        -- checking: spendable balance (credit cards store 0)
    credit_limit     REAL,                 -- credit cards only
    available_credit REAL                  -- credit cards only; owed = credit_limit − available_credit
);

-- Named templates.
CREATE TABLE plan (
    id   TEXT PRIMARY KEY,                 -- UUID
    name TEXT NOT NULL
);

-- The PERMANENT SERIES IDENTITY: minted once, never reused or recomputed, copied onto
-- every instance it stamps as `series_id`. It is NOT derived from the label, so renames
-- are free. One series may be referenced by many plans; editing it affects them all.
CREATE TABLE series (
    id          TEXT PRIMARY KEY,          -- UUID — the durable series id
    kind        TEXT NOT NULL,             -- 'transaction' | 'envelope'
    label       TEXT NOT NULL,             -- cosmetic only; safe to edit
    direction   TEXT,                      -- 'in' | 'out'  (transactions)
    period_type TEXT,                      -- envelopes: 'daily' | 'monthly'
    mode        TEXT                       -- envelopes: 'automatic' | 'manual' | NULL = inherit global default
);

-- A series' membership in a plan. The series says WHAT the item is; the plan_item says
-- HOW MUCH this template budgets for it and WHEN it runs. Both are per-plan; everything
-- else lives on the series.
CREATE TABLE plan_item (
    id            TEXT PRIMARY KEY,        -- UUID (per-plan row; NOT the series identity)
    plan_id       TEXT NOT NULL REFERENCES plan(id),
    series_id     TEXT NOT NULL REFERENCES series(id),
    amount        REAL NOT NULL,           -- envelopes: daily rate or monthly total, by period_type
    active_months  INTEGER,                -- 12-bit mask, bit 0 = Jan; NULL = every month
    forecast_method TEXT NOT NULL DEFAULT 'static'
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
    amount         REAL NOT NULL,          -- this month's monthlyized budget (editable)
    stamped_amount REAL NOT NULL,          -- immutable snapshot, used by "revert to planned"
    period_type    TEXT NOT NULL,          -- 'daily' | 'monthly'
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
- **`series_id`** on an instance is a *copied* value, not a live foreign key. The link to the plan is severed at stamp time (the plan can change or be deleted), but the identifier equals a `series.id` that persists, so trends group across months — and across plans, since plans share series. Restamp matches instances to a plan's items by this value.
- **Effective envelope mode** = `COALESCE(envelope.mode, setting['default_envelope_mode'])`. Setting `mode` explicitly is the per-envelope override.

---

## 4. Derived calculations

### Time elapsed (drives automatic accrual)

```
days_elapsed    = clamp(today - month.start_date, 0, days_in_month)
elapsed_fraction = days_elapsed / days_in_month        -- linear over the month
```

`period_type` is either daily or monthly. A plan item amount is entered in that unit:
daily means "amount per day", monthly means "amount for the month." When a plan is
stamped, daily envelope rates are monthlyized with the stamped month's day count
(`daily_rate * days_in_month`) and stored on the envelope instance as the concrete
monthly budget. Accrual itself is still linear by day. (A $2,000 monthly grocery
envelope at ~17/30 of the month shows ~$1,133 consumed — matching the source
spreadsheet.)

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
funds_available       = SUM(account.balance WHERE type='checking')              -- spendable
card_debt             = SUM(credit_limit - available_credit WHERE type='credit_card')  -- owed
income_remaining      = SUM(txn.amount  WHERE direction='in'  AND settled=0 AND envelope_id IS NULL)
bills_remaining       = SUM(txn.amount  WHERE direction='out' AND settled=0 AND envelope_id IS NULL)
envelopes_remaining   = SUM(envelope.remaining)

whats_left = funds_available
           - card_debt
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
    review = resolve each active item's saved source from months before start_date
             # unavailable history falls back to the static item.amount
    user reviews amounts and may change each saved source
    m = INSERT month(plan_id=plan.id, label, start_date, days_in_month)
    # accounts persist; balances carry forward and are updated by hand as needed
    for item in review:
        if item.kind == 'envelope':
            INSERT envelope(month_id=m, series_id=item.id,
                            amount=item.resolved_amount,
                            stamped_amount=item.resolved_amount,
                            period_type=item.period_type, mode=item.mode,
                            label=item.label)
        else:  # transaction
            INSERT txn(month_id=m, series_id=item.id,
                       amount=item.resolved_amount,
                       stamped_amount=item.resolved_amount,
                       direction=item.direction, settled=0,
                       label=item.label)
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
- **Net-worth / goal tracking** — reserve & investment accounts, and accumulated progress toward savings goals (a target + running balance). Deliberately excluded to keep the tool focused on cash flow.
- **Account management UI** — creating/renaming/deleting accounts (they are currently seeded).
