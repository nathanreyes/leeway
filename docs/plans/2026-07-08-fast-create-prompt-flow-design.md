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

- Open the label prompt with an empty input.
- Keep the pending item in modal state; do not write to SQLite yet.
- When the label prompt is submitted, require a non-empty label and open the amount
  prompt for the same pending item.
- Show the amount prompt with `0.00` prefilled but replacement-ready, so the first typed
  character clears the value.
- When the amount prompt is submitted with a valid amount, insert the row once using the
  collected label and amount.
- Select the newly inserted row after reload using the existing pending-selection fields.

`Esc` cancels the currently open prompt and drops the pending draft. Since nothing has
been inserted yet, canceling leaves no blank row behind.

Existing edit commands stay intact:

- `r` edits the selected label with the current value in the buffer.
- `a` edits the selected amount with the current value in the buffer.
- Only amount prompts reached from the new-item chain start replacement-ready.

## Data Flow

No schema changes are needed. The insert helpers continue to write `NOT NULL` labels and
amounts, but new-item prompts hold those values in memory until the amount is confirmed.

The text prompt state gains a small replacement mode. When active, the first printable
character replaces the current buffer instead of appending to it. Backspace or navigation
keys leave normal editing semantics.

Prompt submission gains draft-create variants that carry the target context:

- Dashboard transaction label -> dashboard transaction amount -> insert one-off txn.
- Dashboard envelope label -> dashboard envelope amount -> insert one-off envelope.
- Plan series label -> plan item amount -> insert recurring plan item.

## Testing

Run the Rust test suite after implementation. Manual verification should check:

- Dashboard `n` in Income opens a blank label prompt, then amount.
- Dashboard `n` in Expenses opens a blank label prompt, then amount.
- Dashboard `n` in Envelopes opens a blank label prompt, then amount.
- Plan editor `n` in each block follows the same flow.
- Typing in the chained amount prompt replaces `0.00` without backspacing.
- `Esc` from either prompt creates nothing.
- Existing `r` and `a` prompts still edit current values normally.
