# Storage & Sync Layout Cleanup

## Context

The Storage & Sync screen presents useful diagnostics, but its rows use hand-written spaces
instead of a shared column layout. The owner appears both inside the status and on a separate
row, the sync parent duplicates the more useful Leeway folder path, and the persistent status
is repeated in the footer.

## Layout

The storage details use one fixed-width, muted label column and one value column:

```text
Status           Read-only — Unnamed device is editing
Device           Nathans-MacBook-Pro
Local database   ~/Library/Application Support/Leeway/leeway.db
Leeway folder    ~/dropbox/Leeway
```

The status value uses a state-appropriate color. The separate Owner and Sync parent rows are
removed. Local-only mode omits the Leeway folder row. The provider-agnostic explanation stays
below the details with normal paragraph wrapping.

## Legacy Database Notice

When a legacy `./leeway.db` is present, it appears as a compact subsection with a highlighted
heading, an aligned Path row, and concise recovery guidance. It remains visually separate from
the active storage details.

## Footer

The screen body is the authoritative place for persistent sync state. The footer shows actions
and navigation normally, and replaces navigation only for transient confirmations or errors.
It does not repeat the persistent sync status.

## Testing

Rendering tests verify aligned labels and values, absence of Owner and Sync parent rows,
presence of the Leeway folder path, and safe rendering at narrow terminal widths.
