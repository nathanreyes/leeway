# Account Management Design

## Context

The dashboard already treats Accounts as a first-class panel in the month view.
Checking accounts store a spendable balance. Credit cards store a credit limit and
available credit, with owed debt derived as `credit_limit - available_credit`.
Carry balance exists on both account types, but it is an adjustment setting rather
than part of the account creation flow.

## Interaction Model

Account management lives in the main month view on the Accounts panel.

- `n` creates a new account.
- `Enter` edits the selected account's normal current figure:
  - checking: current balance
  - credit card: available credit
- `c` edits the selected account's carry balance.
- `l` edits a selected credit card's limit.
- `r` renames the selected account.
- `x` deletes the selected account after confirmation.

The `n` key remains context-sensitive. In budget panels it keeps its existing
"new budget item" behavior. In the Accounts panel it starts account creation.

## Account Creation

Pressing `n` on Accounts opens a small type picker:

- `h` creates a checking account.
- `c` creates a credit card.
- `Esc` cancels.

Checking creation prompts for:

1. account name
2. starting balance

Credit card creation prompts for:

1. account name
2. credit limit
3. available credit

New accounts do not ask for carry balance. They start with no carry balance,
which is treated as zero in the calculation. The user can set carry later with
`c` after selecting the account.

## Data Flow

Account creation inserts directly into the existing `account` table.

- checking: `type = checking`, `balance_cents = starting balance`,
  `credit_limit_cents = NULL`, `available_credit_cents = NULL`,
  `carry_balance_cents = NULL`
- credit card: `type = credit_card`, `balance_cents = 0`,
  `credit_limit_cents = entered limit`,
  `available_credit_cents = entered available credit`,
  `carry_balance_cents = NULL`

The dashboard reloads its `MonthView` after each prompt submission through the
existing event loop, so the account list and "what's left" calculation reflect
changes immediately.

## Error Handling

Empty names are ignored and leave the account uncreated. Invalid money input
shows the existing "couldn't read as an amount" status and keeps the user in the
same prompt when account creation is mid-flow. Deleting an account requires a
confirmation modal.

Deleting an account removes the account row. Existing transactions may reference
accounts, so deletion should either clear those references or be rejected with a
clear status if references exist. The implementation should choose the behavior
that best matches existing delete patterns, without silently destroying
transaction history.

## Testing

Unit tests should cover:

- creating a checking account with no carry balance
- creating a credit card with no carry balance
- editing carry balance changes "what's left" using the existing sign rules
- deleting an account does not corrupt transaction history
- dashboard key routing keeps `n` as budget-item creation outside Accounts and
  account creation inside Accounts
