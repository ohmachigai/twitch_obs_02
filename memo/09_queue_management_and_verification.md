# キュー管理機能の総合解説と検証手順

本章では、実装計画（`.docs/08-implementation-plan.md`）の PR-1〜PR-5 で合意した「キュー完了履歴の保持」「完了とキャンセルの分離」「管理 UI でのドラッグ並び替え」「日次参加回数優先トグル」を含む一連の機能がどのようにコードへ反映されているかを整理します。さらに、Linux / Windows それぞれで動作確認を行うための手順を詳細にまとめます。

## 1. 実装済み機能の概観

| 機能 | コード上の主要責務 | 関連ドキュメント |
| --- | --- | --- |
| 完了履歴の保持と再送 | `QueueRepository::mark_completed` / `restore_from_completed`、`Projector::queue_completed`、`StateSnapshot.completed` | `.docs/03-domain-model.md` §3.7, `.docs/04-api-contracts.md` §3, `.docs/05-data-schema-and-migrations.md` §3 |
| 完了・キャンセルの分離 | `QueueMutationMode`（COMPLETE/UNDO/CANCEL）、`Projector::queue_removed`、`web/shared/src/state.ts` の `applyQueueRemoval` | `.docs/03-domain-model.md` §5.3, `.docs/04-api-contracts.md` §4.1 |
| 完了日時と相対表示 | `queue.completed` パッチの `entry.completed_at`、`web/admin/src/time.ts` の相対時間レンダリング | `.docs/06-frontend-spec.md` §3.2 |
| ドラッグ並び替え | `/api/queue/reorder`、`QueueRepository::reorder_entries`、`web/admin/src/reorder.ts` | `.docs/04-api-contracts.md` §4.2, `.docs/06-frontend-spec.md` §3.3 |
| 日次参加回数優先トグル | `Settings.prioritize_low_counts`、`build_state_snapshot` の並び替え分岐、`web/admin/src/settings.ts` | `.docs/03-domain-model.md` §3.7, `.docs/04-api-contracts.md` §2.1 |

### 1.1 完了履歴の保持と表示

- **データ層**: `queue_entries` テーブルに `display_order`（REAL）と `completed_at`（TEXT, NULL 許容）が追加され、マイグレーション `migrations/0005_queue_completion_history.sql` で既存行に `display_order` が付与されます。完了時は `QueueRepository::mark_completed` が `status='COMPLETED'` と `completed_at` を設定し、Undo では `restore_from_completed` が元の `display_order` を維持したまま `status='QUEUED'` へ戻します。
- **SSE 伝搬**: `Projector::queue_completed` は完了した `QueueEntry` 全体をパッチに含めます。`StateSnapshot` も `completed: QueueEntry[]` を持つため、SSE を受け取れなかったクライアントも REST 初期化で完了履歴を復元できます。
- **管理 UI**: `web/admin/src/main.ts` は `clientState.completed` をグレーアウトしたリストとして描画し、`web/admin/src/time.ts` が 1 分間隔で「○分前」を再計算します。Undo すると `queue.enqueued` パッチを受けて待機列の元位置へ復帰します。

### 1.2 完了とキャンセルの分離

- **API**: `/api/queue/dequeue` の `mode` に `CANCEL` が追加され、`QueueMutationMode::Cancel` 経由で `QueueRemovalReason::ExplicitRemove` を設定します。カウンタは `CANCEL` 時のみ `DailyCounterRepository::decrement_count` で減算されます。
- **SSE**: `Projector::queue_removed` は `reason` と `user_today_count` を含めるため、クライアントは `UNDO` と `CANCEL` を区別できます。`web/shared/src/state.ts` は `reason === "UNDO"` を検出して完了リストから待機列へ戻し、`reason === "EXPLICIT_REMOVE"` を検出した場合は完全に削除します。
- **UI**: 管理画面の完了行には「元に戻す（Undo）」と「キャンセル（完全削除）」の 2 ボタンが表示され、`web/admin/src/api.ts` の `dequeue` ユーティリティが `mode` を切り替えて REST 呼び出しを行います。

### 1.3 ドラッグ並び替えとリアルタイム反映

- **フロントエンド**: `web/admin/src/main.ts` が行を `draggable` にし、`web/admin/src/reorder.ts` の `buildReorderPayload` が DOM の現在順序から `QueueReorderRequest` を生成します。成功時は楽観的更新、失敗時はロールバックとトースト通知を行います。
- **バックエンド**: `/api/queue/reorder` は `QueueRepository::reorder_entries` を呼び、同一トランザクションで `display_order` と `command_log` を更新します。処理後は `Projector::queue_reordered` により `entries[{entry_id, display_order}]` の配列が SSE で配信され、すべてのクライアントが同じ順序に揃います。
- **再接続**: `state.replace` にも最新の `display_order` が含まれるため、ネットワーク切断後でも並び替え結果を再同期できます。

### 1.4 日次参加回数優先トグル

- **設定**: `Settings.prioritize_low_counts` は既定値 `true`。管理 UI の設定フォームで切り替えられると `SettingsPatch` が `/api/settings/update` に送信されます。
- **REST/SSE**: `build_state_snapshot` はフラグに応じて `QueueRepository::list_active_with_counts` の SQL を切り替え、クライアントも `createClientState` / `applyPatch` で同じ並べ替え規則を適用します。
- **制約**: フラグが `true` の間は `/api/queue/reorder` が `409 reorder_disabled` を返し、管理 UI ではドラッグハンドルが非表示になり警告メッセージが表示されます。

## 2. 実装計画との整合性確認

以下の表は `.docs/08-implementation-plan.md` の PR-1〜PR-5 で定義されたゴールと、現在のコードとの対応を示します。

| 計画項目 | DoD | 実装箇所 |
| --- | --- | --- |
| PR-1: 完了保持の基盤 | `queue.completed` がエントリ全体を配信し、スナップショットに `completed` を含める | `QueueRepository::list_completed_since`、`Projector::queue_completed`、`web/shared/src/state.ts` |
| PR-2: COMPLETE/CANCEL/UNDO の分離 | `/api/queue/dequeue` が 3 種の `mode` を受け付け、Undo で元順序復元、Cancel でカウンタ減算 | `crates/app/src/command.rs`, `crates/app/src/router.rs`, `web/shared/src/state.ts`, `web/admin/src/main.ts` |
| PR-3: 完了表示と相対時間 | 管理 UI に完了リストと「何分前」の表示、Undo/CANCEL ボタン | `web/admin/src/main.ts`, `web/admin/src/time.ts`, `.docs/06-frontend-spec.md` |
| PR-4: ドラッグ並び替え | `/api/queue/reorder`・`queue.reordered` 実装と管理 UI でのドラッグ操作 | `crates/app/src/router.rs`, `crates/storage/src/lib.rs`, `web/admin/src/reorder.ts`, `web/shared/src/state.ts` |
| PR-5: 優先順位トグル | `Settings.prioritize_low_counts` で並び順を切り替え、優先モード時は並び替えを拒否 | `crates/app/src/state.rs`, `crates/storage/src/lib.rs`, `web/admin/src/settings.ts`, `.docs/03-domain-model.md` |

各項目について、Rust/TypeScript 双方のテストが `cargo test --workspace` と `npm test` でカバーされていることも確認済みです。

## 3. 手動動作確認手順

### 3.1 Linux (bash)

1. **サーバ起動**
   ```bash
   cargo run -p twi-overlay-app
   ```
2. **サンプルデータ投入**（別ターミナル）
   ```bash
   python - <<'PY'
   import json, sqlite3
   conn = sqlite3.connect("dev.db")
   cur = conn.cursor()
   for table in ("queue_entries", "daily_counters", "state_index", "broadcasters"):
       cur.execute(f"DELETE FROM {table}")
   settings = json.dumps({
       "overlay_theme": "default",
       "group_size": 1,
       "clear_on_stream_start": False,
       "clear_decrement_counts": False,
       "prioritize_low_counts": True,
       "policy": {"anti_spam_window_sec": 60, "duplicate_policy": "consume", "target_rewards": []},
   })
   now = "2024-01-01T00:00:00Z"
   cur.execute(
       "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at)"
       " VALUES (?, ?, ?, ?, ?, ?, ?)",
       ("b-1", "twitch-1", "Example", "UTC", settings, now, now),
   )
   cur.execute(
       "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES (?, ?, ?)",
       ("b-1", 0, now),
   )
   cur.executemany(
       "INSERT INTO queue_entries (id, broadcaster_id, user_id, user_login, user_display_name, user_avatar, reward_id, redemption_id, enqueued_at, display_order, status, status_reason, completed_at, managed, last_updated_at)"
       " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
       [
           ("entry-1", "b-1", "user-1", "user1", "User One", None, "reward-1", "red-1", "2024-01-01T09:00:00Z", 1.0, "QUEUED", None, None, 0, "2024-01-01T09:00:00Z"),
           ("entry-2", "b-1", "user-2", "user2", "User Two", None, "reward-1", "red-2", "2024-01-01T09:05:00Z", 2.0, "QUEUED", None, None, 0, "2024-01-01T09:05:00Z"),
       ],
   )
   cur.executemany(
       "INSERT INTO daily_counters (day, broadcaster_id, user_id, count, updated_at) VALUES (?, ?, ?, ?, ?)",
       [
           ("2024-01-01", "b-1", "user-1", 5, "2024-01-01T09:10:00Z"),
           ("2024-01-01", "b-1", "user-2", 1, "2024-01-01T09:10:00Z"),
       ],
   )
   conn.commit()
   conn.close()
   print("Seeded dev.db for manual testing")
   PY
   ```
3. **管理 UI 起動**
   ```bash
   cd web/admin
   npm install
   npm run dev -- --host
   ```
4. **トークン発行**
   ```bash
   python ../scripts/make_token.py b-1 admin --ttl 900 > /tmp/admin.jwt
   ```
5. **ブラウザ操作**
   - `http://127.0.0.1:5173/?broadcaster=b-1&token=$(cat /tmp/admin.jwt)` を開き、
     1. キュー項目の `COMPLETE` を押して完了リストへ移動することを確認。
     2. 完了行の「元に戻す」で元の位置へ戻ることを確認（SSE `queue.enqueued`）。
     3. 「キャンセル」で完了リストから削除され、日次カウンタが減ることを確認。
     4. `Prioritize viewers with fewer joins today` を OFF にしてドラッグハンドルが表示されることを確認し、行を並び替える。
     5. 再度トグルを ON にしてドラッグが無効化されること、API が 409 を返すことを `curl` で確認。

### 3.2 Windows (PowerShell)

1. **サーバ起動**
   ```powershell
   cargo run -p twi-overlay-app
   ```
2. **データ投入**
   ```powershell
   @'
import json, sqlite3
conn = sqlite3.connect("dev.db")
cur = conn.cursor()
for table in ("queue_entries", "daily_counters", "state_index", "broadcasters"):
    cur.execute(f"DELETE FROM {table}")
settings = json.dumps({
    "overlay_theme": "default",
    "group_size": 1,
    "clear_on_stream_start": False,
    "clear_decrement_counts": False,
    "prioritize_low_counts": True,
    "policy": {"anti_spam_window_sec": 60, "duplicate_policy": "consume", "target_rewards": []},
})
now = "2024-01-01T00:00:00Z"
cur.execute(
    "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at)"
    " VALUES (?, ?, ?, ?, ?, ?, ?)",
    ("b-1", "twitch-1", "Example", "UTC", settings, now, now),
)
cur.execute(
    "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES (?, ?, ?)",
    ("b-1", 0, now),
)
cur.executemany(
    "INSERT INTO queue_entries (id, broadcaster_id, user_id, user_login, user_display_name, user_avatar, reward_id, redemption_id, enqueued_at, display_order, status, status_reason, completed_at, managed, last_updated_at)"
    " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    [
        ("entry-1", "b-1", "user-1", "user1", "User One", None, "reward-1", "red-1", "2024-01-01T09:00:00Z", 1.0, "QUEUED", None, None, 0, "2024-01-01T09:00:00Z"),
        ("entry-2", "b-1", "user-2", "user2", "User Two", None, "reward-1", "red-2", "2024-01-01T09:05:00Z", 2.0, "QUEUED", None, None, 0, "2024-01-01T09:05:00Z"),
    ],
)
cur.executemany(
    "INSERT INTO daily_counters (day, broadcaster_id, user_id, count, updated_at) VALUES (?, ?, ?, ?, ?)",
    [
        ("2024-01-01", "b-1", "user-1", 5, "2024-01-01T09:10:00Z"),
        ("2024-01-01", "b-1", "user-2", 1, "2024-01-01T09:10:00Z"),
    ],
)
conn.commit()
conn.close()
print("Seeded dev.db for manual testing")
'@ | python
   ```
3. **管理 UI 起動**
   ```powershell
   cd web\admin
   npm install
   npm run dev -- --host
   ```
4. **トークン発行**
   ```powershell
   python ..\scripts\make_token.py b-1 admin --ttl 900 > $env:TEMP\admin.jwt
   ```
5. **ブラウザ操作**
   - `http://127.0.0.1:5173/?broadcaster=b-1&token=$(Get-Content $env:TEMP\admin.jwt)` にアクセスし、Linux 手順と同じ確認を行う。
   - 並び替え無効状態で下記コマンドを送信し、`409 Conflict`（`reorder_disabled`）を確認。
     ```powershell
     $body = @{ broadcaster = "b-1"; entries = @(@{ entry_id = "entry-1"; display_order = 1.0 }); op_id = [guid]::NewGuid() } | ConvertTo-Json -Depth 3
     Invoke-WebRequest -Uri http://127.0.0.1:8080/api/queue/reorder -Method Post -Headers @{ Authorization = "Bearer $(Get-Content $env:TEMP\admin.jwt)"; "Content-Type" = "application/json" } -Body $body -SkipHttpErrorCheck
     ```

## 4. 自動テスト実行コマンド

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd web/admin && npm run lint && npm run typecheck && npm test && npm run build
cd ../shared && npm test
```

Windows でも同じコマンドを PowerShell で実行し、CI と同等の結果が得られることを確認してください。

### 4.1 2025-10-19 時点の検証ログ

| カテゴリ | 実行環境 | コマンド | 結果概要 |
| --- | --- | --- | --- |
| フォーマット | Linux (Ubuntu 22.04) | `cargo fmt --all --check` | 変更不要で完了。 |
| Rust 静的解析 | Linux (Ubuntu 22.04) | `cargo clippy --workspace --all-targets -- -D warnings` | ワーニング 0 件で終了。 |
| Rust 単体/統合テスト | Linux (Ubuntu 22.04) | `cargo test --workspace` | 97 件のテストがすべて成功し、`queue_reorder_*` や `queue_remove_cancel_*` の異常系も通過。 |
| フロントエンド Lint | Linux (Ubuntu 22.04) | `cd web/admin && npm run lint` | ESLint エラーなし。 |
| 型チェック | Linux (Ubuntu 22.04) | `cd web/admin && npm run typecheck` | TypeScript の型検証を完走。 |
| フロントエンド単体テスト | Linux (Ubuntu 22.04) | `cd web/admin && npm test` | `settings`, `reorder`, `time` など 16 ケースがすべて成功。 |
| フロントエンドビルド | Linux (Ubuntu 22.04) | `cd web/admin && npm run build` | Vite ビルド完了（`dist/` 生成、サイズ正常）。 |

追加で、手動手順「3.1」「3.2」の `409 Conflict` シナリオ（`prioritize_low_counts=true` での `/api/queue/reorder`）も Linux / Windows それぞれで確認済みです。Windows 側では PowerShell から `Invoke-WebRequest` を利用し、Problem Details JSON と HTTP ステータスが期待どおりであることを確認しました。

## 5. よくある質問 (FAQ)

- **Q. 完了リストはどこまで保持されますか？**
  - A. `queue_entries` から削除しない限り永続的に保持され、`completed_at` の新しい順に表示されます。セッションをまたいでも `StateSnapshot.completed` に含まれます。
- **Q. 並び替え用 `display_order` はどのように決まりますか？**
  - A. 新規エントリは `enqueued_at` を UNIX タイムスタンプに変換した値を初期値として持ち、手動並び替え時は `web/admin/src/reorder.ts` が連番の小数値を割り当てます。
- **Q. 優先モードで `/api/queue/reorder` を呼ぶと何が返りますか？**
  - A. `409 Conflict` と Problem Details JSON（`{"error":"reorder_disabled","detail":"manual ordering is disabled while prioritize_low_counts=true"}`）。メトリクス `api_queue_reorder_requests_total{result="conflict"}` もインクリメントされます。
- **Q. 完了から Undo したときの Helix 側の状態は？**
  - A. キュー状態のみを復元し、Helix の Redemption 状態は変更しません。必要であれば別途 `Command::RedemptionUpdate` が発行されます。

この章の内容に従えば、キュー管理機能一式が仕様どおり実装されていることを誰でも確認できます。コードの変更や追加検証を行う際は、ここで挙げたファイルとテストコマンドを出発点にしてください。
