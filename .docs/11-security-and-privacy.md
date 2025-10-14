# `.docs/11-security-and-privacy.md` — セキュリティ / プライバシ規範

> 本章は本システム（EventSub 受信〜SSE 配信〜管理 UI）の**セキュリティ / プライバシ要件の一次規範**です。
> 実装・運用は本章の **MUST / SHOULD** を満たすこと。矛盾があれば本章を更新し、関連文書（`02/03/04/05/07/10`）と整合させてください。

---

## 1. 脅威モデル / 信頼境界

### 1.1 主要資産

* **資格情報**：Twitch OAuth（access/refresh）、SSE 署名鍵、Client Secret。
* **個人データ（PII, 低機微）**：表示名、ログイン名、アバター URL、リワード入力文字列。
* **業務データ**：Queue/Counters/Settings、CommandLog（72h）、EventRaw（72h）。
* **可用性**：Webhook 即時 ACK、SSE 低遅延継続。

### 1.2 攻撃面

* **Webhook**：偽装通知、リプレイ、遅延による revoke、DoS。
* **管理 UI / REST**：認可不備、CSRF、XSS、ブルートフォース。
* **オーバーレイ**：XSS、外部メディア経由の読み込み（画像/音声）。
* **SSE**：トークン漏洩、接続濫用、リング枯渇。
* **OAuth/Helix**：トークン失効・漏洩、過剰スコープ。
* **サプライチェーン**：依存ライブラリの脆弱性、悪性パッケージ。

### 1.3 信頼境界

* **外部**：Twitch（EventSub/Helix）、配信者・モデレーターのブラウザ/OBS。
* **境界**：Nginx（TLS 終端）／Rust App（Axum）／SQLite（ローカル）。
* **内部**：App 内部の非同期チャネル（Tap/SSE/CommandBus）。

---

## 2. 認証・認可（RBAC / セッション / トークン）

### 2.1 RBAC（**MUST**）

* 役割：`superadmin` / `broadcaster` / `operator`。
* すべての API/SSE は **`broadcaster_id` の文脈**で評価（`03` 参照）。
* SSE トークンの `sub` は **`broadcaster_id`**、`aud ∈ {"overlay","admin"}`。

### 2.2 管理 UI 認証（**MUST**）

* セッション Cookie（`Secure; HttpOnly; SameSite=Lax`）または Bearer（内部利用）を採用。
* **パスワード保存**は **Argon2id**（推奨）で `salt` + メモリコスト十分（≥64MiB 相当）。（※ライブラリ標準推奨値に追従）
* **ログイン試行**の**レート制限**（IP/アカウント）を実装（`429`）。

### 2.3 CSRF（**MUST**）

* 状態変更 API は **POST** + **CSRF トークン**（同一オリジン Cookie + ヘッダ または Double Submit）。
* **SameSite=Lax** を既定。外部オリジンからのフォーム送信を無効化。

### 2.4 SSE 認可トークン（**MUST**）

* 形式：JWT or PASETO（HS256 / EdDSA 等）。
* **claims**：`sub=broadcaster_id`, `aud ∈ {overlay,admin}`, `exp`（5〜15 分）, `iat`, `nbf`。
* **伝達**：EventSource の制約により、**クエリ `token=`** か **同一オリジン Cookie**。
* **保存禁止**：トークンを **localStorage/sessionStorage に保存しない**。URL のクエリは **表示後ただちに履歴置換**（`history.replaceState`）。
* **失効**：サーバ側で `exp` 検証、失効後は**再接続時に 401/403** を返し UI が再取得。

### 2.5 OAuth（**MUST**）

* **Confidential クライアント**としてサーバでトークン管理。
* **スコープ最小化**：必要最小の EventSub/Helix スコープのみ。
* **/oauth2/validate** を起動時＋定期で実行。401 は **refresh**、失敗は**再同意**導線。
* **トークン保存**：`refresh_token` は**暗号化ストア**（OS/ファイル権限 0600 + 将来は KMS/SOPS を検討）。

---

## 3. 送信路 / 終端セキュリティ

### 3.1 TLS（**MUST**）

* **prod は HTTPS のみ**（Nginx で TLS 終端）。
* **HSTS**：`Strict-Transport-Security: max-age=31536000`。
* モダン暗号スイート（OS 既定・更新追従）、TLS1.2+。

### 3.2 HTTP セキュリティヘッダ

* **Admin**（**MUST**）：

  * `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src https: data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'`
  * `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Permissions-Policy: geolocation=(), microphone=(), camera=()`
* **Overlay**（**MUST**）：

  * `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src https: data:; media-src 'self'; connect-src 'self'; frame-ancestors 'none'`
  * クリック操作不要のためポインタイベント無効（CSS）。
* **CORS**：**同一オリジン**前提。dev は Vite プロキシで合わせる。

---

## 4. 入力検証 / 出力エスケープ

### 4.1 Webhook（**MUST**）

* HMAC-SHA256（`Message-Id || Timestamp || RawBody`）を**定数時間比較**。
* `Timestamp` は **±10 分**以内。
* `Message-Id` の**一意**（重複は 204）。
* `client_max_body_size` を 256 KB 程度に制限（Nginx）。
* IP アロウリストは **不可**（Twitch 公開レンジは非固定想定）。**HMAC が一次防御**。

### 4.2 管理 API（**MUST**）

* JSON **スキーマ検証**（列挙・長さ・パターン・範囲）。
* `op_id` は **UUID**。重複時の挙動（同内容=200 / 矛盾=412）を固定。
* すべての入力文字列は**最大長**を強制（例：表示名 64、自由入力 256）。

### 4.3 オーバーレイ（**MUST**）

* **`textContent` で挿入**し、**innerHTML を使わない**。
* ユーザ入力（リワード入力文字列など）を**ホワイトリスト整形**（改行→`<br>` 相当の安全レンダリング等が必要なら信頼済みライブラリを挿入時に使用／既定はプレーンテキスト）。
* 画像/音声は **同一オリジン or https: のみ**。

---

## 5. データ分類 / 最小化 / 保持

### 5.1 区分

* **Secret**：Client Secret、SSE 署名鍵、access/refresh トークン（**暗号化**＋0600）。
* **PII（低機微）**：表示名・ログイン名・アバター URL・入力文字列。
* **運用**：EventRaw（72h）・CommandLog（72h）・Queue/Counters/Settings（永続）。

### 5.2 最小化（**MUST**）

* API 応答・SSE には**必要最小**の PII のみ（表示目的）。
* `/_debug/*` は**既定マスク**（後述）を適用。

### 5.3 保持（**MUST**）

* `05` 規範どおり：EventRaw/CommandLog **72h TTL**、Queue/Counters/Settings は**永続**。
* バックアップ（`10` 参照）：DB は 1 日 1 回、7〜14 世代。

### 5.4 消去要求（任意）

* 配信者からの削除要請（例：特定 `user_id` の履歴マスキング）は **Queue 履歴にマスクフラグ**を導入し、SSE/REST から**不可視**にできる設計（将来）。
* EventRaw/CommandLog は TTL 循環で自然削除。

---

## 6. ログ / Tap / キャプチャの機微情報マスク

### 6.1 マスク規約（**MUST**）

* **完全マスク**：`access_token`, `refresh_token`, `Authorization`, `client_secret`, SSE トークン。
* **部分マスク**：`user_login`, `display_name` → 例 `"A***e"`（先頭/末尾のみ可視）。
* **長文トリム**：payload は 64 KiB で打切り（`truncated=true`）。
* Tap/Capture は**本番では管理者のみ**、IP レート制限推奨（`10` 参照）。

### 6.2 監査ログ（**SHOULD**）

* 管理操作（COMPLETE/UNDO/Settings 更新）：`who`（内部ユーザ ID）・`when`・`op_id` を構造化ログへ。
* 保持は OS ログローテーション規約に従い 30–90 日（DB 保持とは分離）。

---

## 7. SSE 可用性 / 濫用対策

* **リング再送**：直近 **N=1000 または 2 分**（大きい方）（**MUST**）。
* **心拍**：20–30 秒（**MUST**）。
* **クライアント制限**（**SHOULD**）：`sse_clients` をメトリクス監視、上限に達したら**新規接続を間引く**か**低優先ドロップ**。
* **1 接続/1 ブロードキャスタ** の制限（任意）。
* **バックプレッシャ**：ブロードキャストは**非同期**。遅延クライアントは**個別キュー**で切り離し。

---

## 8. OAuth / Helix 安全運用

* **最小権限**（**MUST**）：必要スコープのみ。
* **Redemption 更新の制約**：**「自アプリ作成の Reward のみ」**更新可。対象外は `managed=false` で**記録のみ**（`03` 参照）。
* **失効検知**：`/oauth2/validate` 周期、失敗で refresh → それも失敗なら管理 UI に**再同意**導線。
* **トークン回収**：漏洩が疑われる場合、**全 SSE 署名鍵のローテーション** + OAuth の再同意を強制。

---

## 9. 秘密情報管理 / ローテーション

* **保存**：`/etc/twi-overlay/env`（0600）。Git 管理禁止。
* **SSE 署名鍵ローテーション**（**SHOULD**）：

  * `kid`（鍵 ID）付きトークンを採用し、**旧新併用期間**（例：24h）を設ける。
  * 短寿命トークン（5–15 分）により漏洩面積を最小化。
* **Client Secret/OAuth リフレッシュ**（**MUST**）：疑い時は即時ローテーション → `/oauth2/validate` 監視で逸脱検知。

---

## 10. DoS / レート制御

* **Webhook**：Nginx `client_max_body_size 256k`、`proxy_read_timeout 10s`、アプリ側で**即 204**（重処理後段）。
* **管理 API**：IP / アカウント単位の**レートリミット**、失敗回数アラート。
* **`/_debug/*`**：**管理者のみ** + レート制限 + 可能なら IP 制限。
* **TTL/WAL**：小分け削除で**長時間ロック回避**（`05/10` 参照）。

---

## 11. XSS / クリックジャッキング / CSP

* **Admin/Overlay** ともに **CSP** を強制（上 §3.2）。
* **Inline Script 禁止**（ビルド済み JS のみ）。
* **X-Frame-Options: DENY** / `frame-ancestors 'none'`（OBS はトップロードなので支障なし）。
* **テンプレート挿入**は `textContent`。必要時のみ信頼済みサニタイザ（DOMPurify 等）を**限定導入**。
* **ファイル名ハッシュ**（Vite 既定）でキャッシュポイズニング抑止。

---

## 12. サプライチェーン / ビルドの安全

* **Rust**：`Cargo.lock` 固定、`cargo audit` / `cargo-deny` を CI（**MUST**）。
* **Node**：`package-lock.json` 固定、`npm audit --omit=dev` を CI（**MUST**）。
* **依存更新**：Dependabot/ Renovate を有効化（**SHOULD**）。
* **署名**：将来的にリリースアーティファクトへ **Sigstore/cosign** を検討（任意）。
* **CI**：PR による変更のみを許可、`main` へ直 push 禁止、**必ず CI 緑**でマージ（`08` 参照）。

---

## 13. 運用監視（セキュリティ指標）

* **メトリクス**（`07` 参照）：

  * `eventsub_invalid_signature_total`（署名不正）
  * `oauth_validate_failures_total`（失効）
  * `sse_ring_miss_total`（欠落）
  * `db_busy_total{op}`（ロック詰まり）
  * `sse_clients{aud}`（接続数）
* **閾値アラート**（例）

  * 10 分で `invalid_signature_total > 0` → HMAC/時計/Nginx を即時点検。
  * 1 時間で `oauth_validate_failures_total > 2` → 再同意誘導。
  * `sse_ring_miss_total` が連続増 → リング増量 or `state.replace` 強制送出。

---

## 14. 事故対応（インシデント・プレイブック）

1. **検知**：アラート or 利用者報告。Tap/ログで**時系列**を把握。
2. **初動**：

   * **資格情報漏洩疑い**：SSE 鍵ローテーション → OAuth 再同意 → 旧トークン失効。
   * **Webhook 失敗**：204 即 ACK の動線確認、HMAC と NTP を修正。
   * **SSE 欠落**：リングサイズ/心拍確認、`state.replace` 適用。
3. **封じ込め**：`/_debug/*` を遮断、レート制限強化、Nginx で一時的に RPS 制御。
4. **復旧**：`/oauth2/validate` 正常化、購読再作成、Backfill。
5. **事後**：ログ保全（TTL 前に退避）、根本原因の文書化、本章・`10` の更新。

---

## 15. 開発者チェックリスト（抜粋）

* [ ] **すべての REST/SSE** が `broadcaster_id` 文脈で認可される。
* [ ] **SSE トークン**：短寿命・`sub`/`aud`/`exp` 検証・保存禁止。
* [ ] **Webhook**：HMAC/±10 分/`Message-Id` 冪等・**204 即 ACK**。
* [ ] **CSRF**：状態変更 API は CSRF 対策済み。
* [ ] **CSP/XFO**：Admin/Overlay に適切なポリシー。
* [ ] **XSS**：`textContent`、`innerHTML` 不使用（必要ならサニタイザ）。
* [ ] **PII/Secrets マスク**：ログ・Tap・Capture に**生情報が出ない**。
* [ ] **DB TTL/WAL**：72h TTL と checkpoint が動作、長期稼働でサイズ安定。
* [ ] **依存監査**：`cargo audit` / `cargo-deny` / `npm audit` が CI で緑。
* [ ] **レート制限**：ログイン/管理 API/`/_debug/*` に導入。
* [ ] **バックアップ**：日次 `.backup` と世代管理が実施されている。

---

## 16. 付録：具体的な実装ヒント

* **定数時間比較**：`subtle::ConstantTimeEq`（Rust）で HMAC を比較。
* **Argon2id**：`argon2` crate の `Params::recommended()` をベースに、512 MB VPS ではメモリコストを負荷試験の上で調整。
* **JWT/PASETO**：`jsonwebtoken`/`paseto` crate を使用。`kid` による鍵ローテーションを許容。
* **CSP の分離**：ルート毎にヘッダを出し分け（Admin と Overlay で別）。
* **Cookie**：`Secure; HttpOnly; SameSite=Lax`。管理 UI の CSRF トークンは `SameSite=Strict` の別 Cookie を利用可。
* **エンコード**：表示値は **常に** HTML エスケープ。アバター URL は `new URL()` で妥当性確認。
* **Nginx**：`proxy_buffering off`（SSE）、`client_max_body_size` 制限、HTTP/2 有効。
* **Windows/Ubuntu 差**：OS 依存 API を避け、ファイル権限はテスト時に警告ログ。

---

## 17. 受け入れチェック（本章適合）

* [ ] RBAC/認証/CSRF/SSE トークン（短寿命, `sub`/`aud`/`exp`）
* [ ] Webhook HMAC/±10 分/冪等 + 204 即 ACK
* [ ] CSP/XFO/各種ヘッダ・同一オリジン・CORS 無効
* [ ] 入力検証と出力エスケープ（innerHTML 不使用）
* [ ] データ最小化・72h TTL・バックアップ
* [ ] ログ/Tap/Capture の機微マスク
* [ ] レート制限・DoS 対策・SSE リング/心拍
* [ ] 依存監査・CI ポリシー
* [ ] インシデント手順・鍵/トークンローテーション計画

---

本章は**規範**です。実装・運用の変更や新しい知見に基づき、**事実に合わせて更新**し、関連文書との整合性を保ってください。
