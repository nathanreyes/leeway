-- Leeway schema — v1 (consolidated)
--
-- No database has shipped yet, so rather than carry a chain of migrations we keep ONE
-- clean initial schema that reflects the current design. Once real users exist, evolve it
-- by adding a *new* migration file (see db.rs) instead of editing this one.
--
-- Money: every money value is stored as an INTEGER number of cents (e.g. $12.34 -> 1234),
-- never a float. Floats can't represent most decimal cents exactly, so sums drift
-- (0.1 + 0.2 != 0.3). Integer cents are exact; we convert to/from dollars only at the
-- display edge (see money.rs). Columns are suffixed `_cents` to make the unit obvious.
--
-- rusqlite_migration runs this whole file inside a transaction the first time the app
-- opens a fresh database.

-- Real-world balances. Manually entered ground truth, carried across months.
-- Cash-flow roles come from `type`: checking is spendable, a credit card is a liability
-- entered as limit + available credit (owed = limit − available).
CREATE TABLE account (
    id                     TEXT PRIMARY KEY,  -- UUID
    name                   TEXT NOT NULL,
    type                   TEXT NOT NULL,     -- 'checking' | 'credit_card'
    balance_cents          INTEGER NOT NULL,  -- checking: spendable balance; card: unused (0)
    credit_limit_cents     INTEGER,           -- credit cards only (NULL otherwise)
    available_credit_cents INTEGER,           -- credit cards only (NULL otherwise)
    -- One amount, meaning set by `type` (NULL = none, treated as 0):
    --   checking    -> a buffer you keep parked; held back from "what's left".
    --   credit_card -> a balance you'll carry, not pay off now; forgives that much debt.
    -- Same column, opposite sign in the math — the sign is derived in code, never stored
    -- (see Account::carry_adjustment). Mutually exclusive by `type`, so one column suffices.
    carry_balance_cents    INTEGER
);

-- Named templates.
CREATE TABLE plan (
    id   TEXT PRIMARY KEY,                    -- UUID
    name TEXT NOT NULL
);

-- A first-class recurring-item definition — what a recurring item IS (vs. how much any one
-- plan budgets for it). `series.id` is the durable series identity: minted once, never
-- reused or recomputed, copied onto every instance it stamps as `series_id`, and the value
-- trends group by. It is NOT derived from the label, so renames are free. Many plans can
-- share one series, so trends connect across plans.
CREATE TABLE series (
    id          TEXT PRIMARY KEY,             -- UUID — the durable series id
    kind        TEXT NOT NULL,                -- 'transaction' | 'envelope'
    label       TEXT NOT NULL,                -- canonical; editing it affects every plan
    direction   TEXT,                         -- transactions: 'in' | 'out'
    period_type TEXT,                         -- envelopes: 'daily' | 'monthly'
    mode        TEXT,                         -- envelopes: 'automatic' | 'manual'
    -- An envelope's `mode` is chosen once, at creation (the app seeds it from the global
    -- default_envelope_mode setting), and frozen thereafter: changing the global default
    -- never alters an existing series. Transactions have no mode. Enforcing "envelopes must
    -- carry a concrete mode" here is what makes the default a creation-time seed rather than
    -- a live input that could shift behavior under the user.
    CHECK (kind <> 'envelope' OR mode IS NOT NULL)
);

-- A series' membership in one plan: which series, and this plan's budgeted amount for it.
CREATE TABLE plan_item (
    id           TEXT PRIMARY KEY,            -- UUID
    plan_id      TEXT NOT NULL REFERENCES plan(id),
    series_id    TEXT NOT NULL REFERENCES series(id),
    amount_cents INTEGER NOT NULL             -- this plan's budgeted amount for the series
);

-- A stamped period.
CREATE TABLE month (
    id            TEXT PRIMARY KEY,           -- UUID
    plan_id       TEXT,                       -- which plan was stamped (record only; snapshot is independent)
    label         TEXT NOT NULL,              -- e.g. '2026-06'
    start_date    TEXT NOT NULL,              -- ISO 'YYYY-MM-DD'
    days_in_month INTEGER NOT NULL
);

-- Envelope instance (one per stamped envelope per month).
--
-- `series_id` is a copied `series.id` value, deliberately NOT a live foreign key. We
-- considered enforcing it and chose not to: an instance is a self-contained snapshot (it
-- copies label/period_type/mode at stamp time and reads nothing from the series row, nor
-- from any global setting), so it doesn't depend on the series existing. Deleting a series
-- once no plan uses it (see ops::delete_series) legitimately orphans this id on past
-- months, and that's fine — trends still group by the value. A real FK would force ON
-- DELETE RESTRICT (series never prunable once stamped), SET NULL (wipes trend continuity),
-- or CASCADE (destroys history), all of which fight the snapshot model. Integrity holds by
-- construction: series_id is only ever set by copying a validated series.id at stamp time.
--
-- `series_id` is also NULLABLE, and NULL carries meaning: an **ad-hoc** envelope — one the
-- user added straight into this month rather than by stamping a plan. This mirrors `txn`,
-- where a NULL `series_id` has always marked hand-entered rows. So "did this come from a
-- plan?" is answered the same way for both tables: `series_id IS NOT NULL`. Restamp/Replace
-- treats a NULL-series envelope exactly like a hand-entered txn (kept or wiped together).
--
-- `mode` is NOT NULL: it is frozen at stamp time from the (already-concrete) series mode
-- and never re-resolved, so a stamped month can never change behavior under the user.
CREATE TABLE envelope (
    id                   TEXT PRIMARY KEY,  -- UUID
    month_id             TEXT NOT NULL REFERENCES month(id),
    series_id            TEXT,              -- copied series.id (soft ref, see above); NULL = ad-hoc
    label                TEXT NOT NULL,
    amount_cents         INTEGER NOT NULL,  -- this month's budget (editable)
    stamped_amount_cents INTEGER NOT NULL,  -- immutable snapshot, used by "revert to planned"
    period_type          TEXT NOT NULL,     -- 'daily' | 'monthly'
    mode                 TEXT NOT NULL      -- 'automatic' | 'manual' — frozen at stamp time
);

-- The atomic money event. Standalone (bill/income) or attached to a manual envelope.
CREATE TABLE txn (
    id                   TEXT PRIMARY KEY,  -- UUID
    month_id             TEXT NOT NULL REFERENCES month(id),  -- the PERIOD = the trend time axis
    series_id            TEXT,              -- copied series.id, soft reference (see envelope table); NULL for one-offs
    envelope_id          TEXT REFERENCES envelope(id),        -- NULL = standalone (bill/income)
    account_id           TEXT REFERENCES account(id),
    label                TEXT NOT NULL,
    direction            TEXT NOT NULL,     -- 'in' | 'out'
    amount_cents         INTEGER NOT NULL,  -- forecast input while unsettled; historical actual once settled
    stamped_amount_cents INTEGER,           -- immutable snapshot for revert (NULL for one-offs)
    settled              INTEGER NOT NULL DEFAULT 0,  -- THE driver of "what's left"
    date_paid            TEXT               -- optional metadata; never required
);

-- Global settings. `default_envelope_mode` seeds the mode of newly created envelope series;
-- it is never read again once a series is created (see the series.mode comment).
CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL                     -- e.g. ('default_envelope_mode', 'automatic')
);

INSERT INTO setting (key, value) VALUES ('default_envelope_mode', 'automatic');
