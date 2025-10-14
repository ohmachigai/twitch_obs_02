# `.docs/00-index.md` — ドキュメント索引 / 読み方ガイド

> 本フォルダ `.docs/` は、このリポジトリの**仕様の一次ソース**です。
> 実装・レビュー・運用は **AGENTS.md（リポジトリ直下）** と本ファイルの読み順・依存関係に従って進めます。

---

## 1. 目的と適用範囲

* **目的**：実装エージェント（人間/AI）が、**迷いなく次のタスクに着手**できるよう、必要十分なドキュメントの**読み順・依存関係・所在**を示します。
* **対象**：実装者、レビュア、SRE/運用、プロジェクト管理者。
* **前提**：多テナント（配信者ごと完全分離）、EventSub(Webhook)→REST+SSE、中核インバリアントは `AGENTS.md` と一致。

---

## 2. 読み順（必読 → 参照）

1. **必読**

   * `01-product-scope.md`：ゴール/ノンゴールと主要ユースケース
   * `02-architecture-overview.md`：全体図（Ingress→Policy→CommandLog→Projector→SSE）
   * `03-domain-model.md`：用語と不変条件（QueueEntry/DailyCounter/CommandLog/Settings）
   * `04-api-contracts.md`：REST/SSE/Mutation/Debug の**入出力スキーマ**（契約）
   * `05-data-schema-and-migrations.md`：SQLite(WAL)/インデックス/TTL/チェックポイント
2. **実装・運用時に参照**

   * `06-frontend-spec.md`：OBSオーバーレイ/管理UI、テーマ切替、URLクエリ
   * `07-debug-telemetry.md`：Tap（SSE）、Capture/Replay、ログ/メトリクスの項目
   * `08-implementation-plan.md`：PR 分割、受け入れ基準（DoD）、検証手順
   * `09-testing-strategy.md`：Unit/Integration/E2E/Perf、Windows/Ubuntu マトリクス
   * `10-operations-runbook.md`：ローカル/本番、Nginx、systemd、Secrets、NTP
   * `11-security-and-privacy.md`：多テナント認可、SSE トークン、PII、OAuth/Helix
   * `12-ci-cd.md`：GitHub Actions、matrix、lint、`nginx -t`、ブランチ戦略
   * `99-glossary.md`：用語集（表記ゆれ防止）

---

## 3. 重要な合意（要点まとめ）

* **入力（EventSub/Webhook）**：HMAC-SHA256 検証、±10 分、`Message-Id` 冪等、**即 2xx ACK**。
* **状態の一次ソース**：**CommandLog（append-only, version 単調増加）**。
* **出力（OBS/管理 UI）**：**初期は REST `/api/state`、以後は SSE**（`id=version`、**20–30s 心拍**、**リング再送**）。初回のみ `since_version`、再接続は `Last-Event-ID`。
* **管理操作（Mutation）**：**`op_id` 冪等**、COMPLETE と UNDO を区別（UNDO は “今日の回数” を減算）。
* **反スパム**：同一ユーザ×同一リワード×**60 秒以内の連打は消費（consume）**、それ以外は記録＋返金（refund）。
* **保持**：`event_raw` / `command_log` は **72 時間 TTL**、Queue/Counter/Settings は永続。
* **多テナント**：すべて `broadcaster` コンテキストで分離。SSE は**短寿命署名トークン**（クエリ/Cookie）。

> 詳細は各ファイルの「規範（Normative）」節に記載。**規範に反する実装は不可**です。

---

## 4. ドキュメント一覧（目的・依存・規範性）

| ファイル                               | 目的（概要）                           | 主な依存  | 規範性    |
| ---------------------------------- | -------------------------------- | ----- | ------ |
| `01-product-scope.md`              | ゴール/ノンゴール、ユースケース                 | なし    | **規範** |
| `02-architecture-overview.md`      | データフロー/ステージ/可視化点                 | 01    | **規範** |
| `03-domain-model.md`               | エンティティ/不変条件/状態遷移                 | 01-02 | **規範** |
| `04-api-contracts.md`              | REST/SSE/Mutation/Debug の I/O 契約 | 01-03 | **規範** |
| `05-data-schema-and-migrations.md` | SQLite スキーマ/TTL/WAL              | 03    | **規範** |
| `06-frontend-spec.md`              | Overlay/Admin の UI/テーマ/URL       | 02-04 | 準規範    |
| `07-debug-telemetry.md`            | Tap/Capture/Replay/ログ/メトリクス      | 02-04 | 準規範    |
| `08-implementation-plan.md`        | PR 分割/DoD/検証手順                   | 01-07 | ガイド    |
| `09-testing-strategy.md`           | テスト観点/擬似クライアント                   | 02-05 | ガイド    |
| `10-operations-runbook.md`         | ローカル/本番運用手順                      | 02-05 | ガイド    |
| `11-security-and-privacy.md`       | 認可/SSE トークン/PII/OAuth            | 02-05 | **規範** |
| `12-ci-cd.md`                      | CI 設計と基準（Linux/Windows）          | 08-11 | **規範** |
| `99-glossary.md`                   | 用語定義（表記統一）                       | 01-12 | **規範** |

* **規範（Normative）**：実装・レビューの**拘束力**あり。
* **準規範**：実装に対し拘束力を持つが、UI/運用上の裁量を少し許容。
* **ガイド**：進め方や手順の提案。規範に従う限り裁量可。

---

## 5. 依存関係（概念図）

```
01 Scope
  └─ 02 Architecture
       ├─ 03 Domain
       │    ├─ 04 API
       │    └─ 05 Data
       ├─ 06 Frontend   (depends on 04)
       ├─ 07 Debug/Tel  (depends on 02,04)
       ├─ 08 Impl Plan  (depends on 01-07)
       ├─ 09 Testing    (depends on 02-05)
       ├─ 10 Ops        (depends on 02-05,12)
       ├─ 11 Security   (depends on 02-05)
       └─ 12 CI/CD      (depends on 08,11)
```

---

## 6. PR/実装とドキュメントの対応

* **PR-0〜PR-8** は `08-implementation-plan.md` に詳細。
* **API/DB の変更が絡む PR** は、**先に `04`/`05` を更新**してから実装。
* **観測点（Tap/ログ/メトリクス）** は `07` を基準に、各 PR で**必ず追加/更新**。
* **フロント**は `06` の契約（`?theme/&since_version/&types/&include`）に準拠。

---

## 7. 参照規約・表記

* **時刻**：内部記録は UTC。**“今日”**の判定は **配信者のタイムゾーン**。
* **用語**：COMPLETE / UNDO / refund / consume / session / version / op_id / `Last-Event-ID` などは `99-glossary.md` に準拠。
* **識別子**：`version` はブロードキャスタ単位の**単調増加整数**、SSE の `id:` に使用。
* **保持**：TTL 72h（`event_raw` / `command_log`）、永続（Queue/Counters/Settings）。

---

## 8. ファイル構成と所在（抜粋）

* 仕様：このフォルダ `.docs/*`
* 実装（例）：`crates/app`（axum）, `crates/core`, `crates/storage`, `crates/twitch`, `web/overlay`
* 運用テンプレ：`deploy/`, `nginx/`
* ルール：`AGENTS.md`（リポジトリ直下）

---

## 9. 変更の優先順位

* **矛盾が生じた場合の優先度**：`AGENTS.md` → `.docs/01–05（規範）` → `.docs/11/12（規範）` → 準規範 → ガイド。
* **“事実が先に動いた”場合**：当該 `.docs/*` を最短で更新し、PR に同梱。

---

## 10. 状態マトリクス（初期値）

| 項目     | 状態                                          |
| ------ | ------------------------------------------- |
| 入力方式   | EventSub（Webhook）固定                         |
| 配信方式   | REST 初期 + SSE 増分（`id=version`、心拍、リング）       |
| キュー操作  | COMPLETE / UNDO（`op_id` 冪等）                 |
| 反スパム   | 同一ユーザ×同一リワード×60s 内は consume                 |
| 保持     | `event_raw`/`command_log` は 72h TTL、それ以外は永続 |
| 多テナント  | `broadcaster` コンテキストで完全分離                   |
| セキュリティ | SSE 認可は短寿命署名トークン、PII マスク既定                  |
| CI     | Linux/Windows マトリクス、lint/format/test は必須    |

---

このファイルを起点に、上記の**読み順**で `.docs/` を参照すれば、**CodeX は「次にやるべきこと」へ即時に着手**できます。
