-- 0002_ingress_and_log.sql -- EventSub ingress log and command log scaffolding
CREATE TABLE event_raw (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  msg_id TEXT NOT NULL,
  type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  event_at TEXT NOT NULL,
  received_at TEXT NOT NULL,
  source TEXT NOT NULL CHECK(source IN ('webhook'))
);

CREATE UNIQUE INDEX ux_event_raw_msg_id ON event_raw(msg_id);
CREATE INDEX ix_event_raw_received_at ON event_raw(received_at);
CREATE INDEX ix_event_raw_broadcaster_type ON event_raw(broadcaster_id, type);

CREATE TABLE command_log (
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  op_id TEXT NULL,
  type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (broadcaster_id, version)
);

CREATE UNIQUE INDEX ux_command_op_id
  ON command_log(broadcaster_id, op_id)
  WHERE op_id IS NOT NULL;
CREATE INDEX ix_command_created_at ON command_log(created_at);

CREATE TABLE state_index (
  broadcaster_id TEXT PRIMARY KEY REFERENCES broadcasters(id) ON DELETE CASCADE,
  current_version INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);
