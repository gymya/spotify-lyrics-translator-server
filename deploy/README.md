# Phase 6：分機部署

這些設定可分別啟動／更新 API 與 Worker，不會建立任何雲端資源。先在本機建 image 與測試，再把 image 搬到已取得授權的 VM。不要在小型免費 VM 上編譯整個 Rust workspace。

## 架構與連線

| 主機 | 服務 | 對外入口 |
| --- | --- | --- |
| Machine A（GCP，通常 amd64） | API + Caddy | TCP 80、443；API 3000 不對主機公開 |
| Machine B（Oracle A1，arm64） | Worker + Ollama | 不需要翻譯入口；Ollama 11434 沒有 ports mapping |
| Supabase | Auth + PostgreSQL | API／Worker 主動連出 HTTPS／PostgreSQL TLS |

SSH 管理入口限制為自己的來源位址。不要開放 3000、5432、11434。Ollama 的 `0.0.0.0:11434` 只在容器網路內監聽，並非發布到主機。

API 與 Worker 都以 UID/GID 65532 執行、唯讀 root filesystem、移除 capabilities。正式 image 只含一個 release binary 與 CA 憑證，不含原始碼、`.env`、測試私鑰或模型權重。`.dockerignore` 以 allowlist 限制 build context。Migration 嵌入 Worker binary。Caddy 的憑證與設定存於持久 volume。

## 本機 Docker Desktop

在專案根目錄執行，沿用既有 PostgreSQL volume 與原生 Ollama（需先啟動）：

```sh
# .env 已設定 SUPABASE_URL、OLLAMA_MODEL；勿覆寫已有 secrets
ollama list
docker compose --profile app build
docker compose up -d --wait postgres
docker compose --profile app run --rm worker migrate
docker compose --profile app up -d --wait api worker
curl --fail http://127.0.0.1:3000/health
# 使用原創 fixture 入列，UUID 是本機測試使用者
docker compose --profile app run --rm -T worker enqueue \
  00000000-0000-0000-0000-000000000001 - < tests/fixtures/en.json
# 換成輸出的 JOB_UUID
docker compose --profile app run --rm worker status \
  00000000-0000-0000-0000-000000000001 JOB_UUID
# 只停 API/Worker，保留既有 PostgreSQL 與資料
docker compose --profile app stop api worker
```

本機 profile 的 Worker 使用 Docker Desktop `host.docker.internal:11434`，重用原生 Ollama；不會重新下載模型。這個模式不適用一般 Linux 主機的 loopback Ollama，Linux 請用下方 Worker compose。若本機 3000 已有 API 程序，先停止該程序。開發用 DB 密碼若包含 URI 保留字元，需另以 URL-encoded 連線字串覆寫 DATABASE_URL；正式部署使用完整 URI。

## 建置與搬運 images

Dockerfile 使用多階段編譯，Cargo 依 `Cargo.lock` 建置。基底 image tag 會更新；正式發版請在可信建置機確認版本並將 image 推送至自己的 registry，以 digest 固定部署，保留前版作回滾。以下只建本機 image，不會推送：

```sh
# API 的目標架構依 Machine A 實際架構選擇
docker buildx build --platform linux/amd64 -f Dockerfile.api -t lyrics-api:RELEASE --load .
docker buildx build --platform linux/arm64 -f Dockerfile.worker -t lyrics-worker:RELEASE --load .
# 無 registry 時可存檔，檔案不含 secrets／模型
docker save lyrics-api:RELEASE -o /tmp/lyrics-api.tar
docker save lyrics-worker:RELEASE -o /tmp/lyrics-worker.tar
# 自行將各自的 tar 傳至對應 VM，執行 docker load -i <file>
```

相同 Dockerfile 也能在目標架構的建置機原生編譯；跨架構 build 需要 Docker Buildx/QEMU 支援，速度可能較慢。部署 compose 不硬編碼 platform，會使用 image 的實際架構。不要把只含 arm64 的本機 image 放到 amd64 VM。

## Supabase PostgreSQL

從 Dashboard 的 Connect 面板取得真實 URI，不猜測 pooler region／host。長駐服務在 IPv6 可用時可使用 direct；IPv4-only VM 使用 **session pooler 5432**。本程式有 SQLx prepared statements 與 migration advisory lock，不使用 transaction pooler 6543。依 [Supabase 連線文件](https://supabase.com/docs/guides/database/connecting-to-postgres) 選擇端點。

使用 `sslmode=verify-full` 驗證 TLS 與 hostname，密碼需 URL encode。若專案需額外根憑證，下載 Supabase 提供的憑證、唯讀掛載到容器，再於 URI 設 `sslrootcert=/path/to/root.crt`；不要以停用驗證解決連線錯誤。

目前資料表 RLS 沒有前端 policy；服務 DB 角色必須是表擁有者或具必要的 RLS bypass 與資料表權限。初次 migration 使用專用管理流程／擁有者，服務只保留所需權限；不要把 DB URI 或 service role key 給 Desktop。每個 API／Worker pool 最多 12 條連線，增加副本前確認 Supabase 的連線上限。

範例部署檔不包含真實資料庫密碼。把 `deploy/.env.example` 複製成 `deploy/.env.api`／`deploy/.env.worker`，只保留各主機需要的設定，並 `chmod 600`。這些檔案已由 gitignore 排除。`docker compose config` 可能展開 secrets，檢查時使用 `config --quiet`；不要把完整 config／inspect environment 貼入日誌。

## Machine B：Worker + Ollama

1. 先確認 VM 與 Docker 的架構為 aarch64／arm64，記憶體與磁碟可容納模型及 context。
2. 固定使用 `ollama/ollama:0.35.0`（本機現有版本），官方 manifest 提供 amd64／arm64。Ollama 官方也提供 [Linux ARM64 runtime](https://docs.ollama.com/linux)。
3. 將既有 Ollama **models** 目錄（含 `blobs/` 與 `manifests/`）複製到 VM 的 `/srv/ollama/models`；`OLLAMA_MODELS_PATH` 必須是已存在的絕對路徑。Compose 唯讀掛載此目錄，缺少目錄會直接失敗，不會自動建立空目錄或拉模型。只搬需要的 TranslateGemma manifests／blobs 可減少傳輸，但必須保留它引用的全部 blobs。
4. 設定 `.env.worker` 的 WORKER_IMAGE、DATABASE_URL、OLLAMA_MODEL、OLLAMA_MODELS_PATH。兩台主機模型名稱必須一致。

```sh
docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml config --quiet
docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml up -d --wait ollama
docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml exec ollama ollama list
# 確認 translategemma:4b 存在；不要擅自改成其他模型
# 明確套用 migration；重複執行安全，不會清空資料
docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml run --rm --no-deps worker migrate
# 正式啟用前先測推論時間與輸出
docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml run --rm -T worker benchmark - < tests/fixtures/en.json
docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml up -d worker
```

`ollama list` healthcheck 僅確認服務可回應，不保證特定模型已存在／可推論，所以 benchmark 是啟用前必要驗證。預設只載入一個模型、一次推論；不要為了併發請求直接增加 Ollama parallel。Worker 不暴露埠，工作透過 DB 領取。

Linux CPU 推論速度不等同 macOS Metal。模型檔約 3.3 GB，**不代表 RAM 只需 3.3 GB**；context、KV cache、runtime 會額外使用 RAM。先以真實 VM 跑短歌／長歌與 1／5／10 個佇列請求，觀察 `docker stats`、失敗率、等待時間及每工作 600 秒上限。效能不足時先減少 Worker 副本／一次只跑一個工作；若仍超時，回報測量結果再評估付費資源或調整需求，不自動更換模型或啟用付費 VM。

## Machine A：API + HTTPS

在已取得授權的 VM 上安裝 Docker Compose、載入 API image，設定 `.env.api`。API_DOMAIN 填實際 DNS hostname（不加 scheme／path），ACME_EMAIL 填憑證通知信箱。DNS A／AAAA 必須指向可到達的此主機，只保留有效的 AAAA。

```sh
docker compose --env-file deploy/.env.api -f deploy/compose.api.yml config --quiet
docker compose --env-file deploy/.env.api -f deploy/compose.api.yml up -d --wait
curl --fail https://YOUR_API_DOMAIN/health
```

Caddy 在 API health 通過後啟動，依 [官方 Automatic HTTPS](https://caddyserver.com/docs/automatic-https) 取得／更新憑證並將 HTTP 轉到 HTTPS；需要 DNS 與 TCP 80／443 正確。此步會聯絡 ACME CA，只有在自己的實際網域與 VM 準備好後執行。本機測試用 localhost 內部 CA，不會替示例網域申請憑證。

Caddy 未啟用 access log，不記錄 Authorization 或歌詞 body。不要開啟 debug/request-body logging。API health 是 liveness，不能取代真實 token 的入列／查詢 smoke test。登入測試步驟見根目錄 README。

## 更新、回滾、資料保留

- 分別更新 `.env.api` 的 API_IMAGE 或 `.env.worker` 的 WORKER_IMAGE，再執行該 compose 的 `up -d --no-deps api`／`up -d --no-deps worker`。更新一方不需重啟另一方；正式版本使用不可變 tag／digest。
- 有新 migration 時，先備份並評估新舊程式相容性，再用新 Worker image 執行 `migrate`。不要自動回滾 schema；回滾 binary 之前先確認 schema 相容。
- Worker SIGTERM 後未完成工作保留租約，630 秒後可由新 Worker 重新領取。滾動更新時短暫增加等待時間屬預期。
- Worker 運行時會清理超過 7 天的工作。若 Worker 長期停機，在已有管理主機定期執行 `docker compose --env-file deploy/.env.worker -f deploy/compose.worker.yml run --rm --no-deps worker cleanup`；一次最多刪 1000 筆，積壓時重複執行。資料庫備份的歌詞保留期限需另外配置。
- `docker compose down` 保留 volumes；不要使用 `down -v`，除非明確要刪除對應環境資料／憑證。

## 免費資源與尚待驗證項目

截至 2026-11-23，Google [Free Tier 文件](https://docs.cloud.google.com/free/docs/free-cloud-features) 對 e2-micro 有區域、時數、儲存與流量條件；不能把 VM、外部 IP、網路與其他服務都視為無條件免費。API/Caddy 先量測常駐 RAM，不在小 VM 上編譯。

Oracle [Always Free 文件](https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier.htm) 目前列出 A1 總計 2 OCPU／12 GB；容量可用性與實際帳戶額度仍需在申請時確認，不能沿用舊的 4 OCPU／24 GB。本專案不建立 VM、不開啟付費服務、不保證免費 VM 足夠快。

尚需使用者自己的網域、VM 存取權、Supabase DB URI 與真實登入 token 才能驗證雲端端到端。不要把這些 private credentials 貼到聊天；在目標主機的 env 檔設定。


## 本次實測（2026-11-23，Asia/Taipei）

- ARM64 API／Worker release images：建置成功；兩者 UID/GID 為 65532。映像大小約 150.5 MB／148.7 MB（Docker inspect Size，非下載壓縮大小）。
- amd64 API release image：建置成功；在本機架構模擬下，DB 連線、內建 healthcheck 與 SIGTERM exit 0 均通過。尚未在 GCP 實機驗證。
- 三份 Compose 設定解析成功；另以 JSON 斷言確認分機 API／Worker／Ollama 沒有 ports mapping、模型目錄唯讀且不自動建立缺少路徑。
- Docker Desktop 完整 profile：migration、API healthy、未登入 401、原創 fixture 入列至 completed 通過；輪詢量測約 12.90 秒，包含啟動一次性 CLI 容器的時間。
- Caddy：設定驗證通過，使用 localhost 自建 CA 做實際 HTTPS health 請求成功；客戶端指定該 CA 驗證，未停用憑證檢查，未申請公開網域憑證。
- `ollama/ollama:0.35.0` 的 ARM64 manifest 可用；Linux ARM64 容器唯讀掛載既有模型，`ollama list` 顯示 `translategemma:4b`／`c49d986b0764`，未重新下載模型。`11434/tcp` 的主機映射為 null。
- ARM64 Worker → ARM64 Linux Ollama 真實推論：英文 4 行，含 1 空白行，第一次成功，耗時 **12162 ms**。輸出為「清晨的光照在我的窗戶」「我把昨天帶入雨中」「」「今晚我們的腳步再次找到道路」。模型載入後的一次記憶體取樣約 **4.324 GiB**，不是峰值，也不是 Oracle VM 容量保證。
- 既有 14 項單元／mock 測試、fmt、Clippy 通過。本次未修改佇列或 JWT 邏輯，未重跑 Phase 4–5 已通過的 PostgreSQL 併發測試。
- 測試用 API／Worker 已停止，臨時 HTTPS／amd64／Linux Ollama 容器已移除；原始 PostgreSQL 與模型保留。開發資料庫新增 1 筆原創容器測試工作，依 7 天保留規則清理。
- 未推送 registry、未建立雲端資源、未修改 Supabase 或 Google OAuth；真實登入 token、Supabase DB TLS、公開 DNS／ACME、免費 VM 性能仍待目標環境驗證。
