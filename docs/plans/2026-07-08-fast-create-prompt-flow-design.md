# Fast Create Prompt Flow Design

## Context

Creating income, expenses, and envelopes is too slow. Today `n` creates a row with a
default label, then the user presses `r` to rename it and `a` to set the amount. The
same friction exists in both places where budget items are created:

- Dashboard ad-hoc month items.
- Plan editor recurring income, expenses, and envelopes.

The app's design goal is low daily friction, so `n` should start the necessary edit flow
immediately.

## Design

Apply the same create flow everywhere `n` creates income, expenses, or envelopes:

- Insert the new row immediately with an empty label and a zero amount.
- Select the row after reload using the existing pending-selection fields.
- Open the label prompt with an empty input.
- When the label prompt is submitted, save the label if it is non-empty.
- Immediately open the amount prompt for the same row.
- Show the amount prompt with `0.00` prefilled but replacement-ready, so the first typed
  character clears the value.

`Esc` cancels only the currently open prompt. Since creation still happens before input,
canceling leaves the new row in place, matching the current create-first behavior.

Existing edit commands stay intact:

- `r` edits the selected label with the current value in the buffer.
- `a` edits the selected amount with the current value in the buffer.
- Only amount prompts reached from the new-item chain start replacement-ready.

## Data Flow

No schema changes are needed. The insert helpers continue to write `NOT NULL` labels and
amounts. New rows use an empty string for the initial label and `Money::ZERO` for the
amount.

The text prompt state gains a small replacement mode. When active, the first printable
character replaces the current buffer instead of appending to it. Backspace or navigation
keys leave normal editing semantics.

Prompt submission gains create-chain variants that carry the created row id:

- Dashboard transaction label -> dashboard transaction amount.
- Dashboard envelope label -> dashboard envelope amount.
- Plan series label -> plan item amount.

## Testing

Run the Rust test suite after implementation. Manual verification should check:

- Dashboard `n` in Income opens a blank label prompt, then amount.
- Dashboard `n` in Expenses opens a blank label prompt, then amount.
- Dashboard `n` in Envelopes opens a blank label prompt, then amount.
- Plan editor `n` in each block follows the same flow.
- Typing in the chained amount prompt replaces `0.00` without backspacing.
- Existing `r` and `a` prompts still edit current values normally.
