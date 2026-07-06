-- Ballpark schema — v1
--
-- This is your app-spec §3 schema with ONE deliberate change: every money value is
-- stored as an INTEGER number of cents (e.g. $12.34 -> 1234), never a floating-point
-- dollar amount. Floats can't represent most decimal cents exactly, so sums drift
-- (0.1 + 0.2 != 0.3). Integer cents are exact; we convert to/from dollars only at the
-- display edge (see money.rs). Columns are suffixed `_cents` to make the unit obvious.
--
-- rusqlite_migration runs this whole file inside a transaction the first time the app
-- opens a fresh database. To evolve the schema later you add a *new* migration rather
-- than editing this one.

-- Real-world balances. Manually entered ground truth, carried across months.
CREATE TABLE account (
    id        TEXT PRIMARY KEY,             -- UUID
    name      TEXT NOT NULL,
    type      TEXT NOT NULL,                -- 'checking' | 'credit_card' | 'reserve' | 'investment'
    balance_cents INTEGER NOT NULL,
    protected INTEGER NOT NULL DEFAULT 0    -- 1 = held back from funds (credit cards, reserve)
);

-- Named templates.
CREATE TABLE plan (
    id   TEXT PRIMARY KEY,                  -- UUID
    name TEXT NOT NULL
);

-- Recurring definitions inside a plan.
-- plan_item.id is the PERMANENT SERIES IDENTITY: minted once, never reused or recomputed,
-- copied onto every instance it stamps. It is NOT derived from the label, so renames are free.
CREATE TABLE plan_item (
    id            TEXT PRIMARY KEY,         -- UUID — the durable series id
    plan_id       TEXT NOT NULL REFERENCES plan(id),
    kind          TEXT NOT NULL,            -- 'transaction' | 'envelope'
    label         TEXT NOT NULL,            -- cosmetic only; safe to edit
    slug          TEXT,                     -- optional, for display/search ONLY — never used to match
    category      TEXT,
    direction     TEXT,                     -- 'in' | 'out'  (transactions)
    amount_cents  INTEGER NOT NULL,         -- default / budgeted amount
    period_type   TEXT,                     -- envelopes: 'daily' | 'weekly' | 'monthly'
    mode          TEXT                      -- envelopes: 'automatic' | 'manual' | NULL = inherit global default
);

-- A stamped period.
CREATE TABLE month (
    id            TEXT PRIMARY KEY,         -- UUID
    plan_id       TEXT,                     -- which plan was stamped (record only; snapshot is independent)
    label         TEXT NOT NULL,            -- e.g. '2026-06'
    start_date    TEXT NOT NULL,            -- ISO 'YYYY-MM-DD'
    days_in_month INTEGER NOT NULL
);

-- Envelope instance (one per stamped envelope per month).
CREATE TABLE envelope (
    id                   TEXT PRIMARY KEY,  -- UUID
    month_id             TEXT NOT NULL REFERENCES month(id),
    series_id            TEXT NOT NULL,     -- copied plan_item.id (plain value, NOT a live FK)
    label                TEXT NOT NULL,
    category             TEXT,
    amount_cents         INTEGER NOT NULL,  -- this month's budget (editable)
    stamped_amount_cents INTEGER NOT NULL,  -- immutable snapshot, used by "revert to planned"
    period_type          TEXT NOT NULL,     -- 'daily' | 'weekly' | 'monthly'
    mode                 TEXT               -- 'automatic' | 'manual' | NULL = inherit global default
);

-- The atomic money event. Standalone (bill/income) or attached to a manual envelope.
CREATE TABLE txn (
    id                   TEXT PRIMARY KEY,  -- UUID
    month_id             TEXT NOT NULL REFERENCES month(id),  -- the PERIOD = the trend time axis
    series_id            TEXT,              -- copied plan_item.id; NULL for one-offs (no series)
    envelope_id          TEXT REFERENCES envelope(id),        -- NULL = standalone (bill/income)
    account_id           TEXT REFERENCES account(id),
    label                TEXT NOT NULL,
    category             TEXT,
    direction            TEXT NOT NULL,     -- 'in' | 'out'
    amount_cents         INTEGER NOT NULL,  -- forecast input while unsettled; historical actual once settled
    stamped_amount_cents INTEGER,           -- immutable snapshot for revert (NULL for one-offs)
    settled              INTEGER NOT NULL DEFAULT 0,  -- THE driver of "what's left"
    date_paid            TEXT               -- optional metadata; never required
);

-- Global settings, e.g. the default envelope mode.
CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL                     -- e.g. ('default_envelope_mode', 'automatic')
);

-- A sensible default so COALESCE(envelope.mode, setting) always resolves.
INSERT INTO setting (key, value) VALUES ('default_envelope_mode', 'automatic');
