# Off-Month Accounts Explainer Design

## Behavior

The Accounts panel stays visible for every stamped month, but it only lists real
account balances for the current calendar month.

For past or future months:

- do not render account rows
- render explanatory copy inside the Accounts panel
- skip Accounts in the dashboard focus cycle
- ignore account management actions because the panel is not focusable

## Explainer Copy

```text
Account balances only apply to the current month.

Past and future months are shown as their own plan snapshots:
income left - bills left - envelopes left.

Use the current month to update real checking balances,
card debt, buffers, and carry balances.
```

## Rationale

The app already excludes account balances from off-month "what's left" math.
Keeping the panel visible but explanatory makes that rule clear without implying
that historical or future periods have their own editable account state.
