# 付録

補足資料として、用語集・主要データ構造の早見表・関連ドキュメントをまとめます。
- **確認ファイル**: `crates/app/src/`, `crates/core/src/`, `crates/storage/src/lib.rs`, `web/shared/src/`, `.docs/*`

## 1. 用語集

| 用語 | 説明 | 実装参照 |
| --- | --- | --- |
| EventSub | Twitch が配信イベントを送信する Webhook。 | `crates/app/src/webhook.rs`、`.docs/02-architecture-overview.md` |
| NormalizedEvent | EventSub を正規化した内部イベント。 | `crates/core/src/normalizer.rs` |
| PolicyEngine | イベントに対して `Command` を生成する判定器。 | `crates/core/src/policy.rs` |
| Command | 状態を変更する命令。キュー操作や設定更新など。 | `crates/core/src/types.rs`、`crates/app/src/command.rs` |
| Patch | クライアントへ配信する差分。 | `crates/core/src/types.rs`、`web/shared/src/types.ts` |
| SSE | Server-Sent Events。差分配信に使用。 | `crates/app/src/sse.rs`、`.docs/04-api-contracts.md` |
| Tap | 処理段ごとの可視化ストリーム。 | `crates/app/src/tap.rs` |
| Helix | Twitch API の名称。リワードやユーザ情報を取得。 | `crates/twitch/src` |
| OAuth Link | Helix API 呼び出し用のアクセストークン情報。 | `crates/storage/src/lib.rs`（`OauthLinkRepository`） |

## 2. 主要データ構造早見表

| 構造体 | 主なフィールド | 説明 |
| --- | --- | --- |
| `QueueEntry` | `id`, `user_id`, `reward_id`, `status`, `managed` など | キューに登録されたリワード処理要求。`queue_entries` テーブルに対応。参照: `crates/core/src/types.rs`, `crates/storage/src/lib.rs` |
| `Settings` | `overlay_theme`, `group_size`, `policy` など | 配信者ごとの設定。`settings` テーブルに保存。参照: `crates/core/src/types.rs`, `crates/storage/src/lib.rs` |
| `PolicySettings` | `anti_spam_window_sec`, `duplicate_policy`, `target_rewards` | ポリシー評価で使用する制御値。参照: `crates/core/src/policy.rs`, `crates/core/src/types.rs` |
| `StateSnapshot` | `version`, `queue`, `counters_today`, `settings` | `/api/state` のレスポンス。初期同期に利用。参照: `crates/app/src/state.rs`, `crates/core/src/types.rs`, `.docs/04-api-contracts.md` |
| `Patch` | `version`, `type`, `data` | SSE 差分。バージョンと種別により適用先が決まる。参照: `crates/core/src/types.rs`, `web/shared/src/state.ts` |
| `OauthLink` | `access_token`, `refresh_token`, `expires_at`, `scopes` | Helix 呼び出し用の資格情報。参照: `crates/storage/src/lib.rs`, `crates/app/src/oauth.rs` |
| `HelixBackfillCheckpoint` | `broadcaster_id`, `status`, `last_seen_cursor`, `last_seen_occurred_at` | バックフィルの進捗管理。参照: `crates/storage/src/lib.rs`, `crates/app/src/backfill.rs` |

## 3. ディレクトリ間の依存関係

```
app ──▶ core ──▶ util
 │       │
 │       └──▶ twitch
 └──▶ storage ──▶ util
```

- `app` は `core`, `storage`, `twitch`, `util` に依存します。
  - **確認ファイル**: `Cargo.toml`, `crates/app/Cargo.toml`, `crates/app/src/router.rs`
- `core` は独自のロジックを提供し、`storage` や `twitch` には依存しません。
  - **確認ファイル**: `crates/core/Cargo.toml`, `crates/core/src/lib.rs`
- `web/` は `twi_overlay_core::types` と REST/SSE の契約に依存します。共有型は `web/shared` にまとめられています。
  - **確認ファイル**: `web/shared/src/types.ts`, `web/shared/src/state.ts`, `.docs/04-api-contracts.md`

## 4. 参考ドキュメント

| 資料 | 内容 |
| --- | --- |
| `.docs/02-architecture-overview.md` | システム全体のアーキテクチャとデータフロー。 | 参照: `memo/03_data_flow.md` と対応 |
| `.docs/03-domain-model.md` | ドメイン用語と不変条件。 | 参照: `crates/core/src/policy.rs`, `crates/core/src/types.rs` |
| `.docs/04-api-contracts.md` | REST / SSE / OAuth API の契約。 | 参照: `crates/app/src/router.rs`, `crates/app/src/state.rs`, `crates/app/src/sse.rs` |
| `.docs/05-data-schema-and-migrations.md` | データベーススキーマと TTL ポリシー。 | 参照: `crates/storage/src/lib.rs`, `migrations/` |
| `.docs/07-debug-telemetry.md` | メトリクス・ログ・Tap の詳細。 | 参照: `crates/app/src/telemetry.rs`, `crates/app/src/tap.rs` |
| `.docs/10-operations-runbook.md` | 運用・監視・デプロイ手順。 | 参照: `memo/05_configuration_and_usage.md`, `scripts/` |

## 5. 推奨開発フロー

1. `.docs/08-implementation-plan.md` でタスクを確認。
   - **確認ファイル**: `.docs/08-implementation-plan.md`
2. 該当する仕様ファイルを更新。
   - **確認ファイル**: `.docs/` 配下の各仕様書
3. 実装を行い、`cargo fmt`, `cargo clippy`, `cargo test`, `npm run test` を実行。
   - **確認ファイル**: `AGENTS.md`, `.github/workflows/`, `Cargo.toml`, `package.json` 類
4. `Tap` や `/_debug/helix` で挙動を確認。
   - **確認ファイル**: `crates/app/src/tap.rs`, `crates/app/src/backfill.rs`
5. `Summary.md` に沿ってドキュメントを更新し、PR テンプレートの要件（目的・範囲・手動検証など）を満たす。
   - **確認ファイル**: `Summary.md`, `.github/pull_request_template.md`（存在する場合）

## 6. 役立つコマンドスニペット

```bash
# EventSub Webhook の HMAC 署名を確認
# 参照コード: `crates/app/src/webhook.rs`, `.docs/04-api-contracts.md`
printf "%s%s%s" "$MESSAGE_ID" "$TIMESTAMP" "$BODY" | \
  openssl dgst -sha256 -hmac "$WEBHOOK_SECRET"

# SQLite の中身を確認
# 参照コード: `crates/storage/src/lib.rs`, `migrations/`
sqlite3 dev.db 'SELECT id, status FROM queue_entries WHERE status != "COMPLETED";'

# SSE トークンを生成（OpenSSL 例）
# 参照コード: `crates/app/src/sse.rs`, `crates/util/src/config.rs`
node scripts/generate_token.js  # 独自スクリプトを用意すると便利
```

## 7. 追加リソース

- Twitch EventSub ドキュメント: <https://dev.twitch.tv/docs/eventsub>
- Twitch Helix API: <https://dev.twitch.tv/docs/api/reference>
- Axum フレームワーク: <https://docs.rs/axum>
- metrics クレート: <https://docs.rs/metrics>

この付録を利用することで、主要な概念や参照先を素早く確認しながら開発・運用を進められます。
- **確認ファイル**: 本章で列挙した `crates/*`, `web/*`, `.docs/*`, `scripts/*`
