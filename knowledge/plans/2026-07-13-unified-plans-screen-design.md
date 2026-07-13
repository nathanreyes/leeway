# Unified Plans screen (master/detail)

Date: 2026-07-13

## Problem

Plans was split across two screens: `Screen::Plans` (a list of plan templates) and
`Screen::PlanEditor { plan_id }` (one plan's items), reached by pressing `Enter`
to drill in. For most users the plan list is short, so the drill-in added a
navigation hop without much payoff, and the two screens duplicated layout and
key-handling scaffolding.

## Outcome

A single master/detail screen (`Screen::Plans`):

- **Master** — the plan list on the left.
- **Detail** — the selected plan's item sublists (Income, Expenses, Envelopes)
  stacked in a single column on the right.
- **Summary** — the plan's cash-flow projection spanning the bottom, beneath both
  columns.

This mirrors the Dashboard's header / blocks / summary layout.

## Focus model

`PlanFocus` gained a `List` variant: `{ List, Income, Expenses, Envelopes }`.
`Tab` cycles `List → Income → Expenses → Envelopes → List`; `BackTab` reverses.
The master row stays highlighted regardless of which pane is focused (the mauve
border, not the row highlight, signals focus), so it's always clear which plan the
detail belongs to.

Verbs are focus-scoped:

- **List focused:** `n` new plan, `l` label, `s` stamp, `x` delete plan. `Enter`
  is intentionally inert — the detail already tracks the selected plan live.
- **Item pane focused:** `n` add item, `a` amount, `x` remove item. `m`/`p`
  redirect to the Series page (series-definition edits never happen here).

## Key changes

- `src/main.rs`: removed `Screen::PlanEditor` and `SeriesOrigin::PlanEditor`;
  rewrote the `Screen::Plans` event-loop branch to load the selected plan's entries
  each frame; added `App::pending_plan_select` (resolve a freshly created,
  name-sorted plan to its list index, mirroring `pending_select`); `NewPlan` submit
  now stays on Plans and selects the new plan instead of navigating to an editor.
- `src/plans.rs`: merged `handle_list_key` + `handle_editor_key` into `handle_key`
  (dispatches by `plan_focus`) and `draw_list` + `draw_editor` into `draw`
  (+ `draw_plan_list`).
- `src/help.rs`: folded the `PlanEditor` help arms into `Plans`, keyed off
  `plan_focus`; the screen ring is now `[PlansList, PlanIncome, PlanExpenses,
  PlanEnvelopes]`.
- `docs/help/plans.md`: added a Keys section for the master-pane verbs.

## Contextual `S`

From an item pane, `S` opens the focused row's series detail and returns to Plans
(selection restored via `plans_sel`). From the list, `selected_series_id` returns
`None`, so `S` opens the series list.
