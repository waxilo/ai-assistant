# Qoder 助手（Tauri 桌面端）

一个用 **Tauri v2 + Rust + React** 实现的 Qoder 桌面助手（当前功能：多账号签到），支持：

- **多账号管理**：账号条目**只读展示**（名称 + 手机号 + 凭证有效期 + 剩余积分），不做手工录入与编辑 —— 凭证一律来自「登录新账号」或「导入本机账号」，避免粘贴错 token。删除账号时会一并清理它的签到日志。
  - 手机号两条路都拿得到：**导入本机账号**读 `auth.v1.dat` 里的 `user.phone`；**登录新账号**走
    `GET /api/v1/userinfo` 的 **`security_mobile`** 字段（官方桌面端 `AuthService.fetchUser()`
    就是这么取的）。2026-09-19 更正：此前认为「userinfo 不含手机号」，于是登录来的账号手机号恒空 ——
    而手机号是跨机账号合并（`broker`）的第一顺位锚点，缺了它就只能退化成按昵称认人。
- **后台常驻（系统托盘）**：点窗口关闭按钮 = 隐藏到系统托盘，**进程不退出**（定时签到、token 续签、本地反代持续生效）；托盘菜单提供「显示主窗口 / 退出」，macOS 点 Dock 图标也会唤回主窗口。真正退出请走托盘菜单「退出」（退出时会自动关闭智能接管）。
- **云端凭证池（跨机器共用账号）**：把本机这批账号整体上传成一个池，管家颁发一串 uuid；别的机器填同一串 uuid 即接上同一池（两边**取并集**，本机独有的不会被删）。刷新与续签始终在本机执行，refresh token **不进任何共享文件** —— 旧版「导出凭证文件」正是因此删除的（几台机器各持一份 refresh token，而官方续签是单链轮换，谁先签就把别人踢下线）。
- **一键签到**：单个账号签到，或「全部签到」批量领取每日积分。
- **定时自动签到**：每天在设定时刻（默认 `09:07`）自动跑一遍「全部签到」，**应用运行期间生效**；错过时刻后 30 分钟内打开应用会自动补签一次，跨启动不会重复签（`schedule_state.json` 记录已执行日期）。配套提供「开机自启动」开关，让定时签到真正能每天生效。
- **签到通知（webhook）**：可配置一个 webhook 地址，签到结束后推送结果汇总（成功 / 已签 / 失败数量 + 失败明细）；可分别开关「定时签到后推送」与「手动全部签到后推送」，并内置「测试推送」按钮自查配置。调度触发与推送结果会记入 `scheduler.log`（保留最近 200 行），便于事后排查「为什么没自动签到」。
- **剩余积分展示**：账号条目显示「剩余积分」，取自主流官方接口 `POST {host}/v2/billing/meter/get-user-resource`（汇总各资源包的 `CycleCapacityRemain*`，与官方 Web 端「计划与用量」同口径，可能是小数）；该接口拿不到时退回 `checkin-status` 的 `total_credits`。注意签到响应里的 `credit` 是**本次获得**（单独显示为「本次 +N」），不是余额。
- **token 自动续签**：导入 / 无感登录时会一并保存 `refreshToken` 与 `expiresAt`。应用启动及常驻期间**每 12 小时**扫描一次，**剩余有效期不足 48 小时即自动换新凭证**（签到前另有兜底判定）；续签失败不阻断签到（仍用旧 token 试一次）。
- **智能接管（Qoder 专用反代）**：在 `127.0.0.1:8789`（可改端口；避开同机 workbuddy-assistant 占用的 8787）起一个 Qoder 专用反代，开启后自动把 Qoder 的对话请求接管到本地——只在**勾选的扣费备选账号**里选号（未勾选的不允许扣费，全不勾 = 全部可用；会话粘滞 + 积分最早过期优先轮换）。页面下方有**接管动态时间线**：开启 / 关闭接管、每个会话开始使用哪个账号、代理错误，一目了然。详见 [智能接管](#智能接管qoder-专用)。
- **账号获取（两条通道，无手工录入）**：
  - **导入本机账号**：直接读 Qoder 写在本机的凭据文件 `auth.v1.dat`（Chromium `safeStorage` / **OSCrypt** 加密，**两个平台都能解**：Windows 走 DPAPI、macOS 走系统钥匙串），**不需要应用运行、也不需要调试端口**，一次就能拿到 token + 昵称 + 手机号 + refresh token（已存在的账号会合并补全凭证，不会重复添加）。
  - **登录新账号**：走 Qoder **设备授权流**（`/device/selectAccounts` → 浏览器扫码 → 轮询 `/api/v1/deviceToken/poll`），**不重启、不打断当前 Qoder、不改动本机登录文件**，能主动签发**任意新账号**的凭证、昵称与手机号（与「导入本机账号」互补：后者只能收编已经登录过的那个）。
- **两套部署（国际版 / 国内版）都支持，且可切换**：Qoder 有**两套互不相通**的部署
  —— 国际版登录 `qoder.com` / 接口 `openapi.qoder.sh` / CLI 目录 `~/.qoder` / 应用 `Qoder.app`，
  国内版登录 `qoder.cn` / 接口 `openapi.qoder.com.cn` / `~/.qoder-cn` / `Qoder CN.app`。
  账号、积分、签到活动两边各自独立，所以「哪个区域」是账号的一部分（合并键是「区域 + 手机号」）。
  域名、本地目录、客户端路径与进程名**全部集中在 `src-tauri/src/region.rs` 一处**按区域取，
  别处只消费、不自己拼 —— 少改一处不会报错，只会表现成「这个功能在另一个区域上悄悄用错域」。
  界面上的区域清单由后端的 `regions` 命令给出（中文名与「OpenAPI 在哪个域」只该有一处定义）。
- **GitHub Release 自动更新**：内置 `tauri-plugin-updater`，点击「检查更新」即可从 Release 拉取并安装新版本。
- **积分日报（按天 + 逐小时）**：按**自然日**统计积分消耗与新增，每天一条，展开可见**每小时**明细（总览柱状图 + 逐账号列表）。
  口径是资源包**累计量**的差值（`CapacityUsed` / `CapacitySize`），不是「抓余额算涨跌」——
  所以同一天「先消耗后签到」不会互相抵消，**多个客户端同时消耗也都能统计到**。
  采样时增量就落进「采样时刻所属的小时」，因此小时之和恒等于当天合计、相邻两天可直接相加。
  数据在应用数据目录的 `credit_ledger.json`（台账 + 60 天小时桶）与 `credit_reports.json`（日报，最近 400 条），均 0600。
- **签到日志（按账号查看）**：每次签到结果（账号 / 时间 / 结果 / 积分 / 详情）落库到应用数据目录的 `checkin_logs.json`（权限 0600，保留最近 2000 条）。入口在**每个账号条目上的「日志」按钮**，面板按时间倒序简单列出该账号记录，可一键清空。「本次额度」那一格下面还会带一行 **`至 MM-DD HH:mm`** —— 那是这笔积分通过发放凭据拿到的真实到期时间（已领的账号由幂等回放取回）。
- **资源包到期是「三态」**，界面上分别说：有到期日（`N 天后过期` / `已过期`）· 服务端明说不过期（`不过期`）· 响应里没给到期信息（未知，退回「查看资源包」）。
  - 附加额度（签到送的那 100 Credits）的到期时间**不在额度接口里**：`qoderUsage.expiresAt` 是整个额度概览的到期（= 计划周期终点，免费号还是「无期限」哨兵），旧版把它同时当成两个槽位的到期，于是界面对免费号显示「不过期」。真实日期来自**发放凭据** —— 领取接口 `POST …/{campaignId}/claim` 响应里的 `expiresAt`（实测 `claimedAt + 30 天`，见 `qoder_api::ClaimReceipt`）。已领取的活动**幂等回放**（`replayed: true`）同样返回这张凭据，所以签到路径每天都会顺手把日期取回来落进台账（`ledger::note_grant_expiry`）。
  - 之所以要专门记一笔：免费账号的 usage 响应里 `qoderUsage.expiresAt` 是 **`9999-12-31` 哨兵**（同一账号在 `/api/v2/user/plan` 里的 `end_date` 是 `0`，
    同一个意思、两处两套写法），把它当普通时间戳透传下去，界面就会出现「到期 9999-12-31」和「2922776 天后过期 100」。
    归一在**解析边界**做一次（`ledger::normalize_expiry` → `never_expires: bool`），投影旧台账时再走一遍，别在别处重判一次哨兵。
- 跨平台：macOS（`.app` / `.dmg`）与 Windows（`.msi`）。

> 签到逻辑参考自 `qoder-checkin` 与 `WorkDaddy` 的 `checkin-result.js`：
> 接口 `POST {host}/billing/meter/daily-checkin`（兼容 `/v2/...`），`code===0` 视为成功，
> `code===10001` 且文案命中“已签到”视为今日已签（幂等成功）。

---

## 目录结构

```
QoderAssistant/
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
│       ├── auth_file.rs    # 读本机 Qoder 登录文件（含昵称/手机号）
│       ├── checkin.rs      # 签到 HTTP 逻辑 + host/iss 推断 + 结果判定 + 剩余积分查询
│       ├── oauth.rs        # 「登录新账号」设备授权流（PKCE + 轮询，纯 HTTP）
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

跑后端单测（纯本地，不碰你本机的 Qoder、不发网络请求）：

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

> **图标（字母 Q）有「两个出口、一套几何」，改一处必须同时改另一处**：
> ① `scripts/make-icon.mjs` 画 1024 的源 PNG（`tauri icon` 吃它生成各平台套件）；
> ② `src/components/Icons.tsx` 的 `IconQ` 是应用内侧栏那块蓝方块里的同一个字形。
> 后者的每个数都从前者按 `12/270` 折算而来（换算表写在 `IconQ` 的注释里）。
> 只改一边的后果是应用图标与应用内标记**长成两个东西**——不会报错，只有并排看才发现。
> 调完记得**按 16/24/32/64 看一眼缩略图**（Dock 与列表里都是小尺寸），
> 以及比一下「字形外接半径 ÷ 蓝方块半边长」这个比值（源 PNG 是 `0.6466`，`IconQ size=22` 在 34px 的 `.logo` 里是 `0.6462`）。

> macOS 首次构建若提示「无法验证开发者」，在「系统设置 → 隐私与安全性」中点「仍要打开」。
>
> `tauri build` 结尾若报 **`A public key has been found, but no private key`**：因为
> `tauri.conf.json` 里已内嵌真实 `updater.pubkey` 且 `createUpdaterArtifacts=true`，
> 但没有设置签名私钥。**这不影响 `.app` / `.dmg` 产出**，只是 updater 产物无法签名；
> 按下面「开启 GitHub 自动更新」配好密钥后即消失。
>
> 若 `.dmg` 步骤偶发失败（迁移后旧 `src-tauri/target` 残留绝对路径时遇到过），
> 先 `cd src-tauri && cargo clean` 后重跑；仍失败可用 `npm run build:dmg`
> （`scripts/build-dmg.sh`，纯 `hdiutil`、零依赖）兜底产出可分发的 `.dmg`。

---

## 应用内自动更新

更新链路**已配好并跑通**，日常发版只需打 tag（见下）。本节记录它怎么接的 ——
换仓库或换签名密钥时才需要动。

当前配置（`src-tauri/tauri.conf.json`）：

| 项 | 值 |
|---|---|
| `plugins.updater.endpoints` | `https://github.com/waxilo/ai-assistant/releases/download/qoder-latest/latest.json` |
| `plugins.updater.pubkey` | 已内嵌真实公钥（与 traework / workbuddy 共用同一把 updater 签名密钥） |
| 发布通道 | 固定 tag `qoder-latest`，与 traework / workbuddy 各自独立 |
| 触发工作流 | 仓库根 `.github/workflows/release-qoder.yml` |

> **版本号各自演进、互不参照。** 三款助手共用一套代码但发布通道独立，版本号也各走各的线：
> QoderAssistant 从 **`0.1.0`** 起算自己的版本线，**不跟随** WorkBuddyAssistant 的数字。
> 新增应用或从别的应用 fork 时，**不要**沿用对方的版本序列。

> ⚠️ 历史备注：本应用曾以 `0.1.36` 首次发布（那是照搬 WorkBuddy 当时 `0.1.35` 的结果，
> 并非自己的版本历史），现重新起算为 `0.1.0`。已安装的 `0.1.36` 比它高，**不会**通过
> 「检查更新」降级，需要手动装一次新包；之后再发版就都在自己的 `0.1.x` 线上正常更新了。

### 发版流程

1. **改版本号**，以下三处必须一致（应用内「关于」显示的是 `Cargo.toml` 的 `CARGO_PKG_VERSION`）：

   | 文件 | 字段 |
   |---|---|
   | `src-tauri/tauri.conf.json` | `version` |
   | `src-tauri/Cargo.toml` | `[package] version` |
   | `package.json` | `version` |

   改完跑一次 `npm install --package-lock-only` 同步 `package-lock.json`；
   `src-tauri/Cargo.lock` 里本应用那一条由 `cargo` 自动跟进。

2. **打 tag 推送**（版本号以 tag 为准，会覆盖 `tauri.conf.json`）：

   ```bash
   git tag qoder-v0.1.1
   git push origin qoder-v0.1.1
   ```

3. CI 在 macOS / Windows 两个 runner 上构建、用私钥签名更新产物，按
   `QoderAssistant-macos-<版本>` / `QoderAssistant_windows_<版本>` 重命名，
   再 delete+recreate 固定 tag `qoder-latest` 的 Release
   （`latest.json` + 各平台安装包 + `.sig`）。`latest.json` 由根目录
   `scripts/build-latest.mjs` 用各 `.sig` 拼装（Tauri v2 的 CLI 不再自动产出 manifest）。

也可在 Actions 页面手动 `workflow_dispatch`（此时用 `tauri.conf.json` 里的版本号）。

### 需要配置的 Secrets

仓库 **Settings → Secrets and variables → Actions**：

| Secret | 用途 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | updater 签名私钥（`npx tauri signer generate` 生成，公钥已内嵌在 `tauri.conf.json`） |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥密码，无密码可不配 |

> 注意：更新只在**已签名的 Release** 之间生效。本地 `npm run tauri build` 未设置
> `TAURI_SIGNING_PRIVATE_KEY` 时不会生成 `.sig`，此类构建包无法用于自动更新。

---

## 添加账号

工具栏有两个入口：**登录新账号**（加新号）/ **导入本机账号**（读本机已登录的）。
没有「+ 添加账号」——不支持手工粘贴 token，账号条目也不可编辑。

### 1. 导入本机账号

Qoder 把**当前登录的那一个账号**写在自己的 Electron 用户数据目录里。
两套部署各有自己的目录，**两个都会被扫**（每条结果因此都带着「来自哪个区域」）：

| 平台 | 国际版 | 国内版 |
| --- | --- | --- |
| macOS | `~/Library/Application Support/com.qoder.app.stable/auth.v1.dat` | `~/Library/Application Support/com.qodercn.app.stable/auth.v1.dat` |
| Windows | `%APPDATA%\com.qoder.app.stable\auth.v1.dat` | `%APPDATA%\com.qodercn.app.stable\auth.v1.dat` |

它不是明文 JSON，而是 Chromium `safeStorage`（**OSCrypt**）加密的二进制，
而**两个平台的封锁与密钥都不一样**（两条都是 Chromium 的既有实现，不是本项目发明的格式）：

| | Windows | macOS |
| --- | --- | --- |
| 二进制布局 | `"v10"` + nonce(12B) + **AES-256-GCM** 密文 + tag(16B) | `"v10"` + **AES-128-CBC**（IV = 16 个空格，PKCS#7 填充） |
| 密钥放在哪 | 同目录 `Local State` 的 `os_crypt.encrypted_key` | 系统钥匙串 `Qoder App Safe Storage` / `Qoder CN App Safe Storage`（账号名 `<…> Key`） |
| 密钥怎么来 | `DPAPI` 前缀 + `CryptUnprotectData`（当前用户）→ **32B 原始密钥** | 钥匙串密码 → `PBKDF2-HMAC-SHA1`(`saltysalt`, 1003 轮) → **16B** |

两边解出来的是同一份 JSON：`token` / `refreshToken` / 两个有效期 / `user.{id,name,phone}`，
**一次拿全**。

> macOS 上读钥匙串走的是 `/usr/bin/security`，而不是应用自己调 Keychain API：钥匙串条目的
> ACL 绑定「请求者」，而 `security` 是 Apple 签名、路径固定的可执行文件，用户点一次
> 「始终允许」就永久生效；应用自己调的话，调试构建每次重建（ad-hoc 签名，cdhash 会变）
> 都要重新点一次。首次读取若弹出系统授权，点「始终允许」即可。

> **历史（2026-09-19 纠正）**：这里曾写着「macOS 的密钥在 Keychain 里、**端外解不出**」——
> 那是把「不去解钥匙串」当成了「解不了」。于是 macOS 上这条通道恒为空，
> 而界面还会补一句「请先在 Qoder 桌面端登录一次」，把已经登录的用户指去重登。

因此：

- **不需要 Qoder 正在运行**，也不用改启动方式（不涉及 `--remote-debugging-port`）；
- **每套部署**都是单账号模型：`auth.v1.dat` 只存当前登录那一个，切号 / 重登会覆盖它，
  所以这里最多列出 **2 条**（两套部署各一条），不是多账号列表；
- 列表会标出「当前登录」「有效期至 … / 剩 N 天 / 已过期」；
- 已存在的账号按「区域 + token」识别后合并补全，不会重复添加 ——
  只看 token 是不够的：两套部署签发的 token 互不相通，区域是这条凭据的一半身份。

> 读取本身**不改动任何文件**；续签成功后的写回是另一条路径，见「token 续签」。

### 2. 登录新账号

点工具栏「**登录新账号**」（独立入口，不在导入弹窗里）。走 Qoder 的**设备授权流（device flow）**
—— 官方桌面端自己用的就是这套 —— **不重启、不打断当前 Qoder，也不改动本机登录文件**，
是加第二个 / 第三个账号最省事的路子：

1. **先选「登到哪个区域」**（国际版 / 国内版），再点「打开授权页并开始」。
   这一步非有不可：授权链接本身**不含**区域信息，只有发起方知道用户点的是哪个入口；
   选错了只会把账号收进另一个区域，不影响本机已登录的客户端。
2. 点「打开授权页并开始」→ 本工具本地生成 PKCE 材料（`verifier` / `challenge=S256` /
   `nonce` / `machine_id`），再用**系统浏览器**打开
   （`{auth_base}` 按区域取：国际版 `https://qoder.com` / 国内版 `https://qoder.cn`）：
   `{auth_base}/device/selectAccounts?challenge=…&challenge_method=S256&nonce=…&machine_id=…&client_id=…`
   服务端会自己 302 到 `{auth_base}/users/sign-in?biz_variant=qoder&oauth_callback=…`。
3. 在浏览器里完成登录（扫码即可）。本工具每 2 秒轮询一次
   `GET {openapi_base}/api/v1/deviceToken/poll?nonce=…&verifier=…&challenge_method=S256`
   （国际版 `openapi.qoder.sh` / 国内版 `openapi.qoder.com.cn`）；
   未授权时返回 `HTTP 404 {"errorCode":"NotFound"}` —— **这是正常等待态，不是报错**。
4. 拿到 `token` + `refresh_token` 后自动拉 `GET /api/v1/userinfo`，把**昵称 / uid / 手机号
   （`security_mobile`）** 一并显示；
   点「添加为账号」入库（入库时带上这一步选的区域），或连点「再登一个」继续加号。

> **区域不是「随便填的域」**：两套部署的域、目录、客户端都在 `region.rs` 里写死，界面只能
> 在这两个之中选一个。（历史注：旧版那个下拉框列的是 CodeBuddy 时代的四个域，而那个参数在后端
> 从来就被忽略 —— 一个不起作用的选项比没有更糟，所以当时删掉了它。现在它重新存在，是因为
> 它真的会决定「请求打哪个域、账号收进哪个区域」。）
>
> 授权链接里**不填** `redirect_uri`。官方填的是 `qoder-app://`，那是官方桌面端自己注册占用的
> scheme（`lsregister` 里 `qoder-app:` 归 `/Applications/Qoder.app`）；照抄它会让浏览器在授权
> 结束时把 **Qoder 桌面应用**拉起来。我们靠轮询取凭证，不需要浏览器回调。
>
> 10 分钟未完成授权会自动判定超时，重新点一次即可。

---

## token 续签

账号的 access token 会过期（官方签发 60 天左右）。导入 / 无感登录时本工具会一并保存
`refreshToken` 与 `expiresAt`，之后：

- **自动续签（常驻）**：后台调度线程**启动后立刻扫一次，之后每 12 小时扫一遍**全部账号；
  只要剩余有效期**不足 48 小时**就调
  `POST https://openapi.qoder.sh/api/v1/deviceToken/refresh`
  （`Authorization: Bearer <旧 token>` + JSON `{"refresh_token":"…"}`，另带 `Cosy-ClientType` 身份头）
  换新凭证并写回 `accounts.json`。续签与「定时签到」开关无关——它是保命操作，
  不该因为没设定时签到就被关掉。结果记 `scheduler.log`，前端弹提示并刷新列表。
- **签到前兜底**：每次签到（手动 / 批量 / 定时）前也会再判一次阈值，避免「扫描刚过、签到时刚好过期」。
- 条目上**没有手动「续签」按钮**——续签完全自动（后台扫描 + 签到前兜底），条目只读。
- 条目会显示凭证有效期（已过期标红，7 天内提示）。
- **写回 Qoder 自己的凭据文件**：续签成功后，新 token / refreshToken 与有效期会**原子写回**
  `auth.v1.dat`（临时文件 + rename，避免官方客户端读到半个文件），免得官方与本工具各持一条
  已被轮换掉的 refresh token。写回用的是**与读取同一把钥匙、同一个平台封锁**重新加密，
  并在替换前**解一遍自检** —— macOS 的 CBC 段没有完整性保护，
  这道自检是「写进去就再也读不回来」之前的唯一拦截。
  两个时间戳字段**按原文的写法**回写（官方是 ISO-8601 字符串 `"2026-10-19T03:38:47Z"`，
  写成数字会让客户端读到另一种类型）。
- 续签失败**不阻断**签到，仍用旧 token 试一次，由签到结果给出明确提示；
  若 refresh token 本身已失效，重新「导入本机账号」或「登录新账号」即可。

### 为什么不做「浏览器本地存储扫描」与「调试端口抓包」

两条通道都做过，又都**主动移除**了：

- **浏览器本地存储扫描**：实测已证实不可用。网页端（qoder.cn）的登录态是
  **httpOnly 加密 Cookie**（`www.qoder.cn` 下是 `session` / `KEYCLOAK_SESSION`，均为加密存储），
  其 localStorage 里只剩 SDK 监控用的 `beacon_config` / `__BEACON_*_session_storage_key`
  等无意义键，全机扫描 0 命中；参考实现（`qoder-checkin/extract_token.mjs`、WorkDaddy）
  也从不读浏览器存储。
- **调试端口抓包（CDP）**：需要以 `--remote-debugging-port` 重启 Qoder
  （会关掉正在使用的对话窗口），而且只能拿到**当前登录那一个**账号的 token。
  相比之下「导入本机账号」不重启、不打断、还能一次拿全元信息，
  「登录新账号」还能主动签发任意新账号——CDP 已无不可替代的用途。

---

## 定时自动签到

在「设置」里开启「每天定时自动签到全部账号」并选好时刻（默认 `09:07`，与参考脚本
`qoder_checkin.py` 的 launchd 定时一致）。

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

- 推送内容形如：`Qoder 签到完成：成功 2 / 已签 1 / 失败 1（共 4 个账号）`，
  有失败时附上前 5 条「账号名（手机号）：失败原因」明细——这才是推送里最有价值的信息。
- 可分别开关「定时签到后推送」（默认开）与「手动『全部签到』后推送」（默认关，避免连点刷屏）。
- 内置「**测试推送**」按钮，直接返回推送服务的原始响应，便于自查配置。

实现对齐参考脚本的 `notify_webhook`：`GET {webhook}?message=<消息内容>`（query 需 URL 编码），
**必须带浏览器 User-Agent**，并做 3 次重试（1.5s / 3s 退避）。通知失败只影响推送本身，绝不干扰签到结果。

> 参数名实测（notify-hub，2026-09-12）：`?message=…` → `{"ok":true,"delivered":true}`；
> `?title=…&body=…` 与 `?content=…` 都会被接受但返回 `{"empty":true}`，**内容为空**。
> 另一个坑是 Cloudflare 按 UA 拦截：裸 `Python-urllib` / 空 UA 直接 `403 error code: 1010`。

## 智能接管（Qoder 专用）

独立弹窗（工具栏「智能接管」），把 Qoder 的对话请求接管到本机反代：

```bash
# 接管后 Qoder 的对话实际请求链路（机制已逆向确认，见 basedata/20260918_Qoder接管机制逆向.md）：
# Qoder 桌面端 → 拉起长驻 CLI host（claude 式 agent runtime，模型请求由它发出）
#            → https://127.0.0.1:8789（本应用反代）→ 接管区域的模型网关
#              国际版 https://api2-v2.qoder.sh · 国内版 https://gateway.qoder.com.cn
#
# 杠杆是进程环境变量 QODER_MODEL_SERVER_HOST（CLI 里 gtn() 读它；**scheme 被写死成 https**，
# 所以本地反代必须提供 TLS —— 与 CodeBuddy「写 settings.json 一个键」的做法完全不同）。
```

> ⚠️ **改造中**：以下描述仍是对齐 workbuddy 旧机制的实现现状，Rust 侧尚未切到上面的环境变量机制。
> 切换点是 `stealth.rs` 的「端点存储后端」（租约 / 心跳 / 事件日志骨架可原样复用）。

- **Qoder 专用**：只监听 `127.0.0.1`、无鉴权 Key（不对外提供通用代理能力）；
  开启时把**接管区域**那个 CLI 配置目录（`~/.qoder` 或 `~/.qoder-cn`）的
  `settings.json` 里 `env.CODEBUDDY_BASE_URL` 指向本机，
  关闭 / 换端口 / **换区域** / 应用退出时自动安全摘除（含原子端点切换与重启，不留死端口）。
- **接管目标区域**：控制条上的「区域」选择器决定三件事 —— 端点写进哪套客户端的配置、
  请求转发到哪个模型网关、以及扣费账号**从哪个池里选**（跨区域的 token 在对方网关上无效，
  所以扣费池与模型清单都只列该区域）。开启期间换区域会走安全切换流程
  （摘旧区域的端点 → 重启受影响的客户端 → 装进新区域），并把扣费池重置为「全部」；
  关闭期间换区域只是把设置存下来，不会惊动任何进程。
  两套部署可以同时装着，而「现在该接管哪一个」是用户的意图、不是能从磁盘猜出来的事实，
  所以它是一次**显式选择**（`settings.takeover_region`）。选中区域**一个账号都没有**时，
  控制条下方会出现一行提示 + 一键切到有账号的那个区域（只提示，**不替用户改设置**）。
- **限流切换的模型清单**：三层来源 —— Qoder 模型目录（联网，缓存 1 小时）→ 落盘快照
  （按区域分开存）→ 本机 Qoder 的痕迹（`~/.qoder[cn]/.models/default` + 会话日志，
  只含这台机器用过的模型）。第 1 层**实测基本永远拉不到**，两个区域的失败形态还不一样：

  | 区域 | 宿主 | 常规 HTTPS 客户端 |
  |---|---|---|
  | 国际版 | `api3.qoder.sh` | 空 `404`（任何路径、带不带认证都一样） |
  | 国内版 | `gateway.qoder.com.cn` | `503`（阿里云 ALB 的 HTML 页） |

  国内版那条是照着 CLI 日志逐项复现后仍然失败的：用日志里那两个 httpdns 落点 IP `--resolve`、
  HTTP/1.1 与 2、GET 与 POST、带与不带 UA 一律 503。CLI 自己把整份目录缓存在
  `.models/<uid>/catalog-v6`，但那是 `QMC\x01` 开头的密文（熵 7.997），没有 CLI 手里的密钥解不开。

  **由此推出：凭证对这份清单几乎没有价值，「这个区域还没有账号」不是错误。**
  三层里两层是纯本地的，没账号只是让第 1 层缺席 —— 界面照常显示清单，
  并在胶囊上标出来源（`· 本机记录`）、在弹窗里写明「第 1 层为什么没结果」。
  路由侧的免费集合与界面同源（`models::free_ids`），避免「界面显示免费、路由却不切换」。
- **路由策略**：硬指定「优先扣费账号」> 会话粘滞（30 分钟滑动续期，对话中途不换号）>
  「积分最早过期优先」轮换（快照缓存 10 分钟）。指定账号不存在时自动降级为轮换。
- **`/v2` 路径改写**：CLI 在端点覆盖模式下请求的是裸路径 `/chat/completions`，
  而真实网关路由是 `/v2/chat/completions`——代理转发前自动补 `/v2`，否则网关 302 → CLI 报 Empty stream。
- **接管事件日志**：install / uninstall / 重启 / 每条代理请求（含扣费账号名）记入
  应用数据目录 `takeover-journal.jsonl`。**不设条数上限**：日志与「一次接管会话」绑定
  ——开启接管时整份重置，会话之内一条不丢（页面上另有「清空」按钮可手动清）。
- **网络急救**：设置 →「一键诊断 / 一键恢复」。会扫描配置文件、launchd 全局变量、
  shell 启动脚本、接管事件，能识别「桌面端仍持有已摘除端点」这类隐性故障，一键恢复并自动备份被改文件。

> 关键机制：Qoder 桌面端与其长驻 CLI host 只在**启动时**读一次端点配置。
> 因此开启 / 关闭接管时本应用会自动安全重启 Qoder 与长驻 CLI host（收割孤儿进程），
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
| `list_accounts` | 列出全部账号（每条都带它所属的区域） |
| `import_accounts` | 批量导入账号（「导入本机账号」/「登录新账号」共用）：按「**区域 + 手机号 / token**」识别后**合并补全**凭证，不会重复添加 |
| `remove_account` | 删除账号（同时清理该账号的签到日志） |
| `checkin_one` | 对单个账号签到 |
| `checkin_all` | 批量签到全部账号 |
| `refresh_all` | 一键刷新：重拉并持久化全部账号的积分快照 / 签到状态 / 积分余量 |
| `discover_local_accounts` | 读取本机 Qoder 登录文件（两个 profile 目录都扫；含区域/昵称/手机号/有效期） |
| `oauth_start` | 登录新账号第一步：**按区域**申请 state + 授权链接（不重启应用） |
| `oauth_poll` | 登录新账号第二步：轮询授权结果；`done=false` 表示仍在等用户授权 |
| `open_external` | 用系统默认浏览器打开链接（授权页） |
| `broker_upload` / `broker_link` / `broker_unbind` / `broker_state` | 云端凭证池：上传成一个池并拿 uuid / 绑定别处的 uuid / 解绑 / 只读状态。四个都是**池级**命令，不带账号 id |
| `get_settings` | 读全局设置（含**接管目标区域** `takeover_region`） |
| `regions` | 区域清单（国际版 / 国内版的中文名与说明）：界面上「区域」的**唯一来源** |
| `save_settings` | 保存设置（校验定时时刻与 webhook）；拓扑类字段（启停 / 端口 / 区域）在接管开启时会被拒，必须走 `apply_settings` |
| `apply_settings` | 原子应用设置；接管启停 / 换端口 / **换区域**时会走安全切换流程（摘端点 + 重启受影响的客户端） |
| `test_notify` | 向 webhook 发一条测试通知，返回推送服务原始响应 |
| `get_autostart` / `set_autostart` | 读取 / 设置开机自启动（直接操作系统登录项，失败会返回原因） |
| `get_checkin_logs` | 查询签到日志（倒序、可按账号 id 筛选、最多 300 条） |
| `clear_checkin_logs` | 清空签到日志（传 `accountId` 则只清该账号） |
| `credit_briefing` / `credit_briefing_clear` / `credit_briefing_enable` | 积分简报：读日条目（= 当天时条目之和，现算）/ 清空历史 / 开启（清历史 + 采一次样只对齐基线） |
| `stealth_status` | 读取接管状态（端点是否装上 / 心跳 / 属于哪个区域 / 该做什么）。停止接管走 `apply_settings(proxy_enabled=false)` |
| `takeover_events` / `takeover_events_clear` | 接管事件流（新的在前）/ 清空 |
| `free_models` | 限流切换支持的模型清单（三层来源：Qoder 目录 / 落盘快照 / 本机痕迹）；**按区域**取，快照也分区域存。不要求该区域有账号（无账号只是跳过联网那层，`note` 里写明原因） |
| `net_diagnose` / `net_restore` / `reveal_path` | 网络急救：只读诊断 / 一键恢复（自动备份被改文件，并清理两个区域的残留端点）/ 在文件管理器里定位 |
| `update_accelerated` | 加速下载更新包（多镜像源 + 签名自验） |
| `app_version` | 当前版本号 |
