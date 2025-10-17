# データと処理の流れ

この章では、EventSub の受信からフロントエンドへの差分配信、管理操作、バックグラウンド処理まで、アプリケーション全体のフローを時系列に追いながら詳解します。各ステップで参照するコードと、`.docs/02-architecture-overview.md`・`.docs/04-api-contracts.md` に記述された仕様との対応関係を明示します。

## 1. EventSub Webhook の受信

1. **HTTP 受信とヘッダ検証**: `POST /eventsub/webhook` は `crates/app/src/webhook.rs` の `handle` 関数で処理されます。必須ヘッダ（`Twitch-Eventsub-Message-Id` など）の取得と型変換に失敗した場合、`400 Bad Request` を返します。
2. **タイムスタンプ検証**: `parse_timestamp` でヘッダ値を `DateTime<Utc>` に変換し、`±10 分` の許容範囲を超えていないかを確認します（関連コード: `crates/app/src/webhook.rs` 内 `parse_timestamp`）。
3. **HMAC-SHA256 署名検証**: `verify_signature` が `webhook_secret`（`AppState::webhook_secret`）を使って `message_id || timestamp || body` の HMAC を再計算し、`sha256=` プレフィックス付きの署名と比較します（関連コード: `crates/app/src/webhook.rs` 内 `verify_signature`）。

```rust
// crates/app/src/webhook.rs
verify_signature(&secret, message_id, timestamp_raw, &body, signature).map_err(|err| {
    counter!("eventsub_invalid_signature_total", "type" => message_label).increment(1);
    ProblemResponse::new(StatusCode::FORBIDDEN, "invalid_signature", err)
})?;
```

4. **EventRaw の永続化と冪等性**: 正常な通知は `EventRawRepository::insert` で SQLite に保存されます。`EventRawInsertOutcome::Duplicate` の場合は既に処理済みなので即座に `204 No Content` を返し、以降のパイプラインをスキップします（関連コード: `crates/app/src/webhook.rs`, `crates/storage/src/lib.rs` の `EventRawRepository`）。

```rust
// crates/app/src/webhook.rs
let insert_outcome = repo.insert(record).await?;
let duplicate = matches!(insert_outcome, EventRawInsertOutcome::Duplicate);
if duplicate {
    info!(stage = "ingress", %message_id, broadcaster_id, "duplicate webhook message skipped");
}
```

5. **Tap への可視化イベント**: `TapHub::publish` を通じて `StageEvent` が `_debug/tap` にストリーミングされます。受信時刻やメッセージ種別、サイズなどが記録されます（関連コード: `crates/app/src/tap.rs` の `tap_stream`, `crates/app/src/webhook.rs` の `emit_ingress_stage`）。

## 2. 正規化とポリシー評価

1. **正規化 (`Normalizer`)**: `process_pipeline` 内で `Normalizer::normalize(event_type, json_value)` を呼び出し、EventSub の JSON を `NormalizedEvent` 列挙体へ変換します。エラーが発生した場合は `_debug/tap` にエラーが公開され、処理は中断されます（関連コード: `crates/app/src/webhook.rs` の `process_pipeline`, `crates/core/src/normalizer.rs`）。

```rust
// crates/app/src/webhook.rs
let normalized = match Normalizer::normalize(event_type, json_value) {
    Ok(event) => event,
    Err(err) => {
        emit_normalizer_error(state, context);
        return Err(());
    }
};
```

2. **設定と現在状態の取得**: `state.storage().broadcasters().fetch_settings` が設定 (`Settings`) とタイムゾーンを含む `BroadcasterSettings` を読み込みます。存在しない場合は `StageKind::Policy` の Tap イベントにエラーが記録されます（関連コード: `crates/app/src/webhook.rs` の `load_broadcaster_profile`, `crates/storage/src/lib.rs` の `BroadcasterRepository`）。
3. **ポリシー評価 (`PolicyEngine`)**: `evaluate_policy` は `PolicyEngine::evaluate` を呼び出し、アンチスパム判定や重複処理、ストリームオンライン時のリセットなどを行って `Command` のベクターを返します（関連コード: `crates/app/src/webhook.rs` の `evaluate_policy`, `crates/core/src/policy.rs`）。

```rust
// crates/app/src/webhook.rs
let outcome = evaluate_policy(state, broadcaster_id, &normalized, &profile.settings);
if !outcome.commands.is_empty() {
    dispatch_commands(state, broadcaster_id, &profile, &outcome.commands, &normalized).await;
}
```

`PolicyOutcome` は `.docs/03-domain-model.md` に記されたユースケース（重複検出時の返金、ターゲットリワード制御など）を満たすよう設計されています。
- **確認ファイル**: `.docs/03-domain-model.md`, `crates/core/src/policy.rs`, `crates/app/src/webhook.rs`

## 3. コマンド実行と永続化

1. **トランザクションの開始**: `CommandExecutor::execute` は `CommandLogRepository::begin` でトランザクションを開き、`Command` を順番に適用します（関連コード: `crates/app/src/command.rs`, `crates/storage/src/lib.rs` の `CommandLogRepository`）。
2. **キュー操作**: 例えば `Command::Enqueue` は `QueueRepository::insert_entry` で新規行を追加し、`DailyCounterRepository::upsert_count` で日次カウンタを更新します。重複した `redemption_id` があると `QueueError::DuplicateRedemption` を返し、ポリシー側で再評価されます（関連コード: `crates/app/src/command.rs` の `handle_enqueue`, `crates/storage/src/lib.rs` の `QueueRepository`, `DailyCounterRepository`）。

```rust
// crates/app/src/command.rs
Command::Enqueue(enqueue) => {
    self.handle_enqueue(
        tx,
        broadcaster_id,
        timezone,
        enqueue,
        queue_repo,
        counter_repo,
    )
    .await
}
```

3. **管理操作の冪等性**: `QueueCompleteCommand` や `QueueRemoveCommand` は `command.rs` 内で `op_id` を検証し、`CommandLog` から過去の適用履歴を確認して重複適用を防ぎます（関連コード: `crates/app/src/command.rs` の `ensure_unique_operation`, `crates/storage/src/lib.rs` の `CommandLogRepository`）。
4. **設定更新**: `Command::SettingsUpdate` は `BroadcasterRepository::apply_settings_patch` を通じて JSON Patch 形式の部分更新を行います。適用結果は `SettingsUpdateResultBody` に `applied: bool` で反映されます（関連コード: `crates/app/src/command.rs` の `handle_settings_update`, `crates/storage/src/lib.rs` の `BroadcasterRepository`）。
5. **Helix 連携**: `Command::RedemptionUpdate` は `HelixClient::update_redemption_status` を呼び出し、Twitch 側のリワード状態を変更します。必要スコープやトークンの有効期限は `has_required_scopes` などで検証され、失敗した場合は Tap に `oauth` ステージのエラーが出力されます（関連コード: `crates/app/src/command.rs` の `handle_redemption_update`, `crates/twitch/src/helix.rs`, `crates/app/src/tap.rs`）。

## 4. コマンドログと SSE への変換

1. **コマンドログ**: 各コマンドは `NewCommandLog` として `command_log` テーブルに保存され、`version` が単調増加します。SSE の `Last-Event-ID` と同期させるための基準です（関連コード: `crates/app/src/command.rs`, `crates/storage/src/lib.rs` の `CommandLogRepository`）。
2. **プロジェクタ (`Projector`)**: コマンドの適用結果を `Projector::new` が `Patch` に変換します。例として、`QueueCompleteCommand` は `queue.completed` パッチを生成し、削除対象エントリ ID を含みます（関連コード: `crates/core/src/projector.rs`, `crates/core/src/types.rs`）。
3. **SSE ブロードキャスト**: `SseHub::broadcast_patch` はキューリングバッファにパッチを保存し、接続中のクライアントに即座に送信します。送信に成功すると `emit_sse_stage` が Tap に記録します（関連コード: `crates/app/src/sse.rs`, `crates/app/src/tap.rs`）。

```rust
// crates/app/src/command.rs
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
```

## 5. フロントエンドとの同期

### 5.1 初期状態取得 `/api/state`

1. **トークン検証**: `state_snapshot` ハンドラは `SseTokenValidator::validate_any` でベアラートークンを検証し、オーディエンス（`Overlay` or `Admin`）とブロードキャスタ ID が一致するか確認します（関連コード: `crates/app/src/state.rs`, `crates/app/src/sse.rs` の `SseTokenValidator`）。
2. **スコープ制限**: `parse_state_scope` ヘルパーが `scope` クエリ（`session` or `since`）と `since` タイムスタンプを検証し、`StateScope` を構築します。`scope=since` の場合は RFC 3339 形式の時刻が必須で、バリデーションを通過すると `state_index` を参照して差分のみを返します（関連コード: `crates/app/src/router.rs` の `parse_state_scope`, `crates/app/src/state.rs`, `crates/storage/src/lib.rs` の `StateIndexRepository`）。
3. **レスポンス構築**: `build_state_snapshot` は `QueueRepository::list_active_with_counts`、`DailyCounterRepository::list_today`、`BroadcasterRepository::fetch_settings` からデータを集約し、`StateSnapshot` 構造体に詰めます（関連コード: `crates/app/src/state.rs`, `crates/storage/src/lib.rs`）。

```rust
// crates/app/src/state.rs
pub async fn build_state_snapshot(
    database: &Database,
    broadcaster: &str,
    scope: StateScope,
    now: DateTime<Utc>,
) -> Result<StateSnapshot, StateError> {
    let queue_repo = database.queue();
    let counters_repo = database.daily_counters();
    let broadcaster_repo = database.broadcasters();
    // ... queue, counters, settings を取得して StateSnapshot を組み立てる
}
```

### 5.2 SSE ストリーム `/overlay/sse`, `/admin/sse`

1. **クエリパラメータ**: `SseQuery` は `broadcaster`, `token`, `types`, `since_version` を受け取ります。`types` により `queue.*` や `settings.updated` など特定パッチのみ購読できます（関連コード: `crates/app/src/sse.rs`）。
2. **リングバッファ再送**: `since_version` または `Last-Event-ID` が指定されると、`SseHub::subscribe` がリングバッファをフィルタリングして該当バージョンより新しいメッセージだけを `Subscription::backlog` に積み直します。バッファが不足していた場合（`ring_miss`）は `AppState::sse().build_state_replace` が呼ばれ、最新スナップショットを SSE で再送します（関連コード: `crates/app/src/sse.rs`, `crates/app/src/router.rs`）。
3. **心拍**: Axum の `Sse::keep_alive` で `axum::response::sse::KeepAlive::new().interval(Duration::from_secs(state.sse_heartbeat()))` が設定され、`state.sse_heartbeat()` 秒ごとに `heartbeat` コメントが送信されます。`.docs/02` で定義された 20–30 秒心拍要件に対応しています（関連コード: `crates/app/src/router.rs`, `.docs/02-architecture-overview.md`）。
4. **クライアント適用**: フロントエンドは `web/shared/src/state.ts` の `applyPatch` で受信した `Patch` をクライアント状態に適用し、React コンポーネントは `useEffect` 内で再レンダリングします（関連コード: `web/shared/src/state.ts`, `web/overlay/src/App.tsx`, `web/admin/src/main.ts`）。

## 6. 管理操作のフロー

### 6.1 キュー消化 `/api/queue/dequeue`

1. **入力**: `QueueDequeueRequest` は `entry_id`, `mode` (`COMPLETE` or `UNDO`), `op_id` を受け取ります（関連コード: `crates/app/src/router.rs` の ルート定義, `crates/app/src/command.rs` の `QueueDequeueRequest`, `.docs/04-api-contracts.md`）。
2. **コマンド生成**: `CommandExecutor::execute_dequeue`（`queue_dequeue` ハンドラ内）が `Command::QueueComplete` または `Command::QueueRemove` を生成します（関連コード: `crates/app/src/command.rs` の `execute_dequeue`, `crates/core/src/types.rs` の `Command`）。
3. **レスポンス**: 結果のバージョンと、ユーザの日次カウンタ（`user_today_count`）が返されます。フロントはレスポンスに含まれるパッチと SSE を突き合わせて整合性を確認します（関連コード: `crates/app/src/command.rs` の `QueueDequeueResponse`, `web/admin/src/api.ts`）。

### 6.2 設定更新 `/api/settings/update`

1. **入力**: `SettingsUpdateRequest` は JSON パッチ（部分更新）と `op_id` を送ります（関連コード: `crates/app/src/router.rs` の `/api/settings/update` ルート, `crates/app/src/command.rs` の `SettingsUpdateRequest`, `.docs/04-api-contracts.md`）。
2. **適用**: `Command::SettingsUpdate` が生成され、`CommandExecutor::handle_settings_update` が `BroadcasterRepository::apply_settings_patch` を経由して永続化します（関連コード: `crates/app/src/command.rs` の `handle_settings_update`, `crates/storage/src/lib.rs` の `BroadcasterRepository`）。
3. **結果**: `applied: bool` により変更が適用されたかを示し、同一 `op_id` 再送時は `false` が返ります（関連コード: `crates/app/src/command.rs` の `SettingsUpdateResultBody`, `web/admin/src/settings.ts`）。

## 7. OAuth と Helix バックフィル

### 7.1 OAuth ログイン

- `GET /oauth/login`: `oauth.rs` が `TwitchOAuthClient::authorize_url` を生成し、CSRF 対策の `state` 値を `oauth_login_states` テーブルに保存します（関連コード: `crates/app/src/oauth.rs`, `crates/twitch/src/oauth.rs`, `crates/storage/src/lib.rs` の `OauthLoginStateRepository`）。
- `GET /oauth/callback`: 認可コードを `exchange_code` でトークンに交換し、`oauth_links` テーブルへ保存します（関連コード: `crates/app/src/oauth.rs`, `crates/twitch/src/oauth.rs`, `crates/storage/src/lib.rs` の `OauthLinkRepository`）。
- `POST /oauth2/validate`: 管理 UI から渡されたトークンが有効か検証し、必要スコープの不足をエラーコードで返却します（関連コード: `crates/app/src/oauth.rs`, `.docs/04-api-contracts.md`）。

### 7.2 Helix バックフィル

- `BackfillWorker` は `helix_backfill_interval_secs` ごとに起動し、`oauth_links.list_active` で有効リンクを列挙します（関連コード: `crates/app/src/backfill.rs`, `crates/storage/src/lib.rs` の `OauthLinkRepository`）。
- `HelixClient::list_redemptions` を呼び出し、未処理のリワードを `NormalizedEvent::RedemptionAdd` と同じコマンド経路で再生します（関連コード: `crates/app/src/backfill.rs`, `crates/twitch/src/helix.rs`, `crates/app/src/webhook.rs`）。
- チェックポイント (`helix_backfill` テーブル) に最後に処理した `redeemed_at` と `cursor` を保存し、再起動後も継続できます（関連コード: `crates/app/src/backfill.rs`, `crates/storage/src/lib.rs` の `HelixBackfillRepository`, `migrations/`）。

## 8. メンテナンスタスク

`maintenance.rs` の `MaintenanceWorker` が定期的に以下を実行します。

- `Database::wal_checkpoint_truncate`: WAL ファイルを縮小（関連コード: `crates/app/src/maintenance.rs`, `crates/storage/src/lib.rs` の `wal_checkpoint_truncate`）。
- `EventRawRepository::delete_older_than`: `.docs/05` の TTL（72 時間）を守るよう古いレコードを削除（関連コード: `crates/app/src/maintenance.rs`, `crates/storage/src/lib.rs` の `EventRawRepository`）。
- `CommandLogRepository::delete_older_than`: 同様に古い差分を削除し、ストレージサイズを保ちます（関連コード: `crates/app/src/maintenance.rs`, `crates/storage/src/lib.rs` の `CommandLogRepository`）。

## 9. テレメトリとデバッグ

- **メトリクス**: `/metrics` で Prometheus 形式を返します。`metrics_exporter_prometheus` の `PrometheusHandle` を `AppState` に保持し、`telemetry::render_metrics` で文字列化します（関連コード: `crates/app/src/router.rs`, `crates/app/src/telemetry.rs`）。
- **Tap**: `/_debug/tap?s=ingress,policy,command,...` で処理段ごとの JSON をリアルタイムに確認できます。`tap.rs` の `tap_stream` が `StageEvent` を SSE 形式に変換します（関連コード: `crates/app/src/tap.rs`）。
- **ヘルスチェック**: `/healthz` は単純に `200 OK` を返し、`/oauth2/validate` や `/_debug/helix` でより深い診断が可能です（関連コード: `crates/app/src/router.rs`, `crates/app/src/backfill.rs`, `crates/app/src/oauth.rs`）。

## 10. フロントエンドでのデータ適用

1. **初期ロード**: `web/overlay/src/api.ts` の `fetchState` が `/api/state` を呼び出し、`StateSnapshot` を取得します（関連コード: `web/overlay/src/api.ts`, `.docs/04-api-contracts.md`）。
2. **状態生成**: `createClientState` がスナップショットを `ClientState` へ変換し、`queue` をユーザの日次カウンタと `enqueued_at` の昇順で整列します（関連コード: `web/shared/src/state.ts` の `createClientState`）。
3. **差分適用**: `applyPatch` が SSE で流れてきた `Patch` を検証し、バージョンが飛んだ場合は `VersionMismatchError` を投げて再同期を促します（関連コード: `web/shared/src/state.ts` の `applyPatch`, `web/shared/src/types.ts`）。
4. **UI 更新**: React コンポーネントは `useState` で保持している状態を更新し、Tailwind 風クラスでキュー一覧やカウンタ、設定を描画します（関連コード: `web/overlay/src/App.tsx`, `web/overlay/src/main.tsx`, `web/overlay/src/App.css`）。

このように、EventSub の受信からクライアントの描画まで、一連の処理は `NormalizedEvent → Command → Patch` のパイプラインに沿って厳密に管理されています。`.docs/` に定義されたインバリアント（冪等性、SSE バージョン管理、多テナント境界など）は、ここで紹介したコード上のチェックやテーブル設計により担保されています。
- **確認ファイル**: `.docs/02-architecture-overview.md`, `.docs/03-domain-model.md`, `.docs/04-api-contracts.md`, `.docs/05-data-schema-and-migrations.md`, `crates/app/src/*.rs`, `crates/core/src/*.rs`, `crates/storage/src/lib.rs`, `web/*/src/*.ts*`
