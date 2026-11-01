-- Migration 005: record what a future row-level sync needs.
-- See knowledge/plans/2026-10-31-sync-ready-storage-design.md.
--
-- Every table gains `updated_at` (RFC 3339 UTC with milliseconds). Triggers keep it current,
-- so no write path has to: an insert or update that leaves it untouched gets "now", and a
-- write that sets it explicitly (a future sync applying a remote row) keeps its value.
--
-- Deleted rows leave a tombstone in `deletion`; inserting the same id again removes it.
-- Rows still leave their own tables, so no read needs a "not deleted" filter.
--
-- Month ids become fixed (derived from the label); the Rust hook for this migration
-- rewrites existing months and their envelopes and transactions.

CREATE TABLE deletion (
    table_name TEXT NOT NULL,
    row_id     TEXT NOT NULL,
    deleted_at TEXT NOT NULL,
    PRIMARY KEY (table_name, row_id)
);

-- account
ALTER TABLE account ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE account SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER account_touch_insert AFTER INSERT ON account WHEN NEW.updated_at = '' BEGIN
    UPDATE account SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER account_touch_update AFTER UPDATE ON account WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE account SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER account_tombstone AFTER DELETE ON account BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('account', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER account_undelete AFTER INSERT ON account BEGIN
    DELETE FROM deletion WHERE table_name = 'account' AND row_id = NEW.id;
END;

-- plan
ALTER TABLE plan ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE plan SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER plan_touch_insert AFTER INSERT ON plan WHEN NEW.updated_at = '' BEGIN
    UPDATE plan SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER plan_touch_update AFTER UPDATE ON plan WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE plan SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER plan_tombstone AFTER DELETE ON plan BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('plan', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER plan_undelete AFTER INSERT ON plan BEGIN
    DELETE FROM deletion WHERE table_name = 'plan' AND row_id = NEW.id;
END;

-- series
ALTER TABLE series ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE series SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER series_touch_insert AFTER INSERT ON series WHEN NEW.updated_at = '' BEGIN
    UPDATE series SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER series_touch_update AFTER UPDATE ON series WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE series SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER series_tombstone AFTER DELETE ON series BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('series', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER series_undelete AFTER INSERT ON series BEGIN
    DELETE FROM deletion WHERE table_name = 'series' AND row_id = NEW.id;
END;

-- plan_item
ALTER TABLE plan_item ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE plan_item SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER plan_item_touch_insert AFTER INSERT ON plan_item WHEN NEW.updated_at = '' BEGIN
    UPDATE plan_item SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER plan_item_touch_update AFTER UPDATE ON plan_item WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE plan_item SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER plan_item_tombstone AFTER DELETE ON plan_item BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('plan_item', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER plan_item_undelete AFTER INSERT ON plan_item BEGIN
    DELETE FROM deletion WHERE table_name = 'plan_item' AND row_id = NEW.id;
END;

-- month
ALTER TABLE month ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE month SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER month_touch_insert AFTER INSERT ON month WHEN NEW.updated_at = '' BEGIN
    UPDATE month SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER month_touch_update AFTER UPDATE ON month WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE month SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER month_tombstone AFTER DELETE ON month BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('month', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER month_undelete AFTER INSERT ON month BEGIN
    DELETE FROM deletion WHERE table_name = 'month' AND row_id = NEW.id;
END;

-- envelope
ALTER TABLE envelope ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE envelope SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER envelope_touch_insert AFTER INSERT ON envelope WHEN NEW.updated_at = '' BEGIN
    UPDATE envelope SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER envelope_touch_update AFTER UPDATE ON envelope WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE envelope SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER envelope_tombstone AFTER DELETE ON envelope BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('envelope', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER envelope_undelete AFTER INSERT ON envelope BEGIN
    DELETE FROM deletion WHERE table_name = 'envelope' AND row_id = NEW.id;
END;

-- txn
ALTER TABLE txn ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE txn SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER txn_touch_insert AFTER INSERT ON txn WHEN NEW.updated_at = '' BEGIN
    UPDATE txn SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER txn_touch_update AFTER UPDATE ON txn WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE txn SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER txn_tombstone AFTER DELETE ON txn BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('txn', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER txn_undelete AFTER INSERT ON txn BEGIN
    DELETE FROM deletion WHERE table_name = 'txn' AND row_id = NEW.id;
END;

-- setting
ALTER TABLE setting ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE setting SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER setting_touch_insert AFTER INSERT ON setting WHEN NEW.updated_at = '' BEGIN
    UPDATE setting SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER setting_touch_update AFTER UPDATE ON setting WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE setting SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER setting_tombstone AFTER DELETE ON setting BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('setting', OLD.key, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER setting_undelete AFTER INSERT ON setting BEGIN
    DELETE FROM deletion WHERE table_name = 'setting' AND row_id = NEW.key;
END;
