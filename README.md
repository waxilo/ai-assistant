# ai-assistant

AI 桌面助手相关项目集合（迁移自 TraeWorkAssistant 与 workbuddy-assistant）。

## 子项目

| 目录 | 说明 |
| ---- | ---- |
| [traework-assistant](./traework-assistant) | Trae Work Assistant（原 TraeWorkAssistant） |
| [workbuddy-assistant](./workbuddy-assistant) | WorkBuddy Assistant |
| [qoder-assistant](./qoder-assistant) | Qoder Assistant（多账号签到、token 自动续签、应用内自动更新） |
| [cred-broker](./cred-broker) | 账号凭证池 + 续签闸（Cloudflare Worker + D1） |

前三个子项目基于 Tauri + React + Rust；cred-broker 是部署在 Cloudflare Workers 上的
服务端，跨机器共用同一批账号凭证，保证同一时刻只有一台机器在续签。

## 应用发布（GitHub Actions）

traework-assistant、workbuddy-assistant 与 qoder-assistant 由 CI 构建 **macOS（universal 双架构）** 与
**Windows（NSIS）** 产物，统一发布到本仓库（ai-assistant）的 GitHub Release，应用内更新
（`@tauri-apps/plugin-updater`）从各自的固定 tag 拉取：TraeWorkAssistant 用
`releases/download/traework-latest/latest.json`，WorkBuddyAssistant 用
`releases/download/workbuddy-latest/latest.json`，QoderAssistant 用
`releases/download/qoder-latest/latest.json`。

### 发版流程

1. 改版本号（**六处**必须对齐，否则应用内版本号会显示错 —— 应用内显示的是
   `Cargo.toml` 的 `CARGO_PKG_VERSION`，`package.json` 与 lock 不一致还会让 CI 的 `npm ci` 直接失败）：
   `src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`（本包条目）、
   `package.json`、`package-lock.json` 的**顶层与 `packages[""]` 两处**
   （lock 里每个依赖都有 `version`，**不能 replace_all**，要用带 `"name"` 的上下文一次覆盖相邻两处）
2. 打 tag 推送：`traework-v0.1.23` / `workbuddy-v0.1.39` / `qoder-v0.1.9`（版本以 tag 为准）
3. CI 构建两个平台 → 由 `scripts/build-latest.mjs` 用 `.sig` 生成 `latest.json`
   （v2 的 `tauri build` 不产 manifest）→ delete+recreate 各自的固定 tag Release

也可在 Actions 页面手动 `workflow_dispatch`（用配置里的版本号）。

各应用用**独立的固定 tag**（`traework-latest` / `workbuddy-latest` / `qoder-latest`）作为更新通道，
版本号各自演进、互不干扰——同一个仓库内不会像共用 `releases/latest` 那样串版本。

### 版本号规则（重要）

**每个应用有自己的版本线。新增应用、或从别的应用 fork 时，不要沿用对方的版本序列。**

| 应用 | 当前版本 | 版本线起点 |
|---|---|---|
| TraeWorkAssistant | `0.1.23` | 0.1.x（自身历史） |
| WorkBuddyAssistant | `0.1.39` | 0.1.x（自身历史） |
| QoderAssistant | `0.1.9` | **从 `0.1.0` 起算** |

> 上面两处（发版流程里的 tag 示例、本表的「当前版本」）**每次发版后都要同步**，
> 否则下一版对照时会拿旧值推算。

QoderAssistant 曾以 `0.1.36` 首次发布 —— 那是照搬 WorkBuddy 当时 `0.1.35` 的结果，
不是它自己的版本历史，于是「一个全新应用的首个版本号带着另一个应用的历史」，三个应用
挤在同一条 `0.1.3x` 序列上互相看不出谁是谁。**已重置为 `0.1.0`。新应用一律从 `0.1.0` 起算自己的线。**

> 重置版本号会让已装的旧版本（数字更高）**无法通过应用内更新降级**，需要手动装一次新包；
> 之后就在自己的 `0.1.x` 线上正常更新了。

### 所需仓库 Secrets

| Secret | 用途 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | updater 签名私钥（`npx tauri signer generate` 生成，公钥已内嵌在 tauri.conf.json） |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥密码，无密码可不配 |