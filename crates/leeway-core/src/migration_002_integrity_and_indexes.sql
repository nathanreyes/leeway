-- Query paths used by the dashboard, Series page, and deletion guards.
CREATE UNIQUE INDEX IF NOT EXISTS idx_month_label_unique ON month(label);
CREATE INDEX IF NOT EXISTS idx_month_start_date ON month(start_date);
CREATE INDEX IF NOT EXISTS idx_txn_month_display
    ON txn(month_id, direction DESC, label, id);
CREATE INDEX IF NOT EXISTS idx_txn_month_envelope
    ON txn(month_id, envelope_id, label, id);
CREATE INDEX IF NOT EXISTS idx_txn_envelope ON txn(envelope_id);
CREATE INDEX IF NOT EXISTS idx_txn_account ON txn(account_id);
CREATE INDEX IF NOT EXISTS idx_txn_trend
    ON txn(series_id, month_id) WHERE envelope_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_envelope_month_display
    ON envelope(month_id, label, id);
CREATE INDEX IF NOT EXISTS idx_envelope_trend ON envelope(series_id, month_id);
CREATE INDEX IF NOT EXISTS idx_plan_item_series_plan ON plan_item(series_id, plan_id);

-- A transaction filed in an envelope must live in the same stamped month. The application
-- separately limits the normal spending command to manual envelopes; automatic envelopes
-- may still gain record-only transactions in a future workflow.
CREATE TRIGGER IF NOT EXISTS txn_envelope_month_insert
BEFORE INSERT ON txn
WHEN NEW.envelope_id IS NOT NULL
 AND NOT EXISTS (
     SELECT 1 FROM envelope
     WHERE id = NEW.envelope_id AND month_id = NEW.month_id
 )
BEGIN
    SELECT RAISE(ABORT, 'transaction envelope belongs to another month');
END;

CREATE TRIGGER IF NOT EXISTS txn_envelope_month_update
BEFORE UPDATE OF month_id, envelope_id ON txn
WHEN NEW.envelope_id IS NOT NULL
 AND NOT EXISTS (
     SELECT 1 FROM envelope
     WHERE id = NEW.envelope_id AND month_id = NEW.month_id
 )
BEGIN
    SELECT RAISE(ABORT, 'transaction envelope belongs to another month');
END;
