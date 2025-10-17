# プロジェクト構成ガイド

この章では、リポジトリ内の主要ディレクトリとファイルを俯瞰し、それぞれがどの役割を担っているかを具体的なコード例とともに説明します。必要に応じて `.docs/` の仕様と突き合わせながら読むことで、どの層を変更すれば目的を達成できるか判断できます。

## ルートディレクトリ

| パス | 役割 |
| --- | --- |
| `Cargo.toml` | Rust ワークスペースのエントリー。`crates/` 配下のメンバークレートをまとめ、共通の依存やビルド設定を管理します。 |
| `migrations/` | sqlx 用のマイグレーション（`.sql`）が置かれます。`Database::run_migrations` から読み込まれます。 |
| `scripts/` | ローカル開発や CI で使用する補助スクリプト。例えば `scripts/dev.sh` はバックエンドとフロントエンドを同時起動するシェルです。 |
| `web/` | フロントエンド関連のパッケージ（オーバーレイ UI、管理 UI、共有ライブラリ）。 |
| `.docs/` | アーキテクチャと仕様の一次ソース。`.docs/02-architecture-overview.md` などがバックエンド処理の契約を定義します。 |
| `repo_overview.md` | リポジトリ全体の簡易概要。 |

## Rust ワークスペース（`crates/`）

```
crates/
├── app        # Axum ベースの HTTP サーバ（エントリーポイント）
├── core       # ドメインロジック（正規化・ポリシー・プロジェクタ）
├── storage    # SQLite 永続化レイヤーとリポジトリ
├── twitch     # Twitch API / OAuth クライアント
└── util       # 環境変数読み込みや共通設定ユーティリティ
```

### `crates/app`

Axum サーバのルーティング・ハンドラ・背景処理を実装します。`src/main.rs` の初期化に続き、`router.rs` が API ルートを定義しています。
- **確認ファイル**: `crates/app/src/main.rs`, `crates/app/src/router.rs`

```rust
// crates/app/src/router.rs
pub fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/_debug/tap", get(debug_tap))
        .route("/_debug/helix", get(backfill::debug_helix))
        .route("/overlay/sse", get(overlay_sse))
        .route("/admin/sse", get(admin_sse))
        .route("/api/state", get(state_snapshot))
        .route("/api/queue/dequeue", post(queue_dequeue))
        .route("/api/settings/update", post(settings_update))
        .route("/eventsub/webhook", post(webhook::handle))
        .route("/oauth/login", get(oauth::login))
        .route("/oauth/callback", get(oauth::callback))
        .route("/oauth2/validate", post(oauth::validate))
        .with_state(state)
}
```

主なサブモジュール:

- `webhook.rs`: EventSub シグネチャ検証、`twi_overlay_core::normalizer` を用いた正規化、ポリシー評価・コマンド実行までを統括。
- `command.rs`: `CommandExecutor` と `CommandLog` を通じ、永続化と SSE への差分配信を行う。
- `sse.rs`: Server-Sent Events のコネクション管理。`SseHub` がリングバッファを保持し、`SseStream` ストリームを生成。
- `tap.rs`: `/_debug/tap` の実装。各処理段のステージイベントを購読できる。
- `backfill.rs`: Helix API からのバックフィル。`BackfillWorker` が周期的に未処理レコードを補完。
- `maintenance.rs`: TTL 削除や WAL チェックポイントなどの保守タスクを実行。
- `oauth.rs`: OAuth ログインフローとバリデーション。
- `state.rs`: `build_state_snapshot` で `/api/state` のレスポンスを構築。

これらのモジュールは `.docs/02-architecture-overview.md` と `.docs/04-api-contracts.md` の仕様を満たすように分担されています。
- **確認ファイル**: `crates/app/src/webhook.rs`, `crates/app/src/command.rs`, `crates/app/src/sse.rs`, `crates/app/src/tap.rs`, `crates/app/src/backfill.rs`, `crates/app/src/maintenance.rs`, `crates/app/src/oauth.rs`, `crates/app/src/state.rs`, `.docs/02-architecture-overview.md`, `.docs/04-api-contracts.md`

### `crates/core`

EventSub の処理とアプリケーションのドメインロジックを担当します。
- `normalizer.rs`: Twitch から届く生の JSON を `NormalizedEvent` へ変換。署名済みイベントの型安全な表現を提供します。
- `policy.rs`: `PolicyEngine::evaluate` が設定 (`Settings`) と最新の状態 (`QueueEntry` や `DailyCounter`) を見ながらコマンド列を生成。
- `projector.rs`: `Projector` がコマンドの結果を `Patch` としてまとめ、フロントエンドへ送り出せる差分形式にします。
- `types.rs`: `Settings` や `QueueEntry` など、サーバ・フロント共通のシリアライズ可能な型を定義。
- **確認ファイル**: `crates/core/src/normalizer.rs`, `crates/core/src/policy.rs`, `crates/core/src/projector.rs`, `crates/core/src/types.rs`

例えば `PolicyEngine::evaluate` は以下のようにシンプルな関数インターフェースを提供します。

```rust
// crates/core/src/policy.rs
pub fn evaluate(&self, context: PolicyContext) -> PolicyOutcome {
    let PolicyContext { event, settings, counters, queue } = context;
    match event {
        NormalizedEvent::RedemptionAdd { .. } => self.on_redemption_add(settings, counters, queue, event),
        NormalizedEvent::RedemptionUpdate { .. } => self.on_redemption_update(queue, event),
        NormalizedEvent::StreamOnline { .. } => self.on_stream_online(settings),
        NormalizedEvent::StreamOffline { .. } => self.on_stream_offline(settings),
    }
}
```

### `crates/storage`

SQLite のコネクションプールと各テーブルに対するリポジトリを提供します。`Database` が各リポジトリのファクトリとして機能します。
- **確認ファイル**: `crates/storage/src/lib.rs`, `migrations/`

```rust
// crates/storage/src/lib.rs
impl Database {
    pub fn command_log(&self) -> CommandLogRepository {
        CommandLogRepository { pool: self.pool.clone() }
    }
    pub fn queue(&self) -> QueueRepository {
        QueueRepository { pool: self.pool.clone() }
    }
    pub fn oauth_links(&self) -> OauthLinkRepository {
        OauthLinkRepository { pool: self.pool.clone() }
    }
    // そのほか event_raw, state_index, helix_backfill など
}
```

各リポジトリは sqlx のクエリビルダを用いて実装され、`.docs/05-data-schema-and-migrations.md` に記載されたテーブル構造と整合しています。
- **確認ファイル**: `crates/storage/src/lib.rs` 内 `CommandLogRepository`, `QueueRepository`, `DailyCounterRepository`, `.docs/05-data-schema-and-migrations.md`, `migrations/*.sql`

### `crates/twitch`

Helix API と OAuth を扱うクライアントラッパーです。`HelixClient` が REST API 呼び出し、`TwitchOAuthClient` がアクセストークンの交換や更新を担います。HTTP クライアントには `reqwest` を使用し、レスポンス型は `serde` でデコードされます。
- **確認ファイル**: `crates/twitch/src/helix.rs`, `crates/twitch/src/oauth.rs`, `Cargo.toml` (依存定義)

### `crates/util`

環境変数からの設定読み込み (`AppConfig::from_env`) や、ローカル開発で `.env` を自動読み込みする `load_env_file` などを提供します。
- **確認ファイル**: `crates/util/src/config.rs`, `crates/util/src/lib.rs` の `load_env_file`

```rust
// crates/util/src/config.rs
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub environment: Environment,
    pub database_url: String,
    pub webhook_secret: String,
    pub sse_token_signing_key: Vec<u8>,
    pub sse_heartbeat_secs: u64,
    pub sse_ring_max: usize,
    pub sse_ring_ttl_secs: u64,
    pub twitch_client_id: String,
    pub twitch_client_secret: String,
    pub oauth_redirect_uri: String,
    pub twitch_oauth_base_url: String,
    pub twitch_api_base_url: String,
    pub oauth_state_ttl_secs: u64,
    pub helix_backfill_interval_secs: u64,
    pub helix_backfill_page_size: u32,
}
```

## フロントエンド (`web/`)

```
web/
├── overlay   # 配信画面に表示されるオーバーレイ
├── admin     # 管理 UI（モデレーター・配信者向け）
└── shared    # 両者で共通利用する型とロジック
```

### `web/shared`

- `src/types.ts`: サーバから送信される `Patch` や `StateSnapshot` の型定義。`twi_overlay_core::types` と 1:1 に対応します。
- `src/state.ts`: 受信したパッチをクライアント状態へ適用するための純粋関数（`createClientState`, `applyPatch`）。
- `src/state.test.ts`: 差分適用のユニットテスト。
- **確認ファイル**: `web/shared/src/types.ts`, `web/shared/src/state.ts`, `web/shared/src/state.test.ts`

### `web/overlay`

- `src/api.ts`: REST と SSE のクライアント実装。`fetchState` は `/api/state` を叩き、`createSseConnection` は `/overlay/sse` へ接続します。
- `src/App.tsx`: 受信した状態を描画。Tailwind 風のユーティリティクラスを CSS Modules 的に使用。
- `src/api.test.ts`: API クライアントの基本的な振る舞いテスト。
- **確認ファイル**: `web/overlay/src/api.ts`, `web/overlay/src/App.tsx`, `web/overlay/src/api.test.ts`

### `web/admin`

- `src/config.ts`: 管理 UI の設定（SSE トークンなど）を扱う。`config.test.ts` で検証。
- `src/settings.ts`: 設定変更フォームのロジックと `settings.test.ts` によるテスト。
- `src/api.ts`: 管理 UI 向けの API 呼び出し（キュー操作、設定更新など）。
- **確認ファイル**: `web/admin/src/config.ts`, `web/admin/src/config.test.ts`, `web/admin/src/settings.ts`, `web/admin/src/settings.test.ts`, `web/admin/src/api.ts`

## その他の補助ファイル

- `scripts/dev.sh`: ローカル開発でバックエンド（`cargo watch`）とフロントエンド（`npm run dev`）を並行起動するサンプル。
- `.github/workflows/`（存在する場合）: CI 設定。リント・テスト・ビルドの自動実行を定義。
- `AGENTS.md`: この資料を含む作業規約。PR 方針や CI 要件が明示されています。

この章を参照すれば、関心のある挙動に関連するファイルを素早く特定できます。次章では実際のデータフローに沿って各コンポーネントの連携を追跡します。
- **確認ファイル**: 本章で列挙した各ディレクトリのファイル、特に `crates/*`, `web/*`, `scripts/`, `migrations/`
