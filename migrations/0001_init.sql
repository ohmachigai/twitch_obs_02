-- 0001_init.sql -- foundational tables for broadcasters, users, and OAuth links
CREATE TABLE broadcasters (
  id TEXT PRIMARY KEY,
  twitch_broadcaster_id TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  timezone TEXT NOT NULL,
  settings_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE users (
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('superadmin','broadcaster','operator')),
  broadcaster_id TEXT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE oauth_links (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  twitch_user_id TEXT NOT NULL,
  scopes_json TEXT NOT NULL,
  access_token TEXT NOT NULL,
  refresh_token TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX ux_oauth_broadcaster_twitch_user ON oauth_links(broadcaster_id, twitch_user_id);
