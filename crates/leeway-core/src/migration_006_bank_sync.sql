-- Migration 006: bank sync. The apps fetch a bank feed (SimpleFIN) and `bank.rs` applies it:
-- linked accounts take the bank's balance, and the current month's income and bills settle
-- from the transactions that cleared.
--
-- `account.bank_account_id`: the feed account this account syncs from (NULL = not synced).
-- `series.bank_match`: text a bank description must contain to settle the series' rows
--   (NULL = use the series label).
-- `txn.bank_txn_id`: the `bank_txn` that settled this row. A bank transaction settles at
--   most one row.

ALTER TABLE account ADD COLUMN bank_account_id TEXT;
ALTER TABLE series ADD COLUMN bank_match TEXT;
ALTER TABLE txn ADD COLUMN bank_txn_id TEXT;
CREATE UNIQUE INDEX txn_bank_txn ON txn (bank_txn_id) WHERE bank_txn_id IS NOT NULL;

-- Transactions from the feed, kept so the review can offer them and suggestions can see
-- past months. `id` is `<feed account id>/<feed transaction id>`, since feed ids are only
-- unique within an account. `ignored` marks one the user un-settled, so sync leaves it be.
CREATE TABLE bank_txn (
    id              TEXT PRIMARY KEY,
    bank_account_id TEXT NOT NULL,
    posted          TEXT NOT NULL,              -- ISO 'YYYY-MM-DD'
    amount_cents    INTEGER NOT NULL,           -- negative = money out
    description     TEXT NOT NULL,
    pending         INTEGER NOT NULL DEFAULT 0,
    ignored         INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX bank_txn_posted ON bank_txn (posted);

-- Suggestions the user dismissed, by their cleaned description.
CREATE TABLE bank_dismissal (
    key        TEXT PRIMARY KEY,
    updated_at TEXT NOT NULL DEFAULT ''
);

-- Sync-ready, like migration 005.
CREATE TRIGGER bank_txn_touch_insert AFTER INSERT ON bank_txn WHEN NEW.updated_at = '' BEGIN
    UPDATE bank_txn SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER bank_txn_touch_update AFTER UPDATE ON bank_txn WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE bank_txn SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER bank_txn_tombstone AFTER DELETE ON bank_txn BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('bank_txn', OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER bank_txn_undelete AFTER INSERT ON bank_txn BEGIN
    DELETE FROM deletion WHERE table_name = 'bank_txn' AND row_id = NEW.id;
END;

CREATE TRIGGER bank_dismissal_touch_insert AFTER INSERT ON bank_dismissal WHEN NEW.updated_at = '' BEGIN
    UPDATE bank_dismissal SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER bank_dismissal_touch_update AFTER UPDATE ON bank_dismissal WHEN NEW.updated_at = OLD.updated_at BEGIN
    UPDATE bank_dismissal SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER bank_dismissal_tombstone AFTER DELETE ON bank_dismissal BEGIN
    INSERT OR REPLACE INTO deletion (table_name, row_id, deleted_at) VALUES ('bank_dismissal', OLD.key, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
CREATE TRIGGER bank_dismissal_undelete AFTER INSERT ON bank_dismissal BEGIN
    DELETE FROM deletion WHERE table_name = 'bank_dismissal' AND row_id = NEW.key;
END;
