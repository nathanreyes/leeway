# Income Auto-Collapse Design

## Context

The dashboard and plan editor now group budget items into `Income`, `Expenses`, and
`Envelopes`. Income is expected to have fewer rows than expenses, so a 50/50 split in the
left column gives income too much space and compresses the more active expense block.

## Design

Apply the same auto-collapse rule to both screens:

- The income block height is based on its row count.
- Height formula: `clamp(income_count + 2, 3, 7)`.
- Expenses receives the remaining height in the left column.
- Income remains focusable even when empty, so `Tab` can land there and `n` still creates
  a new income item.

The `+ 2` accounts for the bordered block. The minimum of 3 keeps an empty income block
visible and selectable. The maximum of 7 prevents several income rows from crowding out
expenses.

## Data Flow

No schema or read-model changes are required. Each screen already filters its own income
rows:

- Dashboard: standalone transactions with `Direction::In`.
- Plan editor: transaction plan entries with `Direction::In`.

The layout layer counts those rows and chooses vertical constraints before rendering the
existing list widgets.

## Testing

Run the Rust test suite after implementation. Manual UI checks:

- Empty income still shows a compact focusable block.
- One or two income rows leave most of the left column for expenses.
- More than five income rows cap the income block at seven terminal rows.
- `n` still creates income while the compact income block is focused.
