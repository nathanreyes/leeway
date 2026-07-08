# Dashboard Account and Summary Layout Design

## Layout

The dashboard should use the extra horizontal space for accounts instead of keeping
Summary beside them.

- Top: full-width Accounts
- Middle: 50/50 split between the transaction column and Envelopes
- Bottom: full-width Summary

The transaction column keeps its existing vertical split: Income above Expenses.

## Account Rows

Accounts should render as one row per account now that the Accounts panel is full
width.

- Checking rows show the current balance.
- Credit card rows show owed, available credit, and credit limit.
- The final account column is reserved for the carry-style setting on every row:
  - checking: `buffer`
  - credit card: `carry`

Keeping `buffer` and `carry` in the same visual column makes the adjustment easy
to scan without hiding the type-specific meaning.

## Behavior

This is a layout and rendering change only. Account sorting, editing, carry math,
and account management key bindings remain unchanged.
