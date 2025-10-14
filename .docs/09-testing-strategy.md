# `.docs/09-testing-strategy.md` — テスト戦略（規範 + ガイド）

> 本章は **単体 / 統合 / E2E / パフォーマンス / セキュリティ** を横断するテスト戦略を定義します。
> **規範（MUST）** と **推奨（SHOULD）** を明確に示します。
> 前提仕様：`01–08`、特に `02`（アーキテクチャ）, `03`（ドメイン）, `04`（API）, `07`（デバッグ）に準拠。

---

## 1. 目的・適用範囲

* **目的**：

  1. **決定性**（同入力→同出力）と **冪等**（`msg_id` / `op_id` / `version`）。
  2. **欠落補償**（SSE `id=version` + `Last-Event-ID` + `state.replace`）の実証。
  3. **長期稼働**（TTL / WAL / 再購読 / Backfill）での安定。
  4. **セキュリティ**（認可・PII マスク）。

* **適用**：Rust バックエンド（axum）、SQLite（sqlx）、フロント（Vite + TS）、Nginx はローカルでは不要（本番手順は `10` 参照）。

---

## 2. テスト階層と優先順位

| レベル                      | 目的                        | 実行時間      | 代表ツール                           | 規範                  |
| ------------------------ | ------------------------- | --------- | ------------------------------- | ------------------- |
| Unit（Rust/TS）            | 純粋ロジック・境界条件               | <1s/モジュール | `cargo test`, `vitest`          | **MUST**            |
| Integration（HTTP/DB/SSE） | 1プロセス内で実サーバ起動・SQLite WAL  | 1–5m      | `tokio::test`, `reqwest`        | **MUST**            |
| E2E（モック Helix + Overlay） | 初期 REST→SSE 追随→操作の可視確認    | 2–8m      | `playwright` or headless script | **SHOULD**          |
| Property / Fuzz          | 反スパム・順序不変・冪等              | 任意        | `proptest`, `cargo fuzz`        | **SHOULD**（Fuzzは任意） |
| Perf/Soak                | ACK p95 / SSE 遅延 / TTL 実行 | 任意        | `criterion`, `k6/vegeta` など     | **SHOULD**          |

> CI は **Unit/Integration を必須**, E2E/Perf は**ナイトリー or ラベル付き**で実行。

---

## 3. テスト環境 / 基盤（規範）

* **SQLite**：各統合テストは **一時ファイル DB** を作成（WAL）。`sqlx::migrate::Migrator` で毎回初期化（**MUST**）。
* **Clock 抽象**：`util::Clock` を**依存注入**。テスト時は **固定時刻 / 手動進行**（**MUST**）。
* **ID 生成**：ULID/UUID はテストで**擬似乱数固定**（seed） or 供給関数差し替え（**MUST**）。
* **SSE 心拍**：テストプロセスでは**心拍間隔を短縮**できる環境変数を提供（例：`SSE_HEARTBEAT_MS=500`）（**MUST**）。
* **HTTP サーバ**：`axum::Router` をテスト内で `hyper::Server` にマウントし **0.0.0.0:0**（ephemeral port）で起動（**MUST**）。
* **Windows/Ubuntu**：改行とパス差異に依存しない検証（**MUST**）。

---

## 4. 単体テスト（Rust）

### 4.1 コア・ロジック

* **Normalizer**：同一 EventSub 入力 → **同一 `NormalizedEvent`**（**MUST**）。
* **Policy**：

  * 反スパム `anti_spam_window_sec`：**59s/60s/61s** で結果が分かれる（**MUST**）。
  * 対象外 Reward：**Command なし**。
* **Projector**：

  * `enqueue`→`queue.enqueued` + `counter++`
  * `queue.remove(reason=UNDO)`→`counter--`
  * `queue.complete`→counter 不変
  * **順序決定**：`today_count ASC, enqueued_at ASC` を **常に満たす**（**MUST**）。
* **SSE リング**：範囲外で `state.replace` を生成する条件分岐（**MUST**）。

### 4.2 構造化ログ / Tap / メトリクス

* マスク関数：`access_token`, `refresh_token`, `Authorization`, `user_login` が **必ず `***`**（**MUST**）。
* メトリクス名・ラベル（`07` §6.2）一致（**MUST**）。

### 4.3 Property-based（`proptest`）

* CommandLog の任意列に対して、**version が単調増加**（broadcaster 単位）。
* `op_id` 重複は no-op または 412（API 側）に収束。

---

## 5. 単体テスト（フロント / TypeScript）

* **パッチ適用**：`state + patch -> state'` を**純粋関数**化し網羅（**MUST**）。

  * 逆順/欠落/重複のパッチは**適用拒否**。
  * `state.replace` は**全置換**。
* **テーマトークン**：`theme.json` の `tokens/variants` マージ・`accent` 上書き。
* **URL パラメータ検証**：`broadcaster` 必須、`group_size` 範囲、`since_version` 正整数。

---

## 6. 統合テスト（HTTP/DB/SSE）

### 6.1 Webhook Ingress

* verification=200（平文 challenge）
* notification=**204 即 ACK**、HMAC/±10分/`Message-Id` 冪等（**MUST**）。
* `event_raw` に保存（72h 対象）。

### 6.2 CommandLog / Projector / SSE

* **正常経路**：`redemption.add` → `enqueue` → `queue.enqueued` + `counter.updated` SSE。
* **リング再送**：

  1. `since_version=0` で接続 → `id=version` を順に受領
  2. クライアント側で **任意の N をスキップ** → `Last-Event-ID` 指定で再接続 → **再送補償**
* **フォールバック**：リング外の欠落→**`state.replace`** を受領。

### 6.3 Admin Mutations（`op_id` 冪等）

* COMPLETE：`queue.completed` / counter 不変。
* UNDO：`queue.removed(reason=UNDO)` / counter 減算。
* **同一 `op_id`**：同内容=200、矛盾=412。

### 6.4 State 初期化（REST→SSE）

* scope=`session`：現行セッション。
* scope=`since`：指定時刻以降。
* 初期 `version` と SSE の先頭 `version` が**連続**（**MUST**）。

---

## 7. E2E（最小）

* **構成**：サーバ起動（ephemeral DB, WAL）+ ヘッドレスブラウザ（Overlay）
* **シナリオ**：

  1. 初期 REST → DOM 初期化（Queue/Counters/Settings）
  2. モック `redemption.add` 投入 → `li.queue-item` 追加
  3. `queue/dequeue(UNDO)` → `li` フェードアウト（`.leave`）→ 削除
  4. ページ Reload → **`since_version`** で欠落なく復元
* **アサーション**：DOM 構造/順序/テキスト、`localStorage("overlay:lastVersion:<b>")` 更新。

> ランナーは `playwright`/`puppeteer` いずれでも可。CI 実行は**オプション**（タグ駆動）。

---

## 8. パフォーマンス / Soak

* **Webhook ACK**：負荷 1,000 req/min で **p95 < 200ms**（署名+冪等のみ）。
* **SSE ブロードキャスト**：100 クライアントで **p95 < 50ms**。
* **TTL/WAL**：72h 経過データ 1e6 行 → **小分け DELETE** で OOM/長ロックなし。
* **手段**：`criterion`（関数単位）、`vegeta`/`k6`（HTTP/SSE は `curl -N` 群でも代用可）。

---

## 9. セキュリティ / ネガティブテスト

* **SSE 認可**：トークン `aud`/`sub`/`exp` 検証（期限切れ・aud 不一致・署名不正→403）。
* **PII マスク**：Tap/ログ/キャプチャに**生トークン/生ログイン名が出ない**（**MUST**）。
* **Rate Limit（任意）**：`/_debug/*` に対する制限が効く。
* **CSRF/クリックジャッキング**：Overlay は pointer-events: none 既定（UI 操作なし）を確認。

---

## 10. DB / マイグレーション

* **`sqlx migrate run` 成功**（**MUST**）。
* **Down/Up ドライラン**（破壊的変更なし）。
* **一意制約**：`event_raw.msg_id` / `queue_entries.redemption_id`（partial） / `command_log(broadcaster,op_id)`（partial）を**衝突ケースで検証**。
* **バッチ削除**：TTL ジョブで **LIMIT 1000** を守る。
* **WAL checkpoint**：呼び出し結果コード/時間記録。

---

## 11. テストデータ / ゴールデン

* `tests/fixtures/`：EventSub サンプル JSON（redemption.add|update, stream.online|offline）。
* `tests/golden/`：NDJSON（Tap/Capture, Patch シーケンス）。
* **ゴールデン比較**：`Replay(from-scratch)` の `final_state` と `patches` をスナップショット比較（許容差分なし）。

---

## 12. ツールとコマンド（代表）

### 12.1 Rust（Linux / Windows 共通）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -D warnings
cargo test --workspace -- --nocapture
```

### 12.2 Frontend

```bash
cd web/overlay
npm ci
npm run lint
npm run typecheck
npm run test   # vitest
npm run build
```

### 12.3 SSE 手動確認

```bash
# Linux
curl -N "http://127.0.0.1:8080/overlay/sse?broadcaster=b-dev&since_version=0&token=eyJ..."

# Windows (PowerShell)
$uri = "http://127.0.0.1:8080/overlay/sse?broadcaster=b-dev&since_version=0&token=eyJ..."
Invoke-WebRequest -Uri $uri -UseBasicParsing
```

---

## 13. CI 行列 / 実行ポリシ

* **OS マトリクス**：`ubuntu-latest` / `windows-latest`（**MUST**）。
* **ジョブ**：

  * `rust-unit-integration`（常時）
  * `frontend-lint-typecheck-build`（変更検知で）
  * `e2e`（`e2e: true` ラベル時）
  * `perf`（ナイトリー or 手動）
* **成果物**（オプション）：失敗時に **Tap/Capture NDJSON** と **tracing ログ** を artifacts として保存。

---

## 14. フレーク対策 / 時間依存排除

* **`tokio::time::pause`** と **`advance`** を活用（タイマ依存を潰す）。
* **心拍間隔の短縮**で SSE 待機を最小化。
* **リトライ**：SSE 行単位読みで**一定しきい値**まで待つ（タイムアウトは 3–5s ）。
* **順序保証**：`state.version+1` のみ受理（クライアント側も同じ規約）。

---

## 15. 受け入れチェック（本章適合）

* [ ] Clock/ID を**注入**し、**決定性**が担保される
* [ ] Unit（Rust/TS）が主要ロジック・境界をカバー
* [ ] Integration が Webhook→SSE の**最短径路**を実証
* [ ] E2E で **初期 REST→SSE→操作→再読込冪等**が成立
* [ ] セキュリティ（SSE 認可・PII マスク）
* [ ] DB（migrate / TTL / WAL / 一意制約）
* [ ] CI（Linux/Windows）で必須ジョブが**常にグリーン**

---

## 16. 変更管理

* API/DB/動作に関係する仕様変更は、**先に `04`/`05` を更新**し、本章のテスト観点を**増補**（**MUST**）。
* 新たなバグは **再現手順を NDJSON（Capture）** として保存 → `tests/golden/` に最小化して追加（**SHOULD**）。

---

この戦略に従うことで、**小さく確実な PR** の積み上げと、**本番同等の信頼性**を両立できます。矛盾や不足があれば **先に本章を更新**し、関連文書（`02/03/04/05/07/08/12`）との整合を取ってください。
