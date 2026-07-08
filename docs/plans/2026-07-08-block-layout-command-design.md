# Block Layout and Contextual Create Design

## Context

The dashboard already separates envelopes from standalone transactions, but income and
expenses share one "Income & Bills" panel. The plan editor is further behind: it renders
all plan entries in one mixed list and uses `t`/`e` to create transactions/envelopes.

The agreed direction is to make the daily month view and the plan details screen use the
same mental model:

- Income and expenses are separate blocks, stacked in one column.
- Envelopes have their own column.
- `n` is the canonical command for "new item in the focused block."
- Footer hints only show commands that apply to the focused block.

## Design

### Dashboard

Keep the header and "What's left" summary. Under the summary, split the main body into
two columns:

- Left column: `Income` stacked over `Expenses`.
- Right column: `Envelopes`.

Accounts stay in the dashboard because balance editing remains part of the daily loop.
They should not compete with the budget blocks; render them as a compact support panel
near the summary, and keep account-specific commands visible only when accounts are
focused.

Dashboard focus cycle:

`Header -> Income -> Expenses -> Envelopes -> Accounts -> Header`

Behavior:

- `Enter`/space on income or expenses toggles settlement.
- `Enter`/space on envelopes files spending for manual envelopes.
- `Enter`/space on accounts edits the account balance or available credit.
- `n` on income creates an ad-hoc income transaction.
- `n` on expenses creates an ad-hoc expense transaction.
- `n` on envelopes creates an ad-hoc envelope.
- `n` elsewhere shows a status hint instead of creating anything.

### Plan Editor

Use the same budget-block model as the dashboard:

- Left column: `Income` stacked over `Expenses`.
- Right column: `Envelopes`.

Plan editor focus cycle:

`Income -> Expenses -> Envelopes -> Income`

Behavior:

- `n` on income creates a new transaction series with `Direction::In`.
- `n` on expenses creates a new transaction series with `Direction::Out`.
- `n` on envelopes creates a new envelope series.
- Transaction-only commands (`d`) are only advertised on income/expenses.
- Envelope-only commands (`m`, `p`) are only advertised on envelopes.
- Shared commands (`r`, `a`, `x`) work on the selected row in the focused block.

The existing `i` command for inserting an existing series stays global in the plan
editor, because the picker already shows both kinds and prevents duplicates.

## Data Flow

No schema changes are needed. The existing read models already contain enough data:

- Dashboard filters `MonthView::standalone` by `Direction::In` and `Direction::Out`.
- Plan editor filters `PlanEntry` by `Kind::Transaction` plus direction, or by
  `Kind::Envelope`.
- Existing create/edit/delete operations continue to mutate the same tables.

The UI state gains one selection index per visible block so switching focus does not lose
position.

## Testing

Unit tests should cover the direction-aware creation helpers if their signatures change.
Run the full Rust test suite after implementation. Manual verification should check:

- Tab focus order on both screens.
- `n` creates the right item type/direction in each block.
- Footer hints hide commands that do not apply to the focused block.
- Existing edit/delete/status protections still work for plan-derived dashboard rows.
