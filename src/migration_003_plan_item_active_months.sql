-- Leeway schema — v3: seasonal plan items.
--
-- Some money recurs on a yearly rhythm rather than a monthly one: birthday gifts in March
-- and July, school fees in August, an insurance premium every January. Before this column a
-- plan could only say "every month or never", so the only ways to budget for it were to keep
-- a second plan and remember to merge it, or to hand-add the item to the month.
--
-- `active_months` is a 12-bit mask: bit 0 = January … bit 11 = December. NULL means every
-- month, which is what every existing row reads as — so this migration changes no behavior
-- and needs no backfill. The mask lives on `plan_item` rather than `series` for the same
-- reason `amount_cents` does: it answers "what does THIS plan do with the item", and series
-- edits reach every plan at once.
--
-- Stamping is the only reader. `ops::stamp` (and both restamp paths) skip items whose mask
-- excludes the month being stamped; the resulting `envelope`/`txn` rows copy nothing about
-- seasonality, because an instance already belongs to exactly one month.
ALTER TABLE plan_item ADD COLUMN active_months INTEGER
    CHECK (active_months IS NULL OR (active_months > 0 AND active_months < 4096));
