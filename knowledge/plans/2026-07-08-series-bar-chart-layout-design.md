# Series Bar Chart Layout Adjustment

Status: Approved design adjustment.

## Context

The first Series page implementation used a line chart for monthly series values. That
reads too continuous for data that is actually one monthly total per stamped month.

## Design

Change the selected-series detail pane to put the chart first, then summary, then current
month.

```text
Amount - Last 12 stamped months
<monthly bar chart with amount y-axis and month x-axis>
Aug Sep Oct Nov Dec Jan Feb Mar Apr May Jun Jul

Summary
Rent                                      expense
category: Housing          used in plans: Normal, Tight
latest: $1,875             avg: $1,842
min:    $1,800             max: $1,875
planned avg: $1,825        avg delta: +$17

Current month
1 occurrence      amount $1,875      settled
```

Use Ratatui `Chart` with `GraphType::Bar`, preserving the real x/y axis model. Keep sparse
axis labels on the built-in chart, then render a compact month-label row below the chart
when there is room.

Do not overlay planned values in the chart for this pass. Planned comparison remains in
summary stats through `planned avg` and `avg delta`.

## Testing

Run the Rust test suite. Manual verification should check that:

- The chart appears above the summary.
- Effective values render as monthly bars.
- The summary still reflects the selected time range.
- Month labels remain readable at typical terminal widths.
