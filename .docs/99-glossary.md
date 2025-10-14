# `.docs/99-glossary.md` — 用語集（規範）

> 本章は、本プロジェクトの設計書（`01–12`）全体で用いる**用語の唯一の参照**です。
> 意味が曖昧な場合は本章を**先に**更新し、他章と整合させてください。
> 記法は **太字＝用語 / *英語alias* /〔分類〕**、必要に応じて出典や関連章を併記します。

---

## 0. 規範語彙

* **MUST / SHOULD / MAY** 〔規範〕: RFC 2119/8174 に準拠。

  * **MUST**: 実装・運用が**必ず**満たす。
  * **SHOULD**: 強く推奨。正当理由がある場合のみ逸脱可。
  * **MAY**: 任意。

---

## 1. 役割・アクター

* **配信者** / *Broadcaster* 〔役割〕: イベントの主対象となる Twitch チャンネルの所有者。内部キー **`broadcaster_id`**（内部）と **`twitch_broadcaster_id`**（外部）を区別。→ `03,05`
* **オペレーター** / *Operator* 〔役割〕: 管理 UI で配信者の代わりに操作する権限を持つユーザ。→ `11`
* **管理者** / *Superadmin* 〔役割〕: システム全体の設定・運用を行う最上位ロール。→ `11`
* **視聴者** / *Viewer* 〔役割〕: 配信者のチャンネルで行動（チャット・チャンポイ交換等）するユーザ。

---

## 2. Twitch 関連

* **EventSub** 〔Twitch 概念〕: イベント配信の仕組み。

  * **Webhook** 〔Transport〕: Twitch→サーバへの HTTP POST。**HMAC 署名**・**検証**・**即時 ACK** が必須。→ `02,04,07,10,11`
  * **WebSocket** 〔Transport〕: クライアント常時接続型。1 接続あたりの制約が厳しく、本プロジェクトでは採用しない（参考のみ）。→ `02`
  * **Conduits** 〔Transport〕: 大規模向けの拡張経路。今回は範囲外。→ `02`
* **Helix API** 〔Twitch API〕: REST API 群。Redemption の更新など**能動操作**で利用。→ `02,04,11`
* **購読** / *Subscription* 〔EventSub〕: 受信対象イベントの登録。Webhook では **App Access Token** で作成。→ `02,08`
* **リワード** / *Custom Reward* 〔Channels Points〕: チャンネルポイントの交換項目。

  * **Redemption** 〔Event〕: 視聴者がリワードを引き換えた事実。
  * **管理可能リワード** 〔制約〕: **「そのリワードを作成したクライアントIDのみ」** Helix で状態更新可。ダッシュボード作成分は**読み取りのみ**。→ `02,04,11`
* **EventSub on Chat** 〔チャット受信〕: `channel.chat.message` など。**`channel:bot` 許諾**と `user_id` 条件が必要。→ `02,04`

---

## 3. 認証・認可

* **App Access Token** 〔OAuth〕: クライアント資格（Client Credentials）のトークン。購読作成などに使用。→ `02,11`
* **User Access Token / Refresh Token** 〔OAuth〕: 配信者同意に基づくトークン。`/oauth2/validate` で健全性確認、**refresh** による更新。→ `02,04,11`
* **SSE 認可トークン** 〔認可〕: Overlay/Admin SSE 接続用の**短寿命署名トークン**（JWT/PASETO）。`sub=broadcaster_id`、`aud∈{overlay,admin}`、`exp` 必須。→ `04,06,11`
* **CSRF トークン** 〔Web 安全〕: 状態変更 REST の防御。→ `11`

---

## 4. トランスポート / プロトコル

* **Webhook チャレンジ** / *Verification* 〔EventSub〕: 最初の確認リクエスト。**200 + 平文 challenge** で応答。→ `04`
* **即時 ACK** 〔Webhook〕: Notification を**数秒以内に 2xx** で応答（本設計は **204**）。→ `02,04,10,11`
* **HMAC 署名** 〔Webhook〕: `sha256(secret, id || timestamp || raw_body)`。**定数時間比較**・**±10 分**の時刻検証。→ `02,04,11`
* **SSE** / *Server‑Sent Events* 〔配信〕: `text/event-stream`。`id=version`、`Last-Event-ID`、**心拍**、**リング再送**が規範。→ `02,04,06,07,10`
* **心拍** / *Heartbeat* 〔SSE〕: 20–30 秒のコメント行（`:heartbeat`）。プロキシ切断回避。→ `04,06,07`
* **リングバッファ** 〔SSE〕: 直近 N 件/時間のパッチ保持。**リング外**は `state.replace`。→ `04,06,07`

---

## 5. ドメイン / パイプライン

> 5 つの主経路：**Ingress → Normalizer → Policy → Command → Projector → SSE**

* **Normalizer** 〔変換〕: EventSub ペイロードを内部の**決定的**な `NormalizedEvent` に変換。→ `02,03`
* **Policy** 〔判定〕: 設定（対象リワード・反スパム等）に基づき **Command** を生成。

  * **反スパム** / *Duplicate window*: `anti_spam_window_sec` 内の同一 user×reward を抑制。→ `02,03,06`
  * **Duplicate Policy** 〔判定〕: `consume|refund`（同一窓内の 2 回目以降を消費/払い戻し）。→ `02,03`
* **Command** 〔一次ソース〕: `enqueue` / `dequeue(UNDO|COMPLETE)` / `settings.update` / `redemption.update(dry-run|apply)` などの**事実列**。→ `02,03,04,05`
* **CommandLog** 〔永続〕: **append-only** のコマンド記録（72h 保持）。**version++** と同一 Tx。→ `05`
* **Projector** 〔投影〕: Command を適用して**現在状態**（Queue/Counter/Settings）を更新し、**Patch** を生成。→ `02,03,04`
* **Patch** 〔差分〕: クライアントへ送る増分。`queue.enqueued|removed|completed` / `counter.updated` / `settings.updated` / `state.replace` 等。→ `04,06`
* **State** 〔現在状態〕: `queue（QUEUEDのみ）` / `counters_today` / `settings` / `version`。→ `04,06`
* **state.replace** 〔補償〕: リング外など不整合時に**全置換**で同期させるパッチ。→ `04,06`

---

## 6. キュー / カウンタ / ルール

* **Queue Entry** 〔データ〕: 並び待ちの 1 要素。`status ∈ {QUEUED, COMPLETED, REMOVED}`、`reason` に `UNDO|STREAM_START_CLEAR|EXPLICIT_REMOVE` 等。→ `03,05`
* **“今日の回数”** / *Daily Counter* 〔指標〕: 配信者の **IANA タイムゾーン**で区切られた 1 日単位の user 別回数。表示順は `today_count ASC, enqueued_at ASC`。→ `02,03,05,06`
* **管理可否** / *managed* 〔属性〕: その Entry が Helix で**更新可能**か。不可の場合は**記録のみ**。→ `02,03,05`
* **配信セッション** 〔境界〕: `stream.online/offline` を境にした期間。`scope=session` の初期データの基準。→ `02,04,05`

---

## 7. ID / バージョン / 冪等

* **`version`** 〔単調増加〕: **配信者単位**で Command 適用ごとに +1。SSE の `id` に使用。→ `02,04,05,06`
* **`op_id`** 〔冪等キー〕: 管理操作のクライアント生成 UUID。同一 `op_id` は 1 回のみ反映。→ `04,06,08`
* **`entry_id`** 〔行ID〕: QueueEntry の ULID/UUID。→ `03,05`
* **`trace_id`** 〔相関ID〕: Tap/ログ用のトレース識別子。→ `07`
* **`Message-Id`** 〔EventSub〕: Webhook 通知の一意キー。冪等処理の一次材料。→ `04,07`

---

## 8. データ / データベース

* **SQLite (WAL)** 〔DB〕: 本システムの永続層。`journal_mode=WAL` / `foreign_keys=ON` を**常時**。→ `05,10`
* **TTL（72h）** 〔運用〕: `event_raw` / `command_log` を小分け DELETE で削除。→ `05,10`
* **WAL チェックポイント** 〔運用〕: `wal_checkpoint(TRUNCATE)` を TTL 後に実行。→ `05,10`
* **部分ユニーク** / *Partial UNIQUE* 〔制約〕: 例 `queue_entries.redemption_id WHERE ...`。Helix 重複抑止に使用。→ `05`
* **Backfill** 〔復旧〕: UNFULFILLED 等の過去イベントを Helix から再取得し、Command として再生。→ `02,05,08`

---

## 9. フロントエンド / 表現

* **Overlay** 〔UI〕: OBS ブラウザ用表示。**URL クエリ**でテーマ等を切替。→ `06`
* **Admin UI** 〔UI〕: 管理操作（COMPLETE/UNDO、設定編集）と現況表示。→ `06`
* **テーマパック** 〔表現〕: `themes/<name>/{theme.css,theme.json}`。`tokens`（CSS 変数群）/ `variants` / `sounds` / `images`。→ `06`
* **アクセント** / *accent* 〔表現〕: URL クエリで上書き可能な主色。→ `06`
* **グループ化表示** 〔表現〕: `group_size=n` の**見た目のみ**のブロック化。データ構造はフラット。→ `06`

---

## 10. 可観測性 / デバッグ

* **Tap** 〔SSE 可視化〕: `/_debug/tap`。パイプライン各段（`ingress|normalizer|policy|command|projector|sse|storage|oauth`）の **StageEvent** を流す。→ `07`
* **StageEvent** 〔イベント型〕: `ts, stage, trace_id, op_id, version, broadcaster_id, meta, in, out`。機微情報は**マスク**。→ `07`
* **Capture / Replay** 〔再現性〕: NDJSON を記録/再生して**決定的**に最終状態へ到達する検証機構。→ `07`
* **メトリクス** / *Prometheus* 〔監視〕: `/metrics`。`eventsub_ingress_total`、`webhook_ack_latency_seconds`、`sse_clients{}` 等の規範名を定義。→ `07,10,12`

---

## 11. セキュリティ / プライバシ

* **PII（低機微）** 〔データ分類〕: 表示名・ログイン名・アバター URL・入力文字列。**最小化**が原則。→ `11`
* **マスク** / *Redaction* 〔可観測〕: Tap/ログ/Capture におけるトークン類の**完全マスク**、ログイン名の**部分マスク**。→ `07,11`
* **CSP** / *Content-Security-Policy* 〔ヘッダ〕: Admin/Overlay での実行元制限。`frame-ancestors 'none'` 等。→ `11`
* **HSTS** 〔ヘッダ〕: HTTPS 強制。→ `10,11`
* **SameSite** 〔Cookie〕: `Lax` 既定、CSRF 対策に利用。→ `11`

---

## 12. Nginx / 運用

* **SSE バッファ無効** 〔必須〕: `proxy_buffering off; proxy_http_version 1.1; proxy_set_header Connection "";`。→ `02,10`
* **即時 ACK 経路** 〔Webhook〕: `client_max_body_size 256k; proxy_read_timeout 10s;`。→ `10`
* **systemd 再起動ポリシ**: `Restart=always`、`LimitNOFILE` 上限、`EnvironmentFile` からシークレット読込。→ `10`

---

## 13. API / コントラクト

* **`/eventsub/webhook`** 〔受信〕: 検証・即時 ACK・冪等。→ `04`
* **`/api/state`** 〔初期化〕: `version` / `queue` / `counters_today` / `settings` を返す。→ `04`
* **`/overlay/sse` / `/admin/sse`** 〔配信〕: `id=version` / 心拍 / リング再送 / `types` フィルタ。→ `04,06`
* **`/api/queue/dequeue`** 〔操作〕: `mode=COMPLETE|UNDO`、**`op_id` 冪等**。→ `04,06`
* **`/api/settings/update`** 〔操作〕: 部分更新、**`op_id` 冪等**。→ `04,06`
* **`/_debug/tap|capture|replay`** 〔可観測〕: 本番は**管理者のみ**。→ `07,10,11`

---

## 14. エラー / 例外

* **RFC 7807** / *problem+json* 〔エラー形式〕: `type/title/status/detail/instance` を返す標準形式。→ `04`
* **リングミス** / *Ring miss* 〔SSE〕: クライアントの `Last-Event-ID` 以降がリングに無い状態。**`state.replace`** を送って回復。→ `04,06,07`
* **Revocation** 〔EventSub〕: 購読取り消し。`authorization_revoked` 等の通知。**自動再購読**方針。→ `02,10`

---

## 15. 代表 EventSub タイプ（参照）

| タイプ（`subscription.type`）                                 | 概要              | 主な条件                                                   | 備考                                  |                            |
| -------------------------------------------------------- | --------------- | ------------------------------------------------------ | ----------------------------------- | -------------------------- |
| `channel.channel_points_custom_reward_redemption.add`    | リワード引き換え        | `broadcaster_user_id`（必要なら `reward_id`）                | **対象リワードのみ**処理（Policy）。→ `02,03,04` |                            |
| `channel.channel_points_custom_reward_redemption.update` | Redemption 状態更新 | 同上                                                     | Helix 適用時の追随に利用。                    |                            |
| `channel.chat.message`                                   | チャット受信          | `broadcaster_user_id`, **`user_id`**, `channel:bot` 許諾 | EventSub on Chat。→ `02`             |                            |
| `channel.cheer`                                          | Bits            | `broadcaster_user_id`                                  | 任意購読。                               |                            |
| `stream.online                                           | offline`        | 配信セッション境界                                              | `broadcaster_user_id`               | `scope=session` の境界。→ `04` |

> 正式な型と要件は Twitch 公式の最新文書を参照（本設計は 2025-10 時点の整理に基づく）。

---

## 16. テスト / CI

* **Unit / Integration / E2E** 〔階層〕: `09` に定義。Clock/ID の**依存注入**で決定性を担保。
* **CI** 〔GitHub Actions〕: `ci.yml`（Rust+Front）、`e2e.yml`（任意）、`release.yml`、`security.yml`。→ `12`

---

## 17. 省略・略語

* **ACK**: Acknowledgement（受領応答）。
* **PII**: Personally Identifiable Information（本件では低機微）。
* **CSP**: Content Security Policy。
* **HSTS**: HTTP Strict Transport Security。
* **WAL**: Write‑Ahead Logging。
* **TTL**: Time To Live（保持期限）。
* **CORS**: Cross‑Origin Resource Sharing。
* **RBAC**: Role‑Based Access Control。
* **NDJSON**: Newline‑Delimited JSON。
* **CVE**: Common Vulnerabilities and Exposures。

---

## 18. 参照と整合

* **アーキテクチャ**：`02-architecture-overview.md`
* **ドメインモデル**：`03-domain-model.md`
* **API**：`04-api-contracts.md`
* **データスキーマ**：`05-data-schema-and-migrations.md`
* **フロント仕様**：`06-frontend-spec.md`
* **可観測性**：`07-debug-telemetry.md`
* **実装計画**：`08-implementation-plan.md`
* **テスト戦略**：`09-testing-strategy.md`
* **運用**：`10-operations-runbook.md`
* **セキュリティ**：`11-security-and-privacy.md`
* **CI/CD**：`12-ci-cd.md`

---

### 運用メモ

用語の追加・変更は **この用語集を先に更新**し、該当章の差分に**相互リンク**を付けてください。用語の揺れ（例：「Queue Entry」vs「エントリ」）は**本章の表記に統一**します。
