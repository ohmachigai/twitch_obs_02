-- 0003_queue_and_counters.sql -- Queue entries, daily counters, stream sessions
CREATE TABLE queue_entries (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  user_login TEXT NOT NULL,
  user_display_name TEXT NOT NULL,
  user_avatar TEXT,
  reward_id TEXT NOT NULL,
  redemption_id TEXT,
  enqueued_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('QUEUED','COMPLETED','REMOVED')),
  status_reason TEXT,
  managed INTEGER NOT NULL DEFAULT 0,
  last_updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX ux_queue_redemption_unique
  ON queue_entries(redemption_id)
  WHERE redemption_id IS NOT NULL;

CREATE INDEX ix_queue_broadcaster_status_enqueued
  ON queue_entries(broadcaster_id, status, enqueued_at);

CREATE INDEX ix_queue_broadcaster_user
  ON queue_entries(broadcaster_id, user_id);

CREATE TABLE daily_counters (
  day TEXT NOT NULL,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  count INTEGER NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (day, broadcaster_id, user_id)
);

CREATE INDEX ix_daily_counters_updated_at ON daily_counters(updated_at);

CREATE TABLE stream_sessions (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  started_at TEXT NOT NULL,
  ended_at TEXT
);

CREATE UNIQUE INDEX ux_stream_open_unique
  ON stream_sessions(broadcaster_id)
  WHERE ended_at IS NULL;
