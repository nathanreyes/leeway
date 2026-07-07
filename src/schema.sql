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
--
-- `series_id` is a copied `series.id` value, deliberately NOT a live foreign key. We
-- considered enforcing it and chose not to: an instance is a self-contained snapshot (it
-- copies label/period_type/mode at stamp time and reads nothing from the series row), so it
-- doesn't depend on the series existing. Deleting a series once no plan uses it (see
-- ops::delete_series) legitimately orphans this id on past months, and that's fine — trends
-- still group by the value. A real FK would force ON DELETE RESTRICT (series never prunable
-- once stamped), SET NULL (wipes trend continuity), or CASCADE (destroys history), all of
-- which fight the snapshot model. Integrity holds by construction: series_id is only ever
-- set by copying a validated series.id at stamp time, never from user input.
CREATE TABLE envelope (
    id                   TEXT PRIMARY KEY,  -- UUID
    month_id             TEXT NOT NULL REFERENCES month(id),
    series_id            TEXT NOT NULL,     -- copied series.id; soft reference, NOT a live FK (see table comment)
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
    series_id            TEXT,              -- copied series.id, soft reference (see envelope table comment); NULL for one-offs
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
