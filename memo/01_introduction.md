# はじめに

## この資料の目的

このドキュメントセットは、`twitch_obs_02` リポジトリに含まれる Rust 製バックエンド、TypeScript 製フロントエンド、SQLite 永続層がどのように連携して Twitch のチャンネルポイント運用を自動化するかを、ソースコードの該当箇所を示しながら体系的に解説するものです。`.docs/` ディレクトリに定義された公式仕様と実装の結び付きを明示することで、初学者でもコードを読み進めながらプロダクト全体の振る舞いを理解できるように構成しています。
- **確認ファイル**: `.docs/00-index.md`, `.docs/02-architecture-overview.md`, `Summary.md`

## プロダクトの全体像

### アプリケーションの役割

- Twitch EventSub Webhook からチャンネルポイント関連イベントを受信し、スパム対策や重複制御を行ったうえでキューに登録します（関連コード: `crates/app/src/webhook.rs`, `crates/core/src/normalizer.rs`, `crates/storage/src/lib.rs` 内 `EventRawRepository`）。
- 管理 UI からの操作（キュー消化や設定変更）を冪等なコマンドとして受け付け、サーバ側の状態とフロントエンドを同期させます（関連コード: `crates/app/src/command.rs`, `crates/core/src/projector.rs`, `web/admin/src/api.ts`）。
- 状態の配信には REST `/api/state`（初期同期）と SSE `/overlay/sse`・`/admin/sse`（差分配信）を用います（関連コード: `crates/app/src/state.rs`, `crates/app/src/sse.rs`, `web/shared/src/state.ts`）。

バックエンドのエントリーポイントである `crates/app/src/main.rs` では、設定読み込み、トレース・メトリクス初期化、データベース接続、バックグラウンドワーカー起動、Axum ルーターの構築を順に行います。
- **確認ファイル**: `crates/app/src/main.rs`

```rust
// crates/app/src/main.rs
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    load_env_file();
    let config = AppConfig::from_env()?;
    telemetry::init_tracing(&config)?;
    let metrics = telemetry::init_metrics()?;
    let tap_hub = tap::TapHub::new();
    if config.environment.is_development() {
        tap_hub.spawn_mock_publisher();
    }
    let database = Database::connect(&config.database_url).await?;
    database.run_migrations().await?;
    let _maintenance_handle =
        maintenance::MaintenanceWorker::new(database.clone(), tap_hub.clone()).spawn();
    // Helix / OAuth クライアントや SSE ハブの初期化を行い、AppState を構築
    let (state, backfill_worker) = router::AppState::new(/* 省略 */);
    let _backfill_handle = backfill_worker.spawn();
    let addr: SocketAddr = config.bind_addr;
    axum::serve(tokio::net::TcpListener::bind(addr).await?, router::app_router(state)).await?;
    Ok(())
}
```

`AppState` は Axum ハンドラに共有される依存群で、SSE ハブやコマンド実行器、Helix クライアントなどを保持します（`crates/app/src/router.rs`）。
- **確認ファイル**: `crates/app/src/router.rs`

```rust
// crates/app/src/router.rs
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
    /* OAuth 関連や Helix Backfill 用フィールド */
}
```

### ドメイン層・ストレージ・フロントエンドの概観

- **ドメイン層 (`crates/core`)**: EventSub の生データを `NormalizedEvent` として正規化し（`normalizer.rs`）、ポリシーエンジンで重複や利用制限を判定（`policy.rs`）。判定結果をキュー更新や設定変更などの `Command` に変換し、`projector.rs` でフロントエンド向けの `Patch` を生成します。
- **永続層 (`crates/storage`)**: SQLite を用い、イベント原本（`event_raw`）、コマンドログ、キュー、日次カウンタ、OAuth 情報、Helix バックフィルチェックポイントなどを `Database` ラッパーを通じて操作します。各テーブルごとにサブリポジトリ（`CommandLogRepository` など）が用意されています。
- **フロントエンド (`web/overlay`, `web/admin`, `web/shared`)**: `web/shared` に SSE パッチの適用ロジックや型定義を集約し、オーバーレイ UI と管理 UI から再利用します。例えば `web/shared/src/state.ts` の `applyPatch` 関数は、サーバから送られる `Patch` をクライアント側の状態に逐次適用します。

```ts
// web/shared/src/state.ts
export function applyPatch(state: ClientState, patch: Patch): ClientState {
  if (patch.type === 'state.replace') {
    return createClientState(patch.data.state);
  }
  const expected = state.version + 1;
  if (patch.version !== expected) {
    throw new VersionMismatchError(expected, patch.version);
  }
  switch (patch.type) {
    case 'queue.enqueued': {
      const { entry, user_today_count } = patch.data;
      /* キューの並び替えとカウンタ更新 */
    }
    case 'settings.updated': {
      const settings = mergeSettings(state.settings, patch.data.patch);
      return { version: patch.version, queue: state.queue, counters: state.counters, settings };
    }
    /* そのほかのパッチ種別 */
  }
}
```

## 仕様リファレンスとの関連

`.docs/` 配下にはアーキテクチャ概要、ドメインモデル、API コントラクト、データスキーマなどが整理されています。本資料では、該当箇所に触れるたびに関連する実装ファイルを参照し、仕様とコードが一致していることを確認できるようにしています。たとえば `.docs/02-architecture-overview.md` で説明される EventSub → Policy → Command → SSE のパイプラインは、[データと処理の流れ](03_data_flow.md) で対応する関数群を追いながら説明します。
- **確認ファイル**: `.docs/02-architecture-overview.md`, `.docs/03-domain-model.md`, `.docs/04-api-contracts.md`, `.docs/05-data-schema-and-migrations.md`, `memo/03_data_flow.md`

## 資料の読み進め方

1. [プロジェクト構成ガイド](02_project_structure.md) でディレクトリごとの役割と主要ファイルの配置を把握します。
   - **確認ファイル**: `memo/02_project_structure.md`
2. [データと処理の流れ](03_data_flow.md) では EventSub の受信からフロントエンドへの差分配信まで、実装コードを引用しつつ時系列で追跡します。
   - **確認ファイル**: `memo/03_data_flow.md`
3. [主要コンポーネント詳細](04_core_components.md) でサーバ・ドメイン・フロントの重要クラス／関数を掘り下げます。
   - **確認ファイル**: `memo/04_core_components.md`
4. [環境構築と運用手順](05_configuration_and_usage.md) で環境変数やローカル実行手順、メトリクス確認方法を確認します。
   - **確認ファイル**: `memo/05_configuration_and_usage.md`
5. [拡張とカスタマイズの指針](06_extensions_and_customization.md) と [トラブルシューティング & FAQ](07_troubleshooting_and_faq.md) で変更時・障害時のチェックポイントを押さえ、最後に [付録](08_appendix.md) で用語集や参考リンクを参照してください。
   - **確認ファイル**: `memo/06_extensions_and_customization.md`, `memo/07_troubleshooting_and_faq.md`, `memo/08_appendix.md`

この順序に沿って読み進めれば、リポジトリに含まれるコードと仕様を漏れなく理解できます。
- **確認ファイル**: 本章で案内した各 `memo/*.md`
