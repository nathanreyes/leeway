# Save a month as a plan

Builds on `2026-11-05-main-plan-and-what-ifs-design.md`.

## Problem

People trim a month by hand: drop a bill, lower an envelope, add a one-off. To budget the
same way next month they must make the same edits again, or build a plan by hand.

## Model

"Save month as plan" turns a stamped month, edits and all, into a new plan. Two choices:

- **What-if on Main** (the default). A month stamped from Main is already Main with
  changes, so the new plan keeps following Main's later edits, lists its changes like any
  what-if, and "Apply changes to Main" can turn a trimmed month into the budget.
- **Its own plan.** A standalone copy that no longer follows Main. This is the default when
  the month wasn't stamped from Main or one of its what-ifs.

Afterward the month records the new plan (`month.plan_id`), so restamping it uses that plan.

## Differences

The month is compared with a base plan: Main for a what-if, the month's own plan for a plan
of its own (with no plan, every row is an addition). Only base items that run in the month
count; a seasonal or dated item that skips this month is not missing. Rows pair with base
items through `match_month_rows`, as restamp pairs them, so repeated payments match the
right row.

Each month row is compared by its **budgeted** amount: an envelope's amount, an open
transaction's amount, and a paid transaction's stamped amount, since paying records what
was spent, not a plan. Envelope spending rows are skipped. The base amount is what the base
plan would stamp into this month now (`forecast::resolve_plan_entries`). An item the month's
own plan stamps from a forecast counts as matching while the row still holds its stamped
amount; a what-if's fixed amount over Main's forecast still counts as a change.

`match_month_rows` now pairs a plan item with a row of its series that budgets the same
amount before it falls back to row order. Otherwise two saved payments of one series could
swap rows on the next restamp, and an open payment take the paid one's amount.

A difference is one of:

| Difference | When | Starts |
|---|---|---|
| Changed | A paired row's amount differs | checked |
| Removed | A base item that runs this month has no row | checked |
| Added | A row pairs with no base item | checked, unless added by hand (`series_id IS NULL`): a one-off |

`ops::month_differences(conn, month_id, as_what_if, today)` returns the base plan, its
differences, how many items match, what a typical month of the base leaves, and for each
difference how much it moves that figure. The apps show only the differences, each with a
check, and count the rest in one line.

## Saving

`ops::save_month_as_plan(conn, month_id, name, as_what_if, chosen, today)` works out the
differences again and keeps the chosen ones, by key. Unchosen differences follow the base.

**As a what-if**, the plan's `base_plan_id` is Main, and each chosen difference becomes one
row:

- Changed → a changed row (`base_item_id` set), Main's months and dates, a static amount.
- Removed → a removed row.
- Added → an added row. A hand-added row first gets a new series, and the month row takes
  that series, so restamping matches it instead of adding a second copy.

**As its own plan**, each base item is copied with its months, dates, and source, with
chosen changes applied and chosen removals left out; base items that skip this month come
along too. Chosen additions are added as for a what-if.

Amounts are stored per period: an envelope budgeted per day keeps a daily rate.

## Terminal app

`w` on a stamped month in the budget workspace (`S` already opens Series) prompts for a
name, the month's by default, and saves a what-if on Main with every difference except
one-offs.

## Out of scope

- Saving an envelope's spending, or a paid amount, as the plan's amount.
- Renaming items: a month row's label doesn't change the series.
