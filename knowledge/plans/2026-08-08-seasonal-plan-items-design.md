# Seasonal plan items — active months on `plan_item`

## The problem

Some money is real, recurring, and predictable, but it doesn't land every month:
birthday gifts for the kids in March, July, and November; school fees in August;
an insurance premium every January.

Leeway had no way to say that. A `plan_item` carried one per-plan field,
`amount_cents`, and a `series` carried only intrinsic identity — kind, label,
direction, period type, mode. Nothing said "this applies in months X, Y, Z."

Both workarounds leaned on the user's memory, which is what the app is built to
avoid (§1: "Entry is optional enrichment, never a requirement"):

- Keep a second plan and restamp-merge it onto each birthday month. Works, but
  you must remember every year, months stamped ahead don't show it, and merge
  overwrites `month.plan_id` so the month records the wrong plan.
- Stamp the normal plan and hand-add the envelope to the month. Same remembering
  problem, and no plan ever describes it.

## The shape chosen

One nullable column, `plan_item.active_months`: a 12-bit mask, bit 0 = January.
NULL means every month, so every row that existed before the column keeps its
behavior with no backfill, and an item the user never restricted is
indistinguishable from one that predates the feature.

Stamping is the only reader. `ops::stamp` and both restamp paths skip items whose
mask excludes the month; the `envelope` / `txn` rows they write copy nothing about
seasonality.

### Why `plan_item` and not `series`

"How much, in this plan" already lives on `plan_item`. "When, in this plan" is
the same kind of question.

- Series edits are global and immediate (`set_series_label`, `set_series_mode`,
  `set_series_period` reach every plan at once). A schedule there would silently
  rewire plans the user wasn't looking at. `amount_cents` sits on `plan_item`
  precisely to dodge that.
- Plans exist to hold variants — §2's own example is "normal month, tight month,
  summer-with-the-kids". A tight-month plan should be free to drop the gift
  envelope or move it. A series-level schedule takes that away.
- The stamped `envelope` / `txn` rows are the wrong level outright: an instance
  already belongs to exactly one month. The mask is consumed at stamp time and
  never read again, so unlike `mode` and `period_type` there is nothing to freeze
  into the snapshot.

The cost: a seasonal series shared by several plans carries its months in each.
That is the duplication `amount_cents` already has, and it buys the per-plan
freedom above.

### Why an explicit month list, not a recurrence rule

Birthdays, school terms, and tuition are irregular. A list covers them and also
covers the regular cases (`jun-aug`, a single month) without a cadence engine,
an anchor date, or the question of what "every 3 months" means when you stamp a
month out of order. `MonthSet::parse` reads names, numbers, and ranges, so
`every quarter` is typed as `jan,apr,jul,oct` and needs no new concept.

## What changed

**Schema** — `src/migration_003_plan_item_active_months.sql` adds the column with
a `CHECK` keeping the mask in `1..4096`. `db::SCHEMA_VERSION` goes to 3.

Sync needed no code change (snapshots are whole-file), but `sync.rs` fails closed
on a revision newer than the running build, so a device on an older Leeway will
refuse a v3 snapshot until it updates. Release notes should say so.

**`MonthSet`** (`src/models.rs`) — a `u16` newtype with `ALL`, `contains`,
`months`, `from_db` / `to_db` (NULL ⇄ `ALL`), `parse`, `edit_string`, and
`short_label`. `parse` takes `all` / `*` / empty, comma- or space-separated names
and numbers, and non-wrapping ranges. A name is any prefix that matches exactly
one month, so `mar` and `sept` work and `ju` is an error rather than a guess —
better than an arbitrary minimum length, and it removes a rule.

**Stamping** (`src/ops.rs`) — `stamp` filters on `start_date.month()`.
`restamp_merge` and `restamp_replace` share `active_entries_for_month`, which
reads the `month` row once for both the day count and the month number. The two
consequences are worth naming:

- **Merge** into an off-season month inserts nothing and deletes nothing, so an
  instance standing from an earlier stamp survives.
- **Replace** treats an out-of-season instance as outside the plan, so a clean
  slate wipes it. That is how a gift envelope stamped into the wrong month gets
  cleaned up.

**Projection** (`src/calc.rs`) — `project_plan`'s four totals now count only
always-on items and it returns `seasonal: Vec<SeasonalItem>` alongside. The
headline stays the number you plan against; averaging seasonal money into every
month would make the plan describe a month that never happens.

**Plan screen** (`src/plans.rs`, `src/main.rs`) — `M` on a focused item opens a
prompt prefilled with `edit_string()`; `PromptKind::ItemMonths` writes through
`ops::set_item_active_months`. Seasonal rows carry a dim months tag; the Summary
lists up to three seasonal items under "what's left" and collapses the rest to a
count. The summary block grows only when there is something to list, so a plan
without seasonal items renders exactly as before.

Untouched on purpose: the dashboard (months are plain snapshots), the Series page
(months are per-plan, like amount), and `plan_summaries` (counts items).

## Rejected

- **A month selector on the plan screen** that recomputes the Summary for a
  chosen month. More accurate, but it adds state and a control to a screen whose
  job is to describe a template, and the seasonal lines already answer "what does
  a birthday month cost?".
- **Averaging seasonal amounts into the totals** (120 across three months = 30/mo).
  Smooth, and wrong for a forecasting tool: it understates March and overstates
  April, and no month ever matches the headline.
- **A per-month amount override table.** Strictly more expressive, and more than
  the problem needs — "extra in birthday months" is two items that add up, not one
  item with twelve amounts.
