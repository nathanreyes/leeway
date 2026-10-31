# Restamp keeps paid items paid

## Problem

Restamp has two modes. Merge refreshes unsettled rows that match the plan, adds
missing plan items, and deletes nothing. Replace resets every matched row,
including paid ones, and deletes rows outside the plan unless the user asks to
keep them. Replace also treats spending inside a kept envelope as outside the
plan, so a wipe deletes it.

Resetting a paid bill throws away a real actual and changes what's left. The
iOS mock settled on a rule: a restamp never touches a paid item.

## Rules

1. A restamp never changes a settled standalone transaction: no amount reset,
   no unsettle.
2. Spending inside an envelope the plan still has stays linked to that envelope.
3. With 1 and 2 in place, Replace with "keep outside plan" is Merge. So there is
   one restamp with one option, **remove items the plan doesn't have** (off by
   default).

## Behavior

`ops::restamp(conn, month_id, plan_id, remove_unplanned)` (and `restamp_filtered`
with stamp-window exclusions):

- Each plan entry matches the next unmatched row of the same kind and series, in
  row order, as today.
- A matched envelope is refreshed in place (same id, so its spending stays).
- A matched unsettled standalone transaction is refreshed to the plan amount.
- A matched settled one is left alone but still counts as matched.
- A plan entry with no match is inserted.
- With `remove_unplanned`, unmatched standalone transactions (paid or not) and
  unmatched envelopes, with their spending, are deleted. Deleting a paid item
  doesn't change what's left, so this is safe.
- Envelope spending never counts as "outside the plan" by itself; it goes only
  when its envelope goes.

`month_has_items_outside_plan` skips envelope spending for the same reason.

## TUI

Stamping an already-stamped month opens one choice:

- `r` Restamp (paid items stay paid)
- `x` Restamp and remove items not in the plan (shown only when the month has
  such items)
- `c` Cancel

The Merge/Replace menu and the follow-up wipe/keep prompt go away.

## Out of scope

No schema change. Fresh stamping is unchanged.
