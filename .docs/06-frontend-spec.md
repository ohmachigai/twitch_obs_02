# `.docs/06-frontend-spec.md` — フロントエンド仕様（OBS オーバーレイ / 管理 UI）

> 本章はフロントエンド（**OBS オーバーレイ**および**管理 UI（最小）**）の**一次仕様**です。
> 実装は本章の**契約（URL・テーマ・初期化・SSE 運用・パッチ適用・可観測・セキュリティ）**に適合しなければなりません（**MUST**）。
> 用語は `03-domain-model.md`、API は `04-api-contracts.md` を参照。

---

## 1. 目的 / 対象

* **目的**：配信者ごとの状態（Queue / “今日の回数” / Settings）を **初期 REST** + **増分 SSE** で表示し、**デザイン切替**と**再読込冪等**を保証する。
* **対象**：

  * **OBS オーバーレイ**（`web/overlay`）
  * **管理 UI（最小）**（`web/admin`：フォーム + イベントビュー）

---

## 2. 実行環境（前提）

* **OBS Browser Source（CEF）** / Chromium 相当。モダン Web API（ESM / CSS 変数 / EventSource）利用可。
* **解像度**：OBS のソースサイズに追従（レスポンシブ）。
* **オーディオ**：CEF では自動再生許可が期待できるが、**音量は 0〜1**で調整可能にする（SHOULD）。

---

## 3. URL 契約（オーバーレイ）

**ベース**：`/overlay/index.html?`（Vite 本番ビルド後は `/overlay/`）

| key             | 必須      | 例                               | 説明                                            |
| --------------- | ------- | ------------------------------- | --------------------------------------------- |
| `broadcaster`   | **Yes** | `b-123`                         | 内部 `broadcaster_id`                           |
| `token`         | No      | `eyJ...`                        | SSE 認可トークン（クエリ渡し時のみ）。Cookie 利用実装なら省略可。        |
| `scope`         | No      | `session`（既定） / `since`         | 初期 REST の対象範囲                                 |
| `since`         | No      | `2025-10-12T00:00:00Z`          | `scope=since` の場合に使用                          |
| `since_version` | No      | `12345`                         | **SSE 初回のみ**の巻き戻し起点（`Last-Event-ID` は再接続時に自動） |
| `types`         | No      | `queue,counter,settings,stream` | SSE 配信タイプの粗いフィルタ                              |
| `theme`         | No      | `neon`                          | テーマ名（下 §5）                                    |
| `variant`       | No      | `compact`                       | テーマ内バリアント                                     |
| `accent`        | No      | `%23ff66cc`                     | `#` は URL エンコード（`%23`）                        |
| `group_size`    | No      | `6`                             | 表示グループ粒度（フロント表現）                              |
| `lang`          | No      | `ja` / `en`                     | 表示言語（簡易）                                      |
| `debug`         | No      | `1` / `tap`                     | 受信ログ HUD（`1`）/ Tap ビュー（`tap`）                 |

> **規範**：`broadcaster` が欠ける場合は**即 400 表示**（エラーパネル）。`token` は**保存しない**（MUST）。

---

## 4. 初期化フロー（オーバーレイ）

**手順（MUST）**：

1. **クエリ解析** → 入力検証（`broadcaster` 必須、`group_size` は [1..20] など範囲）
2. **テーマロード**（§5） → CSS/JSON を適用
3. **初期 REST**：`GET /api/state?broadcaster=...&scope=...&since=...`

   * レスポンス `state.version` を `lastAppliedVersion` に保存
   * `queue` / `counters_today` / `settings` を UI に反映（`ORDER BY today_count, enqueued_at` はサーバ規範だが、クライアントでも崩さない）
4. **SSE 接続**：`GET /overlay/sse?broadcaster=...&since_version=<V>&types=...&token=...`

   * `<V>` は次の優先で決定：URL `since_version` ＞ `localStorage("overlay:lastVersion:<b>")` ＞ `state.version`
   * `EventSource` の `onerror` で接続状態を HUD（赤/黄/緑）表示（SHOULD）
5. **パッチ適用**：到着順に `id=version` を**厳格増分**で適用 → `lastAppliedVersion` を更新 → `localStorage` に保存（`overlay:lastVersion:<broadcaster>`）
6. **心拍**：`:heartbeat` は UI には表示しないが**接続継続の指標**として記録（SHOULD）

> **再読込**：`localStorage` の `lastVersion` により **SSE を `since_version=...` で起動**可能（MUST）。

---

## 5. テーマパック仕様（フロントのみで切替可能）

### 5.1 配置

```
web/overlay/themes/<theme_name>/
  ├─ theme.css
  ├─ theme.json
  └─ preview.png (任意)
```

### 5.2 `theme.json` スキーマ（規範）

```json
{
  "name": "neon",
  "variant_default": "default",
  "tokens": {
    "color_bg": "#00000000",
    "color_surface": "#0b0b0fcc",
    "color_text": "#ffffff",
    "color_accent": "#ff66cc",
    "radius": "12px",
    "spacing": "8px",
    "font_family": "Noto Sans JP, system-ui, sans-serif",
    "shadow": "0 8px 28px rgba(0,0,0,.35)",
    "motion_ms": 280
  },
  "variants": {
    "default": {},
    "compact": { "spacing": "4px", "motion_ms": 180 }
  },
  "sounds": {
    "queue_enqueued": "/overlay/assets/snd/enqueue.mp3",
    "queue_completed": "/overlay/assets/snd/complete.mp3",
    "queue_removed": "/overlay/assets/snd/remove.mp3"
  },
  "images": {
    "bg": "/overlay/assets/img/bg_neon.png"
  }
}
```

* **適用**：`theme.css` は **CSS 変数**（例：`--color-accent`）を定義（MUST）。
* `theme.json.tokens` は CSS 変数へ展開（`--color-bg`, `--radius`, …）。
* **アクセント**：URL `accent` が指定されたら `--color-accent` を上書き（SHOULD）。
* **バリアント**：`variant` クエリが `variants` に存在すれば、`tokens` を**浅い上書き**（MUST）。
* **サウンド**：各パッチ到着時に再生（**mute 設定**で抑制可）。OBS CEF では原則自動可。

---

## 6. DOM / コンポーネント指針（オーバーレイ）

### 6.1 ルート構造（規範）

```html
<div id="app" data-theme="neon" data-variant="compact">
  <div id="layer-bg" aria-hidden="true"></div>
  <main id="overlay" role="region" aria-label="Participants queue">
    <ol id="queue" class="queue list"></ol>
  </main>
  <div id="hud" class="debug hidden" aria-live="polite"></div>
</div>
```

* `#queue`：**現在の待機**（`status=QUEUED`）のみ表示（MUST）。
* 項目 DOM 例：

```html
<li class="queue-item enter" data-entry-id="01HZX..." data-user-id="u-42" style="--order:0">
  <img class="avatar" src="..." alt="" />
  <span class="name">Alice</span>
  <span class="meta">x3</span> <!-- 今日の回数 -->
</li>
```

* **アニメーション**：`.enter` / `.leave`（CSS の `transform/opacity`）。`prefers-reduced-motion` 尊重（MUST）。
* **グループ表示**（§8）：`group_size` に従い `li` を**視覚的**に N 件ずつブロック化（構造は単純なまま）。

---

## 7. クライアント状態とパッチ適用（厳密）

### 7.1 クライアント状態（MUST）

```ts
interface ClientState {
  version: number;                     // lastAppliedVersion
  queue: Map<entry_id, QueueItem>;     // 表示中アイテム（QUEUED）
  counters: Map<user_id, number>;      // 今日の回数
  settings: Settings;                  // 表示に影響
}
```

* **順序**：表示は常に `today_count ASC, enqueued_at ASC` を遵守（サーバ順を維持、必要なら再ソート）。
* **永続**：`version` は `localStorage("overlay:lastVersion:<b>")` に保存（MUST）。他は揮発。

### 7.2 パッチ適用規約（MUST）

* **増分整合**：**受信 `data.version` は `state.version + 1`** であること（MUST）。

  * それ以外（欠落/逆順）の場合は**適用せず**、**`state.replace`** を待つか、**自ら再同期**を要求（SHOULD）。

* **型別適用**：

  * `queue.enqueued`：`queue` へ挿入、`counters[user] = data.user_today_count` を同期。
  * `queue.removed`：該当 `entry` を削除、`counters[user]` を `data.user_today_count` に同期。
  * `queue.completed`：該当 `entry` を削除（`counter` は変更なし）。
  * `counter.updated`：`counters[user] = count`。
  * `settings.updated`：`settings` をマージ、必要なら UI 再レンダ。
  * `stream.online/offline`：HUD に反映（任意、**状態計算は Projector に従う**）。
  * `state.replace`：**全置換**。`state.version = data.state.version`、`queue/counters/settings` を置換。

* **適用完了**：`state.version = patch.version` → `localStorage` に保存。

---

## 8. グループ化表示（任意 / 表現）

* **目的**：`group_size=n` で視覚上のブロックを作り、`n` 人単位で「まとまり」を表現。
* **規範**：**データ構造は変えない**（QueueEntry はフラット）。DOM で `li` を `n` 件ずつ `div.group` にラップしてもよい。
* **再構成**：パッチ適用後、**差分更新**で DOM を最小変更（MUST）。
* **アクセシビリティ**：`aria-label="Group k"` を付与（SHOULD）。

---

## 9. 言語・表記

* `lang` クエリ（`ja`/`en`）または `navigator.language` を初期値にする（SHOULD）。
* 文言は `i18n/<lang>.json` で与える（簡易キー辞書）。未定義は英語フォールバック。

---

## 10. エラーパネル / 接続状態

* **初期 REST 失敗**：`problem+json` の内容をパネル表示（`title/detail`）。
* **SSE**：

  * `open`：HUD “LIVE”（緑）
  * `error`：HUD “RECONNECTING”（黄→赤）、再接続試行（EventSource 標準の振る舞い）
  * 一定時間（例：60s）無心拍の場合、警告表示（SHOULD）

---

## 11. デバッグ（オーバーレイ）

* `debug=1`：受信パッチ（型/バイト数/適用時間 ms）を HUD に行追加。
* `debug=tap`：**別の** EventSource で `/_debug/tap?s=projector,sse&broadcaster=...` を受信し、パイプラインの抜粋を HUD に表示（本番では使用しない）。

---

## 12. セキュリティ / プライバシ

* **トークン**（クエリ `token`）は **ログ・localStorage・URL 再書き**に残さない（MUST）。
* **PII**（ユーザ名・アイコン）は**画面表示のみ**で用い、**コンソールログに出さない**（MUST）。
* **クリックジャッキング**：オーバーレイは `pointer-events: none` 既定（UI 操作不要）。管理 UI 上では通常イベント。

---

## 13. 管理 UI（最小仕様）

* **目的**：手動操作（COMPLETE/UNDO / Settings 更新）と**可視化**（イベント・現在キュー）。

* **構成**：

  * `GET /admin/index.html?broadcaster=...`
  * 左：**操作パネル**（フォーム）

    * COMPLETE：`entry_id` 入力 → `POST /api/queue/dequeue { mode:"COMPLETE", op_id }`
    * UNDO：同（`mode:"UNDO"`）
    * Settings 更新：`patch` JSON 入力 → `POST /api/settings/update`
  * 右：**ビュー**

    * `GET /api/state` の内容（queue/counters/settings）
    * `GET /admin/sse` のパッチ反映

* **要件**：

  * `op_id` はクライアントで UUID 生成（MUST）。
  * 送信前に**簡易バリデーション**（`entry_id` 形式など）
  * 応答・エラーを **toast** 表示。
  * フレームワークは htmx + 少量 JS で実装（自由だが**契約は固定**）。

---

## 14. パフォーマンス / 安定性

* **DOM 更新**はパッチごとに **1 フレームにバッチ**（`requestAnimationFrame`）で適用（SHOULD）。
* **画像**：アバターは `loading="lazy"`（SHOULD）。
* **アニメーション**：GPU 友好プロパティ（`transform/opacity`）のみを使用（MUST）。
* **メモリ**：`queue` は QUEUED のみ保持（履歴は保持しない）。巨大化防止に**最大可視 200**件などの上限を UI に設けてもよい（SHOULD）。

---

## 15. キャッシュ / バージョニング

* **静的資産**（JS/CSS/画像/音）：ビルド時に**ファイル名にハッシュ**を付与（Vite 既定、MUST）。
* **オーバーレイ URL**：`?v=YYYYMMDDhhmm` の**手動バージョン**を付ける運用も可（OBS キャッシュ抜け対策、SHOULD）。

---

## 16. テスト（フロント）

* **ユニット**：パッチ適用ロジック（`state + patch -> state'`）の純粋関数化とテスト（MUST）。
* **結合**：モックサーバ（SSE）で初期 REST → SSE 受信 → DOM 反映が成立すること（SHOULD）。
* **E2E**：CI ではヘッドレスで最小シナリオ（enqueue → complete → undo）を確認（任意）。

---

## 17. 受け入れチェック（本章適合）

* [ ] クエリ契約（`broadcaster` 必須、`token` 非保存、`since_version` 初回のみ）
* [ ] 初期 REST → SSE（`id=version`、心拍、リング再送前提）
* [ ] `localStorage("overlay:lastVersion:<b>")` による再読込冪等
* [ ] テーマパック（`theme.css` + `theme.json`）の適用、`accent` 上書き
* [ ] パッチ適用：厳密増分（`state.version + 1`）規約、`state.replace` フォールバック
* [ ] グループ化は**表現のみ**（データ構造を変更しない）
* [ ] デバッグ HUD（`debug=1|tap`）、コンソールに機微情報を出さない
* [ ] 管理 UI：COMPLETE/UNDO/Settings、`op_id` 冪等、SSE 反映
* [ ] アクセシビリティ（`prefers-reduced-motion`、代替テキスト、aria）
* [ ] パフォーマンス（1フレームバッチ、lazy 画像、transform/opacity）

---

### 付録 A：イベント→UI 反映マッピング（抜粋）

| パッチ                     | UI 反映                                                         |
| ----------------------- | ------------------------------------------------------------- |
| `queue.enqueued`        | 末尾に `li` 追加 → 再ソート（`today_count, enqueued_at`） → `.enter` アニメ |
| `queue.removed`         | 対象 `li` をフェードアウト `.leave` → `animationend` で削除                |
| `queue.completed`       | 同上（理由は UI では区別しなくてよい／テーマで色分け可）                                |
| `counter.updated`       | 対象ユーザの `meta` を即時更新、並び順再評価                                    |
| `settings.updated`      | `group_size` やテーマ適用値を再計算、必要なら DOM 再構成                         |
| `state.replace`         | `queue/counters/settings` 全置換、`version` 更新、DOM リビルド           |
| `stream.online/offline` | HUD 表示（セッション境界の通知、任意）                                         |

---

本章に矛盾が見つかった場合は、**先に本章を更新**し、関連文書（`02/03/04/05/07`）を整合させたうえで実装を変更してください。
