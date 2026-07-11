# Plan Details Summary Design

## Context

The dashboard answers "what's left?" with a full-width Summary panel, but Plan Details
only shows the plan's Income, Expenses, and Envelopes blocks. A user comparing planned
scenarios must currently add those commitments mentally.

Plan Details should expose the same decision-making headline without pulling live account
state into a reusable template.

## Design

Add a full-width `Summary` panel at the bottom of Plan Details, below the existing budget
blocks. Keep the existing `Income`, `Expenses`, and `Envelopes` block headers. The summary
uses matching plan terminology:

```text
+ $10,000.00  planned income   − $4,318.00  planned expenses
−  $1,200.00  planned envelopes

=  $4,482.00  what's left
   Daily envelope rates assume a 30-day month.
```

Use the dashboard Summary's colors, amount alignment, border title, and bold result so the
two screens share a visual language. The Plan Details summary itself is static; dashboard
animation behavior and dashboard wording are outside this change.

## Calculation

Calculate a plan projection from its entries:

- Planned income is the sum of transaction entries whose direction is `In`.
- Planned expenses is the sum of transaction entries whose direction is `Out`.
- Monthly envelopes contribute their stored plan amount.
- Daily envelopes contribute their stored daily rate multiplied by 30.
- What's left is planned income minus planned expenses minus planned envelopes.

Do not include account balances, checking buffers, card debt, card carry, settlement, or
calendar progress. Those belong to a concrete month, while a plan is a reusable scenario.
An empty plan renders zero for every total.

Put the calculation in a pure domain helper so UI rendering does not own financial logic
and other plan views can reuse it later. No schema or query changes are required.

## Layout and Behavior

Reserve seven terminal rows for the Summary panel, matching the dashboard panel's overall
height. Its five inner lines contain the income/expenses row, the envelopes row, a blank
separator, the bold result, and the dim 30-day footnote.

The existing plan blocks keep their layout, selection state, and keyboard behavior. Since
Plan Details reloads entries each event-loop iteration, the derived summary updates after
an item is added, removed, or its amount changes without additional application state.

## Testing

Add unit coverage for:

- Income and expense classification.
- Monthly envelope totals.
- Daily envelope rates monthlyized with the 30-day assumption.
- Mixed-entry arithmetic.
- Empty plans and negative results.

Run formatting and the full Rust test suite. Manually verify that the summary fits the
Plan Details layout, uses `Expenses` terminology, and leaves the dashboard unchanged.
