-- Migration 002 — promote "series" to a first-class entity.
--
-- Before: plan_item.id WAS the series identity, copied onto every stamped instance's
-- series_id. That coupled a series to one plan, so "Rent" in two plans were different
-- series and their trends never connected.
--
-- After: a `series` row is the durable identity (what a recurring item IS); `plan_item`
-- just references a series and carries this plan's budgeted amount (how much). Many plans
-- can share one series, so trends connect across plans and restamping with any plan keeps
-- continuity.
--
-- The instance tables (envelope, txn) are untouched: their series_id values already point
-- at the old plan_item ids, and we preserve those ids as the new series ids below, so
-- every existing instance still resolves and all history is kept.

-- 1. The new identity table. Holds the intrinsic fields that used to live on plan_item.
CREATE TABLE series (
    id          TEXT PRIMARY KEY,   -- reuses the old plan_item.id (see step 2)
    kind        TEXT NOT NULL,      -- 'transaction' | 'envelope'
    label       TEXT NOT NULL,      -- canonical label; editing it affects every plan
    category    TEXT,
    direction   TEXT,               -- transactions: 'in' | 'out'
    period_type TEXT,               -- envelopes: 'daily' | 'weekly' | 'monthly'
    mode        TEXT                -- envelopes: 'automatic' | 'manual' | NULL = inherit
);

-- 2. One series per existing plan_item, REUSING the id so instances keep resolving.
INSERT INTO series (id, kind, label, category, direction, period_type, mode)
SELECT id, kind, label, category, direction, period_type, mode FROM plan_item;

-- 3. Rebuild plan_item, slimmed to (plan, series, amount). SQLite can't add a
--    NOT NULL / REFERENCES column in place, so we use the standard create-copy-swap.
CREATE TABLE plan_item_new (
    id           TEXT PRIMARY KEY,
    plan_id      TEXT NOT NULL REFERENCES plan(id),
    series_id    TEXT NOT NULL REFERENCES series(id),
    amount_cents INTEGER NOT NULL
);

-- Each old row's series is the one we just minted with its own id.
INSERT INTO plan_item_new (id, plan_id, series_id, amount_cents)
SELECT id, plan_id, id, amount_cents FROM plan_item;

DROP TABLE plan_item;
ALTER TABLE plan_item_new RENAME TO plan_item;
