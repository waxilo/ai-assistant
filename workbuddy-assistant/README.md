# WorkBuddy 助手（Tauri 桌面端）

一个用 **Tauri v2 + Rust + React** 实现的 WorkBuddy 桌面助手（当前功能：多账号签到），支持：

- **多账号管理**：账号条目**只读展示**（名称 + 手机号 + 凭证有效期 + 剩余积分），不做手工录入与编辑 —— 凭证一律来自「登录新账号」或「导入本机账号」，避免粘贴错 token。删除账号时会一并清理它的签到日志。
- **后台常驻（系统托盘）**：点窗口关闭按钮 = 隐藏到系统托盘，**进程不退出**（定时签到、token 续签、本地反代持续生效）；托盘菜单提供「显示主窗口 / 退出」，macOS 点 Dock 图标也会唤回主窗口。真正退出请走托盘菜单「退出」（退出时会自动关闭智能接管）。
- **账号导入 / 导出（跨机器迁移）**：「导出」把全部账号（含 token / refresh_token / 有效期）写成 JSON 文件（落盘权限 0600）；「导入」选择该文件后按「手机号或 token」合并补全，**不会产生重复条目**。导出文件含登录凭证，请妥善保管。
- **一键签到**：单个账号签到，或「全部签到」批量领取每日积分。
- **定时自动签到**：每天在设定时刻（默认 `09:07`）自动跑一遍「全部签到」，**应用运行期间生效**；错过时刻后 30 分钟内打开应用会自动补签一次，跨启动不会重复签（`schedule_state.json` 记录已执行日期）。配套提供「开机自启动」开关，让定时签到真正能每天生效。
- **签到通知（webhook）**：可配置一个 webhook 地址，签到结束后推送结果汇总（成功 / 已签 / 失败数量 + 失败明细）；可分别开关「定时签到后推送」与「手动全部签到后推送」，并内置「测试推送」按钮自查配置。调度触发与推送结果会记入 `scheduler.log`（保留最近 200 行），便于事后排查「为什么没自动签到」。
- **剩余积分展示**：账号条目显示「剩余积分」，取自主流官方接口 `POST {host}/v2/billing/meter/get-user-resource`（汇总各资源包的 `CycleCapacityRemain*`，与官方 Web 端「计划与用量」同口径，可能是小数）；该接口拿不到时退回 `checkin-status` 的 `total_credits`。注意签到响应里的 `credit` 是**本次获得**（单独显示为「本次 +N」），不是余额。
- **token 自动续签**：导入 / 无感登录时会一并保存 `refreshToken` 与 `expiresAt`。应用启动及常驻期间**每 12 小时**扫描一次，**剩余有效期不足 48 小时即自动换新凭证**（签到前另有兜底判定）；续签失败不阻断签到（仍用旧 token 试一次）。
- **智能接管（WorkBuddy 专用反代）**：在 `127.0.0.1:8787`（可改端口）起一个 WorkBuddy 专用反代，开启后自动把 WorkBuddy 的对话请求接管到本地——只在**勾选的扣费备选账号**里选号（未勾选的不允许扣费，全不勾 = 全部可用；会话粘滞 + 积分最早过期优先轮换）。页面下方有**接管动态时间线**：开启 / 关闭接管、每个会话开始使用哪个账号、代理错误，一目了然。详见 [智能接管](#智能接管workbuddy-专用)。
- **账号获取（两条通道，无手工录入）**：
  - **导入本机账号**：直接读 WorkBuddy 写在本机的 `CodeBuddyExtension/Data/Public/auth/*.info`，**不需要应用运行、也不需要调试端口**，并且一次就能拿到 token + 昵称 + 手机号 + refresh token（已存在的账号会合并补全凭证，不会重复添加）。
  - **登录新账号**：走官方 OAuth state 轮询（`/v2/plugin/auth/state` → 浏览器扫码 → `/v2/plugin/auth/token`），**不重启、不打断当前 WorkBuddy、不改动本机登录文件**，能主动签发**任意新账号**的凭证与昵称/手机号。
- **智能 host 推断**：根据 JWT 的 `iss` 字段自动判断该用 `workbuddy.cn` / `workbuddy.ai` / `codebuddy.cn` / `codebuddy.ai`。默认 Base URL 为内置常量，**界面不提供修改入口**（改错会让签到打到错误的域）。
- **GitHub Release 自动更新**：内置 `tauri-plugin-updater`，点击「检查更新」即可从 Release 拉取并安装新版本。
- **积分日报（按天 + 逐小时）**：按**自然日**统计积分消耗与新增，每天一条，展开可见**每小时**明细（总览柱状图 + 逐账号列表）。
  口径是资源包**累计量**的差值（`CapacityUsed` / `CapacitySize`），不是「抓余额算涨跌」——
  所以同一天「先消耗后签到」不会互相抵消，**多个客户端同时消耗也都能统计到**。
  采样时增量就落进「采样时刻所属的小时」，因此小时之和恒等于当天合计、相邻两天可直接相加。
  数据在应用数据目录的 `credit_ledger.json`（台账 + 60 天小时桶）与 `credit_reports.json`（日报，最近 400 条），均 0600。
- **签到日志（按账号查看）**：每次签到结果（账号 / 时间 / 结果 / 积分 / 详情）落库到应用数据目录的 `checkin_logs.json`（权限 0600，保留最近 2000 条）。入口在**每个账号条目上的「日志」按钮**，面板按时间倒序简单列出该账号记录，可一键清空。
- 跨平台：macOS（`.app` / `.dmg`）与 Windows（`.msi`）。

> 签到逻辑参考自 `workbuddy-checkin` 与 `WorkDaddy` 的 `checkin-result.js`：
> 接口 `POST {host}/billing/meter/daily-checkin`（兼容 `/v2/...`），`code===0` 视为成功，
> `code===10001` 且文案命中“已签到”视为今日已签（幂等成功）。

---

## 目录结构

```
WorkBuddyAssistant/
├── index.html
├── package.json            # 前端依赖（React + Vite + Tauri API）
├── vite.config.ts
├── src/                    # React 前端
│   ├── main.tsx
│   ├── App.tsx             # 主界面（账号列表/签到/弹窗/更新）
│   ├── api.ts              # Tauri invoke 封装
│   ├── updater.ts          # 更新器逻辑
│   ├── types.ts
│   └── styles.css
├── src-tauri/              # Rust 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json     # 含 updater 配置（endpoint + pubkey）
│   ├── capabilities/default.json
│   ├── build.rs
│   ├── icons/              # 图标套件（由 scripts/make-icon.mjs + tauri icon 生成）
│   └── src/
│       ├── main.rs / lib.rs
│       ├── accounts.rs     # 多账号 JSON 存储（含 phone；文件权限 0600）
│       ├── auth_file.rs    # 读本机 WorkBuddy 登录文件（含昵称/手机号）
│       ├── checkin.rs      # 签到 HTTP 逻辑 + host/iss 推断 + 结果判定 + 剩余积分查询
│       ├── oauth.rs        # 「登录新账号」无感登录（OAuth state 轮询，纯 HTTP）
│       ├── refresh.rs      # token 续签（refresh token → 新 access token）
│       ├── notify.rs       # 签到结果推送 webhook（GET ?message=，浏览器 UA + 3 次重试）
│       ├── scheduler.rs    # 定时自动签到 + 自动续签扫描（后台线程，到点即触发 + 30 分钟补跑 + scheduler.log）
│       ├── proxy.rs        # 智能接管反代（127.0.0.1 专用透传 + 优先扣费账号/粘滞/最旧积分路由 + /v2 改写）
│       ├── stealth.rs      # 接管 Fuse：端点装卸、租约、接管事件日志（takeover-journal.jsonl）
│       ├── netfix.rs       # 网络急救：诊断（含接管事件交叉判定）+ 一键恢复 + 自动备份
│       ├── logs.rs         # 签到日志存储（JSON，权限 0600，保留最近 2000 条）
│       └── commands.rs     # Tauri 命令
├── scripts/make-icon.mjs   # 纯 Node 生成图标源 PNG
├── scripts/build-dmg.sh    # 纯 hdiutil 打 dmg（本机缺 create-dmg 模板时的兜底）
└── .github/workflows/release.yml  # 跨平台自动构建 + 发布 + 更新签名
```

---

## 本地开发

前置：Node 20+、Rust 1.77+、系统 WebView（macOS 自带；Windows 需 WebView2 Runtime，通常已预装）。

```bash
npm install
npm run tauri dev      # 启动开发模式（前端热重载 + Rust 重新编译）
```

跑后端单测（纯本地，不碰你本机的 WorkBuddy、不发网络请求）：

```bash
cd src-tauri && cargo test
```

另有 3 个 `#[ignore]` 的**真实接口冒烟测试**（会真的请求官方接口 / 真的发一条 webhook 推送）：

```bash
cd src-tauri && cargo test -- --ignored --nocapture
```

---

## 本地构建

```bash
npm install
node scripts/make-icon.mjs          # 生成图标源 PNG
npx tauri icon src-tauri/icons/icon-source.png   # 生成各平台图标套件
npm run tauri build                 # 产出 src-tauri/target/release/bundle/
npm run build:dmg                   # 可选：纯 hdiutil 兜底打 dmg（不依赖 create-dmg）
```

> macOS 首次构建若提示「无法验证开发者」，在「系统设置 → 隐私与安全性」中点「仍要打开」。
>
> `tauri build` 结尾若报 **`A public key has been found, but no private key`**：因为
> `tauri.conf.json` 里已配置 `updater.pubkey`（占位符也算「已配置」）且 `createUpdaterArtifacts=true`，
> 但没有设置签名私钥。**这不影响 `.app` / `.dmg` 产出**，只是 updater 产物无法签名；
> 按下面「开启 GitHub 自动更新」配好密钥后即消失。
>
> 若 `.dmg` 步骤偶发失败（迁移后旧 `src-tauri/target` 残留绝对路径时遇到过），
> 先 `cd src-tauri && cargo clean` 后重跑；仍失败可用 `npm run build:dmg`
> （`scripts/build-dmg.sh`，纯 `hdiutil`、零依赖）兜底产出可分发的 `.dmg`。

---

## 开启 GitHub 自动更新（重要）

`tauri.conf.json` 里的更新配置当前是**占位符**，需要替换为你自己的仓库与签名密钥后，自动更新才会真正生效。

### 1. 生成更新签名密钥对

```bash
npx tauri signer generate -w ~/.tauri/workbuddy-assistant.key
```

命令会输出**公钥（pubkey）**，并生成私钥文件。私钥请妥善保管，**不要提交进仓库**。

### 2. 替换配置里的占位符

- 把 `src-tauri/tauri.conf.json` 中 `plugins.updater.endpoints` 的 `OWNER/REPO`
  改成你的 GitHub 仓库，例如 `https://github.com/waxilo/workbuddy-assistant/releases/latest/download/latest.json`。
- 把 `plugins.updater.pubkey` 的 `REPLACE_WITH_YOUR_TAURI_UPDATER_PUBLIC_KEY`
  替换为第 1 步输出的公钥。

### 3. 在仓库配置 CI Secret

在本仓库 **Settings → Secrets and variables → Actions** 增加：

- `TAURI_SIGNING_PRIVATE_KEY`：第 1 步生成的私钥文件内容（纯文本）。
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（可选）：若生成密钥时设置了密码。

### 4. 打 tag 触发发布

```bash
git tag v0.1.0
git push origin v0.1.0
```

GitHub Actions 会在 macOS / Windows 两个 runner 上分别构建、用私钥签名更新产物，
并发布一个 Draft Release（`latest.json` + 各平台安装包 + `.sig`）。
在 GitHub 页面把 Draft 改为正式发布后，旧版本客户端即可通过「检查更新」拉取新版本。

> 注意：更新只在**已签名的 Release** 之间生效。本地 `npm run tauri build` 未设置
> `TAURI_SIGNING_PRIVATE_KEY` 时不会生成 `.sig`，此类构建包无法用于自动更新。

---

## 添加账号

工具栏有两个入口：**登录新账号**（加新号）/ **导入本机账号**（读本机已登录的）。
没有「+ 添加账号」——不支持手工粘贴 token，账号条目也不可编辑。

### 1. 导入本机账号（最省事）

WorkBuddy 登录成功后会自己把账号与凭证写到本机：

| 平台 | 路径 |
| --- | --- |
| macOS | `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/*.info` |
| Windows | `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\*.info` |

本工具直接读这个 JSON（`account.uid` / `nickname` / `phoneNumber` +
`auth.accessToken` / `refreshToken` / `expiresAt` / `domain`），因此：

- **不需要 WorkBuddy 正在运行**，也不用改启动方式（不涉及 `--remote-debugging-port`）；
- **一次就能拿到 token + 昵称 + 手机号 + refresh token**，导入后账号条目自动带上手机号，
  并且具备自动续签能力；
- 列表会标出「当前登录」「有效期至 … / 剩 N 天 / 已过期」，可单条导入或「全部导入」；
- token 由 WorkBuddy 自己续期，读到的就是最新的那份（已存在的账号按手机号/token 识别后合并补全，不会重复添加）。

> 只读，不写回、不外传。这条通道的可行性来自对 WorkDaddy 实现的核对——它切换账号、
> 签到也全部基于这个文件，而不是从运行中的应用里抓包。

### 2. 登录新账号

点工具栏「**登录新账号**」（独立入口，不再塞在导入弹窗里）。它走官方的 OAuth state 轮询，
**不重启、不打断当前 WorkBuddy，也不改动本机登录文件**，是加第二个 / 第三个账号最省事的路子：

1. 选好**接口域**（国内版 `www.workbuddy.cn` / 国际版 `www.workbuddy.ai` / CodeBuddy 两个域）。
   默认会自动跟随你「当前登录」账号所属的域。
2. 点「打开授权页并开始」→ 本工具请求
   `POST {host}/v2/plugin/auth/state?platform=workbuddy` 拿到 `state`，
   并用**系统浏览器**打开返回的授权页（`{host}/login?platform=workbuddy&state=…`）。
3. 在浏览器里完成登录（扫码即可）。本工具每 2 秒轮询一次
   `GET {host}/v2/plugin/auth/token?state=…` ——
   未授权时返回的是 `{"code":11217,"msg":"11217:login ing..."}`，**这是正常等待态，不是报错**。
4. 授权完成后自动拉账号信息（`GET {host}/v2/plugin/login/account`，带 `Bearer` + `X-Domain`），
   把**昵称 / 手机号 / uid** 一并显示出来；点「添加为账号」入库，或连点「再登一个」继续加号。

> 10 分钟未完成授权会自动判定超时，重新点一次即可。
> 各区域 host 不能混用——授权与签到都必须打到账号自己所属的域。

---

## token 续签

账号的 access token 会过期（官方签发 60 天左右）。导入 / 无感登录时本工具会一并保存
`refreshToken` 与 `expiresAt`，之后：

- **自动续签（常驻）**：后台调度线程**启动后立刻扫一次，之后每 12 小时扫一遍**全部账号；
  只要剩余有效期**不足 48 小时**就调
  `POST {host}/v2/plugin/auth/token/refresh`（`X-Refresh-Token` 头 + `Bearer` 旧 token）
  换新凭证并写回 `accounts.json`。续签与「定时签到」开关无关——它是保命操作，
  不该因为没设定时签到就被关掉。结果记 `scheduler.log`，前端弹提示并刷新列表。
- **签到前兜底**：每次签到（手动 / 批量 / 定时）前也会再判一次阈值，避免「扫描刚过、签到时刚好过期」。
- 条目上**没有手动「续签」按钮**——续签完全自动（后台扫描 + 签到前兜底），条目只读。
- 条目会显示凭证有效期（已过期标红，7 天内提示）。
- 续签失败**不阻断**签到，仍用旧 token 试一次，由签到结果给出明确提示；
  若 refresh token 本身已失效，重新「导入本机账号」或「登录新账号」即可。

### 为什么不做「浏览器本地存储扫描」与「调试端口抓包」

两条通道都做过，又都**主动移除**了：

- **浏览器本地存储扫描**：实测已证实不可用。网页端（workbuddy.cn）的登录态是
  **httpOnly 加密 Cookie**（`www.workbuddy.cn` 下是 `session` / `KEYCLOAK_SESSION`，均为加密存储），
  其 localStorage 里只剩 SDK 监控用的 `beacon_config` / `__BEACON_*_session_storage_key`
  等无意义键，全机扫描 0 命中；参考实现（`workbuddy-checkin/extract_token.mjs`、WorkDaddy）
  也从不读浏览器存储。
- **调试端口抓包（CDP）**：需要以 `--remote-debugging-port` 重启 WorkBuddy
  （会关掉正在使用的对话窗口），而且只能拿到**当前登录那一个**账号的 token。
  相比之下「导入本机账号」不重启、不打断、还能一次拿全元信息，
  「登录新账号」还能主动签发任意新账号——CDP 已无不可替代的用途。

---

## 定时自动签到

在「设置」里开启「每天定时自动签到全部账号」并选好时刻（默认 `09:07`，与参考脚本
`workbuddy_checkin.py` 的 launchd 定时一致）。

- 实现是 Rust 侧一个**后台线程**：每 30s 读一次配置，命中就调用与「全部签到」完全相同的逻辑，
  并按设置推送通知。改了时刻/开关**无需重启应用**即生效。
- 触发模型是「**到点即触发 + 30 分钟补跑窗口**」，而不是「当前分钟恰好等于设定值」：
  前者能容忍轮询粒度，也能覆盖「09:10 才打开应用」这种情况；超出窗口就不会在晚上开应用时突然签一次。
- **应用必须保持运行**才能触发——桌面端退出后没有后台进程可代为执行。
  错过时刻后重新打开应用，会在 30 分钟内自动补签一次。
- 跨启动去重：执行日期记在应用数据目录的 `schedule_state.json`，重启应用不会在补跑窗口内重复签。
- **「开机自启动」**：设置里另有一个开关（直接操作系统的登录项 / LaunchAgent，不写进 `settings.json`）。
  建议与定时签到一起开启——否则应用不运行时定时永远不会触发。
- 触发与推送结果写入 `scheduler.log`（同目录，权限 0600，保留最近 200 行），排查「为什么没签到」先看它。
- 另有「启动应用时自动签到全部账号」（应用启动即跑一次，与定时互不影响）。

## 签到通知（webhook）

「设置 → 签到通知」里填 webhook 地址（形如 `https://…/hook/<key>`）并开启，就能在签到结束后收到推送。

- 推送内容形如：`WorkBuddy 签到完成：成功 2 / 已签 1 / 失败 1（共 4 个账号）`，
  有失败时附上前 5 条「账号名（手机号）：失败原因」明细——这才是推送里最有价值的信息。
- 可分别开关「定时签到后推送」（默认开）与「手动『全部签到』后推送」（默认关，避免连点刷屏）。
- 内置「**测试推送**」按钮，直接返回推送服务的原始响应，便于自查配置。

实现对齐参考脚本的 `notify_webhook`：`GET {webhook}?message=<消息内容>`（query 需 URL 编码），
**必须带浏览器 User-Agent**，并做 3 次重试（1.5s / 3s 退避）。通知失败只影响推送本身，绝不干扰签到结果。

> 参数名实测（notify-hub，2026-09-12）：`?message=…` → `{"ok":true,"delivered":true}`；
> `?title=…&body=…` 与 `?content=…` 都会被接受但返回 `{"empty":true}`，**内容为空**。
> 另一个坑是 Cloudflare 按 UA 拦截：裸 `Python-urllib` / 空 UA 直接 `403 error code: 1010`。

## 智能接管（WorkBuddy 专用）

独立弹窗（工具栏「智能接管」），把 WorkBuddy 的对话请求接管到本机反代：

```bash
# 接管后 WorkBuddy 的对话实际请求链路：
# WorkBuddy → 127.0.0.1:8787（本应用反代）→ https://copilot.tencent.com/v2/chat/completions
```

- **WorkBuddy 专用**：只监听 `127.0.0.1`、无鉴权 Key（不对外提供通用代理能力）；
  开启时把 `~/.workbuddy/settings.json` 的 `env.CODEBUDDY_BASE_URL` 指向本机，
  关闭 / 换端口 / 应用退出时自动安全摘除（含原子端点切换与重启，不留死端口）。
- **路由策略**：硬指定「优先扣费账号」> 会话粘滞（30 分钟滑动续期，对话中途不换号）>
  「积分最早过期优先」轮换（快照缓存 10 分钟）。指定账号不存在时自动降级为轮换。
- **`/v2` 路径改写**：CLI 在端点覆盖模式下请求的是裸路径 `/chat/completions`，
  而真实网关路由是 `/v2/chat/completions`——代理转发前自动补 `/v2`，否则网关 302 → CLI 报 Empty stream。
- **接管事件日志**：install / uninstall / 重启 / 每条代理请求（含扣费账号名）记入
  应用数据目录 `takeover-journal.jsonl`。**不设条数上限**：日志与「一次接管会话」绑定
  ——开启接管时整份重置，会话之内一条不丢（页面上另有「清空」按钮可手动清）。
- **网络急救**：设置 →「一键诊断 / 一键恢复」。会扫描配置文件、launchd 全局变量、
  shell 启动脚本、接管事件，能识别「桌面端仍持有已摘除端点」这类隐性故障，一键恢复并自动备份被改文件。

> 关键机制：WorkBuddy 桌面端与其长驻 CLI host 只在**启动时**读一次端点配置。
> 因此开启 / 关闭接管时本应用会自动安全重启 WorkBuddy 与长驻 CLI host（收割孤儿进程），
> 老对话才会拿到新链路。

---

## 跨平台说明

- **macOS**：`.dmg` / `.app`。当前 CI 在 `macos-latest`（Apple Silicon）上原生构建 aarch64。
  若需同时支持 Intel，可在 `release.yml` 的 macOS job 加 `--target universal-apple-darwin`
  （并保留已加的 `rustup target add` 步骤）。
- **Windows**：`.msi`（passive 静默安装）。`installMode: passive` 见 `tauri.conf.json`。
- **账号数据安全**：账号 Token 存于应用专属 `AppData` / `Application Support` 目录下的
  `accounts.json`，文件权限设为 `0600`（仅当前用户可读写）。后续可接入系统钥匙串进一步加固。

---

## 已实现的命令（Rust → 前端）

| 命令 | 说明 |
| --- | --- |
| `list_accounts` | 列出全部账号 |
| `import_accounts` | 批量导入账号（「导入本机账号」/「登录新账号」共用）：已存在账号按手机号或 token 识别后**合并补全**凭证，不会重复添加 |
| `export_accounts` / `import_accounts_file` | 账号跨机器迁移：导出全部账号为 JSON（0600 权限落盘）/ 从导出文件导入（同一套合并逻辑） |
| `remove_account` | 删除账号（同时清理该账号的签到日志） |
| `checkin_one` | 对单个账号签到 |
| `checkin_all` | 批量签到全部账号 |
| `discover_local_accounts` | 读取本机 WorkBuddy 登录文件（含昵称/手机号/有效期） |
| `oauth_start` | 登录新账号第一步：申请 state + 授权链接（不重启应用） |
| `oauth_poll` | 登录新账号第二步：轮询授权结果；`done=false` 表示仍在等用户授权 |
| `open_external` | 用系统默认浏览器打开链接（授权页） |
| `get_settings` / `save_settings` | 读写全局设置（保存时校验定时时刻与 webhook） |
| `test_notify` | 向 webhook 发一条测试通知，返回推送服务原始响应 |
| `get_autostart` / `set_autostart` | 读取 / 设置开机自启动（直接操作系统登录项，失败会返回原因） |
| `get_checkin_logs` | 查询签到日志（倒序、可按账号 id 筛选、最多 300 条） |
| `clear_checkin_logs` | 清空签到日志（传 `accountId` 则只清该账号） |
| `refresh_all` | 一键刷新：重拉并持久化全部账号的积分快照 / 签到状态 / 积分余量 |
| `stealth_status` / `stealth_stop` | 读取接管状态（端点是否装上 / 心跳）与立即停止接管（含安全重启） |
| `proxy_routes` | 最近代理路由记录（账号 / 路径 / 是否流式） |
| `net_diagnose` / `net_restore` | 网络急救：只读诊断 / 一键恢复（自动备份被改文件） |
| `restart_workbuddy` | 安全重启 WorkBuddy 与长驻 CLI host（收割孤儿进程） |
| `app_version` | 当前版本号 |
