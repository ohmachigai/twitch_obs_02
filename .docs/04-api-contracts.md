# `.docs/04-api-contracts.md` — API コントラクト（規範）

> 本章は **サーバ I/F の一次仕様**です。
> ここに記す **URI・HTTP メソッド・パラメータ・ボディ・戻り**・**SSE のワイヤ形式**・**認可**・**エラーフォーマット** は規範（**MUST**）です。
> 実装・テスト・フロントは本章に適合しなければなりません。モデル語彙は `03-domain-model.md` を参照。

---

## 0. 共通規約

### 0.1 プロトコル / MIME

* **HTTPS**（本番）。ローカルは HTTP 可。
* **JSON**：`Content-Type: application/json; charset=utf-8`（REST）
* **SSE**：`Content-Type: text/event-stream`（改行 `\n`、イベント間は空行）

### 0.2 時刻・ID

* **時刻**：ISO 8601 UTC（例：`2025-10-12T13:05:00.000Z`）。
* **ID**：`broadcaster_id`（内部）/ `twitch_*_id`（外部）/ `version`（単調増加, per broadcaster）/ `op_id`（UUID）/ `entry_id`（ULID/UUID）。

### 0.3 命名・ケース

* JSON のキーは **snake_case**。

### 0.4 認証・認可（概要）

* **管理系 REST** はセッションまたは Bearer/JWT 等で**認証必須**（方式自体は実装に委任）。
* **SSE（overlay/admin）** は **短寿命の署名トークン**で認可（クエリまたは同一オリジン Cookie）。

  * トークンの **claims（規範）**：

    * `sub`: `broadcaster_id`
    * `aud`: `"overlay"` または `"admin"`
    * `exp`: 有効期限（短寿命、例：5〜15分）
  * 署名方式・鍵長は実装に委任（HS256/RSA など）。

> 補足：EventSource は **カスタムヘッダ不可**のため、SSE は **クエリ `token=...`** か **同一オリジン Cookie** で渡す。

### 0.5 エラー形式（RFC 7807 準拠）

* `Content-Type: application/problem+json`

```json
{
  "type": "https://example.com/problems/invalid-argument",
  "title": "Invalid argument",
  "status": 400,
  "detail": "since must be an ISO-8601 timestamp",
  "instance": "/api/state"
}
```

代表コード：`400 INVALID_ARGUMENT` / `401 UNAUTHENTICATED` / `403 PERMISSION_DENIED` /
`404 NOT_FOUND` / `409 ALREADY_EXISTS` / `412 PRECONDITION_FAILED` / `422 UNPROCESSABLE_ENTITY` / `429 RESOURCE_EXHAUSTED` / `500 INTERNAL`.

### 0.6 冪等・リトライ

* **管理操作**は **`op_id`（UUID）必須**。同一 `op_id` は**1回のみ**反映（**MUST**）。
* **Webhook** は `Message-Id` で入力冪等（**MUST**）。
* **SSE** は `id=version` と `Last-Event-ID` で再送補償（**MUST**）。

---

## 1. Webhook（EventSub）— 受信

### 1.1 `POST /eventsub/webhook`

* **Purpose**：Twitch EventSub の verification / notification / revocation を受け付ける。

* **Auth**：不要（HMAC で検証）。

* **Headers（受信）**：

  * `Twitch-Eventsub-Message-Id: <uuid>`
  * `Twitch-Eventsub-Message-Timestamp: <rfc3339>`
  * `Twitch-Eventsub-Message-Signature: sha256=<hex>`
  * `Twitch-Eventsub-Message-Type: verification|notification|revocation`

* **Verification（チャレンジ）**：

  * **200 OK** + ボディに **生の challenge 文字列**（MUST）

* **Notification**：

  * 署名＝`HMAC-SHA256(secret, id + timestamp + raw_body)` を**定数時間比較**
  * `timestamp` ±10 分内であること
  * **204 No Content**（**即時 ACK**、MUST）

* **Revocation**：**204 No Content**（MUST）

> 受信ペイロードは `EventRaw` に保存（72h）。`Message-Id` は一意。重複は検証後に 204 で終了。

---

## 2. 初期スナップショット（REST）

### 2.1 `GET /api/state`

* **Purpose**：オーバーレイ/管理 UI の初期化。最新 `version` と可視状態を返す。
* **Auth**：要（overlay/admin いずれか）。
* **Query**：

  * `broadcaster`（**必須**）：内部 `broadcaster_id`
  * `scope`（任意, 既定=`session`）：`session`｜`since`
  * `since`（任意）：`scope=since` のときの起点時刻（ISO 8601, UTC）
* **200 OK**：

```json
{
  "version": 12345,
  "queue": [
    {
      "id": "01HZX...",
      "broadcaster_id": "b-123",
      "user_id": "u-42",
      "user_login": "alice",
      "user_display_name": "Alice",
      "user_avatar": "https://...",
      "reward_id": "r-join",
      "enqueued_at": "2025-10-12T13:00:10.000Z",
      "status": "QUEUED",
      "managed": true,
      "last_updated_at": "2025-10-12T13:00:10.000Z"
    }
  ],
  "counters_today": [
    { "user_id": "u-42", "count": 3 }
  ],
  "settings": {
    "overlay_theme": "neon",
    "group_size": 6,
    "clear_on_stream_start": true,
    "clear_decrement_counts": false,
    "policy": {
      "anti_spam_window_sec": 60,
      "duplicate_policy": "consume",
      "target_rewards": ["r-join"]
    }
  }
}
```

* **Semantics**：

  * `scope=session`：`stream.online`〜`offline` の現行セッション（オフライン時は直近セッション）。
  * `scope=since`：`since` 時刻以降の状態に必要な要素を返す。
  * **順序**：`queue` は `today_count ASC, enqueued_at ASC`（MUST）。

---

## 3. SSE — 増分配信（overlay/admin）

### 3.1 `/overlay/sse`・`/admin/sse`

* **Purpose**：パッチ（差分）をリアルタイム配信。

* **Auth**：**短寿命の署名トークン**（クエリ `token=...` または Cookie）。

* **Query**：

  * `broadcaster`（**必須**）：内部 `broadcaster_id`
  * `since_version`（任意）：初回のみ使用（EventSource 制約のため）
  * `types`（任意）：カンマ区切りの配信タイプフィルタ（例：`queue,counter,settings,stream`）
  * `include`（任意）：`status` 等の付加情報（将来拡張）
  * `token`（任意）：クエリで渡す場合のみ必須（Cookie を使う実装なら省略可）

* **Wire format（例）**：

```
id: 12346
event: patch
data: {"version":12346,"type":"queue.enqueued","at":"2025-10-12T13:00:10.123Z","data":{"entry":{...},"user_today_count":3}}

id: 12347
event: patch
data: {"version":12347,"type":"counter.updated","at":"2025-10-12T13:00:10.124Z","data":{"user_id":"u-42","count":3}}

:heartbeat
```

* **規範**：

  * **`id:` に必ず `version`**（MUST）。
  * **20–30 秒**ごとに `:heartbeat` コメント行（MUST）。
  * **リング再送**：直近 **N=1000** または **2 分**（大きい方）（MUST）。
  * リング範囲外の場合、**`state.replace`** を送る（SHOULD）。
  * **再接続**：ブラウザが送る **`Last-Event-ID`** 以降を再送（MUST）。
  * **types**：サーバ側で帯域削減のための coarse フィルタ（任意）。

* **パッチの型（代表）**：
  `queue.enqueued` / `queue.removed` / `queue.completed` / `counter.updated` /
  `settings.updated` / `redemption.updated` / `stream.online` / `stream.offline` /
  `state.replace` （詳細は `03-domain-model.md` §6）

#### `redemption.updated`

* **目的**：Helix `redemptions.update` の適用結果を配信し、UI へ管理状態を同期する。
* **データスキーマ**：

  ```json
  {
    "type": "redemption.updated",
    "version": 12351,
    "at": "2025-10-12T13:00:11.001Z",
    "data": {
      "redemption_id": "84e3...",
      "mode": "consume",            // `consume` or `refund`
      "applicable": true,             // Helix 呼び出しを実行したか
      "result": "ok",                // ok / failed / skipped
      "managed": true,                // キュー項目を Helix 管理下に置いたか
      "error": "twitch:forbidden"    // result=failed のときのみ（PII マスク済み）
    }
  }
  ```

* **規範**：

  * `managed=true` は成功し Helix と整合済みであることを示す。`managed=false` は**手動復旧が必要**（UI で警告表示）。
  * `applicable=false` の場合は Helix 呼び出しをスキップした理由を `error` に符号化する（例：`oauth:reauth-required`）。
  * `error` 文字列は PII を含めず、`prefix:slug` 形式で分類（`twitch:unauthorized`, `network:timeout` など）。
  * Queue snapshot (`state.replace`) 内の `queue[].managed` も同値で更新される（Helix 成功で `true`）。

---

## 4. 管理操作（Mutations）

> すべて **認証必須**（broadcaster の RBAC に従う）。**`op_id`（UUID）必須**で冪等（**MUST**）。

### 4.1 キュー外し（COMPLETE / UNDO）

#### `POST /api/queue/dequeue`

* **Body**：

```json
{
  "broadcaster": "b-123",
  "entry_id": "01HZX...",
  "mode": "COMPLETE",
  "op_id": "5c8d1bfc-2c2c-4d0f-8a8f-2b47a2f1f9e2"
}
```

* **mode**：`"COMPLETE"`（並び終わり、**count 不変**）｜`"UNDO"`（巻き戻し、**count -1**）
* **200 OK**：

```json
{
  "version": 12358,
  "result": {
    "entry_id": "01HZX...",
    "mode": "COMPLETE",
    "user_today_count": 3
  }
}
```

* **Side effects**：SSE に `queue.completed` または `queue.removed`（UNDO）＋必要に応じ `counter.updated` が配信。

* **エラー**：

  * `404 NOT_FOUND`（entry 不在/他 broadcaster）、`409 ALREADY_EXISTS`（終端状態への重複遷移）、
    `412 PRECONDITION_FAILED`（`op_id` 重複だが内容が矛盾する）など。

### 4.2 設定変更

#### `POST /api/settings/update`

* **Body**（部分更新）：

```json
{
  "broadcaster": "b-123",
  "patch": {
    "group_size": 6,
    "clear_on_stream_start": true,
    "policy": {
      "anti_spam_window_sec": 60,
      "duplicate_policy": "consume",
      "target_rewards": ["r-join","r-join2"]
    }
  },
  "op_id": "41bf7c3a-7d56-4c91-b94f-2e6a9f5a9a51"
}
```

* **200 OK**：

```json
{ "version": 12360, "result": { "applied": true } }
```

* **Side effects**：SSE に `settings.updated` を配信。

> **制約**：`target_rewards` に設定された Reward ID の **Helix 管理可否**は runtime で判定され、
> 更新時に `managed=true/false` が適用される（更新不能なものは記録のみ）。

---

## 5. デバッグ / 可観測

> `07-debug-telemetry.md` に運用詳細がある。ここでは I/F の規範のみ定義。

### 5.1 タップ（パイプライン可視化）

#### `GET /_debug/tap`

* **Auth**：dev でのみ無認可 or 管理者認証（本番）。
* **Query**：

  * `broadcaster`（任意）：絞り込み
  * `s`（任意）：`ingress,policy,command,projector,sse` からカンマ区切り
* **SSE**：`event: stage` / `data: StageEvent(JSON)`

  * **StageEvent（例）**：

```json
{
  "ts":"2025-10-12T13:00:10.123Z",
  "stage":"policy",
  "trace_id":"t-abc",
  "op_id":null,
  "version":12346,
  "broadcaster_id":"b-123",
  "in":{"type":"redemption.add", "...":"..."},
  "out":{"commands":[{"type":"enqueue","...": "..."}]},
  "metrics":{"elapsed_ms":7.2}
}
```

### 5.2 キャプチャ

#### `POST /_debug/capture/start`

* **Body**（任意）：

```json
{ "broadcaster": "b-123", "stages": ["ingress","policy","command","projector","sse"] }
```

* **200 OK**：`{"capture_id":"cap-20251012-1300-xyz"}`

#### `POST /_debug/capture/stop`

* **Body**：

```json
{ "capture_id": "cap-20251012-1300-xyz" }
```

* **200 OK**：`NDJSON` をバイナリ応答（`Content-Disposition: attachment`）

### 5.3 リプレイ

#### `POST /_debug/replay`

* **Body**（multipart/form-data または JSON でストア参照）：

```json
{ "source": "upload", "mode": "from-scratch" }
```

* **200 OK**：

```json
{
  "final_state": { "version": 12390, "queue": [...], "counters_today": [...], "settings": {...} },
  "patches": [{ "version": 12346, "type": "...", "data": {...} }, ...]
}
```

#### `GET /_debug/helix`

| 項目 | 内容 |
| --- | --- |
| **目的** | OAuth / Helix Backfill の健全性確認（dev / 管理者専用） |
| **クエリ** | `broadcaster`（必須, 内部 ID） |
| **レスポンス** | `200 OK`：<br>`{"broadcaster":"...","token":{"expires_at":"...","requires_reauth":false,"last_validated_at":"...","last_failure_reason":null},"checkpoint":{"status":"idle|running|error","last_run_at":"...","last_seen_at":"...","last_redemption_id":"...","cursor":"...","error_message":null,"updated_at":"..."},"managed_rewards":["reward-id"]}` |
| **注意** | アクセストークン等の秘匿情報は返さない。`requires_reauth=true` または `status=error` の場合は再同意が必要。 |

> `checkpoint.status=running` のまま `updated_at` が古いときはワーカー停止を疑う。`cursor` や `last_redemption_id` は内部重複抑止カーソルであり、参照専用。

---

## 6. OAuth / 健全性

### 6.1 `GET /oauth/login`

| 項目 | 内容 |
| --- | --- |
| **目的** | 配信者（`broadcaster`）ごとに Twitch 同意画面へ遷移する URL を生成する |
| **クエリ** | `broadcaster`（必須, 内部 ID）、`redirect_to`（任意, `/admin` など相対 URL） |
| **挙動** | `state`（ULID）と `code_verifier` を生成し、`oauth_login_states` に保存。`redirect_to` はホワイトリスト済みパスのみ許容（`/admin`, `/overlay` など）。 |
| **レスポンス** | `302 Found`（`Location` = `https://id.twitch.tv/oauth2/authorize?...`）。CSRF 保護のため `state` をクエリに含める。 |
| **エラー** | `400`（未知の `broadcaster` / `redirect_to` が不正）、`409`（同一配信者の既存 state が有効なまま再発行された場合）。 |

> `scope` は `channel:read:redemptions` / `channel:manage:redemptions` を最低含める。`login_hint` に `twitch_user_id` が既存の場合は `oauth_links` を参照し補助する。

### 6.2 `GET /oauth/callback`

| 項目 | 内容 |
| --- | --- |
| **目的** | Twitch から返ってくる `code` を交換し、`oauth_links` にアクセストークンを保存する |
| **クエリ** | `state`（必須）, `code`（必須）, `error`（任意, Twitch 失敗時） |
| **挙動** | `oauth_login_states` から `state` を引き当て CSRF を検証。`code_verifier` を用いて `TWITCH_OAUTH_TOKEN_URL` に `POST`。応答の `access_token` / `refresh_token` / `expires_in` を保存。`state` 行は消費後に削除。 |
| **レスポンス** | 成功時 `302 Found` → `redirect_to`（なければ `/admin/oauth/success`）。失敗時 `302 Found` → `/admin/oauth/error?reason=...`（エラーコードを列挙）。 |
| **エラー** | `400`（state 不一致/期限切れ）、`401`（code 交換失敗）、`409`（broadcaster が異なる state を利用）。 |

保存するアクセストークン情報：

* `access_token`（平文保存だが OS 権限で保護、ログ/Tap では完全マスク）
* `refresh_token`
* `expires_at`（UTC ISO8601）
* `managed_scopes_json`（最終的に付与された scope の配列）
* `last_validated_at` / `last_refreshed_at` / `requires_reauth`（自動更新）

### 6.3 `POST /oauth2/validate`

| 項目 | 内容 |
| --- | --- |
| **目的** | 既存トークンの健全性確認・自動 refresh・Backfill のトリガ |
| **Body** | `{"broadcaster":"<internal-id>","force":false}`（`force=true` で期限内でも検証） |
| **レスポンス** | `200 OK`：`{"status":"ok|refresh|reauth","managed_rewards":[],"next_check_at":"..."}`。`reauth` の場合は管理 UI で再同意導線を表示。 |
| **副作用** | `oauth_links.requires_reauth` 更新、refresh/validate 結果を `StageKind::Oauth` タップに publish、正常完了時は Helix Backfill ワーカーへ `broadcaster` を即時通知。 |
| **エラー** | `404`（リンクが存在しない）、`409`（別プロセスが refresh 実行中）、`500`（Twitch API 失敗）。 |

### 6.4 健全性

* `GET /healthz`：`200 OK`（依存ヘルス簡易チェック）
* `GET /metrics`：Prometheus テキストフォーマット

---

## 7. セキュリティ・レート制御

* **SSE トークン**：短寿命（5〜15分）、用途（overlay/admin）限定の `aud`、`sub=broadcaster_id`。
* **CORS**：原則同一オリジン。開発時はプロキシで同一化（Vite）。
* **レートリミット**（推奨）：

  * Mutation：`/api/queue/dequeue`（`X-RateLimit-*` を返す）
  * Debug：`/_debug/*` は厳しめ（管理者のみ）
* **PII**：API 応答には必要最小限（表示名・アイコン URL）。ログ/タップはマスク既定。

---

## 8. 例外系・境界条件

* `since` が未来：`400 INVALID_ARGUMENT`。
* `since_version` が最新より新しい：空配信 → 心拍のみ。
* リング不足：`state.replace` をサーバから自動送信（`version` は現行）。
* `op_id` 重複：

  * **同一内容**：`200 OK`（冪等成立）
  * **矛盾**：`412 PRECONDITION_FAILED`（detail に既存記録のハッシュなど）

---

## 9. 手動検証の最小コマンド例

> 参考：Linux（bash） / Windows（PowerShell）。実際の値は適宜置換。

### 9.1 State の取得

```bash
curl -sS "http://127.0.0.1:8080/api/state?broadcaster=b-123&scope=session" | jq .
```

### 9.2 SSE の受信（overlay）

```bash
curl -N "http://127.0.0.1:8080/overlay/sse?broadcaster=b-123&since_version=12345&token=eyJ..."
```

### 9.3 キュー外し（COMPLETE）

```bash
curl -sS -X POST http://127.0.0.1:8080/api/queue/dequeue \
  -H "Content-Type: application/json" \
  -d '{"broadcaster":"b-123","entry_id":"01HZX...","mode":"COMPLETE","op_id":"'"$(uuidgen)"'"}' | jq .
```

### 9.4 設定更新

```bash
curl -sS -X POST http://127.0.0.1:8080/api/settings/update \
  -H "Content-Type: application/json" \
  -d '{"broadcaster":"b-123","patch":{"group_size":6},"op_id":"'"$(uuidgen)"'"}' | jq .
```

### 9.5 Tap（policy のみ）

```bash
curl -N "http://127.0.0.1:8080/_debug/tap?broadcaster=b-123&s=policy"
```

---

## 10. 受け入れチェック（本章適合）

* [ ] Webhook：verification=200+平文、notification=204 即 ACK、HMAC/±10 分/`Message-Id` 冪等
* [ ] State：`/api/state` が `version` 付きで初期化可能（`scope=session|since`）
* [ ] SSE：`id=version`、心拍（20–30s）、リング再送、`Last-Event-ID` 補償、`state.replace` フォールバック
* [ ] Mutation：`op_id` 冪等（COMPLETE/UNDO/Settings）、SSE に差分配信
* [ ] Debug：Tap/Capture/Replay が動作
* [ ] セキュリティ：SSE トークン（短寿命, aud/sub/exp）、RBAC、PII マスク
* [ ] エラー：RFC 7807 に準拠、代表ケースに正しいコードを返す

本章に適合しない設計・実装が見つかった場合、**先に本章を更新**し、関連文書（`02/03/05/07`）を合わせて修正してください。
