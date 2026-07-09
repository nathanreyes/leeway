# Envelope detail focus design

## Goal

Make the envelope detail modal easier to scan by separating envelope-level details
from its transaction record. Each section owns its keyboard controls, so the modal
only shows the actions that apply to the current focus.

## Interaction

The modal has two focusable sections:

1. **Details** is the initial focus. It presents mode, cadence, entered amount,
   monthly amount, consumed amount, and remaining amount in aligned fields.
   Its controls are rename, amount, mode, period, and delete envelope.
2. **Transactions** contains the envelope's recorded transactions. Its controls
   are move selection, edit amount, edit label, delete transaction, and add a
   transaction.

`Tab` and `Shift+Tab` switch focus between the sections. `j`/`k` move a selected
transaction only when Transactions is focused. The active section receives the
accent border and its command hint row; the inactive section remains legible but
quiet.

Transactions can be created, edited, and deleted for envelopes in either mode.
For automatic envelopes they are a record only: time-based envelope consumption
and the dashboard's "what's left" calculation do not use them. For manual
envelopes they continue to determine consumption.

## Implementation

Add a focus enum to `EnvelopeDetail`, route modal keys by that focus, and retain
the focus state whenever a prompt or confirmation restores the detail modal.
Split rendering into a compact Details panel, a Transactions panel, and a small
global navigation/close footer. Use aligned rows and right-aligned money values
for a clear read path, and include an explicit empty state in the transaction
panel.

## Verification

Add tests for focus cycling, details-only and transactions-only shortcuts,
automatic-envelope transaction creation, and restoring the focused detail modal
after nested prompts or confirmations. Run the Rust test suite and format the
affected files.
