# Reliability and Query Performance Design

## Context

The audit identified unsafe money parsing, non-reversible daily/monthly plan conversion,
weak envelope-spending integrity, repeated read queries in the Series screen, repeated
transaction cloning in the dashboard, missing query indexes, and unfinished quality gates.

Ballpark remains a local-first, single-user budgeting application. Changes must preserve
its integer-cent money model and the snapshot semantics of stamped months.

## Decisions

### Money input and arithmetic

User-entered amounts will be parsed from decimal text directly to integer cents. Inputs
must be finite, in range, and have at most the supported cent precision; invalid input
will be rejected rather than coerced. Money arithmetic used by input-dependent paths will
be checked so an invalidly large value cannot wrap or panic.

### Daily and monthly plan amounts

Changing a plan envelope series from monthly to daily will convert its amount to the
nearest cent per day using a 30-day month. Changing it back will multiply that daily rate
by 30. This is intentionally lossy for totals not divisible by 30 cents; the displayed
status will continue to say that the conversion uses a 30-day basis. Regression tests will
make this product rule explicit.

### Envelope spending integrity

`add_envelope_spending` will insert only when the envelope belongs to the supplied month
and uses manual mode. A database trigger will reject inserts or updates that associate a
transaction with an envelope from a different month, protecting callers outside the TUI.

### Read-model performance

The Series screen will obtain all transaction and envelope trend aggregates in two grouped
queries keyed by series and month, plus one query for plan memberships. The view layer will
map those results onto the selected month axis instead of loading every month's full rows
for each series.

The dashboard will group a month's envelope transactions once and pass borrowed groups only
to manual envelopes. Automatic envelopes will not allocate or clone spending rows they do
not use.

### Storage and quality gates

A forward migration will add lookup indexes and a unique month-label index. It will be
safe for both existing and newly created databases. Formatting and Clippy warnings will be
fixed so `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass.

## Testing

- Reject non-finite, malformed, and overflowing money input.
- Verify the chosen 30-day rounding behavior for a non-divisible monthly amount.
- Reject cross-month and automatic-envelope spending.
- Preserve existing Series totals and range behavior using grouped query results.
- Preserve manual-envelope consumption and automatic-envelope accrual on the dashboard.
- Run formatting, all test targets, and Clippy with warnings denied.
