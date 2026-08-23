# Unified budget workspace

Date: 2026-08-22

## Problem

The Dashboard and Plans screens show the same core budget shape: Income, Expenses,
Envelopes, and Summary. Yet they use separate screens, focus types, event-loop branches,
layouts, and navigation keys. This makes plans feel like a side tool even though a plan is
another view of a budget.

The split also costs space and adds a jump. The Plans screen needs its own plan list while
the Dashboard keeps month movement in a focusable header. A user cannot scan months and
plans from one place.

## Outcome

Replace `Screen::Dashboard` and `Screen::Plans` with one budget workspace. A left sidebar
has two sections, Months and Plans. The selected row drives the detail area on the right.

```text
+--------------------------+-----------------------------------------------+
| Months                   | Leeway — 2026-08 (day 22 of 31)              |
| > 2026-08                +-----------------------------------------------+
|   2026-07                | Accounts                                      |
|   2026-06                +-----------------------+-----------------------+
|                          | Income                | Expenses              |
| Plans                    +-----------------------+-----------------------+
|   Monthly baseline       | Envelopes                                     |
|   Tight month            +-----------------------------------------------+
|   Summer                 | Summary                                       |
+--------------------------+-----------------------------------------------+
| context keys                                      global keys            |
+--------------------------------------------------------------------------+
```

Selecting a plan keeps the same frame and replaces only the detail data and commands:

```text
+--------------------------+-----------------------------------------------+
| Months                   | Leeway — Plan: Monthly baseline              |
|   2026-08                +-----------------------+-----------------------+
|   2026-07                | Income                | Expenses              |
|                          +-----------------------+-----------------------+
| Plans                    | Envelopes                                     |
| > Monthly baseline       +-----------------------------------------------+
|   Tight month            | Summary                                       |
+--------------------------+-----------------------------------------------+
| plan or plan-item keys                            global keys            |
+--------------------------------------------------------------------------+
```

The plan view omits Accounts. Accounts are live state, not part of a reusable plan. The
four shared panels keep the same positions, borders, colors, and amount alignment in both
modes. At narrow widths, stack Income above Expenses instead of squeezing both lists.

## Product rules

### Sidebar

- Show stamped months newest first.
- Show plans by name, as they are sorted now.
- Always show the selected month label. This keeps a month reached through `m` visible even
  when it has not been stamped.
- Keep the current calendar month visible when it has not been stamped.
- Headings are not selectable. `j`/`k` and the arrow keys move across data rows and cross
  the section break.
- The selected row stays highlighted when focus moves into the detail area. The sidebar
  border shows whether the sidebar itself has focus.
- Selection is live. Moving to a row refreshes the detail area; Enter is not needed.
- `m` remains the direct way to reach any `YYYY-MM`. A month outside the stored list becomes
  a temporary sidebar row until the user selects another target or stamps it.
- Keep `P` as a shortcut: it moves focus to the last selected plan when that plan still
  exists, or the first plan. It no longer changes screens.

This does not build an endless list of empty calendar months. Stamped months remain the
main history, while `m` keeps direct travel to any missing month.

### Focus

Use one focus ring:

```text
Sidebar -> Accounts? -> Income -> Expenses -> Envelopes -> Sidebar
```

Include Accounts only for the current calendar month. Skip it for past months, future
months, missing months, and plans. A missing month pins focus to the Sidebar because it has
no editable detail rows.

Keep a selected row index for each detail list in each target kind. Switching from a month
to a plan and back should restore both sets of row selections. Clamp all indices after each
data reload, as the app does now.

### Month target

A selected stamped month keeps current Dashboard behavior:

- Current-month Accounts and account terms in Summary.
- Settled income and expense actions.
- Envelope progress, modes, periods, and transaction management.
- Month-only add, edit, record, and delete commands.
- Summary animation.

A selected missing month keeps the current “not stamped” state. Its copy should tell the
user to choose a plan in the sidebar and press `s`. After stamping, select the new month in
the sidebar instead of changing screens.

The detail header still shows the month label and its current, past, upcoming, or not
stamped state. It no longer takes focus because the sidebar now owns target navigation.

### Plan target

A selected plan keeps current Plans behavior:

- Plan item amounts, active months, and forecast choices.
- Static plan projection and seasonal lines.
- Plan-only add, edit, and remove commands.
- Rename, stamp, create, and delete actions when the sidebar has focus.

After stamping a plan, select the stamped month in the same workspace. This makes the
result visible at once and removes the old Plans-to-Dashboard jump.

An empty Plans section shows a short prompt. When no plans exist, `n` from the Sidebar
creates the first one even though a month remains selected. Deleting the last plan selects
the nearest month and leaves focus in the Sidebar.

### Menus and keys

Build footer hints from two facts: the selected target kind and the focused panel.

| Selection | Focus | Local commands |
| --- | --- | --- |
| Month | Sidebar | `j/k` move, `m` go to month; `n` creates the first plan when none exist |
| Plan | Sidebar | `j/k` move, `n` new, `l` label, `s` stamp, `x` delete |
| Month | Accounts | Keep current account commands |
| Month | Income/Expenses | Keep current settle, add, amount, label, and delete commands |
| Month | Envelopes | Keep current transaction, amount, mode, period, record, add, and delete commands |
| Plan | Income/Expenses/Envelopes | Keep current plan-item commands |

`Tab`, `Shift+Tab`, `h`, `S`, `,`, and `q` keep their current roles. `Esc` quits from the
budget workspace. Series and Settings return to the exact month or plan target they came
from.

## Code design

### State

Add a stable target type:

```rust
enum BudgetTarget {
    Month { year: i32, month: u32 },
    Plan { plan_id: String },
}

enum BudgetFocus {
    Sidebar,
    Accounts,
    Income,
    Expenses,
    Envelopes,
}
```

Use `Screen::Budget` for the workspace. Store `BudgetTarget` in `App`; do not use a raw
sidebar index as identity because plan renames, plan deletes, sync, and month inserts can
reorder the rows. Derive the sidebar index from the target after each reload.

Keep the last selected plan ID as a separate, optional shortcut target for `P`. Clear or
replace it after a delete or sync when that plan no longer exists.

Carry the target in `SeriesOrigin` and Settings origin data. Returning from either screen
must restore the selected target and focused detail panel.

Keep month and plan row selections separate. They act on different records and already
have separate pending-selection flows. Rename the fields only when it makes the new code
clear; a large state rewrite is not needed for the first pass.

### Loaded view

Load both short sidebar lists on each budget frame:

- `queries::months()` for stamped months.
- `queries::plan_summaries()` for plans and item counts.

Then load detail for only the selected target:

```rust
enum BudgetDetail {
    Month(Option<MonthView>),
    Plan {
        plan: Plan,
        entries: Vec<PlanEntry>,
        projection: PlanProjection,
    },
}
```

No schema or migration change is needed. The data sets are small, and the app already
reloads the selected screen data on each event-loop pass.

### Rendering

Add `src/budget.rs` as the shared workspace shell. It owns:

- the outer sidebar/detail/footer layout;
- the sidebar renderer;
- the detail header;
- the shared Income, Expenses, Envelopes, and Summary placement;
- the responsive side-by-side or stacked split;
- the focus ring and footer assembly.

Keep target-specific rows and summary content in focused helpers. Month envelopes have
progress meters and remaining amounts; plan envelopes show period, mode, amount, and
active months. Trying to force both into one row model would make the code harder to read.
Share panel placement and block styling, not domain rules.

Move the current Dashboard body into the shared shell first without changing its output.
Then place the plan row renderers and plan summary in the same slots. The month Summary
keeps live account math and animation. The plan Summary keeps `project_plan` and stays
static.

At widths that cannot fit readable side-by-side lists, stack Income and Expenses. Compute
label and amount columns from each panel's `Rect`; do not rely on the current fixed widths.
Tiny terminals may clip content, but they must not panic.

### Commands

Use one top-level budget key handler for focus and sidebar movement. Dispatch detail verbs
to month and plan command helpers:

```text
budget::handle_key
  sidebar target -> sidebar commands
  month target   -> month commands
  plan target    -> plan commands
```

Keep database writes in the current month and plan helpers. Do not add a shared command
trait: plan items and month instances have different write rules.

Replace `pending_plan_select`, `pending_dash_*`, and the current stamp screen jumps with
pending target or row requests that resolve after the next load. A stamp completion should
set `BudgetTarget::Month` for the stamped label.

## Implementation steps

1. Add `BudgetTarget`, `BudgetFocus`, sidebar row building, and index resolution. Cover
   sorted months, sorted plans, virtual missing months, empty sections, deletes, and sync
   reorder with unit tests.
2. Add `Screen::Budget` and the shared shell. Move the Dashboard render and commands into
   it with no financial behavior change. Keep the old plan screen in place for this step so
   test failures stay easy to trace.
3. Render plan detail through the shared panel layout. Route plan-item and plan-level
   commands by target and focus. Then remove `Screen::Plans`, `PlanFocus`, its event-loop
   branch, and the old standalone plan layout.
4. Update stamp completion, Series and Settings origins, global `P`, missing-month copy,
   help-topic mapping, and footer hints for the new target model.
5. Update help files and replace screen-specific render tests with workspace tests. Run
   `cargo fmt`, `cargo clippy --all-targets --all-features`, and `cargo test`.

## Tests

Add coverage for:

- The sidebar groups months and plans, skips headings during movement, and preserves target
  identity when rows reorder.
- Launch selects the current month.
- A typed unstamped month appears and renders the missing-month prompt.
- Selecting a month loads month rows, month Summary, and Accounts only when current.
- Selecting a plan uses the same panel positions, omits Accounts, and renders the plan
  projection and seasonal lines.
- `Tab` and `Shift+Tab` use the right ring for current month, off-month, missing month, and
  plan targets.
- Footer hints change for month versus plan and for Sidebar versus detail focus.
- Month verbs cannot write plan items, and plan verbs cannot write month instances.
- Creating, renaming, and deleting a plan keeps a valid target.
- Stamping selects the resulting month and keeps its independent snapshot rules.
- Series and Settings return to the same target and selection.
- Wide, narrow, tiny, empty, and long-list terminal renders do not panic; long lists show
  scrollbars and keep the selected row visible.

## Scope limits

- Do not change month, plan, series, or stamp data rules.
- Do not show live account values in a plan.
- Do not link stamped months back to plan edits.
- Do not add month deletion.
- Do not add drag, mouse, or collapsible sidebar behavior.
- Do not redesign Series or Settings beyond restoring their budget-workspace origin.

## Done when

The app opens in one budget workspace. A user can move from a month to a plan in the left
sidebar, see the same four budget panels in the same places, use commands that match the
selected target, stamp a plan, and land on the new month without a screen change. All old
month and plan financial behavior remains intact.
