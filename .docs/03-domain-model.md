# `.docs/03-domain-model.md` — ドメインモデル（規範）

> 本章はシステムの**概念・エンティティ・不変条件・状態遷移**を規範として定義します。
> 設計・実装は本章に従うこと（**MUST**）。I/F は `04-api-contracts.md`、データ定義は `05-data-schema-and-migrations.md` を併読してください。

---

## 1. 基本方針と用語

* **一次ソース**：**CommandLog（append‑only）** が全状態変化の一次ソース（**MUST**）。
* **version**：配信者（broadcaster）単位の**単調増加整数**。SSE の `id:` はこの **version** を載せる（**MUST**）。
* **op_id**：管理操作の**冪等キー（UUID）**。同一 `op_id` は 1 回のみ反映（**MUST**）。
* **今日（day）**：`broadcaster.timezone` の 0:00 を境に日付切替（内部保存は UTC）（**MUST**）。
* **配信内（session）**：`stream.online`〜`stream.offline` の区間。`/api/state?scope=session` の境界として使用（**MUST**）。
* **対象リワード**：配信者設定で指定する `policy_points_target_reward_ids[]` に含まれる Reward ID 群。

---

## 2. アイデンティティ・時刻・多テナント

### 2.1 識別子（ID）の規約

* **broadcaster_id**：配信者内部 ID（UUID など）。`twitch_broadcaster_id` と 1:1。
* **twitch_*_id**：Twitch 側の ID（string）。
* **version**：`int64` 単調増加（broadcaster 単位）。
* **op_id**：UUID v4（文字列）。
* **entry_id**：QueueEntry の内部 ID（ULID/UUID いずれかでよい）。
* **msg_id**：Webhook の `Twitch-Eventsub-Message-Id`（一意）。

### 2.2 時刻の規約

* **保存は UTC**（ISO 8601 / `TIMESTAMP WITH TIME ZONE` 相当）。
* **“今日”判定**は `broadcaster.timezone` に従う（`Europe/Berlin` 等の IANA 名）。
* **反スパム時間窓**：`settings.policy.anti_spam_window_sec`（既定 60）内の重複は特別処理。

### 2.3 多テナント分離

* あらゆる行（Row）・API 呼出は **broadcaster_id** を必須とする（**MUST**）。
* ユーザ権限は `role ∈ {superadmin, broadcaster, operator}`。
* SSE 認可は短寿命署名トークンに `sub=broadcaster_id, aud ∈ {overlay,admin}` を含む（検証必須）。

---

## 3. コア・エンティティ（永続）

> 物理スキーマは `05-data-schema-and-migrations.md` に準拠。ここでは論理モデルを定義します。

### 3.1 Broadcaster（配信者）

```ts
Broadcaster {
  id: string,                    // 内部ID
  twitch_broadcaster_id: string,
  display_name: string,
  timezone: string,              // IANA TZ
  settings: Settings
}
```

**Settings（設定）**

```ts
Settings {
  overlay_theme: string,         // 例: "neon", "pastel"
  group_size: number,            // 表示グループの粒度（フロント指標）
  clear_on_stream_start: boolean,
  clear_decrement_counts: boolean, // クリア時に今日の回数を減算するか（既定:false）
  policy: {
    anti_spam_window_sec: number,     // 例: 60
    duplicate_policy: "consume"|"refund", // 衝突時優先ルール（既定:"consume"）
    target_rewards: string[]          // 対象Reward ID群（空=すべて無効）
  }
}
```

### 3.2 User（内部ユーザ）

```ts
User {
  id: string,
  email: string,
  password_hash: string,
  role: "superadmin"|"broadcaster"|"operator",
  broadcaster_id?: string         // roleに応じて必須
}
```

### 3.3 OAuthLink（Twitch 連携）

```ts
OAuthLink {
  id: string,
  broadcaster_id: string,
  twitch_user_id: string,
  scopes: string[],
  access_token: string,
  refresh_token: string,
  expires_at: string // UTC
}
```

### 3.4 EventRaw（受信イベント生ログ・72h）

```ts
EventRaw {
  id: string,                     // ULID/UUID
  broadcaster_id: string,
  msg_id: string,                 // Twitch-Eventsub-Message-Id (unique)
  type: "redemption.add"|"redemption.update"|"stream.online"|"stream.offline"|string,
  payload_json: string,
  event_at: string,               // イベント発生時刻(UTC)
  received_at: string,            // 受信時刻(UTC)
  source: "webhook"
}
```

* **TTL**：72 時間（小分け DELETE）。
* **冪等**：`msg_id` unique。

### 3.5 CommandLog（操作ログ・72h・一次ソース）

```ts
CommandLog {
  version: number,                // PK (broadcaster_id, version)
  broadcaster_id: string,
  op_id?: string,                 // 管理操作の冪等キー (UUID)
  type: CommandType,
  payload_json: string,           // Command の内容（正規化構造）
  created_at: string              // UTC
}
```

**CommandType（代表）**

* `enqueue`（QueueEntry 追加）
* `counter.increment` / `counter.decrement`
* `redemption.update`（`refund` or `consume`、Helix 呼出の意図と結果）
* `queue.complete` / `queue.remove`（COMPLETE/UNDO）
* `queue.clear_session_start`（配信開始クリア）
* `settings.update`

> **規範**：Command は **1 操作 = 1 記録**。管理操作は **`op_id` 冪等**。

### 3.6 StateIndex（version 採番）

```ts
StateIndex {
  broadcaster_id: string,         // PK
  current_version: number,        // 単調増加
  updated_at: string              // UTC
}
```

* **規範**：`CommandLog` append と version 更新は**同一トランザクション**（**MUST**）。

### 3.7 QueueEntry（待機と履歴）

```ts
QueueEntry {
  id: string,
  broadcaster_id: string,
  user_id: string,                // Twitch user id
  user_login: string,
  user_display_name: string,
  user_avatar: string|null,       // 表示用途
  reward_id: string,
  enqueued_at: string,            // UTC
  status: "QUEUED"|"COMPLETED"|"REMOVED",
  status_reason?: "UNDO"|"STREAM_START_CLEAR"|"EXPLICIT_REMOVE"|string,
  managed: boolean,               // Helix 更新が適用されたか（true/false）
  last_updated_at: string         // UTC
}
```

* **表示順**：`ORDER BY today_count ASC, enqueued_at ASC`（**MUST**）。`today_count` は `DailyCounter` を参照。

### 3.8 DailyCounter（“今日の回数”）

```ts
DailyCounter {
  day: string,                    // "YYYY-MM-DD" in broadcaster.timezone
  broadcaster_id: string,
  user_id: string,
  count: number,
  updated_at: string              // UTC
}
```

* **更新規約**：`enqueue` ⇒ `count++`、`UNDO` ⇒ `count--`、`COMPLETE` ⇒ 変化なし。
* **境界**：`timezone` の 0:00 切替で新 day を開始。

### 3.9 StreamSession（配信内）

```ts
StreamSession {
  id: string,
  broadcaster_id: string,
  started_at: string,             // from stream.online
  ended_at: string|null           // set on stream.offline
}
```

* `/api/state?scope=session` は `ended_at IS NULL` の最新を対象（オンライン中）／直近終了セッション（オフライン時）。

---

## 4. 正規化イベント（Normalizer の出力）

**型**（代表）

```ts
NormalizedEvent =
 | { type: "redemption.add", broadcaster_id, user, reward, occurred_at, redemption_id }
 | { type: "redemption.update", broadcaster_id, user, reward, occurred_at, redemption_id, status }
 | { type: "stream.online", broadcaster_id, occurred_at }
 | { type: "stream.offline", broadcaster_id, occurred_at }
```

**user**: `{ id, login, display_name, avatar }`
**reward**: `{ id, title, cost }`

* **規範**：正規化は**決定的**（同入力→同出力）。
* **event_at/occurred_at** は Twitch のイベント発生時刻、`received_at` はサーバ受信時刻。

---

## 5. コマンド（Policy/Mutation の出力）

**共通フィールド**：`{ broadcaster_id, issued_at, source, ... }` で構成。`source ∈ {policy, admin}`。

### 5.1 enqueue

```ts
{ type: "enqueue",
  user, reward, redemption_id,
  managed: boolean|null // Helix 更新可否を後段で埋める
}
```

* `DailyCounter.count++` のトリガ。

### 5.2 redemption.update（Helix 作用）

```ts
{ type: "redemption.update",
  redemption_id, mode: "refund"|"consume",
  applicable: boolean,            // 自アプリ作成リワードなら true
  result: "ok"|"failed"|"skipped",
  error?: string
}
```

* `applicable=false` の場合は `skipped`。`enqueue` 自体は継続。

### 5.3 queue.complete / queue.remove（管理操作）

```ts
// COMPLETE: 並び終わり（count 不変）
{ type: "queue.complete", entry_id, op_id }

// UNDO: 巻き戻し（count--）
{ type: "queue.remove", entry_id, reason: "UNDO", op_id }
```

* **規範**：`op_id` 冪等。二重送信は no-op。

### 5.4 queue.clear_session_start（配信開始クリア）

```ts
{ type: "queue.clear_session_start",
  decrement_counts: boolean // Settings に従う
}
```

### 5.5 settings.update

```ts
{ type: "settings.update", patch: Partial<Settings>, op_id }
```

---

## 6. パッチ（Projector の出力 → SSE）

> SSE の `event:` 名は `twitch` ではなく、**論理イベント**を表す名称で構いません（`04-api-contracts.md` で規定）。

**共通**：`{ version, type, data, at }`

### 6.1 代表パッチ

```ts
{ version, type: "queue.enqueued", data: { entry, user_today_count }, at }
{ version, type: "queue.removed",  data: { entry_id, reason, user_today_count }, at }
{ version, type: "queue.completed",data: { entry_id }, at }
{ version, type: "counter.updated",data: { user_id, count }, at }
{ version, type: "settings.updated", data: { patch }, at }
{ version, type: "stream.online", data:{ session_id }, at }
{ version, type: "stream.offline", data:{ session_id }, at }
```

### 6.2 フォールバック

```ts
{ version, type: "state.replace", data: { state }, at }
// state = { version, queue:[...], counters_today:[...], settings:{...} }
```

* **規範**：リング再送の範囲外なら必ず `state.replace` を送る（**SHOULD**）。

---

## 7. 不変条件（Invariants）

1. **CommandLog.version は単調増加**（broadcaster 単位）（**MUST**）。
2. **Message‑Id 冪等**：`EventRaw.msg_id` の一意制約（**MUST**）。
3. **`op_id` 冪等**：管理操作は同一 `op_id` を 1 回に集約（**MUST**）。
4. **QueueEntry 状態遷移**：

   * `QUEUED` → `COMPLETED`（COMPLETE）
   * `QUEUED` → `REMOVED`（UNDO/EXPLICIT/CLEAR）
   * `COMPLETED`/`REMOVED` → **終端**（**MUST**: 再度 QUEUED に戻さない）
5. **Counter 更新規約**：`enqueue: +1`、`UNDO: -1`、`COMPLETE: ±0`（**MUST**）。
6. **表示順**：`ORDER BY today_count ASC, enqueued_at ASC`（**MUST**）。
7. **セッション境界**：`stream.online/offline` で 1 セッション（**MUST**）。
8. **保持**：`EventRaw`/`CommandLog` は 72h TTL（**MUST**）。`Queue`/`Counter`/`Settings` は永続（**MUST**）。
9. **決定性**：Normalizer/Policy/Projector は同入力に対して同出力（**MUST**）。Capture/Replay で再現可能（**MUST**）。

---

## 8. 反スパムの定義（既定）

* **定義**：同一 `user_id` が同一 `reward_id` を**`anti_spam_window_sec` 秒内**に 2 回以上引き換えた場合、2 回目以降は `consume` 優先。
* **判定データ**：`EventRaw` または `QueueEntry` の `enqueued_at` を参照。
* **可否**：`duplicate_policy` が `"refund"` の場合は返金を優先。

> Helix 更新は **自アプリ作成リワード** のみ適用可。それ以外は `applicable=false` で `skipped` とする。

---

## 9. 例：ドメインシーケンス

### 9.1 Redemption → Enqueue → Refund/Consume

1. `EventRaw(redemption.add)` 受信（msg_id 差分）
2. Normalizer → `NormalizedEvent(redemption.add)`
3. Policy → `enqueue` ＋ `redemption.update(mode="refund"|"consume")`
4. CommandLog append（version=N, N+1）
5. Projector →

   * `queue.enqueued`（`entry`, `user_today_count++`）
   * `counter.updated`（同時に送る or `queue.enqueued` に含める）
   * Helix 実行結果は `redemption.update.result` に記録（`ok|failed|skipped`）

### 9.2 Admin COMPLETE／UNDO

* **COMPLETE**：`queue.complete` → QueueEntry: `COMPLETED`, Counter: 不変 → `queue.completed`
* **UNDO**：`queue.remove(reason="UNDO")` → QueueEntry: `REMOVED`, Counter: `-1` → `queue.removed` + `counter.updated`

---

## 10. エラー処理／部分失敗

* **Helix 失敗**：`redemption.update.result="failed"` にエラー内容を格納。`enqueue` は成立（`managed=false`）。
* **SSE 欠落**：リング超過は `state.replace` を送って修復。
* **Backfill**：資格復旧時に `UNFULFILLED` を取得し、`enqueue` を補完（重複は `msg_id/op_id/version` で抑止）。

---

## 11. ミニ JSON 例

### 11.1 `queue.enqueued` パッチ

```json
{
  "version": 1024,
  "type": "queue.enqueued",
  "at": "2025-10-12T13:00:10.123Z",
  "data": {
    "entry": {
      "id": "01HZX...JK",
      "broadcaster_id": "b-123",
      "user_id": "u-42",
      "user_login": "alice",
      "user_display_name": "Alice",
      "user_avatar": "https://...",
      "reward_id": "r-join",
      "enqueued_at": "2025-10-12T13:00:10.000Z",
      "status": "QUEUED",
      "managed": true
    },
    "user_today_count": 3
  }
}
```

### 11.2 `queue.removed`（UNDO）

```json
{
  "version": 1030,
  "type": "queue.removed",
  "at": "2025-10-12T13:05:00.000Z",
  "data": {
    "entry_id": "01HZX...JK",
    "reason": "UNDO",
    "user_today_count": 2
  }
}
```

---

## 12. セキュリティ・プライバシ（ドメイン観点）

* **PII**：`user_display_name`/`user_login`/`user_avatar` は**表示目的**。ログ/タップでは既定でマスク。
* **多テナント**：`broadcaster_id` が外部流出しないよう、SSE トークンのスコープを限定。
* **最小権限**：Helix 更新は対象スコープが揃う場合のみ実施。不可なら**記録のみ**（`managed=false`）。

---

## 13. 実装指針への写像

* `core/`：`types`（NormalizedEvent, Command, Patch）, `policy`, `projector`
* `storage/`：`repositories`（EventRaw, CommandLog, QueueEntry, DailyCounter, Settings, StateIndex）
* `app/`：Webhook 受信（Ingress）, State REST, SSE Hub（`id=version`）, Debug Tap
* `twitch/`：OAuth/Helix/EventSub 購読
* `util/`：config, tracing, token 署名, Clock 抽象

---

## 14. 受入チェック（本章適合）

* [ ] すべての状態変化が **CommandLog** を経由（append-only / version++）。
* [ ] QueueEntry の**状態遷移**・Counter の**更新規約**・**表示順**を満たす。
* [ ] `op_id` / `msg_id` / `version` の**冪等**が保証される。
* [ ] “今日”と“配信内”の定義が仕様通り。
* [ ] 反スパム判定が設定に従い**決定的**。
* [ ] Helix 作用の可否が `applicable/result/managed` に正しく反映される。
* [ ] `EventRaw`/`CommandLog` は 72h TTL、他は永続。
* [ ] Capture/Replay で同じ state/patch 群を再現できる。

本章に適合しない発見があれば、**先に本章を更新**し、関連章（`02/04/05` 等）の整合を取ってから実装を変更してください。
