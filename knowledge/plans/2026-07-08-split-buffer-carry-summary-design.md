# Split Buffer and Carry Summary Design

## Summary Line

For the current month, the Summary account-derived line should show the account
terms in the same order the math applies them:

```text
 funds - buffer - card debt + carry
```

Zero-valued `buffer` and `carry` terms are omitted. `funds` and `card debt`
remain visible because they are the primary account terms.

## Meaning

- `funds`: checking balances available before buffers
- `buffer`: checking carry balances held back from "what's left"
- `card debt`: credit-card owed balances
- `carry`: credit-card debt not planned for payoff this month

The headline formula remains unchanged:

```text
funds - buffer - card debt + carry + income left - bills left - envelopes
```

The change is to expose the two opposite carry-balance effects separately
instead of netting them into one `carry` number.
