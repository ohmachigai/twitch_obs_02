# 環境構築と運用手順

この章では、ローカル開発・ステージング・本番運用に向けたセットアップ手順、必要な環境変数、監視・運用コマンドをまとめます。`.docs/10-operations-runbook.md` の運用指針と照らし合わせながら利用してください。
- **確認ファイル**: `.docs/10-operations-runbook.md`, `crates/util/src/config.rs`, `scripts/`

## 1. 必須環境と依存

- **Rust**: 1.72 以上（`rustup toolchain install stable`）。
  - **確認ファイル**: `Cargo.toml`, `.github/workflows/`（存在する場合の CI 設定）
- **Node.js**: 18 以上（フロントエンド開発用）。
  - **確認ファイル**: `web/overlay/package.json`, `web/admin/package.json`, `web/shared/package.json`
- **SQLite**: バンドルされた `libsqlite3` を利用。追加インストールは不要ですが、WAL を有効化できる環境が必要です。
  - **確認ファイル**: `crates/storage/src/lib.rs`, `migrations/`
- **npm**: Vite ベースのフロントエンドをビルドするために使用します。
  - **確認ファイル**: `web/overlay/package.json`, `web/admin/package.json`, `web/shared/package.json`

## 2. 環境変数一覧（`AppConfig::from_env`）

`crates/util/src/config.rs` の `AppConfig` で読み取られる主な環境変数とデフォルト値は以下のとおりです。
- **確認ファイル**: `crates/util/src/config.rs`, `crates/util/src/lib.rs`, `.env.example`（存在する場合）

| 変数 | 説明 | デフォルト（開発時） |
| --- | --- | --- |
| `APP_ENV` | `development` / `production` / `test` | `development` |
| `DATABASE_URL` | SQLite 接続文字列 | `sqlite://./dev.db` |
| `WEBHOOK_SECRET` | EventSub のシグネチャ検証で使用する共有秘密鍵 | 開発では `dev-secret-change-me` |
| `SSE_TOKEN_SIGNING_KEY` | SSE 用トークンを署名する 16 進文字列 | 開発では `646576...`（`DEV_SSE_TOKEN_HEX`） |
| `SSE_HEARTBEAT_SECS` | SSE 心拍間隔 | `25` |
| `SSE_RING_MAX` | SSE リングバッファの最大イベント数 | `1000` |
| `SSE_RING_TTL_SECS` | SSE リングの保持秒数 | `120` |
| `TWITCH_CLIENT_ID` | Twitch アプリケーションのクライアント ID | `local-client-id` |
| `TWITCH_CLIENT_SECRET` | Twitch クライアントシークレット | `local-client-secret` |
| `OAUTH_REDIRECT_URI` | OAuth コールバック URL | `http://127.0.0.1:8080/oauth/callback` |
| `TWITCH_OAUTH_BASE_URL` | Twitch OAuth ベース URL | `https://id.twitch.tv/oauth2` |
| `TWITCH_API_BASE_URL` | Helix API ベース URL | `https://api.twitch.tv/helix` |
| `OAUTH_STATE_TTL_SECS` | OAuth state の有効期限 | `600` |
| `HELIX_BACKFILL_INTERVAL_SECS` | バックフィル走査間隔 | `300` |
| `HELIX_BACKFILL_PAGE_SIZE` | Helix ページサイズ | `50` |

`.env` を用意すれば `twi_overlay_util::load_env_file()` により自動で読み込まれます。
- **確認ファイル**: `crates/util/src/lib.rs` の `load_env_file`, `scripts/dev.sh`

## 3. ローカル開発手順

### 3.1 最小構成

1. `rustup toolchain install stable`
2. `npm install --global npm@latest`（任意）
3. リポジトリ直下で以下を実行します。

```bash
# 1) 依存ライブラリのセットアップ
npm install --prefix web/overlay
npm install --prefix web/admin

# 2) バックエンド起動
cargo run -p twi-overlay-app

# 3) フロントエンド起動（別ターミナル）
cd web/overlay && npm run dev -- --host
```

`scripts/dev.sh` を使うとバックエンドとオーバーレイを同時に起動できます。スクリプト内では `APP_ENV` と `DATABASE_URL`、`WEBHOOK_SECRET` をデフォルト値でエクスポートしてから `cargo run` と `npm run dev` を並列実行しています。
- **確認ファイル**: `scripts/dev.sh`, `crates/app/src/main.rs`, `web/overlay/package.json`

```bash
# scripts/dev.sh より
cargo run -p twi-overlay-app &
pushd web/overlay > /dev/null
npm run dev -- --host &
wait "$SERVER_PID" "$WEB_PID"
```

### 3.2 テスト実行

```bash
# Rust ワークスペースのテスト
cargo test --workspace

# Lint / フォーマット
cargo fmt --all --check
cargo clippy --workspace --all-targets -D warnings

# フロントエンド
cd web/overlay && npm run test
cd web/shared && npm run test
```

`.docs/08-implementation-plan.md` の DoD に従い、Rust と Node の両方でテストを実行してください。
- **確認ファイル**: `.docs/08-implementation-plan.md`, `Cargo.toml`, `package.json` 類

## 4. Twitch Webhook のセットアップ

1. `WEBHOOK_SECRET` を Twitch イベントサブスクリプション設定と一致させます（関連コード: `crates/util/src/config.rs`, `crates/app/src/webhook.rs`）。
2. `POST /eventsub/webhook` が公開インターネットから到達できるようトンネリング（ngrok など）を設定し、`TWITCH_EVENTSUB_CALLBACK` を更新します。
3. `Message-Id` と `Message-Timestamp` は `handle` 内で検証されます。署名が一致しない場合 `403`、タイムスタンプが ±10 分を超えると `400` が返ります（関連コード: `crates/app/src/webhook.rs` の `verify_signature`, `parse_timestamp`）。
4. チャレンジ応答（`Message-Type: webhook_callback_verification`）ではボディの `challenge` をそのまま返します（関連コード: `crates/app/src/webhook.rs` の `respond_to_challenge`）。

## 5. SSE トークンの生成

`SseTokenValidator` は HMAC-SHA256 で署名された標準的な JWT を検証します。トークンは `jsonwebtoken` クレートでデコードされ、以下のクレームを前提としています。
- **確認ファイル**: `crates/app/src/sse.rs`, `crates/util/src/config.rs`, `.docs/04-api-contracts.md`, `scripts/make_token.py`

```json
{
  "sub": "<broadcaster_id>",
  "aud": "overlay",        // または "admin"
  "exp": 1680000000,        // UNIX タイムスタンプ（秒）
  "iat": 1679996400,        // 発行時刻（scripts/make_token.py が自動で付与）
  "nbf": 1679996700         // 任意。設定した場合のみ検証対象。
}
```

`TokenClaims` で実際に検証されるのは `sub`, `aud`, `exp`, `nbf` の 4 つで、`iat` など追加のクレームは無視されます。`nbf` を省略した場合は即時有効です。

署名キーは `SSE_TOKEN_SIGNING_KEY` の 16 進文字列で、`scripts/make_token.py` が `--key-hex` オプション経由で読み込みます。開発時は `.docs/07-debug-telemetry.md` に記載の通り、`TapHub::spawn_mock_publisher` がデバッグ SSE を生成します。
- **確認ファイル**: `crates/app/src/sse.rs`, `crates/app/src/tap.rs`, `.docs/07-debug-telemetry.md`, `scripts/make_token.py`

## 6. メトリクスと監視

- `/metrics`: `telemetry::render_metrics` が Prometheus フォーマットを出力します。`eventsub_ingress_total`, `policy_commands_total`, `sse_active_connections` などが確認できます（関連コード: `crates/app/src/telemetry.rs`, `crates/app/src/router.rs`）。
- `/healthz`: 単純な L4 ヘルスチェック。データベース接続の有無は確認しません（関連コード: `crates/app/src/router.rs`, `crates/app/src/main.rs`）。
- `/_debug/tap`: `s=ingress,policy,...` とカンマ区切りでステージを指定し、処理のトレースを SSE で観測できます（関連コード: `crates/app/src/tap.rs`）。
- `/_debug/helix`: Helix バックフィルの状態やエラーを JSON で表示します（関連コード: `crates/app/src/backfill.rs` の `debug_helix`）。

## 7. データベース運用

- マイグレーション: `Database::run_migrations` が `migrations/` を実行します。新しいテーブルを追加したら `.docs/05-data-schema-and-migrations.md` にも変更を反映してください（関連コード: `crates/storage/src/lib.rs`, `migrations/*.sql`, `.docs/05-data-schema-and-migrations.md`）。
- TTL: `maintenance.rs` が 72 時間より古い `event_raw` と `command_log` を削除します。より長い保持期間が必要な場合は `.docs/05` を更新し、`MaintenanceWorker` の間隔や削除ロジックも変更します（関連コード: `crates/app/src/maintenance.rs`, `crates/storage/src/lib.rs`）。
- WAL チェックポイント: `Database::wal_checkpoint_truncate` を定期実行し、`.wal` ファイルの肥大化を防ぎます（関連コード: `crates/storage/src/lib.rs`, `crates/app/src/maintenance.rs`）。

## 8. OAuth / Helix 運用

- `GET /oauth/login` を踏むと `oauth_login_states` に `state` が記録され、`.docs/04-api-contracts.md` のセキュリティ要件（10 分 TTL）が実装されています（関連コード: `crates/app/src/oauth.rs`, `crates/storage/src/lib.rs`）。
- `oauth_links` に保存されたトークンは `BackfillWorker` と管理操作で Helix API を叩く際に使用されます。期限切れやスコープ不足時は `ERR_OAUTH_*` 系のエラーコードが SSE と REST レスポンスに含まれます（関連コード: `crates/app/src/backfill.rs`, `crates/app/src/command.rs`, `crates/storage/src/lib.rs`）。
- `POST /oauth2/validate` は管理 UI がトークンを検証するためのエンドポイントで、`.docs/04` に記載されたレスポンススキーマを満たしています（関連コード: `crates/app/src/oauth.rs`, `.docs/04-api-contracts.md`）。

## 9. 本番デプロイ時の注意

- `APP_ENV=production` を設定し、`WEBHOOK_SECRET` と `SSE_TOKEN_SIGNING_KEY` に本番用のランダム値を設定します（関連コード: `crates/util/src/config.rs`, `.docs/10-operations-runbook.md`）。
- `DATABASE_URL` は永続ボリュームを指すパスに変更してください（関連コード: `crates/util/src/config.rs`, `.docs/05-data-schema-and-migrations.md`）。
- Nginx 等でリバースプロキシする場合、`.docs/10-operations-runbook.md` の SSE 設定（`proxy_buffering off` など）を守ります（関連資料: `.docs/10-operations-runbook.md`, `deploy/` が存在する場合）。
- TLS 終端後のヘルスチェックは `/oauth2/validate` を利用するとデータベース・Helix 連携の状態も確認できます（関連コード: `crates/app/src/oauth.rs`, `crates/app/src/backfill.rs`）。

## 10. 参考: コマンド早見表

| 目的 | コマンド |
| --- | --- |
| バックエンド起動 | `cargo run -p twi-overlay-app` |
| バックエンドビルド | `cargo build -p twi-overlay-app --release` |
| フロントエンド（overlay）起動 | `cd web/overlay && npm run dev -- --host` |
| フロントエンド（admin）ビルド | `cd web/admin && npm run build` |
| すべてのテスト | `cargo test --workspace && npm test --workspaces` |
| Lint / フォーマット | `cargo fmt --all --check && cargo clippy --workspace --all-targets -D warnings` |

これらの手順を踏めば、ローカル環境から本番運用まで一貫した設定でプロジェクトを動かすことができます。
- **確認ファイル**: 本章で参照した `crates/util/src/config.rs`, `crates/app/src/*.rs`, `web/*/package.json`, `scripts/*`, `.docs/10-operations-runbook.md`
