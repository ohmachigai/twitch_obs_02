-- 0005_queue_completion_history.sql
-- Adds display_order/completed_at columns and supporting indexes for queue history.

ALTER TABLE queue_entries ADD COLUMN display_order REAL NOT NULL DEFAULT 0;
ALTER TABLE queue_entries ADD COLUMN completed_at TEXT; -- UTC ISO-8601, NULL when not completed

-- Initialize display_order using enqueued_at timestamp (epoch seconds as REAL).
UPDATE queue_entries
   SET display_order = (julianday(enqueued_at) - 2440587.5) * 86400.0
 WHERE display_order = 0;

-- Populate completed_at for already completed entries using last_updated_at.
UPDATE queue_entries
   SET completed_at = last_updated_at
 WHERE status = 'COMPLETED' AND completed_at IS NULL;

CREATE INDEX ix_queue_broadcaster_status_display
  ON queue_entries(broadcaster_id, status, display_order);
CREATE INDEX ix_queue_broadcaster_status_completed
  ON queue_entries(broadcaster_id, status, completed_at DESC);
