# Main plan, what-ifs, and dated items

## Problem

Many people keep one main plan and a few small ones that add to it: a car loan, a side
job, kid costs. Each month they stamp the main plan, then stamp each small plan on top.
That takes several steps, and the month records only the first plan (`month.plan_id`),
so restamping from the main plan with "remove items the plan doesn't have" deletes the
add-on items.

Stamping several plans at once (`stamp_plans_batch_filtered`) eases the first step but
not the second, and it adds an idea, a month built from a set of plans, that every
screen then has to explain.

## Model

Two rules:

1. **Main is your budget.** Every item you expect lives in it, and each item says when
   it happens: every month, some months each year (`active_months`, exists), or
   between a start month and an end month (new).
2. **Every other plan is a what-if: Main with changes.** A what-if can add an item,
   remove one of Main's, or change one of Main's. Everything else it takes from Main,
   live, so editing Main reaches every what-if.

Main's what-ifs can't themselves have what-ifs. A month still records one plan, and
stamping is still one step.

Someone with one plan and no dates sees today's app. Each idea shows up only once they
use it.

### What this replaces

- Small add-on plans become dated items in Main ("Car payment, Jan 2027 – Jun 2027").
- Scenario plans ("tight month") become what-ifs, which no longer drift from Main the
  way copies do.
- Stamping several plans into one month stops being needed.

## Data

### Dated items

`plan_item` gains `starts_on` and `ends_on`, month labels like `'2027-01'`, each NULL
for open-ended. An item runs in a month when the month falls inside the range **and**
its `active_months` mask includes it. A one-off is `starts_on = ends_on`.

Like `active_months`, these belong to the plan item, not the series: they say what this
plan does with the item.

### Main

A `setting` row, `main_plan_id`, names Main. A setting rather than a flag on `plan`, so
two devices that sync can't both end up with a Main: the last write wins.

### What-ifs

`plan` gains `base_plan_id`, NULL except on what-ifs, where it holds Main's id.

A what-if's changes are its own `plan_item` rows, with one new column, `base_item_id`:

| Row | `base_item_id` | Meaning |
|---|---|---|
| Added | NULL | An item Main lacks; works like any plan item |
| Changed | Main's item id | Replaces that item: amount, months, dates, source |
| Removed | Main's item id, with `removed = 1` | Main's item doesn't run in this what-if |

Changes point at Main's `plan_item.id`, not its series, because a plan can hold the same
series more than once ("1 of 2" payments).

A changed row replaces the whole item, so later edits to that item in Main don't reach
the what-if. That's simpler than tracking each field, and the UI shows Main's current
amount next to the what-if's, so the gap stays visible. Other items still follow Main.

### Resolving a plan

`queries::load_plan_entries` is the one loader that stamp, restamp, and projections
use. For a what-if it returns:

- each of Main's items, unless the what-if removes it, in the what-if's changed form
  where it has one;
- then the what-if's added items.

Since every reader goes through this function, stamping, restamping, forecast sources,
and plan projections work on a what-if with no other change.

## Stamping

- Items whose dates or months exclude the target month are skipped, as seasonal items
  are today.
- The stamp preview returns those skipped items with a reason: starts later, ended,
  other months, or removed in this what-if. The apps list them under "Not in
  <month>", so the user can see why an item is missing.
- In the preview, a changed item carries Main's amount so the apps can show both.

## Restamping

Same rules as today (paid stays paid, matched rows refresh, missing items are added),
plus one:

- **A what-if's removal is intent.** Restamping a month from a what-if deletes unpaid
  rows that match an item it removes. Paid rows stay. Envelopes go only if they hold no
  spending. This applies without "remove items the plan doesn't have", since removing
  an item is the point of a what-if.

Items outside their dates are neither added nor removed, as seasonal items are today.

## Plan operations

- **Edit an item inherited from Main, from a what-if:** creates a changed row in the
  what-if. Editing Main itself stays a separate action.
- **Use Main's version:** deletes the what-if's changed or removed row.
- **Apply changes to Main:** writes each change into Main (changed rows overwrite Main's
  items, removals delete them, added items move over), then deletes the what-if.
- **Make standalone:** copies the resolved items into the plan as its own and clears
  `base_plan_id`. It stops following Main.
- **Merge into Main:** adds a standalone plan's items to Main and deletes the plan. This
  is how today's add-on plans move in; the user then sets dates where they need them.
- **Delete one of Main's items:** also deletes every what-if row that points at it.
- **Delete Main:** blocked while it has what-ifs. Make them standalone or delete them
  first.

## Projections

"A typical month" counts items with no month mask that run this month, so a finished car
loan no longer counts. The month strip covers the next 12 months from now, not January to
December, so dated items show where they land.

## Migration (v7)

- Add `plan_item.starts_on`, `plan_item.ends_on`, `plan_item.base_item_id`,
  `plan_item.removed`, and `plan.base_plan_id`. NULLs and 0 keep every existing plan as
  it is.
- Set `main_plan_id` to the plan with the most stamped months (ties: most items, then
  name).
- Leave every other plan standalone. Converting them to what-ifs is lossless (match by
  kind, series, and occurrence order, as restamp does), but an add-on plan converted
  that way becomes "Main minus nearly everything", which is wrong. The user picks:
  merge into Main, or turn into a what-if (a later version of this action can compute
  the changes).

Standalone plans exist only for migration and "Make standalone". They show under
"Other plans", and only when there are any.

## TUI

- The plan sidebar lists Main, its what-ifs indented under it, then other plans.
- The stamp window lists skipped items with their reasons.
- New keys on a what-if: `u` use Main's version of the focused item, `A` apply changes to
  Main. Item editing accepts start and end months next to `M`.
- Multi-plan stamping stays for now, for standalone plans. Drop it once nothing needs it.

## Open questions

- Should a what-if be able to change Main's item *partly*, say amount only, so later
  date edits in Main still reach it? That needs nullable fields in `plan_item` and a
  table rebuild.
- Should finished items be pruned from Main after a while, or stay under "Ended"?

## Out of scope

- Stacking what-ifs, and what-ifs of plans other than Main.
- Swapping which plan is Main.
- Saving a stamped month as a plan. It fits this model (stamped rows keep their
  `series_id`), but it stands on its own.
