# `.docs/12-ci-cd.md` — CI / CD 設計（規範 + 実装テンプレ）

> 本章は **GitHub Actions** を前提に、**CI（品質担保）**と**CD（配布・リリース）**の一次仕様を定義します。
> ここに記す **ワークフロー / ジョブ / キャッシュ / 成果物 / セキュリティ検査 / 承認フロー** は拘束条件（**MUST**）です。
> 関連：`02-architecture-overview`・`05-data-schema-and-migrations`・`07-debug-telemetry`・`08-implementation-plan`・`10-operations-runbook`。

---

## 0. CI/CD の目的と原則

* **目的**：

  1. PR 単位で**壊れていないこと**を高速に保証（Linux/Windows）。
  2. **再現可能**な成果物（バイナリ・静的アセット・テンプレ）を生成。
  3. **安全なデプロイ**（人手承認・ロールバック容易）。
* **原則**：

  * **小さな PR**（`08` 参照）を**早く確実**に通す。
  * **本番鍵・シークレットを CI に渡さない**（MUST）。
  * 成果物は**不変（immutable）**・**署名／ハッシュ**（将来導入可）。
  * **OS マトリクス**：`ubuntu-latest` と `windows-latest`（MUST）。

---

## 1. ブランチ戦略 / 保護規則（MUST）

* **デフォルトブランチ**：`main`
* **保護**（Repository settings → Branches → `main`）：

  * Require pull request reviews（最小 1）
  * Require status checks to pass（本章で定義する CI 全ジョブ）
  * Require linear history / 禁止：直接 push
  * Dismiss stale approvals on new commits
* **タグ命名**：`vMAJOR.MINOR.PATCH`（例：`v0.1.0`）
* **コミット規約（推奨）**：Conventional Commits。リリースノート自動化に活用。

---

## 2. ワークフロー全体像

| ファイル                             | 用途                 | トリガ                                    | OS             | 概要                                                             |
| -------------------------------- | ------------------ | -------------------------------------- | -------------- | -------------------------------------------------------------- |
| `.github/workflows/ci.yml`       | **PR/Push** の標準 CI | `pull_request`, `push`(main)           | Ubuntu+Windows | Rust fmt/clippy/test、SQLite migrate、Front lint/typecheck/build |
| `.github/workflows/e2e.yml`      | E2E（任意/重い）         | `workflow_dispatch` or ラベル `e2e: true` | Ubuntu         | サーバ起動＋ヘッドレス（Playwright など）                                     |
| `.github/workflows/release.yml`  | **Release** 配布     | `push` tags `v*`                       | Ubuntu+Windows | バイナリ・静的資産をビルド→アーカイブ→リリース作成                                     |
| `.github/workflows/security.yml` | 供給網・静的解析           | `schedule`（毎日）/ `workflow_dispatch`    | Ubuntu         | `cargo audit` / `cargo-deny` / `npm audit` / CodeQL            |

> **paths-filter** を用いて、不要サブジョブを自動スキップ（例：フロント未変更なら Frontend ジョブ省略）。

---

## 3. CI（`ci.yml`）— 規範

### 3.1 トリガ / 共通

* `on: [pull_request, push]`（`main` への push は保護下）
* **concurrency**（重複キャンセル）：`group: ${{ github.workflow }}-${{ github.ref }}`、`cancel-in-progress: true`

### 3.2 ジョブ構成

1. **rust**（マトリクス：OS=Ubuntu/Windows）

* **MUST**：

  * Toolchain：stable（`actions-rs/toolchain` or `dtolnay/rust-toolchain`）
  * キャッシュ：`~/.cargo/registry`、`~/.cargo/git`、`target`
  * `cargo fmt --all --check`
  * `cargo clippy --workspace --all-targets -D warnings`
  * **SQLite マイグレーション検証**：

    * `sqlx migrate run --database-url sqlite://./.tmp/test.db`
  * **ユニット/統合テスト**：

    * `SSE_HEARTBEAT_MS=300` など短縮（`07` 参照）
    * `cargo test --workspace -- --nocapture`
  * 成果物（任意）：Tap/ログを失敗時にアップロード

2. **frontend**（変更時のみ）

* **paths-filter** で `web/**` に差分がある場合のみ実行。
* Node 18+、キャッシュ `~/.npm`。
* `npm ci` → `npm run lint` → `npm run typecheck` → `npm run build`。
* 生成物 `web/overlay/dist` をアーティファクト化（後続 release で使用）。

3. **schema-check**（任意）

* `sqlx prepare -- --all-targets`（offline 検証を採用する場合）。
* あるいは `migrations/` に対する `--dry-run` 検証。

#### 参考テンプレ（抜粋）

```yaml
name: ci
on:
  pull_request:
  push:
    branches: [main]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

jobs:
  rust:
    strategy:
      matrix: { os: [ubuntu-latest, windows-latest] }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Migrate (SQLite)
        run: |
          mkdir -p .tmp
          sqlx migrate run --database-url sqlite://./.tmp/test.db
      - name: Fmt
        run: cargo fmt --all --check
      - name: Clippy
        run: cargo clippy --workspace --all-targets -- -D warnings
      - name: Test
        env:
          SSE_HEARTBEAT_MS: 300
        run: cargo test --workspace -- --nocapture

  frontend:
    if: ${{ github.event_name == 'push' || github.event_name == 'pull_request' }}
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dorny/paths-filter@v3
        id: changes
        with:
          filters: |
            web:
              - 'web/**'
      - if: steps.changes.outputs.web == 'true'
        uses: actions/setup-node@v4
        with: { node-version: 20, cache: 'npm', cache-dependency-path: 'web/overlay/package-lock.json' }
      - if: steps.changes.outputs.web == 'true'
        run: |
          cd web/overlay
          npm ci
          npm run lint
          npm run typecheck
          npm run build
      - if: steps.changes.outputs.web == 'true'
        uses: actions/upload-artifact@v4
        with:
          name: overlay-dist
          path: web/overlay/dist
          retention-days: 7
```

> **注意**：Windows では OpenSSL 周りの依存がある場合、`vcpkg` or `-sys` crate の前提を満たすこと。SQLite はバンドルで問題ない想定。

---

## 4. E2E（`e2e.yml`）— 任意だが推奨

* **トリガ**：`workflow_dispatch`（手動）または PR ラベル `e2e: true`。
* **流れ**：

  1. サーバを起動（ephemeral ポート、`.tmp/test.db` に migrate）
  2. ヘッドレスブラウザ（Playwright/Chromium）でオーバーレイを開く
  3. モック API（`/api/admin/mock`）で `redemption.add` 注入
  4. DOM アサーション（`li.queue-item` が追加→UNDO で削除→Reload で冪等）
* **タイムアウト**：5–8 分。
* **成果物**：スクリーンショット / テストログを artifact 保存。

---

## 5. リリース（`release.yml`）— 配布

### 5.1 トリガ / フロー

* **トリガ**：`push` タグ `v*`
* **フロー**：

  1. Rust バイナリを OS ごとにビルド（`--release`）
  2. Frontend をビルド（`web/overlay/dist`）
  3. **アーカイブ**を生成（`tar.gz` / `zip`）：

     ```
     twi-overlay-<os>-<arch>-<version>/
       bin/twi-overlay-app
       share/migrations/
       share/nginx/overlay.conf.template
       share/deploy/systemd/twi-overlay.service
       web/overlay/  (ビルド済み)
       LICENSE
       README-release.md
     ```
  4. GitHub Release（Draft→Publish）へ添付
  5. **チェックサム**（`SHA256SUMS`）を同梱（署名は任意）

### 5.2 テンプレ（抜粋）

```yaml
name: release
on:
  push:
    tags: ['v*']

jobs:
  build:
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            archive: tar.gz
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            archive: zip
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Build (Rust)
        run: cargo build --release --locked
      - name: Build (Frontend)
        if: ${{ matrix.os == 'ubuntu-latest' }}
        uses: actions/setup-node@v4
        with: { node-version: 20, cache: 'npm', cache-dependency-path: 'web/overlay/package-lock.json' }
      - if: ${{ matrix.os == 'ubuntu-latest' }}
        run: |
          cd web/overlay && npm ci && npm run build
      - name: Prepare bundle
        run: |
          mkdir -p bundle/bin bundle/share/migrations bundle/share/nginx bundle/share/deploy/systemd bundle/web/overlay
          cp target/release/twi-overlay-app bundle/bin/
          cp -r migrations/* bundle/share/migrations/ || true
          cp -r nginx/* bundle/share/nginx/ || true
          cp deploy/systemd/twi-overlay.service bundle/share/deploy/systemd/ || true
          [ -d web/overlay/dist ] && cp -r web/overlay/dist/* bundle/web/overlay/ || true
          cp LICENSE bundle/ || true
          cp README.md bundle/README-release.md
      - name: Archive
        run: |
          NAME="twi-overlay-${{ matrix.target }}-${{ github.ref_name }}"
          if [ "${{ matrix.archive }}" = "tar.gz" ]; then
            tar -C bundle -czf "$NAME.tar.gz" .
            echo "$NAME.tar.gz" > artifact_name.txt
          else
            powershell "Compress-Archive -Path bundle\\* -DestinationPath $env:NAME.zip"
            echo "$NAME.zip" > artifact_name.txt
          fi
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: ${{ steps.archive.outputs.name || 'bundle' }}
          path: |
            *.tar.gz
            *.zip

  publish:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with: { name: bundle, path: dist }
      - name: Checksums
        run: |
          cd dist
          sha256sum * > SHA256SUMS
      - name: Release
        uses: softprops/action-gh-release@v2
        with:
          files: dist/*
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

> **注意**：Windows のアーカイブは `zip` を推奨。Linux は `tar.gz`。将来 Docker/ghcr 提供は別ジョブで。

---

## 6. セキュリティ・健全性ワークフロー（`security.yml`）

* **CodeQL**（C/C++, Go, JavaScript, Python, **Rust** も対象）
* **Rust**：`cargo audit`（脆弱性 DB）、`cargo-deny`（ライセンス/重複）
* **Node**：`npm audit --omit=dev`（本番依存のみ）
* **スケジュール**：毎日 1 回（UTC）+ 手動トリガ
* **失敗時**：PR を自動作成 or Issue 化（任意）

---

## 7. キャッシュ / 成果物 / 保持（規範）

* **キャッシュ**（MUST）

  * Rust：`Swatinem/rust-cache@v2`（キー：`os + rustc + Cargo.lock`）
  * Node：`actions/setup-node` の npm キャッシュ（キー：`package-lock.json`）
* **成果物保管**

  * CI の中間アーティファクト（フロント dist 等）は**7 日**、リリース成果物は GitHub Release に**恒久**。
* **ログ**

  * 失敗時：`target/debug/deps/*.log`、`tracing` ログ、E2E のスクショを artifacts に添付（SHOULD）。

---

## 8. 環境・シークレット管理（MUST）

* CI で **本番シークレットを使わない**。PR では **fork からのシークレット注入禁止**（デフォルト）。
* Release ジョブは**`GITHUB_TOKEN` のみ**で実行可能。外部レジストリ公開（ghcr 等）を行う場合は `CR_PAT` を **環境限定シークレット**で付与。
* **環境（Environments）** を利用する場合：`staging` / `production` を作成し、**Reviewer 承認**を必須に設定（CD を使う場合）。

---

## 9. デプロイ戦略（任意：CD 導入時の規範）

* 本リポジトリは **配布アーティファクト**を作成し、VPS へは **手動反映**（`10-runbook`）が既定。
* CD を導入する場合（任意）：

  * **環境 `staging` / `production`** に GitHub Actions の **環境保護**（手動承認）を付与。
  * `deploy.yml` を追加し、`workflow_dispatch` で**SCP/rsync**反映 → `systemd restart`。
  * **ロールバック**：`/opt/twi-overlay/releases/<ts>` へのシンボリック切替を自動化（`10` 参照）。
  * **前提**：SSH 鍵は **環境シークレット**に格納。`known_hosts` を収録（MITM 防止）。

---

## 10. 版管理 / バージョニング

* **Semantic Versioning**：`MAJOR.MINOR.PATCH`。
* **バンプ手順**（手動例）：

  1. `cargo set-version <x.y.z>`（ワークスペース）
  2. `npm version --no-git-tag-version <x.y.z>`（`web/overlay` が独自バージョンを持つ場合のみ）
  3. CHANGELOG 更新（Release Drafter を使う場合は自動ドラフト→確認）
  4. `git commit -m "chore(release): vX.Y.Z"` → `git tag vX.Y.Z` → `git push --tags`
* **自動ドラフト**：`release-drafter` を導入可（任意）。

---

## 11. フレーク / 時間依存対策（MUST）

* テストは `util::Clock` を DI、`SSE_HEARTBEAT_MS` を短縮。
* `concurrency.cancel-in-progress` を有効化。
* SSE/E2E は**タイムアウト**（5–8 分）と**再試行**を設定。
* Windows 固有の改行/パス差異に依存しないスクリプト。

---

## 12. 変更検知と部分ビルド（推奨）

* `dorny/paths-filter` で差分検知：

  * `crates/**` → Rust ジョブ
  * `web/**` → Frontend ジョブ
  * `migrations/**` → SQLite migrate 検証
* 将来：`bazelisk` や `nx` のような分散キャッシュは**不要**（現規模）。

---

## 13. 失敗時の対応（オペレーション連携）

* CI 失敗 → PR に自動ステータス。Tap/Capture は**本番のみ**だが、CI 失敗時はテストログを artifacts として提示。
* Release 失敗 → **リリースは公開しない（Draft 止まり）**、再実行。

---

## 14. 受け入れチェック（本章適合）

* [ ] `ci.yml`：Rust（fmt/clippy/test/migrate）＋ Frontend（lint/typecheck/build）を OS マトリクスで実行
* [ ] `e2e.yml`：サーバ起動→モック→ヘッドレスで Overlay を検証（任意実行）
* [ ] `release.yml`：タグでアーカイブ作成（バイナリ＋migrations＋nginx＋systemd＋overlay/dist）
* [ ] `security.yml`：`cargo audit` / `cargo-deny` / `npm audit` / CodeQL（スケジュール）
* [ ] キャッシュと成果物保持が設定済み
* [ ] シークレットは最小（`GITHUB_TOKEN`）、本番鍵は CI に投入しない
* [ ] ブランチ保護（必須ステータスチェック）
* [ ] フレーク対策（Clock DI / heartbeat 短縮 / タイムアウト）
* [ ] 変更検知（paths-filter）で不要ジョブが走らない

---

## 15. 将来拡張（任意）

* **Docker/ghcr**：`docker/build-push-action` で `ghcr.io/<org>/twi-overlay:<tag>` を公開。
* **SBOM**：`cyclonedx-gomod` / `cyclonedx-npm` / `cargo auditable` で SBOM/再現可能ビルド。
* **署名**：`sigstore/cosign` によるアーティファクト署名。
* **Perf ジョブ**：ナイトリーで `vegeta/k6` を実行し、p95 指標を履歴管理。

---

この CI/CD 設計に従えば、**小さく安全にマージ → 自動検証 → 再現可能な配布物**のサイクルが確立します。矛盾や不足が判明した場合は **本章を先に更新**し、関連章（`07/08/10` など）と整合を取ってから変更してください。
