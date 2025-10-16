# `.docs/05-data-schema-and-migrations.md` — データスキーマ / マイグレーション（規範）

> 本章は **SQLite を用いた永続層の一次仕様**です。
> ここに記す **テーブル定義・制約・インデックス・TTL 運用・WAL 管理・トランザクション規約** は実装の拘束条件（**MUST**）です。
> ドメイン語彙は `03-domain-model.md`、API は `04-api-contracts.md` を参照。

---

## 0. 方針と対象

* **方針**：コマンド駆動（append‑only）の**決定性**・**再現性**を最優先。履歴は必要最小限 72h を保持。
* **物理 DB**：**SQLite 3.35+**（RETURNING 付き DML が利用可）。
* **アクセス**：`sqlx`（Rust）。`PRAGMA foreign_keys = ON` を**常時有効**（**MUST**）。

---

## 1. SQLite 推奨 PRAGMA（接続確立時に適用・規範）

```sql
PRAGMA foreign_keys = ON;          -- 参照整合性を強制（MUST）
PRAGMA journal_mode = WAL;         -- ライター/リーダー平行性確保（MUST）
PRAGMA synchronous = NORMAL;       -- WAL と併用（推奨）
PRAGMA busy_timeout = 5000;        -- ミリ秒（推奨）
PRAGMA temp_store = MEMORY;        -- 一時 B-tree をメモリに（任意）
```

> Windows/Linux 共通。`journal_mode=WAL` は**プロセス共有**のため、同一 DB を複数プロセスで開く場合は**同一ユーザ権限**・**同一ファイルシステム**を前提とする。

---

## 2. マイグレーション運用（規範）

* **配置**：リポジトリ直下の `migrations/`（`sqlx` 互換）。
* **命名**：昇順プレフィクス（例：`0001_init.sql`, `0002_core_tables.sql`, …）。
* **適用**：起動前に `sqlx migrate run`。CI は `sqlx migrate run --dry-run` を含む。
* **ロールフォワード**原則：**破壊的変更は既存を残し新テーブル/列を追加→移行→旧を段階撤去**。
* **外部キー**使用時の削除：**アプリ側で順序を制御**（TTL はバッチで小分け）。

---

## 3. 論理→物理マッピング（概要）

| ドメイン                 | 物理テーブル            | 主キー                                | 代表インデックス・制約                                                                       |
| -------------------- | ----------------- | ---------------------------------- | --------------------------------------------------------------------------------- |
| Broadcaster/Settings | `broadcasters`    | `id`                               | `twitch_broadcaster_id` UNIQUE                                                    |
| User                 | `users`           | `id`                               | (`email` UNIQUE), (FK→`broadcasters.id` nullable)                                 |
| OAuthLink            | `oauth_links`     | `id`                               | `broadcaster_id` FK, `twitch_user_id` UNIQUE per broadcaster                      |
| OAuthLoginState      | `oauth_login_states` | `state`                         | TTL 付き（`expires_at`）、`broadcaster_id` FK                                       |
| BackfillCheckpoint   | `helix_backfill_checkpoints` | `broadcaster_id`      | `cursor` / `last_redemption_id` / `status`、`updated_at`                           |
| EventRaw(72h)        | `event_raw`       | `id`                               | `msg_id` UNIQUE, `received_at` INDEX                                              |
| CommandLog(72h)      | `command_log`     | (`broadcaster_id`,`version`)       | `op_id` UNIQUE (partial), `created_at` INDEX                                      |
| StateIndex           | `state_index`     | `broadcaster_id`                   | `current_version`                                                                 |
| QueueEntry           | `queue_entries`   | `id`                               | (`broadcaster_id`,`status`,`enqueued_at`) INDEX, `redemption_id` UNIQUE (partial) |
| DailyCounter         | `daily_counters`  | (`day`,`broadcaster_id`,`user_id`) | `updated_at` INDEX                                                                |
| StreamSession        | `stream_sessions` | `id`                               | (`broadcaster_id`,`ended_at IS NULL`) UNIQUE(partial)                             |

> **部分（一部）ユニークインデックス**は SQLite の `WHERE` 句付き UNIQUE INDEX を使用。

---

## 4. 初期 DDL（推奨マイグレーション分割）

### 4.1 `0001_init.sql` — 基本マスタ

```sql
-- broadcasters: 配信者 + 設定(JSON)
CREATE TABLE broadcasters (
  id TEXT PRIMARY KEY,                            -- UUID/ULID
  twitch_broadcaster_id TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  timezone TEXT NOT NULL,                         -- IANA TZ
  settings_json TEXT NOT NULL,                    -- Settings を JSON 文字列で保存
  created_at TEXT NOT NULL,                       -- ISO8601 UTC
  updated_at TEXT NOT NULL
);

-- users: 内部ユーザ（RBAC）
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('superadmin','broadcaster','operator')),
  broadcaster_id TEXT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- oauth_links: Twitch 連携
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
```

> `settings_json` は `Settings`（`03-domain-model.md`）の JSON。**アプリ側で構造体へデコード**（**MUST**）。

---

### 4.2 `0002_ingress_and_log.sql` — EventRaw / CommandLog / StateIndex

```sql
-- 受信イベント生ログ（72h 保持）
CREATE TABLE event_raw (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  msg_id TEXT NOT NULL,                            -- Twitch-Eventsub-Message-Id
  type TEXT NOT NULL,                              -- 'redemption.add' 等
  payload_json TEXT NOT NULL,
  event_at TEXT NOT NULL,                          -- イベントの発生時刻(UTC)
  received_at TEXT NOT NULL,                       -- 受信時刻(UTC)
  source TEXT NOT NULL CHECK(source IN ('webhook'))
);
CREATE UNIQUE INDEX ux_event_raw_msg_id ON event_raw(msg_id);
CREATE INDEX ix_event_raw_received_at ON event_raw(received_at);
CREATE INDEX ix_event_raw_broadcaster_type ON event_raw(broadcaster_id, type);

-- コマンドログ（append-only, 72h 保持）
CREATE TABLE command_log (
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,                        -- 単調増加（broadcaster 単位）
  op_id TEXT NULL,                                 -- 管理操作の冪等キー
  type TEXT NOT NULL,                              -- 'enqueue' など
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (broadcaster_id, version)
);
-- op_id は部分ユニーク（NULLを除外）
CREATE UNIQUE INDEX ux_command_op_id
  ON command_log(broadcaster_id, op_id)
  WHERE op_id IS NOT NULL;
CREATE INDEX ix_command_created_at ON command_log(created_at);

-- version 採番
CREATE TABLE state_index (
  broadcaster_id TEXT PRIMARY KEY REFERENCES broadcasters(id) ON DELETE CASCADE,
  current_version INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);
```

> **規範**：`command_log` への挿入と `state_index.current_version` の更新は**同一トランザクション**で行う（§7 参照）。

---

### 4.3 `0003_queue_and_counters.sql` — QueueEntry / DailyCounter / StreamSession

```sql
-- 待機キュー（履歴も保持）
CREATE TABLE queue_entries (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,                          -- Twitch user id
  user_login TEXT NOT NULL,
  user_display_name TEXT NOT NULL,
  user_avatar TEXT,
  reward_id TEXT NOT NULL,
  redemption_id TEXT,                             -- Twitch redemption id（重複防止）
  enqueued_at TEXT NOT NULL,                      -- UTC
  status TEXT NOT NULL CHECK(status IN ('QUEUED','COMPLETED','REMOVED')),
  status_reason TEXT,                             -- 'UNDO'|'STREAM_START_CLEAR'|'EXPLICIT_REMOVE' 等
  managed INTEGER NOT NULL DEFAULT 0,             -- Helix更新適用可否（0/1）
  last_updated_at TEXT NOT NULL
);
-- redemption_id が存在するときのみ一意
CREATE UNIQUE INDEX ux_queue_redemption_unique
  ON queue_entries(redemption_id)
  WHERE redemption_id IS NOT NULL;

CREATE INDEX ix_queue_broadcaster_status_enqueued
  ON queue_entries(broadcaster_id, status, enqueued_at);
CREATE INDEX ix_queue_broadcaster_user
  ON queue_entries(broadcaster_id, user_id);

-- 今日の回数（配信者の TZ で切替、保存は day 文字列）
CREATE TABLE daily_counters (
  day TEXT NOT NULL,                              -- "YYYY-MM-DD" in broadcaster.timezone
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  count INTEGER NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (day, broadcaster_id, user_id)
);
CREATE INDEX ix_daily_counters_updated_at ON daily_counters(updated_at);

-- 配信セッション境界
CREATE TABLE stream_sessions (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  started_at TEXT NOT NULL,
  ended_at TEXT
);
-- 未終了セッションは 1 つのみ許可（部分 UNIQUE）
CREATE UNIQUE INDEX ux_stream_open_unique
  ON stream_sessions(broadcaster_id)
  WHERE ended_at IS NULL;
```

### 4.4 `0004_oauth_backfill_scaffolding.sql` — OAuth state / Helix Backfill

```sql
-- oauth_login_states: OAuth state (PKCE) を保存し CSRF を防ぐ
CREATE TABLE oauth_login_states (
  state TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  code_verifier TEXT NOT NULL,
  redirect_to TEXT,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);

-- oauth_links: トークン健全性の追跡列を追加
ALTER TABLE oauth_links ADD COLUMN managed_scopes_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE oauth_links ADD COLUMN last_validated_at TEXT;
ALTER TABLE oauth_links ADD COLUMN last_refreshed_at TEXT;
ALTER TABLE oauth_links ADD COLUMN last_failure_at TEXT;
ALTER TABLE oauth_links ADD COLUMN last_failure_reason TEXT;
ALTER TABLE oauth_links ADD COLUMN requires_reauth INTEGER NOT NULL DEFAULT 0 CHECK(requires_reauth IN (0,1));

-- helix_backfill_checkpoints: UNFULFILLED 再取得の進捗管理
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
```

> `oauth_login_states` は **短寿命 TTL（既定 10 分）でクリーンアップ**。`helix_backfill_checkpoints.status` は Backfill ワーカーの状態（`idle`／`running`／`error`）を示し、`error_message` で最新の Helix 応答を残す。`cursor` / `last_redemption_id` / `last_seen_at` は Helix UNFULFILLED 再取得の再開ポイントであり、ワーカーは `running` → `idle|error` の順で更新する。。

---

## 5. 代表クエリ（規範・参考）

### 5.1 version の採番と Command 追加（**1トランザクション**）

> **疑似コード**（SQLite, `sqlx`）

```sql
BEGIN IMMEDIATE;

-- 1) version をインクリメント
UPDATE state_index
   SET current_version = current_version + 1,
       updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
 WHERE broadcaster_id = :b;

-- 2) 現在値を取得（SQLite 3.35+ は RETURNING が使える）
SELECT current_version
  FROM state_index
 WHERE broadcaster_id = :b;

-- 3) command_log を version 付きで INSERT
INSERT INTO command_log(broadcaster_id, version, op_id, type, payload_json, created_at)
VALUES(:b, :v, :op_id, :type, :payload_json, strftime('%Y-%m-%dT%H:%M:%fZ','now'));

COMMIT;
```

> **規範**：`UPDATE state_index` と `INSERT command_log` は**必ず同一トランザクション**。

---

### 5.2 キューの現在並び（**表示順ルールの実装**）

> **前提**：`today_day` はアプリで `broadcaster.timezone` に従い算出し、バインドする。

```sql
SELECT q.*, COALESCE(dc.count, 0) AS today_count
  FROM queue_entries AS q
  LEFT JOIN daily_counters AS dc
    ON dc.day = :today_day
   AND dc.broadcaster_id = q.broadcaster_id
   AND dc.user_id = q.user_id
 WHERE q.broadcaster_id = :b
   AND q.status = 'QUEUED'
 ORDER BY today_count ASC, q.enqueued_at ASC;
```

---

### 5.3 反スパム（60s 窓の判定の一例）

```sql
SELECT COUNT(*) > 0 AS within_window
  FROM queue_entries
 WHERE broadcaster_id = :b
   AND user_id = :user
   AND reward_id = :reward
   AND enqueued_at >= :occurred_at_minus_60s
   AND enqueued_at <= :occurred_at;
```

> 実際には**Normalizer の occurred_at**を使い、アプリ側 Clock を注入して決定性を担保。

---

## 6. TTL（72h）と WAL 管理（規範）

### 6.1 TTL 対象と条件

* **対象**：`event_raw`, `command_log`
* **条件**：`created_at/received_at < now() - 72h`

> `oauth_login_states` は **10 分** を上限に別ジョブで削除（Backfill/OAuth ワーカーが担保）。

### 6.2 小分け削除ジョブ（**必須**）

> **単位**：1 周期で **最大 1000 行**。枯渇までループ。

```sql
-- event_raw
DELETE FROM event_raw
 WHERE received_at < :threshold
 ORDER BY received_at
 LIMIT 1000;

-- command_log
DELETE FROM command_log
 WHERE created_at < :threshold
 ORDER BY created_at
 LIMIT 1000;
```

> **規範**：削除は**broadcaster 混在可**。アプリ側で**繰り返し**実行する。

### 6.3 WAL チェックポイント

* 周期的に **`PRAGMA wal_checkpoint(TRUNCATE);`** を実行（**推奨：TTL サイクル後**）。
* 実行時間・件数をメトリクスに記録（`db_checkpoint_seconds` など）。

---

## 7. 制約の留意（強制 / ソフト）

* **強制（DB で拘束）**

  * 外部キー：`broadcaster_id` は `broadcasters.id` に参照整合。
  * 列制約：`role`, `status`, `source` の **CHECK**。
  * ユニーク：`event_raw.msg_id`、`oauth_links(broadcaster_id,twitch_user_id)`、`oauth_login_states.state`、`helix_backfill_checkpoints.broadcaster_id`、`queue_entries.redemption_id`（partial）。
  * 部分 UNIQUE：`command_log(broadcaster_id, op_id) WHERE op_id IS NOT NULL`。
  * 未終了セッションの一意：`stream_sessions` の部分 UNIQUE。

* **ソフト（アプリで保証）**

  * **version の単調増加**（`state_index` で採番＋同一 Tx）。
  * **QueueEntry の終端**（`COMPLETED/REMOVED` から戻さない）。
  * **“今日”判定**（IANA TZ による day 算出）。
  * **表示順**（`ORDER BY today_count, enqueued_at`）。
  * **Helix 更新の可否**（`managed` の意味づけ）。

---

## 8. サンプル・シード（開発便宜）

> 開発用（**本番禁止**）。`migrations/0000_dev_seed.sql` 等で明示 opt-in。

```sql
INSERT INTO broadcasters(id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at)
VALUES ('b-dev','123456','Dev Broadcaster','Asia/Tokyo',
        '{"overlay_theme":"neon","group_size":6,"clear_on_stream_start":true,"clear_decrement_counts":false,
          "policy":{"anti_spam_window_sec":60,"duplicate_policy":"consume","target_rewards":["r-join"]}}',
        '2025-10-12T12:00:00.000Z','2025-10-12T12:00:00.000Z');

INSERT INTO state_index(broadcaster_id, current_version, updated_at)
VALUES ('b-dev', 0, '2025-10-12T12:00:00.000Z');
```

---

## 9. バックフィル（健全性復旧）

* **目的**：資格失効→復旧時に、未処理の Redemption（`UNFULFILLED`）を Helix から取得し、
  **Normalizer→Policy→CommandLog→Projector** の順に再生。
* **重複防止**：`queue_entries.redemption_id` の **partial UNIQUE** で抑止（**MUST**）。
* **時系列**：`issued_at` に従い Command を古い順に挿入。version は採番。

---

## 10. マイグレーション変更の PR 手順（規範）

1. **`.docs/03-*.md` / `04-*.md`** を先に更新（契約変更の明文化）。
2. `sqlx migrate add <summary>` で新ファイルを作成。
3. DDL を記述し、**手動テスト**（`sqlx migrate run` → `sqlx migrate revert` で往復）。
4. **データ移行**がある場合：**新旧併用期間**を持ち、アプリは両対応コードで先にリリース。
5. CI：`sqlx migrate run --dry-run` を通す。
6. **ロールフォワード**原則：後戻りが必要になれば新マイグレーションで打ち消す。

---

## 11. セキュリティ / プライバシ

* **トークン類（access/refresh）**は `oauth_links` に**平文保存を避ける**（OS レベル保護、必要に応じ暗号化ストア・KMS を検討）。
* **PII**（表示名・アバター URL）は最小保持。`event_raw.payload_json` にはフルイベントが含まれるため、TTL を厳守。
* **監査**：`command_log` と `event_raw` を合わせて 72h 追跡可能。

---

## 12. 運用チェックリスト（本章適合）

* [ ] `PRAGMA foreign_keys=ON`, `journal_mode=WAL` が有効
* [ ] すべてのテーブル・インデックスが本章定義どおりに作成
* [ ] `command_log` と `state_index` が**同 Tx**で更新（version 単調）
* [ ] `event_raw.msg_id` / `queue_entries.redemption_id` / `command_log(broadcaster,op_id)` の**一意**が効く
* [ ] **TTL バッチ**と **WAL checkpoint(TRUNCATE)** が定期実行されている
* [ ] 表示順クエリ（today_count ASC, enqueued_at ASC）が正しく動く
* [ ] Backfill が重複なく適用される（partial UNIQUE が効く）

---

## 付録 A：DDL 全量（統合版）

> 初期セットアップを一括で行いたい場合の参考。実運用は **分割マイグレーション**に従うこと。

<details>
<summary>統合 DDL</summary>

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

-- broadcasters
CREATE TABLE broadcasters (
  id TEXT PRIMARY KEY,
  twitch_broadcaster_id TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  timezone TEXT NOT NULL,
  settings_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- users
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('superadmin','broadcaster','operator')),
  broadcaster_id TEXT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- oauth_links
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

-- event_raw
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

-- command_log
CREATE TABLE command_log (
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  op_id TEXT NULL,
  type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (broadcaster_id, version)
);
CREATE UNIQUE INDEX ux_command_op_id ON command_log(broadcaster_id, op_id) WHERE op_id IS NOT NULL;
CREATE INDEX ix_command_created_at ON command_log(created_at);

-- state_index
CREATE TABLE state_index (
  broadcaster_id TEXT PRIMARY KEY REFERENCES broadcasters(id) ON DELETE CASCADE,
  current_version INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);

-- queue_entries
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
CREATE UNIQUE INDEX ux_queue_redemption_unique ON queue_entries(redemption_id) WHERE redemption_id IS NOT NULL;
CREATE INDEX ix_queue_broadcaster_status_enqueued ON queue_entries(broadcaster_id, status, enqueued_at);
CREATE INDEX ix_queue_broadcaster_user ON queue_entries(broadcaster_id, user_id);

-- daily_counters
CREATE TABLE daily_counters (
  day TEXT NOT NULL,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  count INTEGER NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (day, broadcaster_id, user_id)
);
CREATE INDEX ix_daily_counters_updated_at ON daily_counters(updated_at);

-- stream_sessions
CREATE TABLE stream_sessions (
  id TEXT PRIMARY KEY,
  broadcaster_id TEXT NOT NULL REFERENCES broadcasters(id) ON DELETE CASCADE,
  started_at TEXT NOT NULL,
  ended_at TEXT
);
CREATE UNIQUE INDEX ux_stream_open_unique ON stream_sessions(broadcaster_id) WHERE ended_at IS NULL;
```

</details>

---

本章の定義は**規範**です。実装や運用により矛盾が見つかった場合、**先に本章を更新**し、関連文書（`02/03/04/07/10`）の整合を取ってください。
