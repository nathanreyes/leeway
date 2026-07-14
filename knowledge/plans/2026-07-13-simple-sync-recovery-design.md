# Simple Folder Sync Recovery

## Context

Folder sync currently exposes its internal revision graph to the user. A normal Storage
screen shows an opaque revision UUID and offers manual publish, takeover, disable, and
conflict actions at the same time. Manual publication creates a new full database snapshot
even when nothing changed, while ordinary retention keeps twenty snapshots. When the
synchronized head changes, the UI reports an "unexpected synchronized revision" without
explaining the decision the user must make.

The immutable snapshot protocol remains useful for keeping the live SQLite database out of
the provider-managed folder. This change simplifies the user contract without replacing
that safety boundary.

## User Contract

Folder sync has four ordinary user-facing states:

- **On this computer** — folder sync is disabled.
- **Up to date** / **Updating folder** — changes are published automatically.
- **View only** — another active Leeway session owns editing.
- **Choose a version** — both this computer and the synchronized folder may contain work.

The Storage screen never displays a revision identifier. In the choice state it says that
changes were found in both places and offers only the two decisions that resolve it:
**use synced folder** or **use this computer**. The selected copy becomes current and the
other copy is preserved. Provider conflicts, corrupt files, and unavailable folders remain
distinct **Sync paused** errors and do not offer version-selection actions that cannot fix
them.

Publication remains automatic. The manual publish action is removed because it creates
duplicate snapshots and implies that users must manage synchronization themselves.
Takeover is shown only while Leeway is view-only, and disable remains available as an exit
from folder sync.

## Snapshot Retention

Keep the current ordinary revision and one ordinary fallback. Conflict candidates explicitly
marked for recovery remain protected. Successful publication and conflict resolution both
run pruning, so an existing folder converges to the smaller history without a separate
cleanup command.

The current on-disk protocol and layout remain compatible. This avoids migrating or
rewriting an existing synchronized budget merely to simplify its presentation.

## Race Handling

Publishing advances `head.json` before the background worker can report completion to the
UI thread. The watcher must recognize a head written by the current device and session while
publication is still in flight and wait for the worker result instead of reporting a false
version choice.

## Testing

Tests cover the user-facing status labels, explicit version-choice classification, omission
of revision UUIDs and irrelevant actions from the Storage screen, current-publication watcher
behavior, conflict-only recovery actions, and two-revision ordinary retention.
