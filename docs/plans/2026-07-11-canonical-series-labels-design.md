# Canonical Series Labels Design

## Goal

Give every recurring income, expense, and envelope one unambiguous name. A series owns
that canonical name; plans and stamped months display it consistently. The Dashboard
continues to edit month-specific financial state without also offering a second naming
surface.

## Product Rule

For a series-backed budget row, the visible label is live shared metadata rather than a
frozen part of the month snapshot:

```text
display label = current series.label, when the series still exists
                stored instance.label, otherwise
```

Renaming `Rent` to `Apartment Rent` on the Series page therefore updates the name shown
by every plan and stamped month. Amounts, settlement state, dates, envelope mode, envelope
period, and other month-owned values remain snapshots and do not change with the rename.

The stored `txn.label` and `envelope.label` columns remain. They preserve the label copied
at creation or stamp time and provide a fallback if a series is later deleted. They are no
longer an editable alias for a series-backed budget occurrence.

## Dashboard Interaction

Income, Expenses, and Envelopes display the effective label described above.

The Dashboard no longer advertises `l label` for those three blocks. Pressing `l` while a
series-backed budget row is selected does not rename it or navigate automatically. It
shows this guidance instead:

```text
Rename this item from its Series page — press S
```

`S` keeps its existing contextual behavior: it opens the selected row's Series detail.
The user can then press `l` on that page to rename the shared series. This makes Series the
only recurring-item naming surface and teaches the same navigation used for the series'
other shared fields and trends.

Other Dashboard behavior is unchanged:

- `l` still renames the selected account when Accounts is focused.
- Amount edits, settlement, deletion, envelope spending, and other month-specific actions
  continue to operate on the selected month occurrence.
- Transactions recorded inside an envelope keep their own editable descriptions. A
  spending event such as `Kroger` is not the recurring `Groceries` envelope series.

Legacy seriesless budget rows may continue to display and edit their stored label as a
compatibility behavior. The normal Dashboard add flow already creates or reuses a series,
so new top-level income, expense, and envelope rows follow the canonical rule.

## One-Time Items

Creating a new item from the Dashboard creates a backing series even when the user never
reuses it. A one-use series is acceptable: it gives the item the same navigation and data
rules as every other top-level budget row.

Series are cheap to create, but each one remains a distinct trend and search identity.
Users should create a new series for a genuinely distinct one-time item, not merely to add
a month-specific annotation to an existing recurring item. If that need emerges later, it
should use an explicit note or description field rather than a second kind of name.

## Data and Query Behavior

Month reads need access to the optional matching series label. This can be implemented by
joining `txn.series_id` and `envelope.series_id` to `series.id`, or by loading a compact
series-label map and resolving the effective label in the view layer. The effective label
must be used consistently for display and ordering.

No label cascade is required when a series is renamed. Existing instance labels stay
untouched as snapshots. If the series is deleted, its soft references remain and the
stored labels immediately become the display fallback, preserving readable history.

Restamping may continue refreshing stored instance labels from the current series along
with the other stamped fields. This updates the fallback snapshot without changing the
normal display rule.

## Verification

Add coverage for:

- a series rename appearing on the current Dashboard;
- the same rename appearing in previously stamped months;
- plans continuing to show the renamed series;
- a deleted series falling back to the stored instance label;
- ordering by the effective displayed label;
- `l` on a series-backed Dashboard row showing guidance without opening a prompt or
  changing data;
- contextual `S` still opening the selected Series detail;
- account label editing remaining available from the Dashboard;
- envelope-spending transaction descriptions remaining editable;
- legacy seriesless rows retaining a usable stored label; and
- amount, settlement, mode, period, and other stamped values remaining unchanged by a
  series rename.
