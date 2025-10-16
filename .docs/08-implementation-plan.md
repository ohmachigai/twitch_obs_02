# `.docs/08-implementation-plan.md` — 実装計画 / PR 駆動ガイド（規範）

> 本章は **PR 単位での実装手順 / 受け入れ基準（DoD）/ 手動・自動検証** を明確化します。
> ここに記す **順序・範囲・観測点・CI 要件** は拘束力を持ちます（**MUST**）。
> 上位仕様：`01–07`。本章と矛盾があれば、上位仕様を優先し本章を更新してください。

---

## 0. 前提（共通ルール）

* **ブランチ戦略**：`feature/<短い要約>`／`fix/<短い要約>`。PR は小さく（~50–200行目安、上限 ~400）。
* **PR テンプレ**（`AGENTS.md` 準拠、必須）：目的／範囲／変更点／手動検証（Linux/Windows）／自動テスト／観測点／ロールバック／関連 `.docs/*`。
* **CI 要件（全 PR 共通, MUST）**

  * Rust：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、`cargo test --workspace`
  * Frontend（該当時）：`npm ci && npm run lint && npm run typecheck && npm run build`
  * OS マトリクス：ubuntu-latest / windows-latest
* **観測点の追加**：各 PR で **Tap（SSE）/ 構造化ログ / メトリクス** を追加・更新（`07` 規範）。
* **セキュリティ**：PII/トークンはログ/タップで **既定マスク**。`/_debug/*` は dev のみ無認可可、本番は管理者必須。

---

## 1. フェーズ別ゴール（概観）

| フェーズ                  | 目的                   | 必須アウトカム（抜粋）                                                |
| --------------------- | -------------------- | ---------------------------------------------------------- |
| **Phase 1（MVP パス確立）** | Webhook → SSE の最低限パス | 署名検証・204 即 ACK、Normalizer/Policy 骨格、SSE（id=version/心拍/リング） |
| **Phase 2（運用効率）**     | ローカル検証・自動復旧          | Mock/Tap/Capture/Replay、購読ユーティリティ、管理 UI 初期機能               |
| **Phase 3（永続化）**      | 保存・TTL 安定運用          | SQLite/WAL、`command_log`/`event_raw` の 72h TTL、`/metrics`  |
| **Phase 4（付加価値）**     | 実運用に必要な周辺            | Helix 本呼び出し、Backfill、OAuth 完走、テーマ拡張                        |

> 実作業は **PR-0 → PR-8** の順で前進。後続 PR は前提の DoD を満たしたあとに着手。

---

## 2. ツールチェーン & ルートコマンド

* **Rust**：stable（`rustup`）
* **Node**：18+（Vite 用）
* **DB**：SQLite（`sqlx`）
* **便利スクリプト**（追加予定）：`scripts/dev.sh` / `scripts/dev.ps1`（サーバ & Vite 同時起動）

**共通手動検証**（Linux 例、Windows は PowerShell 版を併記）：

```bash
cargo run -p twi-overlay-app
curl -f http://127.0.0.1:8080/healthz
```

---

## 3. PR ごとの実装計画（DoD 付き）

> 各 PR は **範囲を固定**し、**小さく確実に**。下記の **コード変更点・観測点・テスト・手動検証** を満たしてください。

---

### **PR-0：スキャフォールド & CI ベースライン**

**目的**：モノレポ骨格と CI を整備して「何も壊れていない」ことを保証。

**変更点（最小）**

* `Cargo.toml`（workspace）／`crates/{app,core,storage,twitch,util}` の空ひな型
* `web/overlay`（Vite + TS の最小テンプレ）
* `.github/workflows/ci.yml`（Linux/Windows マトリクス）
* `.env.example`（最小キー）
* `scripts/`：`dev.sh` / `dev.ps1`（起動補助）

**観測点**

* `/healthz` の 200（固定値）

**テスト**

* Rust：空テスト（`assert!(true)`）
* Frontend：`npm run build` 成功のみ

**手動検証**

```bash
cargo run -p twi-overlay-app
curl -f http://127.0.0.1:8080/healthz
```

**DoD**

* CI グリーン（Linux/Windows）
* 既存 `.docs/*` 参照の README 断片を `repo_overview.md` に追記（任意）

---

### **PR-1：設定 & 観測基盤（tracing / metrics / Tap 足場）**

**目的**：以降のデバッグを成立させる観測の土台。

**変更点**

* `util::config`（dotenv 読込 + 必須キー検証）
* `tracing-subscriber` 初期化（prod=json / dev=pretty）
* `metrics`（Prometheus）導入、`GET /metrics` 追加
* **Tap チャンネル導入**：`/_debug/tap` の骨格（SSE）と `StageEvent` 型（空イベントでも可）

**観測点**

* `app_build_info{version,git}`、`app_uptime_seconds`
* Tap にダミー StageEvent を 10s 間隔で publish（dev）

**テスト**

* 構造化ログのキー存在
* `/metrics` の基本エクスポート

**手動検証**

```bash
curl -N http://127.0.0.1:8080/_debug/tap
curl http://127.0.0.1:8080/metrics | head
```

**DoD**

* `/_debug/tap` が SSE で流れる
* `/metrics` が読める（Prometheus 互換）

---

### **PR-2：Webhook Ingress（HMAC/±10分/冪等/204 即 ACK）**

**目的**：EventSub の受け口を仕様どおりに実装。

**変更点**

* `POST /eventsub/webhook`

  * HMAC（`id||timestamp||raw`）検証（定数時間比較）
  * `timestamp` ±10 分
  * `Message-Id` 冪等（重複は 204）
  * verification=200+平文 / notification=**204 即 ACK** / revocation=204
* `storage::event_raw`（`05` の DDL に準拠、72h 対象）
* Tap：`stage="ingress"` に `in/out/meta` 出力
* メトリクス：`eventsub_ingress_total{type}`、`eventsub_invalid_signature_total`、`webhook_ack_latency_seconds`

**テスト**

* 正常/署名不正/±10分外/`Message-Id` 重複
* 204 ACK のレスポンス時間（統合テストで p95 の閾値断言は不要、代わりにロジック単体）

**手動検証**

```bash
# verification
curl -X POST http://127.0.0.1:8080/eventsub/webhook \
  -H 'Twitch-Eventsub-Message-Type: verification' \
  -d '{"challenge":"XYZ"}'

# notification（署名はテスト用ヘルパで生成）
```

**DoD**

* 仕様どおりのステータス（200/204）と検証
* Tap に `ingress` が出力
* `event_raw` へ追加される（72h 対象）

---

### **PR-3：Normalizer & Policy（enqueue / refund|consume の決定）**

**目的**：ドメインイベントとポリシー決定を実装（Helix はまだ **dry-run**）。

**変更点**

* `core::normalizer`：EventSub → `NormalizedEvent`（決定的）
* `core::policy`：

  * 対象 Reward の判定
  * **反スパム**：同 user×reward×`anti_spam_window_sec` 以内は `consume`
  * コマンド出力：`enqueue` + `redemption.update(mode=refund|consume, applicable?)`（dry-run）
* Tap：`stage="normalizer"|"policy"` を出力
* メトリクス：`policy_commands_total{kind}`

**テスト**

* 正規化の決定性（同入力→同出力）
* 反スパム判定（59s/61s）
* 対象外 Reward の無視

**手動検証**

```bash
curl -N "http://127.0.0.1:8080/_debug/tap?s=normalizer,policy"
# テスト用サンプル JSON を POST（モック）→ Tap で command を確認
```

**DoD**

* `normalized`/`policy` の StageEvent が観測可能
* 反スパムの境界が期待どおり

---

### **PR-4：CommandLog（version++）/ Projector（パッチ生成）/ SSE Hub**

**目的**：**一次ソース**と**増分配信**の中核を完成。

**変更点**

* `storage::state_index` / `command_log`（同一 Tx で version++ & append, MUST）
* `core::projector`：Command → `queue/counters/settings` 更新、**Patch 生成**
* SSE Hub：`/overlay/sse` & `/admin/sse`

  * **`id=version`**、**20–30s 心拍**（`:heartbeat`）
  * **リング再送**（直近 N=1000 or 2min）
  * 初回のみ `since_version`、再接続は `Last-Event-ID`
  * **トークン検証**（短寿命署名トークン：`sub=broadcaster_id`、`aud ∈ {overlay,admin}`）
* Tap：`stage="command"|"projector"|"sse"`
* メトリクス：`projector_patches_total{type}`、`sse_clients{aud}`、`sse_broadcast_latency_seconds`、`sse_ring_miss_total{aud}`

**テスト**

* version の単調性（Tx 内）
* 各 Command → 期待パッチ
* SSE 擬似クライアント（`Last-Event-ID` を使う統合テスト）

**手動検証**

```bash
curl -N "http://127.0.0.1:8080/overlay/sse?broadcaster=b-dev&since_version=0&token=eyJ..."
# Tap で command→projector→sse の時系列を確認
```

**DoD**

* SSE が `id=version` で配信、心拍・リング再送が機能
* 欠落時は `state.replace` が送られる（リング外フォールバック）

---

### **PR-5：State REST（初期化） & Overlay 最小 UI**

**目的**：**初期 REST → SSE 追随**の UX を成立させる。

**変更点**

* `GET /api/state?broadcaster=&scope=session|since&since=`

  * `queue` / `counters_today` / `settings` / `version`
* `web/overlay`

  * クエリ解析（`broadcaster` 必須、`theme/variant/accent/group_size/types/debug`）
  * 初期 REST → `lastAppliedVersion` 保存 → SSE 接続
  * パッチ適用（厳密増分：`state.version+1` のみ）・`localStorage("overlay:lastVersion:<b>")` 保存
  * 簡易 HUD（接続/エラー/受信件数）

**テスト**

* `api/state` の整合（ソート規約）
* パッチ適用の純粋関数ユニットテスト（`state + patch -> state'`）
* TypeScript 型チェック & ESLint

**手動検証**

* ブラウザ/OBS でオーバーレイを開く
* モック投入 → 画面更新
* Reload → 欠落なく追随（`since_version` / `Last-Event-ID`）

**DoD**

* 初回 REST → SSE で問題なく表示
* 再読込冪等が成立

---

### **PR-6：Admin Mutations（COMPLETE / UNDO / Settings） + 管理 UI 最小**

**目的**：手動操作の冪等・差分伝搬を完成。

**変更点**

* `POST /api/queue/dequeue {entry_id, mode:"COMPLETE"|"UNDO", op_id}`
* `POST /api/settings/update {patch, op_id}`
* `web/admin`（htmx ベースの軽量フォーム + SSE ビュー）
* `op_id` 冪等（`command_log(broadcaster, op_id)` partial UNIQUE）

**テスト**

* COMPLETE：`queue.completed`、counter 不変
* UNDO：`queue.removed(reason=UNDO)` + `counter--`
* `op_id` 冪等：同内容=200、矛盾=412

**手動検証**

```bash
curl -sS -X POST /api/queue/dequeue -d '{"broadcaster":"b-dev","entry_id":"...","mode":"UNDO","op_id":"..."}'
```

**DoD**

* Overlay/Admin 双方に差分が即時反映
* `op_id` 冪等が成立

---

### **PR-7：TTL ジョブ（72h）/ WAL チェックポイント / メトリクス**

**目的**：長期稼働のための保守運用を有効化。

**変更点**

* バックグラウンドジョブ：

  * `event_raw` / `command_log` の **小分け DELETE（LIMIT 1000）**
  * `PRAGMA wal_checkpoint(TRUNCATE)` の周期実行
* メトリクス：`db_ttl_deleted_total{table}`、`db_checkpoint_seconds`、`db_busy_total{op}`

**テスト**

* `Clock` 注入で閾値を跨ぐレコード削除
* WAL checkpoint の呼び出し（結果コード検証）

**手動検証**

* 旧レコード投入 → TTL 実行 → 削除確認（件数メトリクス増加）
* WAL サイズの推移確認

**DoD**

* DB サイズが安定し、現行 state に影響なし（Queue/Counters/Settings 永続）

---

### **PR-8：Helix 実呼び出し / Backfill / OAuth 完走**

**目的**：本番連携を完了させる（安全に段階的適用）。

**変更点**

* `twitch::oauth`：`/oauth/login` / `/oauth/callback` / `/oauth2/validate` 周期検証
* `twitch::helix`：`redemptions.update`（**自アプリ作成リワードのみ**適用）
* `backfill`：未処理（UNFULFILLED）を取得し、Command を古い順に生成
* `/_debug/helix`（dev 限定ログ閲覧）

**テスト**

* Helix モック（HTTP サーバ）で成功/失敗/適用不可の分岐
* Backfill の重複抑止（`queue_entries.redemption_id` partial UNIQUE）
* `/oauth2/validate` の失効検出と再同意パス

**手動検証**

* `dry-run=true` で挙動確認 → `false` にして限定的に実呼出
* revoke → 自動再同意誘導（ログ）

**DoD**

* 返金/消費が対象 Reward にのみ適用
* 失敗時は `managed=false` で記録、サービス継続
* Backfill が決定的に再生される

#### 分割ステップ（小 PR）

1. **Stage-1：仕様更新と基盤整備**（本ドキュメントの追加変更 / OAuth & Backfill スキーマ / AppConfig 拡張 / メトリクス宣言）
2. **Stage-2：ストレージ + Twitch クライアント層**（SQLx リポジトリ、HTTP クライアント足場、Tap/メトリクス登録）
3. **Stage-3：OAuth ハンドラと検証ループ**（`/oauth/login|callback|oauth2/validate` 実装、refresh/validate 結果の観測）
4. **Stage-4：Helix redemption.update と UI 反映**（CommandExecutor 拡張、SSE パッチ/フロントエンド対応）
5. **Stage-5：Backfill ワーカーとデバッグ導線**（周期 backfill、`/_debug/helix`、重複抑止・観測）

> 各ステージは **ドキュメント→実装→テスト→観測点** の順で完結させ、CI 緑の Draft PR として段階的に昇格させる。

---

## 4. リスク & フェイルセーフ（実施順とセット）

| リスク          | 対応 PR  | フェイルセーフ                                          |
| ------------ | ------ | ------------------------------------------------ |
| Webhook 応答遅延 | PR-2   | **204 即 ACK**、重処理は後段                             |
| 欠落・二重反映      | PR-4/5 | `id=version` / `Last-Event-ID` / `state.replace` |
| スパム          | PR-3   | `anti_spam_window_sec` / `duplicate_policy`      |
| DB 肥大        | PR-7   | TTL 小分け / WAL TRUNCATE                           |
| トークン失効       | PR-8   | `/validate` 周期チェック / refresh / 再同意               |
| PII 漏洩       | PR-1/7 | 既定マスク / `/_debug/*` 認可                           |

---

## 5. 実装チップス（落とし穴防止）

* **EventSource** は **カスタムヘッダ不可** → SSE 認可はクエリ `token=` か **同一オリジン Cookie**。
* SSE は **プロキシでバッファ禁止**（本番は Nginx：`proxy_buffering off`）。
* `state.version+1` 以外のパッチは **適用しない**。リング外は `state.replace` を待つ。
* **Windows** の改行や PATH 差異を吸収するため、スクリプトは bash と PowerShell を両方用意。
* 時刻は **UTC**、”今日”の判定は **配信者 TZ**（`Clock` 抽象を注入）。

---

## 6. 進捗可視化（バーンダウン）

* **PR 状態**：`Draft` → `Ready for review` → `Approved` → `Merged`
* **毎 PR の DoD** を満たしたら進捗+1。
* **Tap/metrics スナップショット**（Ingress 件数 / SSE クライアント数）を週次で記録（任意）。

---

## 7. 付録：手動検証コマンド集（Linux/Windows）

> 代表のみ。全コマンドは PR テンプレにも記載すること（MUST）。

**SSE 受信**

```bash
curl -N "http://127.0.0.1:8080/overlay/sse?broadcaster=b-dev&since_version=0&token=eyJ..."
```

**State 初期取得**

```bash
curl -sS "http://127.0.0.1:8080/api/state?broadcaster=b-dev&scope=session" | jq .
```

**キュー外し（UNDO）**

```bash
curl -sS -X POST http://127.0.0.1:8080/api/queue/dequeue \
  -H "Content-Type: application/json" \
  -d '{"broadcaster":"b-dev","entry_id":"01HZX...","mode":"UNDO","op_id":"'"$(uuidgen)"'"}'
```

**Tap（policy のみ）**

```bash
curl -N "http://127.0.0.1:8080/_debug/tap?s=policy&broadcaster=b-dev"
```

---

## 8. 受け入れチェック（フェーズ完了判定）

**Phase 1**

* [ ] Webhook：HMAC/±10 分/冪等/204 即 ACK
* [ ] Normalizer/Policy の決定性
* [ ] SSE：`id=version`/心拍/リング

**Phase 2**

* [ ] Tap/Capture/Replay が動作
* [ ] 管理 UI 最小機能（COMPLETE/UNDO/Settings）
* [ ] 自動再購読ユーティリティ（購読作成は別 PR で可）

**Phase 3**

* [ ] SQLite(WAL)、TTL ジョブ、WAL checkpoint
* [ ] `/metrics` の主要メトリクス

**Phase 4**

* [ ] Helix 実呼び出し（適用可否の扱い）、Backfill
* [ ] OAuth 完走（/validate 周期）

---

この計画に従えば、**毎 PR ごとに「観測可能な完成」を積み上げ**られます。
以降の PR では、**新規 API/DB 変更**がある場合に **`04` / `05` を先に更新**し、ここ（`08`）の手順・DoD を追記してください。
