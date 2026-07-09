# UI Hotkey Labels Design

## Goal

Make keyboard shortcuts describe the thing being edited when they change a
property, and describe the action when they perform one.

## Key map

- `l` edits a label everywhere it is available. `r` has no action.
- `a` edits an amount.
- `x` deletes everywhere, including envelope-detail transactions.
- `n` creates a new item; action-specific keys such as `s` for recording a
  transaction remain unchanged.

The Accounts panel uses lowercase `l` for its account label and uppercase `L`
for a credit-card limit, avoiding the existing collision while retaining each
property's initial.

## UI copy and documentation

Visible hints and prompts use `label` rather than a mixture of `rename` and
`name` for the editable display label. The envelope-detail footer shortens
`delete envelope` to `delete`, because the screen already establishes the
object. The README key reference follows the same map.

## Verification

Update focused keyboard tests so they exercise `l`, assert that `r` is inert,
and cover the transaction-detail `x` delete mapping. Run the Rust test suite
and formatter.
