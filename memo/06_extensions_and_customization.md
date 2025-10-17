# 拡張とカスタマイズの指針

この章では、新機能の追加や既存機能の変更を行う際に押さえておくべきポイント、関連ファイル、テスト観点を整理します。`.docs/08-implementation-plan.md` の進め方と合わせて参照してください。
- **確認ファイル**: `.docs/08-implementation-plan.md`, `AGENTS.md`, `crates/app/src/*`, `crates/core/src/*`, `web/*`

## 1. イベント処理の拡張

### 1.1 新しい EventSub 種別への対応

1. `.docs/04-api-contracts.md` で受信するイベント仕様を追加・更新します（関連ドキュメント: `.docs/04-api-contracts.md`, `.docs/02-architecture-overview.md`）。
2. `crates/core/src/normalizer.rs` に分岐を追加し、`NormalizedEvent` を拡張します。必要に応じて `types.rs` に新しい構造体を定義してください（関連コード: `crates/core/src/normalizer.rs`, `crates/core/src/types.rs`）。
3. `PolicyEngine::evaluate`（`policy.rs`）で新しいイベントに対する処理を実装します。`PolicyOutcome` に期待する `Command` を返すようにします（関連コード: `crates/core/src/policy.rs`, `crates/app/src/webhook.rs`）。
4. `Projector` に `CommandResult` から `Patch` への変換を追加し、フロントエンドで適用できるようにします（関連コード: `crates/core/src/projector.rs`, `web/shared/src/state.ts`）。
5. `web/shared/src/types.ts` と `state.ts` に新しい `Patch` を追加し、React 側で状態が更新されるようにします（関連コード: `web/shared/src/types.ts`, `web/shared/src/state.ts`）。
6. `web/overlay/src/App.tsx` や `web/admin` の UI が新しいデータに対応するか確認します（関連コード: `web/overlay/src/App.tsx`, `web/admin/src/main.ts`, `web/admin/src/settings.ts`）。

### 1.2 ポリシーのカスタマイズ

- `PolicySettings` に新しい設定項目を追加する際は、`types.rs` と `web/shared/src/types.ts` の両方を更新します（関連コード: `crates/core/src/types.rs`, `web/shared/src/types.ts`）。
- スパム制御や重複判定ロジックを変更する場合、`policy.rs` の `on_redemption_add` や `allow_duplicate` 系の関数を参照してください（関連コード: `crates/core/src/policy.rs`）。
- 設定値の保存は `BroadcasterRepository::apply_settings_patch` で行われるため、マイグレーションでスキーマを変更しつつ JSON パッチに対応する必要があります（関連コード: `crates/storage/src/lib.rs` の `BroadcasterRepository`, `migrations/*.sql`）。

## 2. コマンドと状態の拡張

### 2.1 新しいコマンドの追加

1. `twi_overlay_core::types::Command` 列挙体にバリアントを追加し、`CommandResult`・`Patch` など関連型も更新します（関連コード: `crates/core/src/types.rs`, `web/shared/src/types.ts`）。
2. `crates/app/src/command.rs` の `CommandExecutor::apply_command` に分岐を追加し、永続化・副作用を定義します（関連コード: `crates/app/src/command.rs`, `crates/storage/src/lib.rs`）。
3. `Projector::project` に新しい `CommandResult` から `Patch` への変換を追加します（関連コード: `crates/core/src/projector.rs`）。
4. `SseHub` は `Patch` 構造体を透過的に扱うため、追加の対応は不要です。
5. フロントエンドの `applyPatch` で新しいパッチ種別を処理し、UI を更新します（関連コード: `web/shared/src/state.ts`, `web/overlay/src/App.tsx`, `web/admin/src/main.ts`）。

### 2.2 状態スナップショットの拡張

- `/api/state` のレスポンスを拡張する場合、`StateSnapshot`（`twi_overlay_core::types`）にフィールドを追加し、`state.rs` の `build_state_snapshot` でデータを取得・整形します（関連コード: `crates/core/src/types.rs`, `crates/app/src/state.rs`, `crates/storage/src/lib.rs`）。
- フロントエンドの `createClientState` が新しいフィールドを取り込むよう更新し、必要に応じて UI を変更します（関連コード: `web/shared/src/state.ts`, `web/overlay/src/App.tsx`, `web/admin/src/main.ts`）。
- 既存クライアントとの後方互換性を保つため、可能であれば optional フィールド（`Option`）やデフォルト値を利用してください（関連コード: `crates/core/src/types.rs`, `web/shared/src/state.ts`）。

## 3. ストレージの変更

### 3.1 テーブル追加・変更

1. `migrations/` に sqlx 用マイグレーションを追加します。`.docs/05-data-schema-and-migrations.md` にも必ず更新内容を記載します（関連ファイル: `migrations/*.sql`, `.docs/05-data-schema-and-migrations.md`）。
2. `crates/storage/src/lib.rs` に対応するリポジトリ・メソッドを追加し、テストでクエリを検証します（関連コード: `crates/storage/src/lib.rs`）。
3. `Database` のファクトリメソッドを追加し、`AppState::new` からアクセスできるようにします（関連コード: `crates/storage/src/lib.rs`, `crates/app/src/router.rs` の `AppState::new`）。
4. 新しいデータが SSE や REST に流れる場合は `StateSnapshot` や `Patch` も更新してください（関連コード: `crates/core/src/types.rs`, `crates/app/src/state.rs`, `crates/core/src/projector.rs`, `web/shared/src/state.ts`）。

### 3.2 インデックス・パフォーマンス

- 大量データを扱う場合、`sqlx::query!` のパラメータにインデックスが効くよう `CREATE INDEX` をマイグレーションに追加してください（関連ファイル: `migrations/*.sql`, `crates/storage/src/lib.rs`）。
- WAL と TTL のポリシーは `maintenance.rs` に実装されているため、削除対象を増やした場合は同ファイルを更新します（関連コード: `crates/app/src/maintenance.rs`, `.docs/05-data-schema-and-migrations.md`）。

## 4. SSE とフロントエンド

### 4.1 SSE イベントフィルタ

- `/overlay/sse` と `/admin/sse` の `types` クエリパラメータは `SseHub::subscribe` のフィルタに渡されます。新しい `Patch::kind_str()` を追加したら、フィルタロジックが期待通り動作するか確認します（関連コード: `crates/app/src/sse.rs`, `crates/core/src/types.rs`）。

### 4.2 クライアント状態管理

- `web/shared/src/state.ts` の `applyPatch` はバージョン整合性を厳密にチェックします。バージョンが連番でなくなる変更を加える場合は、`.docs/02` に定義された `CommandLog.version` のインバリアントを再検討する必要があります（関連コード: `web/shared/src/state.ts`, `.docs/02-architecture-overview.md`, `crates/app/src/command.rs`）。
- Redux 等の別の状態管理を採用する場合も、`ClientState` と `Patch` の互換性を保ってください（関連コード: `web/shared/src/state.ts`, `web/shared/src/types.ts`）。

## 5. OAuth / Helix まわり

- 新しいスコープが必要になった場合、`command.rs` の `REQUIRED_OAUTH_SCOPES` を更新し、`.docs/04` の要件も変更します（関連コード: `crates/app/src/command.rs`, `.docs/04-api-contracts.md`）。
- Helix API の追加エンドポイントを利用する際は、`twi_overlay_twitch::HelixClient` にメソッドを追加し、`BackfillWorker` や管理操作から呼び出します（関連コード: `crates/twitch/src/helix.rs`, `crates/app/src/backfill.rs`, `crates/app/src/command.rs`）。
- エラーコード体系（`ERR_OAUTH_*`, `ERR_HELIX_*`）を拡張したら、REST・SSE・フロントエンドの表示が統一されているか確認します（関連コード: `crates/app/src/command.rs`, `crates/app/src/oauth.rs`, `web/admin/src/api.ts`, `web/shared/src/state.ts`）。

## 6. テレメトリとデバッグ

- 新しい処理段を追加する際は、`tap.rs` の `StageKind` と `TapHub` の publish 箇所を更新して `_debug/tap` で可視化できるようにします（関連コード: `crates/app/src/tap.rs`）。
- メトリクスは `metrics::counter!` や `metrics::histogram!` で発行します。名称は `.docs/07-debug-telemetry.md` と整合させてください（関連コード: `crates/app/src/telemetry.rs`, `.docs/07-debug-telemetry.md`）。

## 7. テストと検証

- Rust では `#[cfg(test)]` フィールドを利用して依存を差し替えられるよう `AppState` が設計されています。Helix をモックした統合テストを追加する際はこの仕組みを活用してください（関連コード: `crates/app/src/router.rs`, `crates/util/src/lib.rs` の `test_support`）。
- フロントエンドは Vitest を使っているため、新しい状態遷移やコンポーネントは `web/shared/src/state.test.ts` や個別コンポーネントのテストに追加します（関連コード: `web/shared/src/state.test.ts`, `web/overlay/src/api.test.ts`, `web/admin/src/config.test.ts`, `web/admin/src/settings.test.ts`）。
- CI で通すべきコマンドは `AGENTS.md` の「CI / Lint / Test」に記載されています。新しいジョブを追加する場合は GitHub Actions のワークフローも更新してください（関連ファイル: `AGENTS.md`, `.github/workflows/`）。

## 8. 変更のレビュー観点チェックリスト

- [ ] `.docs/` の仕様が最新化されているか。
- [ ] `migrations/` にスキーマ変更が含まれているか（必要な場合）。
- [ ] SSE と REST の公開契約が後方互換か検証したか。
- [ ] Tap / メトリクス / ログで新しい挙動が観測できるか。
- [ ] 手動検証手順を README か PR に記載したか。
- [ ] Windows / Linux 双方でコマンドが成功するか確認したか。

このチェックリストをもとに変更を小さく安全に進めることで、インバリアントを崩さずに機能拡張が行えます。
- **確認ファイル**: 本章で参照した `.docs/*`, `crates/*`, `web/*`, `migrations/*`
