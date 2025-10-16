# `.docs/07-debug-telemetry.md` — デバッグ / テレメトリ仕様（規範）

> 本章は **可観測性（Observability）** の一次仕様です。
> すべての実装はここに記す **Tap（SSE）／Capture／Replay／構造化ログ／メトリクス** の契約に適合しなければなりません（**MUST**）。
> API のエンドポイントは `04-api-contracts.md` にも記載がありますが、**本章が詳細の規範**です。

---

## 1. 目的と設計原則

* **目的**：

  1. **データフローの可視化**（Ingress → Normalizer → Policy → Command → Projector → SSE）
  2. **決定的再現**（Capture/Replay）
  3. **計測と警報**（Prometheus メトリクス + 構造化ログ）

* **原則**：

  * **ゼロ手戻りの原因特定**：各ステージで**入力/出力/遅延**を記録（**MUST**）。
  * **本番安全**：PII/Secrets は**既定でマスク**、Debug エンドポイントは**管理者のみ**（**MUST**）。
  * **決定性**：Capture→Replay で**同一最終状態**に到達（**MUST**）。
  * **軽量**：Tap/メトリクスは**低オーバーヘッド**、落としても**本系に影響しない**（**MUST**）。

---

## 2. ステージの定義（名称は固定）

| ステージ名        | 意味                                           | 代表出所                            |
| ------------ | -------------------------------------------- | ------------------------------- |
| `ingress`    | Webhook 受信/検証（HMAC/±10分/冪等）                  | `app::routes::eventsub_webhook` |
| `normalizer` | EventSub → ドメインイベント変換                        | `core::normalizer`              |
| `policy`     | ドメインイベント → Command（enqueue/refund/consume/…） | `core::policy`                  |
| `command`    | CommandLog への append（version++）              | `app::command_bus`              |
| `projector`  | 状態適用と Patch 生成                               | `core::projector`               |
| `sse`        | SSE Hub 配信（id=version, 心拍, リング）              | `app::sse::hub`                 |
| `storage`    | DB 操作の要所（TTL/WAL/Backfill）                   | `storage::*`                    |
| `oauth`      | `/oauth2/validate` / refresh / 購読棚卸し         | `twitch::oauth/subscriptions`   |

> **規範**：上記ステージ名は Tap/ログ/メトリクスの **label 値として固定**（**MUST**）。

---

## 3. Tap（パイプライン可視化）— SSE

### 3.1 エンドポイントとクエリ（規範）

* `GET /_debug/tap`（SSE）

  * `broadcaster`（任意）：絞り込み（未指定は**全配信者**のイベント）
  * `s`（任意）：`ingress,normalizer,policy,command,projector,sse,storage,oauth` からカンマ区切り
  * `q`（任意）：文字列フィルタ（`data`/`meta` に対する **単純部分一致**）
  * `rate`（任意）：サンプリング（例：`rate=0.1` で 10%）
  * **Auth**：dev では無認可可、本番は**管理者認証必須**（**MUST**）

### 3.2 ワイヤ形式

* `event: stage`
* `data: StageEvent(JSON)`
* 20–30s ごとに `:heartbeat`

#### StageEvent スキーマ（規範）

```json
{
  "ts": "2025-10-12T13:00:10.123Z",      // 送信時刻（UTC）
  "stage": "policy",                      // §2 の固定語彙
  "trace_id": "t-0d1a...",               // 1 リクエスト起点の相関 ID（Webhook/Mutation 等）
  "op_id": null,                          // 管理操作時のみ UUID、その他は null
  "version": 12346,                       // コマンド適用後にのみ付与、前段は null でも可
  "broadcaster_id": "b-123",
  "meta": {
    "msg_id": "d3c2...",                  // ingress のときのみ
    "event_type": "redemption.add",       // 正規化/ポリシーでのイベント/コマンド種別
    "size_bytes": 2048,                   // data の概算サイズ
    "latency_ms": 7.2,                    // ステージ内部処理時間（測定点は各実装で統一）
    "thread": "tokio-0"                   // 任意
  },
  "in":  { "redacted": true,  "payload": { "...": "..." } },  // 入力（必要に応じマスク）
  "out": { "redacted": false, "payload": { "...": "..." } }   // 出力（Command/Patch 等）
}
```

* **マスク規約（MUST）**：`user_login`, `access_token`, `refresh_token`, `Authorization` 等は `***` に置換。
* **サイズ上限**：`payload` は**64 KiB を上限**、超過時は切り詰め `truncated=true` を付与（**MUST**）。
* **背圧**：クライアントが遅い場合、**最古イベントからドロップ**（`dropped=N` の Tap 内メトリクスを増加）。

**Storage ステージ固有のメッセージ**：TTL/WAL ジョブは `stage="storage"` で `meta.message ∈ {"ttl.event_raw","ttl.command_log","wal.checkpoint"}` を publish し、`out.payload.deleted` や `out.payload.busy` などの統計を含める（MUST）。

### 3.3 UI（任意）

* `GET /_debug/tap/ui`：簡易 HTML（フィルタ、Pause、JSON 展開）。**本番では管理者のみ**。

---

## 4. Capture / Replay（決定的再現）

### 4.1 Capture

* `POST /_debug/capture/start` → `{"capture_id":"cap-20251012-1300-xyz"}`

  * Body（任意）：`{ "broadcaster": "b-123", "stages": ["ingress","policy","command","projector"], "max_bytes": 10485760 }`
* `POST /_debug/capture/stop` → NDJSON（`Content-Disposition: attachment; filename="cap-*.ndjson"`）

**NDJSON 行の型（規範）**

```json
{ "kind":"stage","ts":"...","stage":"policy","trace_id":"...","op_id":null,"version":123,"broadcaster_id":"b-123","meta":{...},"in":{...},"out":{...} }
{ "kind":"event_raw","ts":"...","broadcaster_id":"b-123","msg_id":"...","type":"redemption.add","payload":{...} }
{ "kind":"command","ts":"...","broadcaster_id":"b-123","version":123,"type":"enqueue","payload":{...} }
{ "kind":"patch","ts":"...","broadcaster_id":"b-123","version":124,"type":"queue.enqueued","data":{...} }
{ "kind":"metrics","ts":"...","name":"projector_latency_ms","value":7.2,"labels":{"broadcaster":"b-123"} }
```

* **既定**：`kind ∈ {"stage","command","patch"}` を収集。`event_raw` は opt-in。
* **上限**：`max_bytes` 既定 10 MiB、超過で stop + `problem+json`（**MUST**）。

### 4.2 Replay

* `POST /_debug/replay`（multipart または JSON）

  * Body 例：`{ "source": "upload", "mode": "from-scratch" }`
* **モード**：

  * `from-scratch`：**空のメモリ状態**に対し `command` の順序で適用 → 最終 state を返却（**MUST**）
  * `from-state`：現行 DB state をベースに差分検証（任意）
* **200 OK**：

```json
{
  "final_state": { "version": 12390, "queue": [...], "counters_today": [...], "settings": {...} },
  "patches": [{ "version": 12346, "type": "...", "data": {...} }, ...],
  "stats": { "commands": 45, "patches": 73, "duration_ms": 120.4 }
}
```

* **決定性（MUST）**：同じ入力で**同一 `final_state.version` と `queue/counters/settings`** を得る。
* **安全**：Replay は**DB に書き込まない**（**MUST**）。完全に**分離したメモリ投影**で実施。

---

## 5. 構造化ログ（tracing）

### 5.1 ログ形式

* **prod**：JSON（1 行 1 イベント）
* **dev**：pretty（人間可読）

**共通キー（規範）**

| key                          | 例                          | 備考                        |
| ---------------------------- | -------------------------- | ------------------------- |
| `ts`                         | `2025-10-12T13:00:10.123Z` | UTC                       |
| `level`                      | `INFO` `WARN` `ERROR`      |                           |
| `stage`                      | `policy`                   | §2                        |
| `trace_id`                   | `t-0d1a...`                |                           |
| `op_id`                      | UUID or null               |                           |
| `version`                    | 12346                      | command/projector/sse で付与 |
| `broadcaster_id`             | `b-123`                    |                           |
| `msg_id`                     | `d3c2-...`                 | ingress で付与               |
| `event_type`                 | `redemption.add`           |                           |
| `latency_ms`                 | 7.2                        |                           |
| `size_bytes`                 | 2048                       | 入力/出力の概算                  |
| `http.status`                | 204                        | ingress 等                 |
| `error.kind`/`error.message` | `HelixForbidden` / `...`   | エラー時のみ                    |
| `redacted`                   | true/false                 | PII マスクが適用されたか            |

* **PII/Secrets**：トークン・パスワード・生の入力文字列は**必ず `***`** に置換（**MUST**）。
* **サンプリング**：INFO は 1.0（既定）→負荷に応じ 0.1 まで落として良い（ERROR/WARN は常に 1.0）。

---

## 6. メトリクス（Prometheus）

### 6.1 エンドポイント

* `GET /metrics`（テキストフォーマット）

### 6.2 指標（規範名）と意味

> **ラベル設計**：**高カーディナリティ禁止**。`broadcaster` は **ID を短縮（例：先頭 6 文字）**するか**未付与**。`stage`/`type`/`kind` は固定語彙のみ。

**Ingress**

* `eventsub_ingress_total{type}` **counter**：検証成功件数
* `eventsub_invalid_signature_total` **counter**
* `eventsub_clock_skew_seconds` **histogram**（|now - timestamp|）
* `webhook_ack_latency_seconds` **histogram**

**Policy / Projector**

* `policy_commands_total{kind}` **counter**（enqueue/refund/consume/clear/settings）
* `projector_patches_total{type}` **counter**
* `projector_latency_seconds` **histogram**

**SSE**

* `sse_clients{aud}` **gauge**（overlay/admin/debug）
* `sse_broadcast_latency_seconds` **histogram**
* `sse_ring_size{aud}` **gauge**（現在リング保持数）
* `sse_ring_miss_total{aud}` **counter**（リング外 → `state.replace`）

**DB / TTL**

* `db_ttl_deleted_total{table}` **counter** — `table ∈ {event_raw, command_log}`。TTL ジョブ 1 バッチあたりの削除件数を加算。
* `db_checkpoint_seconds` **histogram** — `wal_checkpoint(TRUNCATE)` の実行時間（秒）。
* `db_busy_total{op}` **counter**（busy_timeout 到達）— `op ∈ {ttl, checkpoint}`。ロック競合で処理をスキップした回数。

**OAuth / Backfill**

* `oauth_validate_failures_total` **counter**
* `backfill_processed_total` **counter**
* `backfill_duplicates_total` **counter**

**App**

* `app_build_info{version,git}` **gauge**（常時 1）
* `app_uptime_seconds` **counter**

### 6.3 SLO とアラート例（任意）

* **SLO**：`webhook_ack_latency_seconds{}` p95 < 0.2、`sse_broadcast_latency_seconds{}` p95 < 0.05
* **Alert**：`sse_ring_miss_total` の増加、`oauth_validate_failures_total` のスパイク、`eventsub_invalid_signature_total` 上昇

---

## 7. 実装要件（Tap/メトリクスの落とし穴回避）

* **非同期チャンネル**：Tap の送出は**非同期 broadcast**で行い、本系（Webhook ACK / SSE 配信）を**絶対にブロックしない**（**MUST**）。
* **リングとドロップ**：Tap/SSE ともに**リングバッファ**を持ち、溢れたら**最古から破棄**（**MUST**）。
* **時計**：`Clock` 抽象を注入し、テストで**固定時刻**を使う（**MUST**）。
* **サイズ制限**：Tap の `payload` は 64 KiB、Capture 全体は既定 10 MiB（§4）（**MUST**）。
* **Windows/Ubuntu 一致**：`monotonic clock` ではなく **UTC 時刻**で記録（**MUST**）。
* **バックプレッシャ**：SSE クライアント増加時の送出は**ワーカー分散**（broadcast channel → per-connection queue）。

---

## 8. セキュリティ / 運用規約

* **Auth**：`/_debug/*` は dev 以外で**管理者認証必須**（**MUST**）。
* **PII/Secrets**：

  * Tap/ログ/キャプチャは**既定でマスク**（ユーザ名は `"A***e"` のように先頭/末尾のみ可視）。
  * アクセストークン類は**完全マスク**。
* **Rate Limit**：`/_debug/*` に IP 単位のレート制御を推奨。
* **保存**：`/_debug/capture` の生成ファイルは**自動削除**（既定 24h）。
* **CORS**：`/_debug/*` は同一オリジンのみ。

---

## 9. テスト観点（自動化）

* **Unit**：

  * StageEvent マスク関数（秘密が漏れない）
  * メトリクス記録（名前/ラベル）
* **Integration**：

  * tap を購読しながら Webhook→SSE の最短経路で**各ステージが出力**される
  * Capture→Replay で **final_state が一致**
* **Property**：

  * 同一入力 NDJSON → 同一 `final_state.version`（決定性）
* **Perf**：

  * 1,000 イベント/分で `webhook_ack_latency_seconds` p95 < 0.2 を確認

---

## 10. 開発者の手動検証レシピ

1. **Tap を開く**
   `curl -N "http://127.0.0.1:8080/_debug/tap?s=ingress,policy,command,projector,sse&broadcaster=b-dev"`
2. **モック送出**（別端末で）
   `curl -X POST http://127.0.0.1:8080/api/admin/mock -d @samples/redemption_add.json`
3. **SSE を確認**
   `curl -N "http://127.0.0.1:8080/overlay/sse?broadcaster=b-dev&since_version=0&token=eyJ..."`
4. **Capture/Replay**

   * `POST /_debug/capture/start` → しばらく操作 → `.../stop` → ファイル保存
   * `POST /_debug/replay`（アップロード）→ `final_state` と `patches` を確認
5. **メトリクス**
   `curl http://127.0.0.1:8080/metrics | grep sse_clients`

---

## 11. 受け入れチェック（本章適合）

* [ ] Tap：`/_debug/tap` が**全ステージ**（§2）を **SSE で観測**、PII マスク/サイズ制限が効く
* [ ] Capture/Replay：**NDJSON** を生成し、`from-scratch` で **同一最終状態**に到達
* [ ] 構造化ログ：キー体系（§5.1）が満たされ、機微情報が含まれない
* [ ] メトリクス：規範名（§6.2）が `/metrics` に出現、ラベルの高カーディナリティなし
* [ ] バックプレッシャ：Tap/SSE のリングが溢れた場合に**安全にドロップ**し本系に影響なし
* [ ] セキュリティ：`/_debug/*` が dev 以外で**管理者認証必須**、自動削除/レート制御
* [ ] クロス OS：Linux/Windows の CI で Tap/Capture/Replay/metrics の最小テストが通る

---

本章は**規範**です。実装や運用で矛盾が生じた場合は、**先に本章を更新**し、`02/03/04/05/12` の整合を取ってから実装を変更してください。
