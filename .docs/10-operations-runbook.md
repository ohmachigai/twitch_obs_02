# `.docs/10-operations-runbook.md` — 運用ランブック（規範 + 実務手順）

> 本章は **本番/検証環境の構築・運用・保守・障害対応**を行うための一次資料です。
> ここに記す **前提・構成・Nginx/systemd/Secrets/デプロイ/DB 運用/可観測性/インシデント対応** は拘束条件（**MUST**）です。
> 仕様は `01–09` を前提とします（特に `02` アーキテクチャ、`04` API、`05` データ、`07` 可観測）。

---

## 1. 運用プロファイルと前提

### 1.1 環境プロファイル

* **dev**：ローカル開発。Nginx 不要。HTTP 直結。
* **stg**（任意）：本番相当の疎通確認。
* **prod**：**HTTPS（TLS）+ Nginx** で公開。**ポート 443 必須**（EventSub Webhook）。
  * `.env` の `APP_ENV` は `production` を指定する（ロギング JSON / Tap モック停止）。

### 1.2 前提（VPS 512 MB〜1 GB）

* OS：Ubuntu 22.04 LTS など安定版。
* ユーザ：専用の **非 root system user** を作成（例：`overlay`）。
* 時刻：NTP 有効（**MUST**）。**±10分**超の時計ずれは Webhook 拒否の原因。
* 永続ストレージ：`/var/lib/twi-overlay`（SQLite/asset）。
* ネットワーク：外向き（HTTPS）・内向き（443→Nginx, 8080→アプリ）を許可。
* Swap：512 MB 程度のスワップを推奨（512 MB RAM プランの OOM 回避）。

---

## 2. コンポーネント構成（prod）

```
[Client(OBS, Admin)] --HTTPS--> [Nginx] --HTTP--> [Rust app (Axum)]
                                          |
                                      [SQLite(WAL)]
```

* Nginx：TLS 終端、SSE の**バッファ無効**、/metrics は内部のみ公開。
* Rust app：`/eventsub/webhook`, `/overlay/sse`, `/api/*`, `/_debug/*`, `/metrics`, `/healthz`。
* SQLite：WAL 有効。TTL ジョブと checkpoint をアプリが実行。

---

## 3. ディレクトリ／ファイル配置（推奨）

```
/etc/twi-overlay/
  env                 # 環境変数 (0600)
  nginx-site.conf     # Nginx サイト設定 (有効化先は /etc/nginx/sites-enabled)
  tls/                # 取得済み証明書 (Let’s Encrypt など)

 /var/lib/twi-overlay/
  data/app.db         # SQLite DB
  logs/               # アプリ出力 (journald を推奨し、ここは任意)

 /opt/twi-overlay/
  current/            # 展開ディレクトリ (bin/static)
  releases/<ts>/      # ロールバック用保持
```

---

## 4. Secrets / 設定（**MUST**）

`/etc/twi-overlay/env`（0600, 所有 `overlay:overlay`）

```dotenv
APP_ENV=production
APP_BASE_URL=https://overlay.example.com
RUST_LOG=info

# Twitch OAuth
TWITCH_CLIENT_ID=...
TWITCH_CLIENT_SECRET=...
OAUTH_REDIRECT_URI=https://overlay.example.com/oauth/callback

# EventSub Webhook
WEBHOOK_SECRET=32+bytes-random-hex

# SSE 認可トークン署名鍵（短寿命）
SSE_TOKEN_SIGNING_KEY=32+bytes-random-hex

# SQLite
DATABASE_URL=sqlite:///var/lib/twi-overlay/data/app.db

# Optional: Heartbeat 間隔やリングサイズのチューニング
SSE_HEARTBEAT_SECS=25
SSE_RING_MAX=1000
```

> **規範**：Secrets は **Git 未管理**・**0600**・**journald/ログへ出さない**。

---

## 5. Nginx 設定（**SSE バッファ無効は MUST**）

`/etc/twi-overlay/nginx-site.conf`

```nginx
server {
  listen 443 ssl http2;
  server_name overlay.example.com;

  # TLS (Let’s Encrypt 等で取得)
  ssl_certificate     /etc/letsencrypt/live/overlay.example.com/fullchain.pem;
  ssl_certificate_key /etc/letsencrypt/live/overlay.example.com/privkey.pem;
  add_header Strict-Transport-Security "max-age=31536000" always;

  # Common proxy headers
  proxy_set_header Host $host;
  proxy_set_header X-Forwarded-Proto $scheme;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;

  # SSE (OBS overlay, admin)
  location ~ ^/(overlay|admin)/sse$ {
    proxy_pass         http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header   Connection "";    # keep-alive
    proxy_buffering    off;              # ★重要：SSE バッファ禁止
    proxy_cache        off;
    proxy_read_timeout 3600s;            # 長い待受
  }

  # Webhook: 即時 ACK, ボディ小
  location /eventsub/webhook {
    client_max_body_size 256k;
    proxy_pass           http://127.0.0.1:8080;
    proxy_read_timeout   10s;
  }

  # API / 静的
  location / {
    proxy_pass http://127.0.0.1:8080;
  }
}

# HTTP→HTTPS リダイレクト
server {
  listen 80;
  server_name overlay.example.com;
  return 301 https://$host$request_uri;
}
```

> **注意**：`proxy_buffering off;` を忘れると SSE が**詰まります**。`proxy_set_header Connection "";` で HTTP/1.1 keep-alive 維持。

---

## 6. systemd ユニット（自動復旧）

`/etc/systemd/system/twi-overlay.service`

```ini
[Unit]
Description=Twitch Overlay & Event Relay
After=network-online.target
Wants=network-online.target

[Service]
User=overlay
Group=overlay
EnvironmentFile=/etc/twi-overlay/env
WorkingDirectory=/opt/twi-overlay/current
ExecStart=/opt/twi-overlay/current/bin/twi-overlay-app
Restart=always
RestartSec=2
StartLimitIntervalSec=30
StartLimitBurst=10
# ファイルディスクリプタ上限（SSE 多接続向け）
LimitNOFILE=65535
# セキュリティ強化（必要に応じて）
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=true

[Install]
WantedBy=multi-user.target
```

> **規範**：`Restart=always`。NTP/ネットワーク依存は `network-online.target`。`LimitNOFILE` は 512MB VPS でも余裕を確保。

---

## 7. 初回セットアップ手順（prod, Ubuntu 例）

```bash
# 1) OS 準備
sudo adduser --system --home /opt/twi-overlay --group overlay
sudo mkdir -p /var/lib/twi-overlay/data /etc/twi-overlay /opt/twi-overlay/releases
sudo chown -R overlay:overlay /var/lib/twi-overlay /opt/twi-overlay
sudo apt update && sudo apt install -y nginx sqlite3

# 2) Secrets / env
sudoedit /etc/twi-overlay/env   # 上記サンプルをベースに投入（0600）

# 3) Nginx
sudo cp /etc/twi-overlay/nginx-site.conf /etc/nginx/sites-available/twi-overlay.conf
sudo ln -s /etc/nginx/sites-available/twi-overlay.conf /etc/nginx/sites-enabled/twi-overlay.conf
sudo nginx -t && sudo systemctl reload nginx

# 4) バイナリ配置（アーティファクト展開）
sudo mkdir -p /opt/twi-overlay/releases/$(date +%Y%m%d%H%M%S)
sudo tar -C /opt/twi-overlay/releases/<ts> -xzf twi-overlay-artifacts.tar.gz
sudo ln -sfn /opt/twi-overlay/releases/<ts> /opt/twi-overlay/current
sudo chown -R overlay:overlay /opt/twi-overlay

# 5) DB 初期化
sudo -u overlay /opt/twi-overlay/current/bin/sqlx migrate run

# 6) 起動
sudo systemctl daemon-reload
sudo systemctl enable --now twi-overlay.service
curl -f https://overlay.example.com/healthz
```

> TLS は Let’s Encrypt（`certbot`/`acme.sh`）等で取得。自動更新後は `systemctl reload nginx`。

---

## 8. デプロイ／ロールバック

### 8.1 デプロイ（ローリング/短時間停止）

1. 新リリースを `/opt/twi-overlay/releases/<ts>` に展開。
2. `sqlx migrate run` を**サービス停止前に**実行（互換 OK の場合）。
3. `ln -sfn` で `current` を切替。
4. `systemctl restart twi-overlay`。
5. `/healthz` 200、`/_debug/tap`（dev）で心拍確認。

> **API/DB 変更**がある場合は **ロールフォワード原則**（`05` §10）。必要ならメンテナンス窓。

### 8.2 ロールバック

1. 直前の `<ts>` へ `current` を戻す。
2. `systemctl restart twi-overlay`。
3. 破壊的マイグレーションがある場合は**巻き戻し不可**。新マイグレーションで吸収（ロールフォワード）。

---

## 9. EventSub 購読・OAuth 運用

* 初回：配信者が `/oauth/login` → `/oauth/callback`。
* アプリ（または `scripts/make-subscriptions.sh`）で **App Access Token** により購読作成。
* 失効（revocation）／通知失敗過多は**自動再購読**（ログ/メトリクスに記録）。
* **/oauth2/validate** を起動時＋定期で実行。401→**refresh**、不可→**再同意**誘導。
* **Webhook の callback URL** は `https://<domain>/eventsub/webhook`（TLS 443 必須）。

---

## 10. 可観測性（`07` 準拠）

* `GET /metrics`（Prometheus）：

  * `eventsub_ingress_total{type}` / `webhook_ack_latency_seconds`
  * `sse_clients{aud}` / `sse_broadcast_latency_seconds` / `sse_ring_miss_total`
  * `db_ttl_deleted_total{table}` / `db_checkpoint_seconds`
* `GET /healthz`：依存の軽量チェック（プロセス稼働、WAL 可能、時計ずれ閾値）。
* `/_debug/tap`：**本番は管理者のみ**。レートリミット推奨。

**アラート例（任意）**

* `sse_ring_miss_total` の短時間増分 > 0
* `eventsub_invalid_signature_total` の増加
* `oauth_validate_failures_total` の連続増加
* `webhook_ack_latency_seconds` p95 > 0.2s

---

## 11. DB 運用（SQLite, WAL）

* **PRAGMA**（接続時）：`foreign_keys=ON, journal_mode=WAL, synchronous=NORMAL, busy_timeout=5000`。
* **TTL（72h）**：`event_raw` / `command_log` を **小分け DELETE（LIMIT 1000）**（**MUST**）。
* **WAL checkpoint**：`wal_checkpoint(TRUNCATE)` を TTL の後に実行。
* **VACUUM**：**実施しないのが既定**。必要時のみメンテ窓で。
* **バックアップ**：`sqlite3 /path/app.db ".backup '/path/app-YYYYMMDD.db'"`（**MUST**）。

  * **頻度**：1 日 1 回。保存は 7〜14 世代。
  * **含意**：`event_raw`/`command_log` は 72h で消えるが、**Queue/Counters/Settings は永続**。
* **リストア**：サービス停止 → `.backup` から置換 → `sqlx migrate run` → 起動。

---

## 12. セキュリティ運用

* **防御**：

  * FW：`ufw allow 443`, `allow 80`, `deny 8080`（ローカルのみ）。
  * SSH：鍵認証、fail2ban。
  * Secrets：`/etc/twi-overlay/env`（0600）。
  * SSE トークン：**短寿命**、`aud/sub/exp` 検証を**サーバ側**で強制。
  * `/_debug/*`：管理者のみ、必要なら Basic 認証 + IP 制限 + レートリミット。
* **ログ**：`tracing` 構造化。**トークン/PII はマスク**（**MUST**）。
* **依存更新**：月次で `apt upgrade`、アプリはタグ付きリリース。

---

## 13. 定期運用タスク（チェックリスト）

* [ ] TLS 更新（Let’s Encrypt の自動更新確認、期限アラート）
* [ ] `/metrics` の主要メトリクス確認（SSE クライアント数、ACK 遅延）
* [ ] TTL ジョブと checkpoint の実行状況（件数/時間）
* [ ] `/oauth2/validate` 失敗の有無（refresh/再同意の誘導）
* [ ] ディスク容量（`/var/lib/twi-overlay`）
* [ ] バックアップの整合（`sqlite3 .recover` テストを月1で）
* [ ] NTP 正常（`timedatectl`）

---

## 14. インシデント対応（症状→原因→対処）

| 症状             | 代表原因              | 初動/対処                                                                    |
| -------------- | ----------------- | ------------------------------------------------------------------------ |
| OBS の表示が止まる    | Nginx が SSE をバッファ | `proxy_buffering off` を確認。`:heartbeat` が出ているか `/_debug/tap` で確認。         |
| Webhook revoke | 遅い ACK / HMAC 不一致 | `webhook_ack_latency_seconds` を確認。204 即時返却か、時刻（NTP）ずれ検査。自動再購読ログを追う。      |
| SSE 欠落が頻発      | リング不足 / 再送不能      | `sse_ring_miss_total` 監視。`SSE_RING_MAX` を増やす。必要なら `state.replace` を強制送出。 |
| DB が肥大         | TTL 未実行 / WAL 未切詰 | TTL ジョブ実行、`wal_checkpoint(TRUNCATE)`。古い `.db-wal` を削除しない（checkpoint 経由）。 |
| 403 on Helix   | 自アプリ作成でない Reward  | `managed=false` で記録される設計。対象 Reward を設定から除外 or ガイダンス提示。                   |
| OAuth 無効       | ユーザが連携解除          | `/oauth2/validate` → refresh 失敗 → 再同意 URL を管理画面で提示。                      |
| OOM/高メモリ       | 接続過多 / リーク        | `sse_clients` を確認。`LimitNOFILE`/プロセス上限調整、512 MB プランは swap 追加。            |
| CPU 高騰         | スパム/無限リプレイ        | `policy_commands_total` のスパイクとトレース確認。`/_debug/*` を閉じる/レート制限。             |

---

## 15. 容量/スケールの目安

* **SSE**：1 接続 ≈ 数 KB/分 + 心拍。100–300 接続でも 1 GB RAM で十分（アプリはノンブロッキング）。
* **Webhook**：数十〜数百 req/分 程度は楽勝。ACK 処理は軽量（検証のみ）。
* **DB**：`event_raw`/`command_log` は 72h で自然縮小。Queue/Counters はユーザ数に比例（小）。
* **上限感**：1 VPS で中規模配信者（数十〜百視聴者の 同時使用数）を想定。
* さらに増える場合は **分離（broadcaster ごと）** や **外部 Pub/Sub** を検討。

---

## 16. メンテナンス・手順ひな型

### 16.1 証明書更新（Let’s Encrypt）

```bash
sudo certbot renew --dry-run
sudo systemctl reload nginx
```

### 16.2 バックアップ（1 日 1 回, cron/systemd timer）

```bash
sqlite3 /var/lib/twi-overlay/data/app.db ".backup '/var/lib/twi-overlay/data/backup/app-$(date +%F).db'"
find /var/lib/twi-overlay/data/backup -type f -mtime +14 -delete
```

### 16.3 TTL / checkpoint を即時実行（手動トリガ）

* 管理 UI から「メンテ」ボタン、またはアプリの管理エンドポイント（実装時）。

---

## 17. 運用上の禁止事項（安全装置）

* `proxy_buffering on` で SSE を中継しない（**禁止**）。
* `.env` を Git 管理しない。
* `/_debug/*` を無認可で公開しない。
* SQLite を NFS/リモート FS 上で共有しない（ロック特性が異なる）。
* `VACUUM` を無計画に実行しない（長時間ロックの原因）。

---

## 18. スモークテスト（デプロイ直後 2 分手順）

1. `curl -f https://<domain>/healthz` → `200`
2. `/_debug/tap`（管理者）で心拍と `stage=sse` を確認
3. `scripts/make-subscriptions.sh` で購読棚卸し（差分 0）
4. 管理 UI で `settings.update` → SSE に `settings.updated` が出る
5. モック `redemption.add` 投入 → Overlay に `queue.enqueued` 表示

---

## 19. 付録：Windows での検証運用（任意）

* 本番は Linux 推奨。Windows ではローカル検証のみ（Nginx 不要、`cargo run` + Vite）。
* SSE/REST の疎通、`sqlite3` コマンドは WSL or Windows 用バイナリで代替。

---

## 20. 受け入れチェック（本章適合）

* [ ] NTP 正常、TLS 443、Nginx の **SSE バッファ無効**
* [ ] systemd：`Restart=always`、`LimitNOFILE`、非 root、`EnvironmentFile`
* [ ] Secrets：`.env` 外出し（0600）、PII/トークンはログに出ない
* [ ] デプロイ：`releases/<ts>` 展開→`current` 切替→ヘルス確認、ロールバック手順あり
* [ ] DB：WAL/TTL/checkpoint/backup 運用が確立
* [ ] 可観測：`/metrics`・`/healthz`・Tap（本番は認可）
* [ ] インシデント時の初動表がある（§14）
* [ ] 容量/スケール目安と拡張方針を提示

---

本ランブックは**運用の一次資料**です。現場での知見に基づき、**事実に合わせて更新**し、関連仕様（`02/05/07/08/11/12`）との整合を常に保ってください。
