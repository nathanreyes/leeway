# Envelope detail screen design

## Goal

Make the envelope detail view easier to scan by separating envelope-level details
from its transaction record. It is a drill-in screen rather than a modal so the
dashboard does not compete for attention. Each section owns its keyboard controls,
so the screen only shows the actions that apply to the current focus.

## Interaction

The screen has two focusable sections:

1. **Details** is the initial focus. It presents mode, cadence, entered amount,
   monthly amount, consumed amount, and remaining amount in aligned fields.
   Its controls are rename, amount, mode, period, and delete envelope.
2. **Transactions** contains the envelope's recorded transactions. Its controls
   are move selection, edit amount, edit label, delete transaction, and add a
   transaction.

`Enter` opens the selected envelope from the dashboard; `e` has no envelope-specific
meaning. `Tab` and `Shift+Tab` switch focus between the sections. `j`/`k` move a
selected transaction only when Transactions is focused. The active section receives
the accent border and its command hint row; the inactive section remains legible but
quiet. `Esc` returns to the dashboard with the selected envelope preserved.

Transactions can be created, edited, and deleted for envelopes in either mode.
For automatic envelopes they are a record only: time-based envelope consumption
and the dashboard's "what's left" calculation do not use them. For manual
envelopes they continue to determine consumption.

## Implementation

Add a drill-in `EnvelopeDetail` screen that carries the selected envelope and focus
state. Route its keys by focus, and retain that state whenever a prompt or
confirmation returns to the screen. Split rendering into a compact Details panel,
a Transactions panel, and a small global navigation/back footer. Use aligned rows
and right-aligned money values for a clear read path, and include an explicit empty
state in the transaction panel.

## Verification

Add tests for focus cycling, details-only and transactions-only shortcuts,
automatic-envelope transaction creation, screen entry with `Enter`, screen exit
with `Esc`, and restoring the focused detail screen after nested prompts or
confirmations. Run the Rust test suite and format the affected files.
