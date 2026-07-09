# Searchable Series Add Flow Design

## Context

Adding items should not split the user's mental model between "new" and "existing."
When adding an item to a plan or month, the user should type the thing they mean and let
the app either reuse the matching series or create it. The separate plan-editor `i`
command is unnecessary once `n` can search existing income, expenses, and envelopes in
the focused context.

The design also simplifies month items: income, expenses, and envelopes are all instances
of durable series. A one-off item can still be represented by a series that happens to
appear only once.

## Design

`n` becomes the only add command for budget blocks in both the plan editor and month
dashboard. The plan editor removes `i` and no longer needs the full-screen series picker.

When `n` opens in a focused block, it shows a searchable add prompt:

- Income searches transaction series with `Direction::In`.
- Expenses searches transaction series with `Direction::Out`.
- Envelopes searches envelope series.

As the user types, matching series appear immediately. Arrow keys move the highlight
through matches. `Enter` selects the highlighted series. If the input is
non-empty and no visible match is selected, the prompt offers a create action such as
`Create new expense named "Rent"`.

After choosing a series or create-new action, the app prompts for amount and then inserts
one row:

- In a plan, insert a `plan_item` for the selected or newly created series.
- In a month, insert a concrete transaction or envelope instance for the selected or
  newly created series.

The dashboard no longer treats `series_id IS NULL` as the marker for hand-entered budget
items. Main budget rows are series-backed by default, so the visible recurring/ad-hoc
distinction can be removed for income, expenses, and envelopes.

## Restamp Policy

Restamp behavior keeps two identities separate:

- `series.id` is the durable trend/reuse identity.
- A plan item or month row is an occurrence of that series.

The same series may appear more than once in a plan or month. That supports real-world
cases such as two paychecks from the same income series in one monthly template.

Merge:

- Match each target-plan occurrence to an existing month occurrence of the same series
  where possible.
- Refresh the matched instance from that plan occurrence, preserving settled transaction
  actuals as today.
- If a target-plan occurrence has no available month occurrence, insert it.
- If a month already has multiple occurrences for a series, match them in stable row order.

Replace:

- Reset or insert every occurrence in the target plan.
- Remove extra month occurrences whose series cannot be matched to a target-plan
  occurrence unless the user chooses to keep items outside the plan.
- Manual spending inside envelopes remains linked when its envelope is kept; if its
  envelope is removed while spending is kept, detach the spending to standalone as today.

This keeps the useful Replace prompt, but reframes it as "keep items not in this plan"
instead of "keep hand-entered items."

## Data Flow

The core reuse identity remains `series.id`. Plan entries and month instances both refer
to it. Creating a new item creates the series first, then inserts the plan item or month
instance.

Series id is not unique inside a plan or month. The row id is the occurrence identity:

- `plan_item.id` distinguishes repeated plan occurrences of the same series.
- `txn.id` and `envelope.id` distinguish repeated month occurrences of the same series.

The implementation should add explicit month insert helpers for a series-backed
transaction and envelope. Existing one-off helpers can either be retired for main budget
items or limited to manual envelope spending, where a standalone spending row still makes
sense.

Do not block adding the same series twice. The search prompt should allow selecting an
already-present series and create another occurrence with its own amount.

## Error Handling

Empty input does not create anything. `Esc` closes the prompt without inserting rows.

If the amount cannot be parsed, the amount prompt stays open with the user's text and
shows the existing parse error status.

Selecting a series that is already present creates another occurrence. The user can remove
an unwanted occurrence with the existing remove/delete command.

## Testing

Run the Rust test suite. Manual verification should check:

- Plan editor `n` searches only the focused block's series.
- Plan editor no longer advertises or responds to `i`.
- Month dashboard `n` searches only the focused block's series.
- Selecting an existing series inserts a new occurrence of that series.
- Selecting the same existing series twice creates two occurrences with shared
  `series_id` and separate row ids.
- Creating from no match creates a series and then inserts it.
- Replace offers to keep items outside the target plan and handles kept/removed rows
  without orphaning manual envelope spending.
