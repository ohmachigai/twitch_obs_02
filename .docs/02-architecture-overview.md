# `.docs/02-architecture-overview.md` — アーキテクチャ概要（規範）

> 本章は本システムの**構成要素・データフロー・不変条件・失敗時の挙動**を規範として定義します。
> 実装・テスト・運用は以下の要件を満たさなければなりません（**MUST**）。詳細な I/F は `04-api-contracts.md` を参照。

---

## 1. 目的と設計原則

**目的**：Twitch EventSub 通知を受けて、配信者ごとの**待機キューと“今日の回数”**を正しく管理し、**OBS オーバーレイ**および**管理 UI**に**初期状態＋差分**を配信する。
**原則**：

* **単一の真実**：**CommandLog（append-only, version 単調増加）**を一次ソースにする（MUST）。
* **二段配信**：初期は **REST `/api/state`**、以降は **SSE パッチ**（MUST）。
* **冪等性**：入力（`Message-Id`）、管理操作（`op_id`）、出力（`id=version`）で**必ず冪等**（MUST）。
* **多テナント分離**：全 API/SSE は **`broadcaster` コンテキスト**で評価（MUST）。
* **可視化と再現**：各ステージで**Tap/Capture/Replay**が可能（MUST）。

---

## 2. 全体構成（コンポーネント図）

```
[Twitch EventSub] -> (1) Ingress(Webhook)
     |                     ├─ HMAC/±10分/冪等 → 204 即ACK
     |                     └─ EventRaw(72h) へ保存
     v
(2) Normalizer  →  (3) Policy Engine  →  (4) Command Log (version++)
                                           │
                                           v
                                   (5) Projector ─┬─ update Queue/Counters/Settings
                                                   └─ (6) SSE Hub → /overlay.sse, /admin.sse
                                                            id=version / 心拍 / リング再送
   ↑                       ↑
   |                  (8) Admin Mutations (COMPLETE/UNDO/Settings) → Command Log → Projector
   └──── (7) REST /api/state（初期スナップショット） ──────────────────────────────┘

デバッグ系： Tap(SSE) / Capture / Replay は (1)〜(6) の各ステージから観測可能
```

---

## 3. 主要インバリアント（破ってはならない契約）

1. **Webhook 即時 ACK**：verification は 200+平文、notification は **204**（MUST）。
2. **署名・時刻**：HMAC-SHA256（`Message-Id || Timestamp || RawBody`）、±10 分内（MUST）。
3. **入力冪等**：`Message-Id` 重複は取り込みスキップ（MUST）。
4. **一次ソース**：**CommandLog.version（単調増加、broadcaster 単位）**（MUST）。
5. **SSE 再送**：**`id=version`**、**20–30s 心拍**、**リング再送**（MUST）。
6. **初期＋増分**：初回は `/api/state`（**`version` 付き**）、以降は SSE（MUST）。
7. **管理操作冪等**：全 Mutation は **`op_id`（UUID）必須**で一回だけ反映（MUST）。
8. **多テナント**：`broadcaster` を要求し、権限はその配信者に限定（MUST）。
9. **保持**：`EventRaw`/`CommandLog` は **72h TTL**、Queue/Counters/Settings は永続（MUST）。
10. **可視化**：Tap（SSE）で全ステージを観測できる（MUST）。

---

## 4. ステージ別の責務と I/F

### 4.1 (1) Ingress（Webhook 受信器）

**責務**：署名検証・時刻検証・重複排除・204 即 ACK。
**入力**：Twitch EventSub POST。
**出力**：`EventRaw` append、Tap(ingress) 出力、Normalizer にドメインイベント送出。

**規範**：

* `challenge`：200 + チャレンジ文字列（MUST）。
* `notification`：署名/±10 分 OK → **即 204**（MUST）。
* `Message-Id` 既知 → 204（MUST）。
* `EventRaw` 保存（72h TTL 対象）（MUST）。
* Tap に `stage="ingress"` を publish（SHOULD）。

### 4.2 (2) Normalizer（正規化）

**責務**：EventSub ペイロード→**ドメインイベント**へ変換。
**対象**（MVP）：

* Redemption（add/update）、Stream（online/offline）。
  **出力**：`{type, broadcaster_id, user, reward, occurred_at, ...}` の正規化イベント。

**規範**：変換は**決定的**であること（同入力→同出力）（MUST）。Tap(policy) への入力として記録（SHOULD）。

### 4.3 (3) Policy Engine（ポリシー決定）

**責務**：ドメインイベント → **Commands**（enqueue / refund|consume / counter++ / clear など）。
**反スパム**：**同一 user×reward×60s** の 2 回目以降は **consume**（既定）。
**配信開始クリア**：`stream.online` で設定 `clear_on_stream_start=true` の場合、**一括 clear**（COMPLETED or REMOVED, 設定に準拠）。

**規範**：

* 対象外イベントは無視（MUST）。
* Helix 更新は**抽象コマンド**として出し、実呼び出しの可否は後段で判断（MUST）。
* 決定は**決定的**（MUST）。Tap(policy) に入出力を出す（SHOULD）。

### 4.4 (4) Command Log（append-only, version 採番）

**責務**：Commands を**不変ログ**として記録し、**version++**。
**入力**：Policy 出力 / Admin Mutations。
**出力**：`{version, cmd, at, broadcaster_id, op_id?}`。

**規範**：

* **version は broadcaster 単位で単調増加**（MUST）。
* Admin 由来のコマンドは **`op_id` 冪等**（MUST）。
* トランザクションで **Projector と一貫性**を保つ（MUST）。
* Tap(command) を出す（SHOULD）。

### 4.5 (5) Projector（状態反映とパッチ生成）

**責務**：CommandLog を順に適用し、**Queue/Counters/Settings** を更新、**パッチ**を生成。
**出力例**：

* `queue.enqueued {entry, user_today_count}`
* `queue.removed {entry_id, reason, user_today_count}`
* `queue.completed {entry_id}`
* `counter.updated {user_id, count}`
* `settings.updated {...}`
* `state.replace {version, state}`（フォールバック）

**規範**：

* **1 コマンド＝1 パッチ以上**（MUST）。
* キュー表示順は `ORDER BY user_today_count ASC, enqueued_at ASC`（MUST）。
* `UNDO` は `count--`、`COMPLETE` は不変（MUST）。
* Tap(projector) を出す（SHOULD）。

### 4.6 (6) SSE Hub（OBS/管理 UI への増分配信）

**責務**：version 付きパッチを SSE で配信、**再送補償**。
**エンドポイント**：

* `/overlay/sse?broadcaster=...&since_version=...&token=...`
* `/admin/sse?broadcaster=...&since_version=...&token=...`

**規範**：

* **`id=version`**（MUST）。
* **心拍**：20–30s ごとに `:heartbeat`（MUST）。
* **リング**：直近 **N=1000** もしくは **2 分**（大きい方）（MUST）。
* 初回のみ `since_version` クエリ、再接続は **`Last-Event-ID`**（MUST）。
* 欠落がリングを超えた場合は **`state.replace`** を送る（SHOULD）。
* 認可：短寿命署名トークン（クエリ or 同一オリジン Cookie）（MUST）。
* Tap(sse) を出す（SHOULD）。

### 4.7 (7) State Service（初期スナップショット）

**責務**：`/api/state` が **最新 state と `version`** を返す。
**クエリ**：`scope=session|since`、`since`（任意時刻）
**返却**：`{version, queue:[...], counters_today:[...], settings:{...}}`

**規範**：

* scope=session は `stream.online/offline` 区間（MUST）。
* 整合：直後の SSE パッチと矛盾しない（MUST）。

### 4.8 (8) Admin Mutations（管理操作）

**責務**：**COMPLETE / UNDO / Settings 更新**。
**I/F**：

* `POST /api/queue/dequeue {entry_id, mode:"COMPLETE"|"UNDO", op_id}`
* `POST /api/settings/update {patch, op_id}`

**規範**：

* **`op_id` 冪等**（MUST）。
* 反映は **CommandLog → Projector → SSE** 経由のみ（MUST）。

---

## 5. データライフサイクル

| データ                 | ライフサイクル  | 用途             |
| ------------------- | -------- | -------------- |
| EventRaw            | 72h（TTL） | 入力検証/再現用の監査ログ  |
| CommandLog          | 72h（TTL） | 差分の一次情報・再送・再生成 |
| QueueEntries        | 永続       | 待機・履歴（状態遷移を保持） |
| DailyCounters       | 永続       | “今日”の回数（TZ 境界） |
| Settings            | 永続       | ポリシー・表示設定      |
| StateIndex(version) | 永続       | version の復元    |

**TTL 処理**：小分け DELETE（`LIMIT 1000`）、WAL checkpoint(TRUNCATE)。
**時刻**：内部は UTC、**“今日”**は配信者の TZ（MUST）。

---

## 6. 起動・復旧・Backfill

1. `/oauth2/validate` で資格確認、必要なら refresh／再同意導線。
2. EventSub 購読の棚卸し・不足作成。
3. Helix で**未処理（UNFULFILLED）**の backfill を取得 → Normalizer→Policy→CommandLog→Projector。
4. `state_index.version` をロードし、SSE Hub のカウンタを合わせる。
5. Tap/metrics が有効であることを確認。

---

## 7. セキュリティ / 多テナント / PII

* **認可**：全 API/SSE は `broadcaster` を必須とし、RBAC（superadmin / broadcaster / operator）。
* **SSE トークン**：短寿命署名、漏洩に備え検証厳格化。
* **PII**：ログ/タップでは既定マスク（表示名・入力値）。
* **Helix 制約**：**自アプリ作成リワードのみ** Redemption 更新可。更新不可時は `managed=false` を付けて記録。

---

## 8. 可観測性（Tap / Capture / Replay / Logs / Metrics）

* **Tap**：`/_debug/tap?s=ingress,policy,command,projector,sse&broadcaster=...`（SSE）

  * StageEvent には `ts, stage, trace_id, op_id, version, broadcaster, in, out, elapsed_ms` を含める（MUST）。
* **Capture**：`/_debug/capture/start|stop` → NDJSON を出力（MUST）。
* **Replay**：`/_debug/replay` → 同一の state/patch 群に到達（決定性）（MUST）。
* **Logs**：`tracing` 構造化 JSON（prod）/ pretty（dev）。
* **Metrics**：Prometheus エンドポイント `/metrics`。

  * 例：`eventsub_ingress_total{type=...}`、`policy_commands_total{kind=...}`、`sse_clients{kind=...}`、`db_ttl_deleted_total{table=...}`。

---

## 9. 性能目標（SLO 目安）

* **Webhook ACK p95 < 200ms**（検証・冪等のみ）
* **SSE ブロードキャスト遅延 p95 < 50ms**
* **復旧**：起動後、購読健全化＋バックフィル完了まで実用時間内（規模に依存）

---

## 10. 失敗時の挙動 / バックプレッシャ

* **署名/時刻不正**：4xx、Tap(ingress) に記録。
* **重複**：204（取り込みスキップ）。
* **Helix 失敗**：Command は `managed=false` で継続、後続の再試行は指数バックオフ。
* **SSE 欠落**：リング範囲外は `state.replace` を送信。
* **DB 圧迫**：TTL 小分け削除、WAL checkpoint を前倒し。

---

## 11. モジュール境界（実装配置）

```
crates/
  app/       : HTTP (Webhook/REST/SSE), Tap, metrics
  core/      : Normalizer, Policy, Projector(パッチ生成), types
  storage/   : SQLite(sqlx), repositories, TTL/WAL ジョブ
  twitch/    : OAuth/Helix/EventSub 購読ユーティリティ
  util/      : config, tracing 初期化, token 署名, 時刻抽象
web/
  overlay/   : OBS 用フロント（Vite + TS、テーマパック）
```

---

## 12. I/F サマリ（詳細は `04-api-contracts.md`）

* **Webhook**：`POST /eventsub/webhook`（verification 200、notification 204）
* **State**：`GET /api/state?broadcaster=...&scope=session|since&since=...`
* **SSE**：`GET /overlay/sse?broadcaster=...&since_version=...&token=...`
  `GET /admin/sse?broadcaster=...&since_version=...&token=...`
* **Mutations**：`POST /api/queue/dequeue {entry_id, mode, op_id}`
  `POST /api/settings/update {patch, op_id}`
* **Debug**：`GET /_debug/tap`、`POST /_debug/capture/*`、`POST /_debug/replay`
* **Ops**：`GET /healthz`、`GET /metrics`

---

## 13. 拡張ポイント（非規範）

* EventSub 対象の追加（Bits/サブスク/チャット）。
* ルールの DSL 化（CEL/JSONLogic）— 評価はサーバ側で。
* マルチノード化（将来の Conduits や外部 Pub/Sub への移行）。

---

## 14. 受け入れチェックリスト（本章適合確認）

* [ ] Webhook：HMAC/±10 分/冪等/204 即 ACK を満たす
* [ ] CommandLog：version 単調増加（broadcaster 単位）
* [ ] SSE：`id=version`、心拍、リング再送、`state.replace` フォールバック
* [ ] 初期＋増分：`/api/state` → `/overlay.sse` の整合
* [ ] Admin：`op_id` 冪等、COMPLETE/UNDO の差分挙動
* [ ] 保持：EventRaw/CommandLog 72h TTL、Queue/Counters/Settings 永続
* [ ] Tap/Capture/Replay：各ステージで可視化・再現が可能
* [ ] 多テナント：broadcaster 分離、SSE 認可（短寿命署名）
* [ ] メトリクス：主要カウンタ/ヒストグラムが `/metrics` に露出
* [ ] 性能目標：ACK p95/配信遅延 p95 が基準内

本章に適合する限り、実装は任意の内部最適化を行って構いません。実装が事実として本章と矛盾した場合は、**先に本章を更新し整合を取る**こと（`AGENTS.md` 準拠）。
