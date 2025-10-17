# トラブルシューティング & FAQ

本章では、運用時に遭遇しやすいエラーや挙動を原因ごとに整理し、対応手順と確認コマンドをまとめます。各項目は `.docs/07-debug-telemetry.md` の観測ポイントと合わせて参照してください。
- **確認ファイル**: `.docs/07-debug-telemetry.md`, `crates/app/src/webhook.rs`, `crates/app/src/tap.rs`, `crates/app/src/telemetry.rs`

## 1. Webhook が 403 / 400 を返す

| 症状 | 考えられる原因 | 確認方法 / 対応 |
| --- | --- | --- |
| `403 invalid_signature` | `WEBHOOK_SECRET` が Twitch 側と一致していない、またはヘッダ `Twitch-Eventsub-Message-Signature` が欠落。 | `crates/app/src/webhook.rs` の `verify_signature` で検証。`Tap` で `stage=ingress` のイベントを確認し、`error` フィールドを参照。 |
| `400 timestamp_out_of_range` | リクエストヘッダのタイムスタンプが ±10 分外。ローカル時計がずれている。 | `state.now()` と `parse_timestamp` の差分をログ出力。サーバの NTP 設定を確認。 |
| `400 invalid_json` | Twitch からの JSON が壊れている、または ngrok 等で書き換えが発生。 | `Tap` の `ingress` ステージで `payload` を確認。 |

## 2. EventSub は届くがキューに積まれない

1. `Tap` で `stage=policy` を確認し、`payload` の `error` が `settings_error` や `duplicate` になっていないか調べる。
2. `policy.rs` の `PolicySettings::is_reward_enabled` で対象リワードが有効か確認。設定に `target_rewards` が空の場合、全てのリワードが対象です（関連コード: `crates/core/src/policy.rs`, `crates/core/src/types.rs`）。
3. `QueueRepository::insert_entry` で `DuplicateRedemption` が返っている場合、既存エントリと `redemption_id` が重複しています。`command_log` に同じ `redemption_id` が記録されていないか確認（関連コード: `crates/storage/src/lib.rs` の `QueueRepository`, `CommandLogRepository`）。
4. `DailyCounterRepository::upsert_count` の結果によってアンチスパム制限がかかっている可能性。`daily_counters` テーブルの値を確認し、`PolicySettings::anti_spam_window_sec` を調整します（関連コード: `crates/storage/src/lib.rs`, `crates/core/src/policy.rs`）。

## 3. SSE が切断される / イベントが欠落する

- `/metrics` の `sse_active_connections` と `sse_broadcast_failures_total` を確認し、異常値がないかチェック（関連コード: `crates/app/src/telemetry.rs`, `crates/app/src/sse.rs`）。
- クライアント側で `VersionMismatchError` が発生した場合、`Last-Event-ID` をヘッダに設定して再接続すると `SseHub::subscribe` が `Subscription::backlog` を使って差分を再送します。バッファを取りこぼしていた場合は `AppState::sse().build_state_replace` が全体スナップショットを流し直します（関連コード: `crates/app/src/sse.rs`, `crates/app/src/router.rs`, `web/shared/src/state.ts`）。
- リバースプロキシを使用している場合、`proxy_buffering off` と `http2` 無効化（HTTP/1.1 を利用）が設定されているか確認。`.docs/10-operations-runbook.md` を参照。
  - **確認資料**: `.docs/10-operations-runbook.md`, `deploy/nginx/`（存在する場合）
- 管理 UI 用の SSE には `types` フィルタを指定しているか確認。`settings.updated` を受け取れない場合、クエリパラメータを省略してください（関連コード: `crates/app/src/sse.rs`, `web/admin/src/api.ts`）。

## 4. Helix API エラー

| エラーコード | 説明 | 対応 |
| --- | --- | --- |
| `oauth:not-linked` | `oauth_links` にブロードキャスタのリンクが存在しない。 | `GET /oauth/login` から再リンク。`BackfillService::run_single` でも同様のコードが返ります（関連コード: `crates/app/src/oauth.rs`, `crates/app/src/backfill.rs`, `crates/storage/src/lib.rs`）。 |
| `oauth:missing-scope` | `REQUIRED_OAUTH_SCOPES` を満たしていない。 | Twitch アプリケーションで必要スコープを追加し、ユーザに再認可してもらう（関連コード: `crates/app/src/command.rs`, `.docs/04-api-contracts.md`）。 |
| `twitch:rate-limited` | Helix API のレートリミットに達した。 | リトライ間隔を延ばすか、`HELIX_BACKFILL_PAGE_SIZE` を減らす。`backfill.rs` のログで詳細確認（関連コード: `crates/app/src/backfill.rs`, `crates/util/src/config.rs`）。 |
| `twitch:not-found` | 対象の redemption がすでに削除済み。 | `QueueRepository::find_entry_by_redemption_for_update` の結果を確認し、エラーを握りつぶしてよいか判断（関連コード: `crates/storage/src/lib.rs`, `crates/app/src/command.rs`）。 |

## 5. OAuth 認可で失敗する

1. `/oauth/login` で生成される URL に正しい `client_id` と `redirect_uri` が含まれているか確認。`AppConfig` を再確認（関連コード: `crates/app/src/oauth.rs`, `crates/util/src/config.rs`）。
2. `/oauth/callback` で `state` 検証に失敗する場合、`oauth_login_states` の TTL（`OAUTH_STATE_TTL_SECS`）を超えていないか調べる（関連コード: `crates/app/src/oauth.rs`, `crates/storage/src/lib.rs` の `OauthLoginStateRepository`）。
3. `TwitchOAuthClient::exchange_code` が `invalid_grant` を返す場合、リダイレクト URI が Twitch Developer Console と一致していない可能性（関連コード: `crates/twitch/src/oauth.rs`）。
4. `POST /oauth2/validate` のレスポンスで `valid: false` の場合、アクセストークンが失効しているので `refresh_token` を使用するか再認可する（関連コード: `crates/app/src/oauth.rs`, `crates/twitch/src/oauth.rs`）。

## 6. データが古い / 反映が遅い

- `MaintenanceWorker` が停止していると、`command_log` と `event_raw` に古いデータが溜まり、クエリが重くなる。`tokio::task::JoinHandle` のログを確認（関連コード: `crates/app/src/maintenance.rs`, `crates/storage/src/lib.rs`）。
- `helix_backfill` テーブルの `status` が `Error` のままになっている場合、`/_debug/helix` で詳細を確認し、必要に応じて `BackfillService::trigger` を呼び出す（関連コード: `crates/app/src/backfill.rs`, `crates/storage/src/lib.rs`）。
- `state_index` が更新されていないと `/api/state?scope=since` のレスポンスが空になります。`CommandExecutor` が `CommandLog` を記録できているかチェック（関連コード: `crates/app/src/state.rs`, `crates/app/src/command.rs`, `crates/storage/src/lib.rs`）。

## 7. よくある質問 (FAQ)

### Q. 初期同期と SSE の整合性はどうやって保証されていますか？

`/api/state` は `StateSnapshot.version` を返し、SSE は `CommandLog.version` を `id` に設定します。クライアントはスナップショットのバージョンを保持し、次のパッチで `version` が +1 であることを `applyPatch` の内部で検証します。飛びが発生すると `VersionMismatchError` を投げ、再同期を促します。
- **確認ファイル**: `crates/app/src/state.rs`, `crates/app/src/sse.rs`, `crates/core/src/types.rs`, `web/shared/src/state.ts`

### Q. 複数の配信者（テナント）を同じサーバで扱う場合の注意点は？

`AppState` は多テナントを想定し、各 API で `broadcaster` クエリを必須にしています。SSE トークンにも `broadcaster` が埋め込まれており、別テナントへのアクセスは `SseTokenValidator` で拒否されます。ストレージも各テーブルで `broadcaster_id` を主キーに含めており、クエリに必ず条件が付きます。
- **確認ファイル**: `crates/app/src/router.rs`, `crates/app/src/sse.rs`, `crates/storage/src/lib.rs`

### Q. ログやメトリクスから原因を追跡するには？

`tracing` で出力されるログには `stage`, `op_id`, `version`, `broadcaster` が含まれます（`telemetry.rs` 参照）。`Tap` を組み合わせると EventSub → Policy → Command → SSE の各段階を時系列で確認できます。Prometheus ではレイテンシ (`webhook_ack_latency_seconds`) やコマンド件数 (`policy_commands_total`) を監視できます。
- **確認ファイル**: `crates/app/src/telemetry.rs`, `crates/app/src/tap.rs`, `.docs/07-debug-telemetry.md`

### Q. 本番でデバッグ機能を無効化するには？

`TapHub::spawn_mock_publisher` は `APP_ENV=development` でのみ有効化されます。`/_debug` 系エンドポイントは本番で認証をかけるか、リバースプロキシで制限を設けてください。`.docs/11-security-and-privacy.md` の方針に従い、PII を含むログはマスクされています。
- **確認ファイル**: `crates/app/src/tap.rs`, `crates/app/src/main.rs`, `.docs/11-security-and-privacy.md`

## 8. 参考ログパターン

| ログメッセージ | 意味 | 対応 |
| --- | --- | --- |
| `duplicate webhook message skipped` | `EventRawRepository::insert` が既存 `msg_id` を検出した。 | リトライ時の重複なので問題なし。大量に出る場合は Twitch 側の再送設定を確認（関連コード: `crates/storage/src/lib.rs`, `crates/app/src/webhook.rs`）。 |
| `failed to execute commands` | `CommandExecutor::execute` がエラー。 | `command.rs` のエラーハンドリングでログされる。SQL エラーか Helix エラーかメッセージで判断（関連コード: `crates/app/src/command.rs`, `crates/twitch/src/helix.rs`）。 |
| `failed to broadcast patch` | SSE 送信が失敗した。 | クライアント切断やネットワーク障害。必要に応じてリングバッファのサイズ (`SSE_RING_MAX`) を増やす（関連コード: `crates/app/src/sse.rs`, `crates/util/src/config.rs`）。 |

## 9. 連絡先・サポート情報

- `.docs/00-index.md` にメンテナンスフローと問い合わせ先が記載されています（関連資料: `.docs/00-index.md`）。
- CI 失敗時は GitHub Actions のログと `metrics` を照合して原因を切り分けてください（関連資料: `.github/workflows/`, `crates/app/src/telemetry.rs`）。

これらの手順を踏むことで、一般的な障害や疑問に迅速に対処できます。
- **確認ファイル**: 本章で参照した `crates/app/src/*.rs`, `crates/core/src/*.rs`, `crates/storage/src/lib.rs`, `web/*`, `.docs/*`
