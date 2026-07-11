# Provider-Agnostic Folder Sync Design

## Context

Leeway currently opens `./leeway.db` for the lifetime of the process. This makes the
budget portable as a file, but the active budget depends on the shell's working directory
and cannot move safely between computers through Dropbox, iCloud Drive, OneDrive,
Syncthing, or a similar folder-sync service.

Opening a live SQLite database inside a synchronized folder is not a safe foundation.
SQLite's locks only coordinate processes that see the same filesystem, while a sync client
copies files asynchronously and may observe the database and its journal at different
moments. A provider-agnostic filesystem also cannot report whether it has finished
uploading or downloading.

The desired experience is local-first and automatic for a single active Leeway instance.
The application should preserve conflicts and help the user recover from them, but it does
not promise concurrent or offline editing on multiple computers.

## Goals

- Let a user enable sync from inside Leeway by choosing any synchronized folder.
- Keep normal reads and writes on a private local SQLite database.
- Publish consistent database snapshots automatically after changes.
- Make moving from one computer to another require no manual file copying.
- Detect incompatible versions, stale ownership, divergent revisions, corrupt snapshots,
  and provider-created conflict files before overwriting data.
- Preserve every candidate involved in a conflict and provide an explicit resolution flow.
- Import existing `./leeway.db` installations into a stable managed location.

## Non-Goals

- Concurrent multi-device editing or record-level merge.
- A hard distributed lock or proof that an external provider is fully synchronized.
- Dropbox-, iCloud-, OneDrive-, or Syncthing-specific APIs or status integration.
- Editing while the configured sync folder is unavailable or known to be stale.
- Leeway-managed encryption in the first version. Local and synchronized databases remain
  plain SQLite files and rely on the provider and operating system for storage security.
- Multiple named budgets. This design manages Leeway's single active budget.

## Considered Approaches

### Managed local database with synchronized snapshots

Leeway edits a private local database and publishes validated snapshots plus revision
metadata into a synchronized directory. This duplicates the current database, but keeps
SQLite off the synchronization boundary and enables validation, history, conflict
detection, and recovery. This is the selected approach.

### Open the database directly in the synchronized folder

This has the smallest implementation and only one database file. It exposes a database
being actively modified, provides no cross-device locking, and gives weak recovery when a
provider creates a conflicted copy. The safety tradeoff is unacceptable for budget data.

### Synchronize and merge an operation log

An append-only operation log could support concurrent offline editing. It would require
tombstones, ordering rules, schema-wide merge semantics, and user-facing resolution for
financial conflicts. That complexity is not justified by the single-active-instance goal.

## Storage Model

Every computer keeps its working state in the operating system's standard per-user Leeway
app-data directory:

```text
Leeway/
  leeway.db
  config.json
  device.json
  recovery/
```

`device.json` contains a stable random device ID and a user-facing device label. The
configuration records local-only or synchronized mode, the selected sync root, and the
last accepted revision. Recovery databases are never used as the live connection.

The selected provider folder contains a dedicated Leeway directory:

```text
Leeway/
  sync-root.json
  head.json
  lease.json
  revisions/<revision-id>.json
  snapshots/<revision-id>.db
```

`sync-root.json` identifies the budget and sync protocol. `head.json` points to the
selected current revision. A revision descriptor records its ID, parent revision or
resolution parents, device and session IDs, publication time, Leeway version, schema
version, snapshot name, byte length, and digest. Revision descriptors and database
snapshots are immutable after publication.

Leeway retains a bounded history along the accepted linear branch. Revisions participating
in an unresolved conflict and the unchosen side of a resolved conflict remain protected
until the user explicitly removes them. The exact ordinary retention count is an
implementation constant rather than a user setting in the first version.

## In-App Setup

An application-wide Settings command, documented in the footer and help, opens a
**Storage & Sync** screen. In local-only mode it shows the local database location and an
**Enable folder sync** action.

The user pastes or types the path to a synchronized parent directory. Leeway expands the
platform's normal home-directory syntax, validates read and write access, and creates or
recognizes the dedicated `Leeway/` child directory.

If no synchronized budget exists, Leeway creates the sync root and publishes the current
local budget as revision 1. If a valid budget already exists, the screen shows its revision,
publisher, publication time, schema version, and integrity status. The default action is
**Use synced budget**; Leeway archives the current local database before importing it.
**Replace synced budget with this computer's budget** remains available behind an explicit
destructive confirmation and also preserves the previous synchronized head.

The screen uses `Published`, `Publishing`, `Attention needed`, and `Local only`. It never
claims that a file is "cloud synced," because provider completion is not observable through
a generic filesystem.

Disabling sync keeps the current local database active and leaves the synchronized folder
unchanged. Removing synchronized data is a separate explicit action.

## Existing Database Migration

New installations create `leeway.db` in the managed app-data directory instead of the
current working directory. On upgrade, if the managed database is absent and
`./leeway.db` exists, Leeway offers to import it. The original file is preserved as a
backup and the UI explains the new managed location.

Leeway must not silently combine or select between a managed database and a different
`./leeway.db`. If both exist, it uses the managed database and directs the user to the
Storage & Sync screen to inspect or import the other file deliberately.

## Session Ownership and Handoff

The synchronized budget has a cooperative lease containing the device and session IDs,
base revision, acquisition and heartbeat times, expiry, and release state. A session must
acquire the lease before editing. The application refreshes it while open and publishes a
released state after its final successful publication on clean shutdown.

On launch, Leeway:

1. Reads the sync root, head, lease, immutable revision metadata, and possible provider
   conflict artifacts.
2. Verifies sync-protocol and database-schema compatibility.
3. Validates the referenced snapshot's existence, length, digest, and SQLite integrity.
4. If the synchronized revision is newer and local work is fully published, archives the
   local database and imports the new snapshot.
5. Acquires a session lease before enabling edits.

If another device has an active lease, Leeway opens the budget read-only, identifies that
device, and periodically retries. A clearly stale lease can be replaced only with a
**Take over editing** confirmation. Taking over does not delete the other device's
snapshot.

On clean quit, Leeway completes a pending publication before releasing the lease. The user
may then wait for their provider and open Leeway on another computer. Leeway cannot prove
that this handoff has propagated; the lease is a guardrail and diagnostic, not a hard
distributed lock.

## Publication Data Flow

Application writes continue to use ordinary SQLite transactions against the local
database. After a transaction commits, the sync state machine marks the local generation
dirty and schedules publication after a short debounce. Sync is automatic; "background"
means it does not require a user command or block ordinary UI interaction.

A serialized publisher performs these steps:

1. Re-read the head and lease. Stop if the session no longer owns the lease or the current
   head is not the publication's expected parent.
2. Use SQLite's backup API to create a transactionally consistent snapshot in a temporary
   file. Never copy the bytes of the open working database directly.
3. Verify the expected schema and run an SQLite integrity check.
4. Compute the snapshot's byte length and digest.
5. Atomically install the immutable snapshot within the synchronized directory.
6. Atomically install its immutable revision descriptor.
7. Recheck the expected head and lease, then advance `head.json`.
8. Mark the local generation published only after the new head can be read and validated.

A consumer may observe these files out of order because the provider transfers them
independently. It therefore waits when a head references a missing or incomplete snapshot;
it never silently substitutes an older revision. The previous head and snapshot remain
valid throughout publication.

If another local edit commits while a publication is running, it increments the local
generation. Completion of the older publication immediately queues the newer one rather
than incorrectly marking all work published.

The application watches the head and lease while open. An unexpected change suspends
editing and publication immediately and enters conflict handling.

## Schema and Protocol Compatibility

The sync protocol version and SQLite schema version are separate. A publisher records both
along with its Leeway application version.

A newer Leeway version acquires ownership and archives the pre-migration local database
before applying migrations. It validates and publishes the migrated database as a new
revision. Migration is one-way; the synchronized snapshot is never migrated in place.

An older application must reject a snapshot whose schema version exceeds its compiled
maximum, even when the migration appears additive. It keeps its local database unchanged,
disables publication, and reports the minimum action clearly, for example:

```text
This budget was updated by Leeway 0.x.y. Upgrade Leeway on this computer to continue.
```

It likewise refuses an unsupported sync-protocol version. Every sync-aware version checks
the expected parent again before publication, preventing an older local database from
replacing a newer revision it has observed.

## Conflicts and Resolution

A conflict exists when Leeway finds multiple revisions descended from the same parent, an
unexpected valid head, a provider-created conflicting manifest, or unpublished local work
whose base revision is no longer current. The dedicated folder allows Leeway to scan and
parse renamed provider artifacts without deleting them.

Leeway does not merge rows. The conflict screen presents each candidate's device,
publication time, application and schema versions, integrity result, revision ancestry,
and a small read-only budget summary. The actions are:

- **Use synchronized version**
- **Publish this computer's version**
- **Keep both and decide later**

Choosing a version creates a resolution revision that records both conflicting revisions
as parents. Its database content comes from the chosen candidate. The unchosen database is
retained as a protected recovery snapshot so resolution never destroys the alternative.

Provider-created conflict files are preserved. Once their content has been represented by
an immutable revision, Leeway may label them as handled in its own metadata, but it does
not rename or delete the provider's artifact automatically.

## Failure Behavior

- If the sync folder disappears or becomes unreadable or unwritable, Leeway preserves any
  committed local change, pauses further editing, and continues in read-only mode.
- If publication fails, the status is **Saved locally — not published**. The failed local
  generation remains recoverable and Leeway never advances the head.
- If the lease or head changes unexpectedly, editing and publication stop immediately.
- A missing, delayed, length-mismatched, digest-mismatched, or corrupt snapshot is never
  imported. Leeway waits or asks for recovery rather than falling back silently.
- A failed schema migration leaves both the pre-migration recovery database and the
  synchronized head intact.
- Quitting is always permitted. Unpublished local work is surfaced on the next launch.

## Testing

Unit tests cover manifest and revision parsing, version gates, lease decisions, revision
ancestry and divergence, retention protection, path validation, local-generation state,
and every displayed sync status.

Integration tests use temporary app-data and synchronized directories to model two
devices. They cover new setup, adoption of an existing budget, disabling sync, legacy
`./leeway.db` import, clean handoff, stale ownership, divergent publication, explicit
resolution, and recovery after a failed or incompatible migration.

Fault-injection tests stop publication between each step and introduce missing, truncated,
corrupt, delayed, and reordered files. At every interruption, either the previous head or
the new head must remain fully verifiable, and no candidate database may be silently
discarded.

Compatibility tests prove that an older supported schema or protocol can upgrade and that
an older application refuses a newer one without replacing its local database. UI tests
exercise setup, read-only ownership, publication errors, version errors, and conflict
confirmations.

Manual acceptance testing uses Dropbox, iCloud Drive, OneDrive, and Syncthing to verify
only the common filesystem contract. No provider-specific behavior is required for
correctness.
