# Sync-ready storage

Date: 2026-10-31

## Problem

Leeway keeps one SQLite file per machine. A future frontend may keep a copy of the budget
on more than one device and sync rows between them. Row-level sync needs three facts the
schema does not record today:

- when each row last changed, so a sync can send only what changed and keep the newer copy;
- which rows were deleted, so a delete can reach other devices;
- a month identity that two devices agree on, so stamping October offline on two devices
  does not create two October rows.

This change records those facts. It adds no sync. Nothing a user sees changes.

## Shape chosen

### `updated_at` on every table

Each table — `account`, `plan`, `series`, `plan_item`, `month`, `envelope`, `txn`,
`setting` — gains `updated_at TEXT NOT NULL`, an RFC 3339 UTC time with milliseconds.

Triggers keep it current, so no write path in `ops.rs` changes:

- after an insert that leaves `updated_at` empty, set it to now;
- after an update that leaves `updated_at` as it was, set it to now.

A write that sets `updated_at` itself keeps that value. A future sync uses this to apply a
row from another device without stamping it with the local clock.

### A `deletion` table instead of a `deleted_at` column

`deletion(table_name, row_id, deleted_at)` records each deleted row. An after-delete
trigger on every table writes it; an after-insert trigger removes it when a row with the
same id comes back.

A `deleted_at` column on every table would do the same job, but every query and join would
then need `deleted_at IS NULL`, and one missed filter would show deleted rows. Rows still
leave their tables, so reads stay as they are. Deletes in `ops.rs` already remove child
rows one by one, and the schema has no cascades, so the triggers see every delete.

### Fixed month ids

`ops::month_id(label)` derives a month's id from its label (`2026-10`) as a UUID v5 under a
fixed Leeway namespace. Stamping uses it, so every device gives October the same id. Other
tables keep random v4 ids: a user creates those rows on purpose, and two devices should not
merge two plans that share a name.

The migration rewrites the ids of months that already exist and updates the `month_id` of
their envelopes and transactions in the same transaction, with foreign-key checks deferred
to commit.

## Not in this change

- A change queue, sync engine or any network code.
- Conflict rules beyond "the newer `updated_at` wins", which a sync layer will apply.
- Pruning old `deletion` rows.

## Testing

- A fresh database and a migrated version-4 database both reach the latest schema.
- Inserts set `updated_at`; updates move it forward; an explicit value is kept.
- Deleting a row leaves a tombstone; inserting the same id clears it.
- Migrating a version-4 database gives each month its fixed id and keeps its envelopes and
  transactions attached.
- Stamping the same label twice yields the same month id.
