# Contextual Series Detail Design

## Goal

Make the global `S` shortcut context-aware. When a dashboard or plan-editor row
represents a series, `S` should open that series directly for trend inspection and
shared-definition edits. The complete Series list remains available when there is
no contextual series or when the user deliberately expands from the detail view.

## Interaction

The Series screen has two modes:

1. **Detail** shows one series using the existing chart, range-scoped aggregate
   summary, plan membership, and current-month summary. It supports the existing
   series-level edit commands: `l` for label, `m` for envelope mode, `p` for
   envelope cadence, and `t` for trend range.
2. **List** is the existing grouped, searchable Series workspace. It retains
   list-wide management commands such as search, filter, create, and delete.

On the Dashboard or in a Plan editor, `S` opens Detail when the focused budget row
has a `series_id`. Dashboard header and account focus, an unstamped month, the Plans
list, and a legacy seriesless row open List instead. Pressing `S` from Detail
promotes the view to List with the same series selected. If an existing search or
membership filter hides that series, clear only the conflicting state so the target
is visible. `S` in List is a no-op.

Both modes remember where the Series workflow began. `Esc` from either Detail or
List returns to that origin with its existing focus and selection preserved. An
explicit global jump such as `P` leaves the workflow and discards the origin.

## Navigation State

Represent this as one origin-aware Series screen rather than a modal over another
screen:

```rust
SeriesScreen {
    mode: SeriesMode,
    origin: SeriesOrigin,
}

enum SeriesMode {
    Detail { series_id: String },
    List,
}

enum SeriesOrigin {
    Dashboard,
    Plans,
    PlanEditor { plan_id: String },
    EnvelopeDetail { detail: EnvelopeDetail },
}
```

`SeriesOrigin` is deliberately a typed return address, not a general history stack.
The dashboard's period and selections and the plan editor's block selections already
live in `App`; the origin only carries the identity required to restore the screen.
If an originating plan or envelope disappears while Series is open, use the existing
missing-record fallback to return to its parent screen.

This approach remains appropriately small while Series is the only contextual
cross-screen drill-in. If another feature needs nested origins or multi-step back
history, replace the typed return address with a general navigation abstraction
rather than adding recursive or increasingly specific origin variants.

## Presentation

Detail uses the full Series screen rather than drawing the originating page beneath
it. Render one prominent detail workspace using the existing chart and summary
components, with a header that names the series and active time range. Its footer
shows only applicable edit commands on the left and `S` for all series, `Esc` for
back, and the normal global commands on the right. Empty trend and current-month
states retain the existing Series messages.

List rendering and interaction remain unchanged apart from origin-aware `Esc` and
contextual initial selection.

## Data Flow and Editing

The contextual source supplies only a durable series id. Build the existing
`SeriesPageView` for the active range and find the matching `SeriesDetailView`; no
new database query or schema change is required. Editing continues through the
existing operations and prompt/choice modal system. Because Detail is a screen mode,
nested prompts naturally return to the same Series state without adding a second
modal layer.

If the target series no longer exists, show a short status message and return to the
recorded origin. Transaction-only mode and cadence commands keep the existing
informational status messages.

## Verification

Add focused tests for:

- contextual `S` from selected dashboard income, expense, and envelope rows;
- contextual `S` from each Plan editor block;
- List fallback from dashboard header/accounts, Plans list, and seriesless rows;
- `Esc` restoring Dashboard, Plans, Plan editor, and envelope-detail origins;
- `S` promoting Detail to List with the same series visible and selected;
- a conflicting Series search or membership filter being relaxed on promotion;
- missing target and missing-origin fallbacks;
- label, mode, cadence, and range actions in Detail;
- unchanged lowercase `s` actions and global `P`, `h`, and `q` behavior.

Run `cargo fmt --check`, the Rust test suite, and targeted terminal-buffer rendering
tests for both Series modes.
