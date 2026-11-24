# Spotify Lyrics Translator Server

目前已實作 **Phase 1–6**，包含後端與容器部署設定：Rust workspace、Supabase JWT 驗證的翻譯 API、Ollama Worker 與 PostgreSQL 非同步佇列。真實 Supabase 公開 JWKS 已確認相容；尚未以真實使用者 Access Token 完成登入整合測試，未進行雲端部署。

## 本機準備

- Rust stable（本機驗證：rustc 1.100.0）、Cargo。
- 已啟動的 Ollama，已有 `translategemma:4b`。不需重新下載模型。
- Docker Desktop／Docker Compose（只在啟動 PostgreSQL 時需要）。

```sh
cp .env.example .env
# 編輯 .env 後，將設定匯入目前 shell；程式不會自動讀取 .env。
set -a
. ./.env
set +a
ollama list
cargo build --workspace
cargo test --workspace
```

先用 `ollama list` 確認模型名稱，填入 `OLLAMA_MODEL`。若 Ollama 沒啟動，可啟動本機 Ollama App 或執行 `ollama serve`。程式不會自動拉取或切換模型。

## API

```sh
docker compose up -d --wait postgres
cargo run -p worker -- migrate
# .env 必須設定 SUPABASE_URL、DATABASE_URL、OLLAMA_MODEL
cargo run -p api
# 另一個終端
curl http://127.0.0.1:3000/health
# {"status":"ok"}
```

API 預設只監聽 `127.0.0.1:3000`。啟動時需連上資料庫，並具備有效的 Supabase URL 與模型設定；不需要 Ollama 連線。支援 Ctrl-C／SIGTERM graceful shutdown。`/health` 不需要登入，只表示 API 存活，不代表 DB／JWKS／Worker 持續可用。API 只建立／查詢工作，不執行推論。

## Worker 與 benchmark

`translate`／`benchmark` 是單次執行命令，成功後退出；`run` 會持續輪詢資料庫（見下方工作佇列流程）。

```sh
cargo run -p worker -- translate tests/fixtures/en.json
cargo run -p worker -- benchmark tests/fixtures/en.json
cargo run -p worker -- benchmark tests/fixtures/ja.json
cargo run -p worker -- benchmark tests/fixtures/ko.json
# 也可從 stdin 讀取
cat tests/fixtures/en.json | cargo run -p worker -- translate -
```

`translate` 的 stdout 只輸出 `{"translations":[...]}`；`benchmark` 額外輸出模型、來源語言、行數與 elapsedMs（包含 HTTP、模型載入、推論與重試，不含編譯）。stderr 僅記錄嘗試次數、耗時、分類錯誤；不記錄歌詞、譯文、HTTP response body 或連線 Secrets。stdout 是使用者要求的結果，請自行決定是否保存。

輸入格式：

```json
{
  "lyrics": ["Morning light falls on my window", "", "We find our way home"],
  "sourceLanguage": "en",
  "targetLanguage": "zh-TW"
}
```

支援 `en`、`ja`、`ko` → `zh-TW`。一次送入全部歌詞作為上下文，以 Ollama JSON Schema 限制結構；再次驗證 JSON、行數、空白行位置、單行輸出及生成是否截斷。不接受未定義欄位、內嵌換行或全空白輸入。最多 200 行、歌詞總長 16 KiB、輸入 JSON 64 KiB；無每日或累積配額。輸入不包含時間戳，Worker 只按索引回傳譯文，既有 App 的時間戳保持在客戶端，本專案未修改 Tauri。

模型設定 temperature=0、context=16384、最大輸出 8192 tokens；過長輸出會失敗而非接受截斷結果。有限次重試涵蓋連線／逾時、HTTP 429／5xx、JSON 或行數錯誤；其他 HTTP 4xx 不重試。重試間隔 250ms、500ms，依序增加。Ctrl-C／SIGTERM 取消本地等待並以非零狀態退出；遠端模型未必立即停止運算。

## 環境變數

| 變數 | 預設／用途 |
| --- | --- |
| `API_BIND` | `127.0.0.1:3000` |
| `OLLAMA_BASE_URL` | `http://localhost:11434`；不可包含帳密、query 或 fragment |
| `OLLAMA_MODEL` | 必填；本機 `translategemma:4b` |
| `OLLAMA_TIMEOUT_SECONDS` | 每次 HTTP 請求 180 秒；允許 1–600 |
| `OLLAMA_MAX_ATTEMPTS` | 含首次共 3 次；允許 1–5 |
| `POSTGRES_USER` | Compose 本機帳號 `translator` |
| `POSTGRES_PASSWORD` | 僅供本機的示例密碼 |
| `POSTGRES_DB` | Compose 資料庫 `translator` |
| `DATABASE_URL` | API 與 Worker 佇列命令必填；單次翻譯不需要 |
| `SUPABASE_URL` | API 必填，專案 HTTPS origin；JWKS 與 issuer 由此推導 |
| `TEST_DATABASE_URL` | PostgreSQL 整合測試專用；需有建立 schema 權限 |

`.env` 已被 gitignore 排除。SQLx 使用 PostgreSQL 與 Tokio Rustls；資料庫錯誤只回報分類，不輸出可能含歌詞或 Secrets 的原始錯誤。

## PostgreSQL 開發環境

```sh
docker compose config --quiet
docker compose up -d --wait postgres
docker compose exec postgres sh -c 'pg_isready -U "$POSTGRES_USER" -d "$POSTGRES_DB"'
docker compose down
```

只綁定 loopback 5432，資料保留在 `postgres_data` volume；`down` 不會刪除資料。帳密僅適用本機開發。`migrations/0001_translation_jobs.sql` 建立工作佇列；請用下方 `migrate` 命令套用。單次翻譯不寫入資料庫；佇列模式會保存輸入與結果，預設建立後 7 天清除。

## 測試

```sh
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# 明確執行真實模型測試；預設測試會略過這一項。
OLLAMA_MODEL=translategemma:4b cargo test -p worker --test ollama real_translategemma_all_languages -- --ignored --nocapture
```

自動測試涵蓋 API health／未開放的翻譯路由、輸入語言與限制、Ollama 完整上下文及 JSON Schema、行數／空白行錯誤、截斷與多行輸出、重試恢復與上限、HTTP 錯誤去敏、timeout、設定驗證。Mock 使用隨機 loopback 埠，不依賴模型或 DB。真實測試依序執行英／日／韓原創測試歌詞，檢查結構，不把生成文字當作固定 golden answer。

## 結構與後續範圍

- `crates/api`：Axum 翻譯 API、JWKS/JWT 驗證、統一錯誤與 API 測試。
- `crates/shared`：輸入／輸出型別、驗證、日誌、關閉訊號與 SQLx 佇列。
- `crates/worker`：Ollama client、重試、翻譯與 benchmark CLI、mock／真實測試。
- `tests/fixtures`：原創英／日／韓歌詞，包含空白行。
- `docker-compose.yml`：本機 PostgreSQL。

Phase 6 已加入分離 Docker images、分機 Compose 與 HTTPS 設定，操作見 [部署文件](deploy/README.md)。本次未建立雲端資源、未索取 Supabase credentials、未修改桌面 App。公開部署前需配置 HTTPS、部署權限及真實登入端到端驗證。本機 Linux ARM64 容器驗證與實際雲端 VM 效能是不同層次；後者尚未驗證。

提示格式參考 [Ollama TranslateGemma 模型說明](https://ollama.com/library/translategemma)，結構化輸出參考 [Ollama API](https://github.com/ollama/ollama/blob/main/docs/api.md)。行數與結構可驗證；翻譯語意、逐行對齊與繁體用字仍需人工品質評估。

## 本次驗證紀錄（2026-11-23，Asia/Taipei）

- `cargo build --workspace`：成功。
- `cargo test --workspace`：8 項通過；1 項真實模型測試預設 ignored。
- 額外明確執行 `real_translategemma_all_languages`：通過，涵蓋 en／ja／ko，整項測試耗時 4.32 秒（已暖機）。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`：成功。（Phase 1–2 當時的 SQLx 未使用提示已由 Phase 3 佇列實作解決。）
- 實際啟動 API、直接 HTTP GET `/health`、SIGTERM 正常退出：通過。
- `docker compose config --quiet`、`docker compose up -d --wait postgres`：通過；容器 healthy，僅綁定 `127.0.0.1:5432`。
- PostgreSQL 17.12（本機 aarch64 容器）：TCP 帳密連線、暫存表 JSONB 寫入／讀回斷言、交易回滾皆通過，未留下測試資料。容器保持啟動供本機開發；此結果不代表已驗證雲端 ARM64 Ollama／Worker。
- 本機模型：`translategemma:4b`，Ollama ID `c49d986b0764`，約 3.3 GB。

首次英文 benchmark（4 行含空白行，總耗時 **5375 ms**，一次成功）：

```json
{
  "elapsedMs": 5375,
  "lineCount": 4,
  "model": "translategemma:4b",
  "sourceLanguage": "en",
  "translations": [
    "清晨的陽光灑在我的窗戶",
    "我把昨天帶入雨中",
    "",
    "今晚我們的腳步再次找到道路"
  ]
}
```

這是本機小型原創測試資料結果，不代表長篇歌詞、併發量或雲端 VM 的效能保證；模型輸出可能因版本與執行環境而變動。


## Phase 3：非同步工作佇列

先啟動本機 PostgreSQL 並匯入 `.env`，再執行：

```sh
cargo run -p worker -- migrate
cargo run -p worker -- enqueue 00000000-0000-0000-0000-000000000001 tests/fixtures/en.json
# 另一個終端匯入同一份 .env 後，持續執行 Worker
cargo run -p worker -- run
# 將 JOB_UUID 換成 enqueue 回傳的 jobId
cargo run -p worker -- status 00000000-0000-0000-0000-000000000001 JOB_UUID
# 手動觸發一次清理（最多 1000 筆）
cargo run -p worker -- cleanup
```

`enqueue`／`status` 是可信任本機管理者的測試 CLI，可指定 user UUID，**不代表已完成登入驗證**。HTTP API 已只使用驗證後的 JWT sub，不能接受客戶端自填 user ID。資料表啟用 RLS 且沒有前端存取 policy；目前使用本機 DB 擁有者執行，未配置 Supabase 權限。

- 工作狀態：pending → processing → completed；推論失敗回到 pending，第 3 次失敗後為 failed。
- `FOR UPDATE SKIP LOCKED` 與 UPDATE 放在同一 SQL statement，原子領取；只領取相同模型與 prompt 版本。
- 每次領取最多處理 600 秒，租約 630 秒，最多領取 3 次。Ollama HTTP 的有限重試仍保留，預設每次領取至多 3 次 HTTP 請求。耗盡重試的 failed 工作會持續去重，不會因再次提交而無限重試。
- 租約到期可以重新領取。儲存成功或失敗時，必須同時符合 job ID、嘗試編號、processing 狀態與未到期租約，舊 Worker 不能覆寫新 Worker 的結果。
- Ctrl-C／SIGTERM 取消本地處理並保留租約，由其他 Worker 在租約到期後恢復。網路中斷或遠端推論未取消時，Ollama 可能仍做重複運算；佇列採至少一次處理，僅有效租約可提交結果，並非分散式 exactly-once 推論。
- 去重鍵包含使用者 UUID、歌詞陣列 SHA-256、來源／目標語言、模型與 prompt 版本。保留空白行、順序與空白字元；不同使用者不共用資料。模型 tag 應固定版本；若替換同名 tag 的權重，須更換模型識別值，否則保留期間仍會命中舊工作。
- 建立後超過 7 天的資料（包含原文／譯文）整列刪除，每批最多 1000 筆；有效 processing 租約暫不清除。Worker 啟動時及工作之間每 60 秒檢查一次，長推論可能延後檢查最多一次處理時間。未運行 Worker 時需排程執行 `cleanup`；備份保留期需另行配置。
- 每個 Worker 同時處理 1 個工作；可啟動多個 Worker，沒有每日或累積配額。未建立公開的 Ollama 埠。
- Migration 使用 SQLx migration history 與鎖，可重複執行；新 migration 需重新編譯二進位檔。正式部署應由管理流程執行 migration，Worker 的 `run` 不自動改 schema。

PostgreSQL 整合測試（隔離 schema，成功後移除，不清空現有資料）：

```sh
export TEST_DATABASE_URL="$DATABASE_URL"
cargo test --workspace -- --ignored --skip real_translategemma_all_languages
```

測試涵蓋 1／5／10 同時提交與領取、重複提交、使用者隔離、模型／prompt 版本、租約重新領取、舊 Worker 結果拒收、最大重試、清理保護有效租約、實際 Worker 推論失敗後恢復與處理中斷恢復。這是佇列併發正確性測試，不是 10 路真實模型效能 benchmark。測試若中途 panic，可能留下一個 `test_...` 或 `worker_test_...` schema，確認名稱後可手動移除。

### Phase 3 驗證紀錄（2026-11-23，Asia/Taipei）

- `cargo build --workspace`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --offline -- -D warnings`：通過。
- `cargo test --workspace`：8 項通過；3 項需要外部服務的測試預設 ignored。
- 額外執行 PostgreSQL queue 整合測試：通過，含 1／5／10 路併發與租約／保留機制。
- 額外執行 Worker + PostgreSQL + Ollama mock 整合測試：通過，含推論失敗重試與中斷恢復。
- 本機開發資料庫已套用 migration，連續執行兩次均成功。
- 真實端到端驗證：重複 enqueue 得到同一 jobId，Worker 呼叫 `translategemma:4b`，寫回 completed；推論 5160 ms，從啟動 Worker 至查詢完成約 5.50 秒（含輪詢間隔）。4 行輸出與空白行位置正確，內容與前述英文 benchmark 相同。
- 測試 Worker 已正常 SIGTERM 結束。開發資料庫留下 1 筆原創 fixture 的 completed 工作，可用 user `21571338-da79-4c36-8fa1-70ca6f3dcf89` 與 job `8ee65c9f-25ea-4107-bc40-756c7e14ba39` 查詢；依 7 天保留規則清理。
- 未重新執行英／日／韓單次模型測試（本次未改翻譯 prompt）；未做 Supabase 或雲端整合。


## Phase 4–5：Supabase Auth 與翻譯 API

使用 [Supabase JWT／JWKS 文件](https://supabase.com/docs/guides/auth/jwts) 的公開金鑰驗證方式，不需要 Supabase Service Role Key、JWT secret 或 Google OAuth secret。Google OAuth 登入仍由 Desktop App／Supabase 負責，本專案不修改 Tauri 或管理密碼。

1. 在 Supabase 的 Authentication 設定 Google provider、OAuth client 與允許的桌面 redirect URL；本次沒有修改雲端設定。
2. 在 JWT Signing Keys 確認使用 ES256 或 RS256；舊式 HS256 不符合本專案 JWKS 驗證方案，不會降級接受。
3. 設定 `SUPABASE_URL=https://<project-ref>.supabase.co`。issuer 固定為 `<SUPABASE_URL>/auth/v1`，公開金鑰只從 `<SUPABASE_URL>/auth/v1/.well-known/jwks.json` 取得；不使用 token 中提供的 key URL。
4. API 與 Worker 的 `OLLAMA_MODEL` 必須一致，先執行 migration，再分別啟動 `api` 與 `worker run`。
5. Desktop 登入後傳送 Supabase **Access Token**（不是 Google ID Token、anon key 或 service role key）。測試時只在本機設定 `SUPABASE_ACCESS_TOKEN`，不要貼到聊天、commit 或日誌。

```sh
# 已在本機設定 SUPABASE_ACCESS_TOKEN，且 api / worker run 均已啟動
curl --fail-with-body http://127.0.0.1:3000/api/translations \
  -H "Authorization: Bearer $SUPABASE_ACCESS_TOKEN" \
  -H 'Content-Type: application/json' \
  --data-binary @tests/fixtures/en.json

# 將 JOB_UUID 換成上一步回傳的 jobId
curl --fail-with-body http://127.0.0.1:3000/api/translations/JOB_UUID \
  -H "Authorization: Bearer $SUPABASE_ACCESS_TOKEN"
```

| 路由 | 行為 |
| --- | --- |
| `GET /health` | 公開，200 `{"status":"ok"}` |
| `POST /api/translations` | 必須登入，驗證輸入、去重並入列；pending／processing 回傳 202，已存在的 completed／failed 回傳 200 |
| `GET /api/translations/{jobId}` | 必須登入，只查詢自己的工作；不存在與別人的工作都回傳 404 |

POST body 與單次翻譯相同。回應包含 `jobId`、`status`，完成時附 `translations`；不回傳其他使用者資料、原文、資料庫錯誤或模型內部錯誤。所有回應帶 `Cache-Control: no-store`。

統一錯誤範例：

```json
{"error":{"code":"unauthorized","message":"A valid access token is required"}}
```

HTTP 狀態：401 缺少／無效 token（附 `WWW-Authenticate: Bearer`）、400 JSON／語言／UUID／內容不合法、413 JSON 超過 64 KiB、404 找不到工作／路由、405 方法不支援、503 DB／JWKS 暫時不可用、504 API 請求超過 20 秒。未登入的受保護路由先回 401，不解析歌詞。輸入還有既有的 200 行／16 KiB 歌詞限制，沒有每日或累積配額。

驗證要求：簽章、受信任 `kid`、ES256／RS256、`iss`、`aud=authenticated`、`exp`、`iat`、UUID `sub`、`role=authenticated`；若有 `nbf` 也檢查，允許 30 秒時鐘偏差。拒絕空 UUID、缺漏 claims、過期或未來簽發 token、重複 Authorization header，以及不符用途／演算法的 JWK。身份只來自驗證後的 `sub`。

JWKS 快取 5 分鐘，未知 kid 可觸發重新取得，但每 30 秒最多一次，避免任意 kid 對認證服務造成大量請求；因此輪替後新金鑰最長可能需等待 30 秒。快取過期且無法更新時拒絕驗證並回 503，不使用過期金鑰。移除金鑰／登出不會立即撤銷已簽發且仍有效的 token；依 Supabase token 壽命及此快取期限生效。HTTP JWKS 僅允許 loopback 本機測試，正式專案強制 HTTPS，禁用 redirects，限制 JWKS response 64 KiB／5 秒。

API 不設定寬鬆的瀏覽器 CORS。Tauri 若透過 WebView fetch 而非原生 HTTP client 呼叫，需在後續桌面整合確認 origin 與 CORS；本次未改動 Tauri。

測試 fixtures 的 ES256／RSA 私鑰是公開測試資料，只在測試編譯使用，不能用於真實服務。JWT 測試簽署真實 ES256／RS256 token，驗證錯誤 claims、竄改簽章、HS256 拒絕、JWKS 快取／輪替／故障；資料庫整合測試則以隔離 schema 驗證 API 的 1／5／10 路併發、去重、狀態與跨使用者隔離。

### Phase 4–5 驗證紀錄（2026-11-23，Asia/Taipei）

- `cargo build --workspace --offline`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --offline -- -D warnings`：通過。
- `cargo test --workspace`：14 項通過，4 項外部服務測試預設 ignored。
- 額外執行 PostgreSQL 整合測試：API、Queue、Worker 共 3 項通過，含 1／5／10 路提交、簽章 JWT 與跨使用者查詢隔離。
- 實際啟動 API，使用設定的 Supabase URL 與本機 DB：`/health` 200、未登入 POST 401、SIGTERM 正常退出，均通過。
- 已讀取專案 `qdjbiejdeefbrdcfkvej` 的公開 JWKS：ES256／P-256，`use=sig`、`key_ops=[verify]`，與驗證器相容；沒有使用私密金鑰。`.env` 已設定此專案 URL、權限 0600，且由 gitignore 排除。
- 未取得或記錄真實使用者 Access Token，因此尚未驗證真實 Google OAuth 登入後的端到端請求；未修改 Supabase Google provider／redirect／資料庫設定。資料庫仍使用本機 Docker PostgreSQL。
- 未重跑模型翻譯 benchmark，因本次未變更 Worker 推論程式；Phase 6 Docker images／雲端部署尚未實作。


## Phase 6：Docker 與部署

完整操作請看 [deploy/README.md](deploy/README.md)，包含本機 Docker Desktop、GCP API／Caddy、Oracle Worker／Ollama、Supabase TLS 連線、模型搬運、更新與回滾。

新增 `Dockerfile.api`、`Dockerfile.worker`、`.dockerignore`、`deploy/compose.api.yml`、`deploy/compose.worker.yml`、`deploy/Caddyfile` 與部署環境範例。根目錄 Compose 保留原本 PostgreSQL 行為，另以 `--profile app` 啟用 API／Worker。Images 不含模型、Secrets 或測試金鑰；API／Worker 使用非 root 使用者。

API 新增 `--healthcheck` 命令供 image HEALTHCHECK 使用，只檢查同一容器 `127.0.0.1:3000/health`，3 秒逾時，不需要額外 curl 套件或 secrets。部署時 API 固定監聽容器 3000；自訂埠須同步調整 healthcheck。


Phase 6 已實測 ARM64 API／Worker、amd64 API（本機模擬）、Caddy HTTPS 與 Linux ARM64 Ollama。既有模型在 Linux 容器翻譯 4 行耗時 **12.162 秒**；詳細環境、限制與驗證紀錄見 [部署文件](deploy/README.md)。未建立或啟用雲端資源。
