-- Leeway schema — v7: a main plan, what-ifs on it, and dated plan items.
--
-- See knowledge/plans/2026-11-05-main-plan-and-what-ifs-design.md.
--
-- `plan_item.starts_on` / `ends_on`: the first and last month ('YYYY-MM') the item runs,
-- NULL for open-ended. An item runs in a month inside the range whose bit is set in
-- `active_months`. A one-off has both set to the same month.
--
-- `plan.base_plan_id`: NULL for Main and standalone plans. On a what-if, Main's id; the
-- what-if takes every item from Main except those its own rows change or remove.
--
-- `plan_item.base_item_id`: on a what-if's row, the Main item it changes (NULL for an item
-- the what-if adds). With `removed = 1` the row removes that item instead. Changes point at
-- Main's item rather than its series because a plan can hold one series more than once.
--
-- Main itself is the `main_plan_id` setting, set by the migration hook. A setting rather
-- than a flag on `plan`, so two syncing devices can't both end up with a Main.
--
-- Every new column is NULL or 0 on existing rows, so every existing plan stamps as before.
ALTER TABLE plan ADD COLUMN base_plan_id TEXT REFERENCES plan(id);
ALTER TABLE plan_item ADD COLUMN starts_on TEXT
    CHECK (starts_on IS NULL OR starts_on GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]');
ALTER TABLE plan_item ADD COLUMN ends_on TEXT
    CHECK (ends_on IS NULL OR ends_on GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]');
ALTER TABLE plan_item ADD COLUMN base_item_id TEXT REFERENCES plan_item(id);
ALTER TABLE plan_item ADD COLUMN removed INTEGER NOT NULL DEFAULT 0
    CHECK (removed IN (0, 1) AND (removed = 0 OR base_item_id IS NOT NULL));
CREATE INDEX plan_item_base_item ON plan_item (base_item_id) WHERE base_item_id IS NOT NULL;
