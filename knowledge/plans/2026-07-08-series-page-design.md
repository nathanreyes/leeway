# Series Page Design

Status: Approved design.

## Context

Series are the durable identities behind recurring income, expenses, and envelopes.
Plans and stamped months use those identities, but amounts belong to plan items or month
instances. The Series page should make those recurring identities easy to find, inspect,
and maintain while also answering the trend question: how has this recurring thing moved
over time?

The page is a top-level TUI screen alongside Dashboard and Plans. It is not a plan editor
and not a month editor.

## Goals

- Show every durable series grouped as Income, Expenses, and Envelopes.
- Let the user search quickly across series labels and categories.
- Show min, max, average, latest, planned average, and average delta for the selected
  series.
- Render a real time-series chart with months on the x-axis and amount on the y-axis.
- Let the user change the chart/stat time range.
- Manage shared series definition fields only: label, category, envelope mode, and
  envelope period.

## Non-Goals

- Editing plan amounts.
- Editing stamped month actuals or forecasts.
- Adding a series occurrence to a plan or month.
- Restamping months.
- Deleting series in the first version.

Deletion is intentionally deferred because a series is also a trend identity. Past months
can retain soft `series_id` values after a series definition is removed, so delete needs a
separate warning and historical-display policy.

## Layout

Use the same three-band structure as the existing screens:

- Header
- Body
- Footer

The page opens globally with `S`. Existing lowercase `s` actions remain unchanged on
Dashboard and Plans.

### Header

The header shows the page name, visible count, and active time range:

```text
Series - 24 recurring items - Last 12 stamped months
```

When search is active, the count reflects filtered results.

### Body

The body is a two-column split.

Left column, roughly 40% width: grouped series index with a search field at the top.

```text
Search: elec

Expenses
  Electric                         avg $142
```

With no search text, all groups are shown:

```text
Income
  Paycheck                         avg $4,200
  Freelance                        avg   $600

Expenses
  Rent                             avg $1,850
  Electric                         avg   $142

Envelopes
  Groceries              monthly   avg   $780
  Dining                 manual    avg   $310
```

Right column, roughly 60% width: selected-series detail.

```text
Rent                                      expense
category: Housing          used in plans: Normal, Tight
latest: $1,875             avg: $1,842
min:    $1,800             max: $1,875
planned avg: $1,825        avg delta: +$17

Amount - Last 12 stamped months
<ratatui Chart with month x-axis and amount y-axis>

Current month
1 occurrence      amount $1,875      settled
```

The detail pane updates as the highlighted result changes, including while searching.

### Chart

Use Ratatui's built-in `Chart` widget, not horizontal bars. The local dependency is
`ratatui = 0.30.2`, which supports cartesian axes, explicit bounds, sparse labels, and
multiple `Dataset`s.

Chart behavior:

- X-axis is months in the selected range.
- Y-axis is amount in dollars.
- Primary dataset is effective monthly total from `amount_cents`.
- Planned comparison dataset uses `stamped_amount_cents` where present.
- Use `GraphType::Line` with `Marker::Braille` for the primary line.
- Render the planned line in dim gray.
- Use sparse start/middle/end labels for both axes because Ratatui axis labels are
  unreliable with more than three labels.
- Recompute y-axis bounds for the selected series and selected range.
- Add padding around min/max so flat series still render visibly.
- Missing months are gaps, not zeroes. If a single dataset cannot express gaps, split the
  line into contiguous datasets internally.

## Search

`/` focuses the search field in the left sidebar.

Behavior:

- Typing filters series live across all groups.
- Search matches label and category.
- Empty search shows the full grouped list.
- Group headers are shown only when that group has visible matches.
- `j/k` and arrow keys move through filtered results while search is active.
- `Enter` exits the search field and keeps the highlighted result selected.
- `Esc` clears search if search has text; otherwise it leaves the Series page.
- If no rows match, show `No matching series`.

## Time Ranges

`t` opens a choice modal for the time range.

Initial options:

- Last 12 stamped months
- This year, based on today's local calendar year
- Last year
- All history

The selected range controls the chart and all displayed stats unless a stat explicitly
says otherwise. The first version should show range-scoped stats only. If lifetime or
overall stats are added later, label them directly, for example `lifetime avg`.

## Stats Semantics

All stats are computed from monthly totals in the selected time range:

- `latest`
- `min`
- `max`
- `avg`
- `planned avg`
- `avg delta`
- occurrence count
- chart y-axis bounds

Rules:

- A monthly point is the sum of all matching occurrences for that series in that month.
- `latest` is the last month in the range that has data for the selected series.
- `avg` is the average of months with data, not missing months counted as zero.
- `min` and `max` use months with data.
- `planned avg` uses months with planned data.
- `avg delta` is effective average minus planned average over months where planned data
  exists.
- If the selected range has no data for the selected series, show muted placeholders and
  `No trend data in this range`.

## Data Model And Queries

The page needs a read model rather than schema changes.

Suggested read-model pieces:

- All durable `series` rows, ordered by group and label.
- Plan usage for each series: plan names that currently reference it.
- Month axis for the selected time range.
- Trend points aggregated by selected `series_id`.
- Current-month occurrence summary for the selected `series_id`.

Aggregation:

- Standalone transactions match `txn.series_id = :series_id` and
  `txn.envelope_id IS NULL`.
- Envelopes match `envelope.series_id = :series_id`.
- Repeated occurrences in the same month are summed into one point.
- Transactions use `amount_cents` as effective actual/forecast.
- Envelopes use `amount_cents` as effective monthly budget.
- Transactions use `stamped_amount_cents` as planned where present.
- Envelopes use `stamped_amount_cents` as planned.
- Months with no matching row are missing data, not zero.

Historical rows whose `series_id` no longer has a matching `series` definition are out of
scope for the first version because series deletion is also out of scope.

## Interaction

Navigation:

- `S`: open Series screen globally.
- `Esc`: leave Series screen, or clear active search text first.
- `j/k` or arrow keys: move through visible series rows.
- `/`: search.
- `t`: choose time range.
- `d`: dashboard.
- `P`: plans.
- `q`: quit.

Management:

- `r`: rename selected series.
- `c`: edit category.
- `m`: toggle envelope mode, only for envelope series.
- `p`: cycle envelope period, only for envelope series.

The important product rule is that time range owns `t` on the Series page. Plans navigation
uses uppercase `P` on this screen to avoid conflicting with envelope period edits.

## Error Handling

- Empty rename/category submits should be rejected or leave the existing value unchanged,
  following existing prompt conventions.
- If no series exist, show an empty-state sidebar and a blank detail pane.
- If a selected series disappears after editing or external database changes, clamp
  selection to the nearest visible row.
- If the selected time range has no stamped months, render an empty chart block and a
  muted `No stamped months in this range` message.
- If the selected range has stamped months but no data for the selected series, render
  `No trend data in this range`.

## Testing

Unit/query tests should cover:

- Series grouping into Income, Expenses, and Envelopes.
- Search matching by label and category.
- Monthly aggregation of repeated occurrences.
- Effective totals from `amount_cents`.
- Planned totals from `stamped_amount_cents`.
- Range-scoped min/max/avg/latest.
- Missing months excluded from averages and represented as chart gaps.
- Time range month-axis selection for last 12 stamped months, this year, last year, and
  all history.

UI-level tests should cover:

- `S` opens the Series screen without breaking existing lowercase `s` actions.
- `/` filters the sidebar and updates detail selection.
- `t` changes the range used by both stats and chart.
- Envelope-only actions do nothing or show status on transaction series.
