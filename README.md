# ai-assistant

AI 桌面助手相关项目集合（迁移自 TraeWorkAssistant 与 workbuddy-assistant）。

## 子项目

| 目录 | 说明 |
| ---- | ---- |
| [traework-assistant](./traework-assistant) | Trae Work Assistant（原 TraeWorkAssistant） |
| [workbuddy-assistant](./workbuddy-assistant) | WorkBuddy Assistant |
| [cred-broker](./cred-broker) | 账号凭证池 + 续签闸（Cloudflare Worker + D1） |

前两个子项目基于 Tauri + React + Rust；cred-broker 是部署在 Cloudflare Workers 上的
服务端，跨机器共用同一批账号凭证，保证同一时刻只有一台机器在续签。

## 应用发布（GitHub Actions）

traework-assistant 与 workbuddy-assistant 由 CI 构建 **macOS（universal 双架构）** 与
**Windows（NSIS）** 产物，统一发布到本仓库（ai-assistant）的 GitHub Release，应用内更新
（`@tauri-apps/plugin-updater`）从各自的固定 tag 拉取：TraeWorkAssistant 用
`releases/download/traework-latest/latest.json`，WorkBuddyAssistant 用
`releases/download/workbuddy-latest/latest.json`。

### 发版流程

1. 改版本号：`src-tauri/tauri.conf.json` 与 `package.json` 里的 `version`
2. 打 tag 推送：`traework-v0.1.12` 或 `workbuddy-v0.1.29`（版本以 tag 为准）
3. CI 构建两个平台 → 由 `scripts/build-latest.mjs` 用 `.sig` 生成 `latest.json`
   （v2 的 `tauri build` 不产 manifest）→ delete+recreate 各自的固定 tag Release

也可在 Actions 页面手动 `workflow_dispatch`（用配置里的版本号）。

两个应用用**独立的固定 tag**（`traework-latest` / `workbuddy-latest`）作为更新通道，
版本号各自演进、互不干扰——同一个仓库内不会像共用 `releases/latest` 那样串版本。

### 所需仓库 Secrets

| Secret | 用途 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | updater 签名私钥（`npx tauri signer generate` 生成，公钥已内嵌在 tauri.conf.json） |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥密码，无密码可不配 |