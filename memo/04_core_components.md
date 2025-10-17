# 主要コンポーネント詳細

この章では、バックエンド・ドメイン・フロントエンドそれぞれの主要コンポーネントをコードレベルで掘り下げます。関数や構造体、インターフェースの役割を理解し、どこを拡張・改修すべきか判断できるようにします。

## 1. バックエンド（`crates/app`）

### 1.1 エントリーポイントと状態管理

- `main.rs`: アプリケーション起動処理。`AppConfig::from_env` で環境変数から設定を読み込み、トレース・メトリクス・データベース・背景ワーカー・Axum サーバを初期化します。
- `AppState`（`router.rs`）: Axum の `State` として共有される依存をまとめた構造体。SSE ハブ、Tap ハブ、Helix クライアントなどすべてのハンドラが必要とするオブジェクトを格納します。

```rust
// crates/app/src/router.rs
#[derive(Clone)]
pub struct AppState {
    metrics: PrometheusHandle,
    tap: TapHub,
    storage: Database,
    webhook_secret: Arc<[u8]>,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    policy_engine: Arc<PolicyEngine>,
    command_executor: CommandExecutor,
    sse: SseHub,
    token_validator: SseTokenValidator,
    sse_heartbeat_secs: u64,
    oauth_client: TwitchOAuthClient,
    oauth_redirect_uri: String,
    oauth_state_ttl: Duration,
    backfill: backfill::BackfillService,
}
```

`AppState::new` は Helix クライアントや SSE ハブを組み立て、`BackfillService` とそのワーカーを生成します。
- **確認ファイル**: `crates/app/src/router.rs`, `crates/app/src/backfill.rs`, `crates/app/src/sse.rs`

### 1.2 HTTP ルーティング

`router.rs` の `app_router` は Axum の `Router` を構築し、`/eventsub/webhook` や `/overlay/sse` などのハンドラを登録します。ヘルスチェック、Prometheus メトリクス、デバッグ Tap などもここにまとめられています。
- **確認ファイル**: `crates/app/src/router.rs`, `crates/app/src/webhook.rs`, `crates/app/src/sse.rs`, `crates/app/src/tap.rs`, `crates/app/src/telemetry.rs`

### 1.3 Webhook 処理 (`webhook.rs`)

- **署名検証**: `verify_signature` は `hmac` クレートを使い、`sha256=` 付き署名を検証します。失敗すると `403 Forbidden`。
- **EventRaw 永続化**: `EventRawRepository::insert` で原本を保存し、`duplicate` フラグで冪等性を確保します。
- **正規化→ポリシー→コマンド**: `process_pipeline` が正規化 (`Normalizer`)、設定読み込み、ポリシー評価、コマンド実行、SSE 配信を順番に呼び出します。
- **Tap ステージ**: `emit_normalizer_stage`、`emit_policy_stage`、`emit_sse_stage` などが `_debug/tap` 用イベントを構築します。
- **確認ファイル**: `crates/app/src/webhook.rs`, `crates/app/src/tap.rs`, `crates/core/src/normalizer.rs`, `crates/core/src/policy.rs`

```rust
// crates/app/src/webhook.rs
async fn dispatch_commands(
    state: &AppState,
    broadcaster_id: &str,
    profile: &BroadcasterSettings,
    commands: &[Command],
    normalized: &NormalizedEvent,
) {
    match state
        .command_executor()
        .execute(broadcaster_id, &profile.timezone, commands)
        .await
    {
        Ok(patches) => {
            for patch in patches {
                if let Err(err) = state
                    .sse()
                    .broadcast_patch(broadcaster_id, &patch, state.now())
                    .await
                {
                    error!(stage = "sse", broadcaster_id, error = %err, "failed to broadcast patch");
                    continue;
                }
                emit_sse_stage(state, broadcaster_id, &patch);
            }
        }
        Err(err) => {
            error!(stage = "command", broadcaster_id, error = %err, "failed to execute commands");
        }
    }
}
```

### 1.4 コマンド実行 (`command.rs`)

`CommandExecutor` は永続化とドメイン状態更新を一手に担います。

- `execute`: コマンドのバッチを処理し、トランザクション内で `CommandLog`・`queue_entries`・`daily_counters`・`settings` などを更新します。
- `handle_enqueue`: `QueueRepository::insert_entry` と `DailyCounterRepository::upsert_count` を呼び出してキューとカウンタを更新。
- `handle_queue_complete` / `handle_queue_remove`: ステータス更新と理由付与を行い、Helix 側の redemption も必要に応じて更新します。
- `handle_settings_update`: JSON パッチを適用し、適用結果を `CommandApplyResult::SettingsUpdated` で返します。

コマンド適用後は `Projector` により `Patch` が生成され、SSE へ渡されます。
- **確認ファイル**: `crates/app/src/command.rs`, `crates/core/src/projector.rs`, `crates/app/src/sse.rs`

### 1.5 SSE ハブ (`sse.rs`)

- `SseHub::new`: リングバッファの容量・TTL を受け取り、ブロードキャスタごとのバージョン管理を行います。
- `broadcast_patch`: `CommandLog` の `version` を SSE `id` として使用し、接続中のクライアントにイベントを送信します。リングバッファへ保存することで再接続時の再送が可能になります。
- `SseTokenValidator`: HMAC ベースのトークン署名を検証し、オーディエンスと有効期限を確認します。
- **確認ファイル**: `crates/app/src/sse.rs`, `crates/app/src/router.rs`, `crates/util/src/config.rs`

### 1.6 Tap (`tap.rs`)

Tap は内部処理の可視化用 SSE です。`tap_stream` が `TapHub` から `StageEvent` を受け取り、JSON を SSE 形式で配信します。`tap_keep_alive` は 20 秒間隔の `heartbeat` コメントを構成し、`.docs/07-debug-telemetry.md` の要件と一致します。
- **確認ファイル**: `crates/app/src/tap.rs`, `.docs/07-debug-telemetry.md`

### 1.7 メンテナンス (`maintenance.rs`)

`MaintenanceWorker` は以下のタスクを `tokio::interval` で実行します。

- `event_raw.delete_older_than` と `command_log.delete_older_than` による TTL 削除。
- `Database::wal_checkpoint_truncate` による WAL ファイルの縮小。
- `oauth_links.prune_revoked` などのクリーンアップ。
- **確認ファイル**: `crates/app/src/maintenance.rs`, `crates/storage/src/lib.rs`, `.docs/05-data-schema-and-migrations.md`

### 1.8 OAuth (`oauth.rs`)

- `login`: Twitch の認可エンドポイントへリダイレクトする URL を生成し、`oauth_login_states.insert` で state を保存します。
- `callback`: 認可コードをトークンへ交換し、`oauth_links.upsert` で保存します。
- `validate`: 管理 UI から送られたトークンを検証し、必要なスコープがそろっているかをチェックします。
- **確認ファイル**: `crates/app/src/oauth.rs`, `crates/twitch/src/oauth.rs`, `crates/storage/src/lib.rs`

### 1.9 Helix バックフィル (`backfill.rs`)

`BackfillService` は `tokio::mpsc` でワーカーに命令を送り、周期的または手動で Helix API を呼び出して過去のリワードを補完します。`process_link` 内で `HelixClient::list_redemptions` を反復し、取得したレコードを `CommandExecutor` へ渡します。
- **確認ファイル**: `crates/app/src/backfill.rs`, `crates/twitch/src/helix.rs`, `crates/app/src/command.rs`

## 2. ドメイン層（`crates/core`）

### 2.1 型定義 (`types.rs`)

- `Settings`, `PolicySettings`: オーバーレイのテーマやアンチスパム設定を含む。
- `QueueEntry`: キューの行。ユーザ情報、リワード ID、ステータス、管理フラグなどを保持します。
- `NormalizedEvent`: EventSub をドメインイベントに変換した列挙体。`RedemptionAdd`, `RedemptionUpdate`, `StreamOnline`, `StreamOffline` をサポート。
- `Patch`: クライアントへ送る差分（`queue.enqueued`, `queue.completed`, `counter.updated`, `settings.updated`, `redemption.updated`, `state.replace` など）。
- **確認ファイル**: `crates/core/src/types.rs`, `web/shared/src/types.ts`

### 2.2 正規化 (`normalizer.rs`)

`Normalizer::normalize` は EventSub の `subscription.type` と `event` 内容を解析し、`NormalizedEvent` を生成します。型安全な構造体を返すことで後続処理が JSON に依存しないようになっています。例えば `channel.channel_points_custom_reward_redemption.add` は以下のように処理されます。
- **確認ファイル**: `crates/core/src/normalizer.rs`, `crates/app/src/webhook.rs`, `.docs/04-api-contracts.md`

```rust
// crates/core/src/normalizer.rs
match event_type {
    "channel.channel_points_custom_reward_redemption.add" => {
        let event = payload.event.ok_or(NormalizerError::MissingField("event"))?;
        Ok(NormalizedEvent::RedemptionAdd {
            broadcaster_id: event.broadcaster_user_id,
            occurred_at: parse_timestamp(event.redeemed_at)?,
            redemption_id: event.id,
            user: NormalizedUser::from(event.user),
            reward: NormalizedReward::from(event.reward),
        })
    }
    // ... ほかのイベント型
}
```

### 2.3 ポリシー (`policy.rs`)

`PolicyEngine::evaluate` は `PolicyContext` を受け取り、以下を行います。
- **確認ファイル**: `crates/core/src/policy.rs`, `crates/app/src/webhook.rs`, `.docs/03-domain-model.md`

- **アンチスパム**: `PolicySettings::anti_spam_window_sec` 内に同じユーザ・リワードのリクエストがある場合に `DuplicatePolicy` に応じて `Command::RedemptionUpdate`（返金）や `Command::QueueRemove` を生成。
- **ターゲットリワード**: `PolicySettings::is_reward_enabled` により対象リワードのみ処理します。
- **配信開始/終了**: `StreamOnline` で `Command::QueueRemove`（全削除）やカウンタリセットを生成、`StreamOffline` で追加操作を定義。

### 2.4 プロジェクタ (`projector.rs`)

`Projector` は `CommandResult` から `Patch` を生成します。
- **確認ファイル**: `crates/core/src/projector.rs`, `crates/core/src/types.rs`, `web/shared/src/state.ts`

```rust
// crates/core/src/projector.rs
pub fn project(result: &CommandResult, now: DateTime<Utc>) -> Vec<Patch> {
    match result {
        CommandResult::Enqueued { entry, user_today_count } => vec![Patch::queue_enqueued(now, entry.clone(), *user_today_count)],
        CommandResult::QueueRemoved { entry_id, reason } => vec![Patch::queue_removed(now, entry_id.clone(), *reason)],
        CommandResult::CounterUpdated { user_id, count } => vec![Patch::counter_updated(now, user_id.clone(), *count)],
        CommandResult::SettingsUpdated { patch } => vec![Patch::settings_updated(now, patch.clone())],
        CommandResult::RedemptionUpdated { redemption_id, managed } => vec![Patch::redemption_updated(now, redemption_id.clone(), *managed)],
    }
}
```

`Patch` は `version` と `at` タイムスタンプを持ち、クライアント側のバージョン一致を強制します。
- **確認ファイル**: `crates/core/src/types.rs`, `web/shared/src/state.ts`

## 3. 永続層（`crates/storage`）

### 3.1 `Database`

`Database::connect` は sqlx の `SqlitePool` を初期化し、`PRAGMA foreign_keys`, `journal_mode=WAL`, `busy_timeout` などを設定します。`run_migrations` は `../../migrations` ディレクトリを指し、`sqlx::migrate!` マクロでマイグレーションを実行します。
- **確認ファイル**: `crates/storage/src/lib.rs`, `migrations/`

### 3.2 `EventRawRepository`

EventSub の原本を保持します。
- **確認ファイル**: `crates/storage/src/lib.rs` の `EventRawRepository`, `.docs/05-data-schema-and-migrations.md`

- `insert`: `msg_id` のユニーク制約で冪等性を確保。
- `delete_older_than`: TTL を超えたデータを削除。

### 3.3 `CommandLogRepository`

- `begin`: トランザクションを開始。
- `insert_entry`: `version` をオートインクリメントしつつログを追記。
- `delete_older_than`: TTL 削除。
- `list_since_version`: SSE 再送のための差分取得。
- **確認ファイル**: `crates/storage/src/lib.rs` の `CommandLogRepository`, `.docs/05-data-schema-and-migrations.md`

### 3.4 `QueueRepository`

- `insert_entry`: キューを追加し、重複 `redemption_id` を検知。
- `find_entry_for_update` / `find_entry_by_redemption_for_update`: トランザクション内でレコードをロック。
- `update_status`: 完了・削除時にステータス・理由・`last_updated_at` を更新。
- `list_active_with_counts`: 日次カウンタを JOIN して並び替え済みのリストを返却。
- **確認ファイル**: `crates/storage/src/lib.rs` の `QueueRepository`, `.docs/05-data-schema-and-migrations.md`

### 3.5 `DailyCounterRepository`

- `upsert_count`: ユーザごとの日次使用回数を更新。
- `list_today`: フロントエンド表示用に現在日のカウンタ一覧を取得。
- **確認ファイル**: `crates/storage/src/lib.rs` の `DailyCounterRepository`, `crates/app/src/state.rs`

### 3.6 OAuth / Helix 関連

- `OauthLoginStateRepository`: `state` 値の保存と TTL 管理。
- `OauthLinkRepository`: アクセストークン、リフレッシュトークン、期限、スコープを保存。`list_active` は期限切れを除外。
- `HelixBackfillRepository`: バックフィルのチェックポイント管理。
- **確認ファイル**: `crates/storage/src/lib.rs`, `migrations/202309*`（OAuth テーブル定義）, `.docs/05-data-schema-and-migrations.md`

## 4. Twitch API クライアント（`crates/twitch`）

### 4.1 `HelixClient`

- `list_redemptions`: `/channel_points/custom_rewards/redemptions` を呼び出し、`HelixRedemption` の配列とページネーション情報を返します。
- `update_redemption_status`: Redemption を `FULFILLED` や `CANCELED` へ更新。
- `validate_token`: `/oauth2/validate` を叩き、アクセストークンのスコープと期限を取得。
- **確認ファイル**: `crates/twitch/src/helix.rs`, `.docs/04-api-contracts.md`

### 4.2 `TwitchOAuthClient`

- `authorize_url`: ユーザに提示する認可 URL を生成。
- `exchange_code`: 認可コードと引き換えにアクセストークンを取得。
- `refresh_token`: リフレッシュトークンを新しいアクセストークンへ更新。
- **確認ファイル**: `crates/twitch/src/oauth.rs`, `.docs/04-api-contracts.md`

## 5. ユーティリティ（`crates/util`）

`AppConfig` は開発・本番環境で安全に起動するためのバリデーションを行います。存在しない変数や不正な数値があれば `ConfigError` を返します。`load_env_file` は `.env` を読み込んでローカル開発を容易にします。
- **確認ファイル**: `crates/util/src/config.rs`, `crates/util/src/lib.rs`

## 6. フロントエンド

### 6.1 共有ライブラリ (`web/shared`)

- `state.ts`: `createClientState` と `applyPatch` がクライアント状態管理の中心。`VersionMismatchError` でバージョン飛びを検出します。
- `types.ts`: Rust 側の `twi_overlay_core::types` と構造的に一致する型定義。`Patch` の `type` 文字列は SSE `event` 名に対応。
- **確認ファイル**: `web/shared/src/state.ts`, `web/shared/src/types.ts`, `web/shared/src/state.test.ts`

### 6.2 オーバーレイ (`web/overlay`)

- `api.ts`: REST と SSE のクライアント。`createSseConnection` は `EventSource` を生成し、`types` フィルタや `since_version` をクエリへ付与します。
- `App.tsx`: React コンポーネント。SSE からの `message` をパースして `applyPatch` を呼び、状態を更新します。キューとカウンタをコンポーネント化して描画します。
- **確認ファイル**: `web/overlay/src/api.ts`, `web/overlay/src/api.test.ts`, `web/overlay/src/App.tsx`, `web/overlay/src/main.tsx`

### 6.3 管理 UI (`web/admin`)

- `api.ts`: 管理操作（キュー消化、設定更新）を REST で呼び出します。
- `config.ts`: SSE トークンやエンドポイント URL を管理。テストでエラーケースを確認しています。
- `settings.ts`: React Hook で設定フォームを制御し、`SettingsPatch` を生成します。
- **確認ファイル**: `web/admin/src/api.ts`, `web/admin/src/config.ts`, `web/admin/src/config.test.ts`, `web/admin/src/settings.ts`, `web/admin/src/settings.test.ts`, `web/admin/src/main.ts`

## 7. テスト

- Rust 側は各モジュール内にユニットテストを定義。`router.rs` や `command.rs` は `#[cfg(test)]` で Helix クライアントをモックできます（関連コード: `crates/app/src/router.rs`, `crates/app/src/command.rs`, `crates/util/src/lib.rs` の `test_support`）。
- フロントエンドは Vitest を利用。`web/shared/src/state.test.ts` では `applyPatch` の振る舞い、`web/overlay/src/api.test.ts` では API クライアントの例外処理を検証します（関連コード: `web/shared/src/state.test.ts`, `web/overlay/src/api.test.ts`, `web/admin/src/config.test.ts`, `web/admin/src/settings.test.ts`）。

この章で紹介したコンポーネントを理解しておくと、仕様変更や機能追加を行う際に影響範囲を適切に評価できます。次章では実際のセットアップと運用手順を解説します。
- **確認ファイル**: 本章で列挙した `crates/app`, `crates/core`, `crates/storage`, `crates/twitch`, `crates/util`, `web/*` の各ソースファイル
