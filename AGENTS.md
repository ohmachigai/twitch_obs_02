# AGENTS.md — 実装エージェント（CodeX / Contributors）作業規約

> **対象**：このリポジトリで実装やレビュー、CI 設定、ドキュメント更新を行う “エージェント（人間/AI）” 全員
> **目的**：**小さく確実に**進めるための共通ルールと、**PR 駆動**の実行プロトコルを定義します。
> **位置づけ**：本ファイルは**リポジトリ直下**に置かれ、配下の `.docs/*.md` が**仕様の一次ソース**です。

---

## 0) 最初に読むもの（読み順）

1. `.docs/00-index.md`（索引・読み方）
2. `.docs/01-product-scope.md`（スコープ/非スコープ）
3. `.docs/02-architecture-overview.md`（全体アーキテクチャ/データフロー）
4. `.docs/03-domain-model.md`（用語/不変条件/エンティティ）
5. `.docs/04-api-contracts.md`（REST/SSE/Mutation/Debug IF）
6. `.docs/05-data-schema-and-migrations.md`（SQLite/WAL/TTL）
   …以降は必要に応じて参照

> **原則**：実装や仕様の変更時は、**該当する `.docs/*` を先に更新**し、**PR に同梱**してください。

---

## 1) ゴールと基本原則

* **小さく速い PR**：1PR=1目的。差分は概ね 50–200 行を目安（上限でも ~400 行程度）。
* **ドキュメント整合**：コード変更は**必ず対応する `.docs/*` の更新**を含める（API, データ, テスト, 運用）。
* **CI グリーン**：Lint/Format/Test が**Linux/Windows**で通ること（破った PR はマージ不可）。
* **再現性**：すべての変更は手動手順（検証コマンド）と自動テスト（ユニット/統合/E2E）を**セット**で提供。
* **セキュリティ**：秘密・PII は**ログ/PR に載せない**。Debug 機能は dev でのみ既定有効 or 管理者認証必須。

---

## 2) 実装の中核インバリアント（破ってはいけない契約）

**EventSub/Webhook（入力）**

* HMAC-SHA256 署名検証（`Message-Id || Timestamp || RawBody`）。
* ±10分の時刻ウィンドウ外は拒否。
* `Message-Id` による**冪等**（重複は即スキップ）。
* **数秒以内に 2xx**（`challenge` は 200/平文、通知は 204 即 ACK）。

**差分伝搬（出力）**

* **CommandLog.version（単調増加）**を唯一の時系列基準とする（ブロードキャスタ単位）。
* **SSE** の `id:` は **version** を使う。**20–30s 心拍**、**リングバッファ**で再送補償。
* 初期は **REST `/api/state`**、以後は **SSE パッチ**で同期。初回のみ `since_version` クエリ、再接続は `Last-Event-ID`。

**管理操作（Mutation）**

* すべて **`op_id`（UUID）で冪等**。同一 `op_id` は一度だけ反映。
* 「外し」は **COMPLETE（並び終わり/カウンタ不変）** と **UNDO（巻き戻し/カウンタ–1）** を区別。
* 差分は必ず **CommandLog→Projector→SSE** 経由で配信。

**多テナント**

* すべての API は **`broadcaster` コンテキスト必須**。境界は厳格に分離。
* SSE 認可は **短寿命の署名トークン（クエリ or 同一オリジン Cookie）**を用いる。

**保存と保持**

* **SQLite / WAL** を既定。`event_raw` と `command_log` は **72h TTL**。
* `queue_entries` / `daily_counters` / `settings` は永続。
* 定期 **WAL checkpoint(TRUNCATE)**、TTL は**小分け DELETE**。

> これらは `.docs/02/03/04/05` に詳細があり、**本文が矛盾する場合は `.docs/*` を優先**します。

---

## 3) PR の作り方（ブランチ運用／テンプレ／DoD）

**ブランチ命名**：`feature/<短い要約>` / `fix/<短い要約>`
**コミット**：Conventional Commits（例：`feat(sse): add ring buffer`）

**PR テンプレ（必須項目）**

* **目的**：何を達成するか（1–2行）
* **範囲**：本 PR が**変えるもの**と**変えないもの**
* **変更点**：主要ファイル／公開契約の差分（API/DB が変わる場合は `.docs/*` の該当ファイル名を列挙）
* **手動検証**：誰でも再現できる**コマンド列**（Linux/Windows 両対応の例を各1本）
* **自動テスト**：追加/更新したテストの観点と実行方法
* **メトリクス/ログ**：観測ポイント（StageEvent, Prometheus 名）
* **ロールバック**：Revert で戻せるか、データ移行がないか
* **関連ドキュメント**：該当 `.docs/*` の相対パス

**Definition of Done（一般）**

* CI（Linux/Windows）で **format/lint/test** がパス
* `.docs/*` 更新済み（契約変更や運用手順に反映）
* **手動検証**のステップで**成功が再現**できる
* **観測**（tap/metrics/log）の視点が追加・更新されている

---

## 4) ローカル実行（最小手順・クロスプラットフォーム）

**Linux（例）**

```bash
# 1) 依存
rustup toolchain install stable
node --version  # 18+ を想定

# 2) サーバ
cargo run -p twi-overlay-app

# 3) フロント（別端末）
cd web/overlay && npm ci && npm run dev -- --host

# 4) 疎通
curl -f http://127.0.0.1:8080/healthz
```

**Windows PowerShell（例）**

```powershell
# 1) 依存
rustup toolchain install stable
node --version  # 18+

# 2) サーバ
cargo run -p twi-overlay-app

# 3) フロント（別端末）
cd web/overlay; npm ci; npm run dev -- --host

# 4) 疎通
Invoke-WebRequest http://127.0.0.1:8080/healthz
```

> 開発では Nginx 不要。SSE はアプリ直結で動作します（本番の Nginx は `.docs/10-operations-runbook.md` を参照）。

---

## 5) デバッグ可視化（必須の観測点）

* **Stage Tap（SSE）**：`GET /_debug/tap?s=ingress,policy,command,projector,sse&broadcaster=...`

  * すべての処理段で **StageEvent(JSON)** を publish。
* **Capture/Replay**：`/_debug/capture` / `/_debug/replay`（決定的再現）
* **ログ（tracing）**：`ts, stage, trace_id, op_id, version, broadcaster, latency_ms` を**必ず**出力
* **メトリクス（/metrics）**：受信件数/ACK 遅延/SSE 接続数/リング再送/TTL 削除件数 など

> 本番では Tap/Capture を無効化 or 管理者認証必須。PII は既定でマスク。

---

## 6) 変更時の「接触点」チェックリスト

変更の種類ごとに**触れるべき場所**（漏れ防止）：

* **API（REST/SSE/Mutation）**：`.docs/04-api-contracts.md`／`web/*`／`tests/*`
* **データスキーマ**：`.docs/05-data-schema-and-migrations.md`／`migrations/*`／`storage/*`
* **ドメインロジック/規則**：`.docs/03-domain-model.md`／`core/*`
* **観測（ログ/メトリクス/タップ）**：`.docs/07-debug-telemetry.md`／`app/*`
* **運用（Nginx/systemd/TLS/NTP）**：`.docs/10-operations-runbook.md`／`deploy/*`
* **セキュリティ/認可/PII**：`.docs/11-security-and-privacy.md`／`app/*`

> **公開契約（API/DB）を変える PR** は、**必ず**該当 `.docs/*` を先に書き換えてから実装してください。

---

## 7) 禁則事項（スコープ安全装置）

* EventSub の取り込み経路を **Webhook 以外に変更しない**（WebSocket/Conduits は将来検討）。
* SSE の **`id=version`/心拍/リング** を外さない。
* `op_id` 冪等を回避しない（管理操作は必ず `op_id` を検証）。
* 多テナント境界（`broadcaster`）を跨ぐ実装をしない。
* 機微情報（トークン・PII）をログ/PR に出力しない。
* CI を無効化/緩和しない（warning 許可など）。

---

## 8) CI / Lint / Test（合格基準）

* Rust：

  * `cargo fmt --all --check`
  * `cargo clippy --workspace --all-targets -D warnings`
  * `cargo test --workspace`
* Frontend：

  * `npm ci && npm run lint && npm run typecheck && npm run build`（プロジェクトに合わせて）
* OS マトリクス：**ubuntu-latest / windows-latest** で**同じコマンド**が成功
* （本番適用前のリポ）：Nginx を使う場合は `nginx -t -c nginx/site.example.conf` を Ubuntu ランナーで実行

> すべて**PR と main**で走ります。**グリーンでないとマージ不可**。

---

## 9) CodeX への実行指示（常に同じ手順）

1. **`.docs/00-index.md` を読み、依存関係に沿って対象章を把握**。
2. **`.docs/08-implementation-plan.md` の「次の PR」**に着手（PR-0 → PR-1 → … の順）。
3. 実装と同時に **該当 `.docs/*` を更新**。
4. **PR テンプレ**を埋め、**手動検証コマンド（Linux/Windows）**と**自動テスト**をセットで提出。
5. CI がグリーンで、レビュー要件（DoD）を満たしたらマージ。

> 不確実性がある場合、**最小スコープでドラフト PR**を立て、観測/ログ/タップを使って事実を固めてください。

---

## 10) 付記（非機能要件の閾値）

* Webhook ACK p95 < **200ms**（検証/冪等のみで返す）
* SSE ブロードキャスト p95 < **50ms**（リング常時有効、Nginx では `proxy_buffering off`）
* 起動/復旧時：`/oauth2/validate` ヘルス、未処理 backfill の完了、`state.version` 整合の確認

---

この規約に従えば、**小さな PR の反復**で**安全に前進**できます。
**不明点や衝突が起きた場合は `.docs/*` を**先に**更新し、PR で議論してください。
