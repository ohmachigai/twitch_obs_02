-- 0004_oauth_backfill_scaffolding.sql -- OAuth PKCE state store and Helix backfill checkpoints
CREATE TABLE oauth_login_states (
  state TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  code_verifier TEXT NOT NULL,
  redirect_to TEXT,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);

ALTER TABLE oauth_links ADD COLUMN managed_scopes_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE oauth_links ADD COLUMN last_validated_at TEXT;
ALTER TABLE oauth_links ADD COLUMN last_refreshed_at TEXT;
ALTER TABLE oauth_links ADD COLUMN last_failure_at TEXT;
ALTER TABLE oauth_links ADD COLUMN last_failure_reason TEXT;
ALTER TABLE oauth_links ADD COLUMN requires_reauth INTEGER NOT NULL DEFAULT 0 CHECK(requires_reauth IN (0,1));

CREATE TABLE helix_backfill_checkpoints (
  broadcaster_id TEXT PRIMARY KEY REFERENCES broadcasters(id) ON DELETE CASCADE,
  cursor TEXT,
  last_redemption_id TEXT,
  last_seen_at TEXT,
  last_run_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('idle','running','error')),
  error_message TEXT,
  updated_at TEXT NOT NULL
);
