# Qoder 助手（Tauri 桌面端）

一个用 **Tauri v2 + Rust + React** 实现的 Qoder 桌面助手（当前功能：多账号签到），支持：

- **多账号管理**：账号条目**只读展示**（名称 + 手机号 + 凭证有效期 + 剩余积分），不做手工录入与编辑 —— 凭证一律来自「登录新账号」或「导入本机账号」，避免粘贴错 token。删除账号时会一并清理它的签到日志。
  - 手机号两条路都拿得到：**导入本机账号**读 `auth.v1.dat` 里的 `user.phone`；**登录新账号**走
    `GET /api/v1/userinfo` 的 **`security_mobile`** 字段（官方桌面端 `AuthService.fetchUser()`
    就是这么取的）。2026-09-19 更正：此前认为「userinfo 不含手机号」，于是登录来的账号手机号恒空 ——
    而手机号是跨机账号合并（`broker`）的第一顺位锚点，缺了它就只能退化成按昵称认人。
- **后台常驻（系统托盘）**：点窗口关闭按钮 = 隐藏到系统托盘，**进程不退出**（定时签到、token 续签、本地反代持续生效）；托盘菜单提供「显示主窗口 / 退出」，macOS 点 Dock 图标也会唤回主窗口。再点一次应用图标不会另开一份（单实例守卫，Windows 上尤其明显），而是唤回已有窗口。真正退出请走托盘菜单「退出」（退出时会自动关闭智能接管）。
- **云端凭证池（跨机器共用账号）**：把本机这批账号整体上传成一个池，管家颁发一串 uuid；别的机器填同一串 uuid 即接上同一池（两边**取并集**，本机独有的不会被删）。刷新与续签始终在本机执行，refresh token **不进任何共享文件** —— 旧版「导出凭证文件」正是因此删除的（几台机器各持一份 refresh token，而官方续签是单链轮换，谁先签就把别人踢下线）。跨机认人有**两级锚点**：「同区域同人」（key 相等）与「**同一份凭证**」（access / refresh token 相等，兜住手机号后补、昵称被改导致的 key 漂移）；同一凭证的漂移副本会被从池里清掉，本机重复条目在读账号时自动收敛 —— 详见 [跨机认人](#跨机认人为什么不能只看-key)。
- **一键签到**：单个账号签到，或「全部签到」批量领取每日积分。
- **定时自动签到**：每天在设定时刻（默认 `09:07`）自动跑一遍「全部签到」，**应用运行期间生效**；错过时刻后 30 分钟内打开应用会自动补签一次，跨启动不会重复签（`schedule_state.json` 记录已执行日期）。配套提供「开机自启动」开关，让定时签到真正能每天生效。
- **签到通知（webhook）**：可配置一个 webhook 地址，签到结束后推送结果汇总（成功 / 已签 / 失败数量 + 失败明细）；可分别开关「定时签到后推送」与「手动全部签到后推送」，并内置「测试推送」按钮自查配置。调度触发与推送结果会记入 `scheduler.log`（保留最近 200 行），便于事后排查「为什么没自动签到」。
- **剩余积分展示**：账号条目显示「剩余积分」，取自主流官方接口 `POST {host}/v2/billing/meter/get-user-resource`（汇总各资源包的 `CycleCapacityRemain*`，与官方 Web 端「计划与用量」同口径，可能是小数）；该接口拿不到时退回 `checkin-status` 的 `total_credits`。注意签到响应里的 `credit` 是**本次获得**（单独显示为「本次 +N」），不是余额。
- **token 自动续签**：导入 / 无感登录时会一并保存 `refreshToken` 与 `expiresAt`。应用启动及常驻期间**每 12 小时**扫描一次，**剩余有效期不足 48 小时即自动换新凭证**（签到前另有兜底判定）；续签失败不阻断签到（仍用旧 token 试一次）。
- **智能接管（Qoder 专用反代）**：在 `127.0.0.1:8789`（可改端口；避开同机 workbuddy-assistant 占用的 8787）起一个 Qoder 专用反代，开启后自动把 Qoder 的对话请求接管到本地——只在**勾选的扣费备选账号**里选号（未勾选的不允许扣费，全不勾 = 全部可用；会话粘滞 + 积分最早过期优先轮换）。页面下方是**接管动态**：只列对客通知（开启 / 关闭接管、**这轮对话由哪个账号提供**、限流切换、异常），请求级细节落在调试日志、界面上「查看日志」一键定位。详见 [智能接管](#智能接管qoder-专用)。
- **账号获取（两条通道，无手工录入）**：
  - **导入本机账号**：直接读 Qoder 写在本机的凭据文件 `auth.v1.dat`（Chromium `safeStorage` / **OSCrypt** 加密，**两个平台都能解**：Windows 走 DPAPI、macOS 走系统钥匙串），**不需要应用运行、也不需要调试端口**，一次就能拿到 token + 昵称 + 手机号 + refresh token（已存在的账号会合并补全凭证，不会重复添加）。
  - **登录新账号**：走 Qoder **设备授权流**（`/device/selectAccounts` → 浏览器扫码 → 轮询 `/api/v1/deviceToken/poll`），**不重启、不打断当前 Qoder、不改动本机登录文件**，能主动签发**任意新账号**的凭证、昵称与手机号（与「导入本机账号」互补：后者只能收编已经登录过的那个）。
- **两套部署（国际版 / 国内版）都支持，且可切换**：Qoder 有**两套互不相通**的部署
  —— 国际版登录 `qoder.com` / 接口 `openapi.qoder.sh` / CLI 目录 `~/.qoder` / 应用 `Qoder.app`，
  国内版登录 `qoder.cn` / 接口 `openapi.qoder.com.cn` / `~/.qoder-cn` / `Qoder CN.app`。
  账号、积分、签到活动两边各自独立，所以「哪个区域」是账号的一部分（合并键 = **区域 +**
  （手机号 → 昵称 → id），另有「同一份 token」作兜底锚点，见 [跨机认人](#跨机认人为什么不能只看-key)）。
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
│       ├── proxy.rs        # 智能接管反代（127.0.0.1 专用透传 + 优先扣费账号/粘滞/最旧积分路由；首字节探测 TLS，用 rustls 终止握手）
│       ├── certs.rs        # 接管用的本地 TLS 材料：自签 CA + 只签给 127.0.0.1/localhost 的叶证书（私钥 0600）
│       ├── patch.rs        # 接管注入补丁器：改客户端 app.asar.unpacked 里那份 worker 产物（端点 + 本地 CA + 指纹自愈 + 精确剥离）
│       ├── stealth.rs      # 接管 Fuse：端点装卸、租约、接管事件日志（takeover-journal.jsonl）
│       ├── netfix.rs       # 网络急救：诊断（配置文件 / launchd / shell 启动脚本 / worker 产物注入）+ 一键恢复 + 自动备份
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
| `MACOS_CERT_P12_QODER` | macOS 代码签名证书（base64 的 p12，内含叶证书 + 私钥 + 根 CA）。来源：`~/.qoder-signing/ci-cert-p12.b64` |
| `MACOS_CERT_PASSWORD_QODER` | 该 p12 的密码。来源：`~/.qoder-signing/ci-cert-password.txt` |

> ⚠️ macOS 证书是**必需项**：`tauri.conf.json` 里写死了 `bundle.macOS.signingIdentity`，
> 缺证书时 `tauri build` **直接失败**（故意如此 —— 静默产出未签名的包会让上面那个
> 「App 管理静默拒绝」的问题在用户机器上复发）。为什么必须有固定身份见
> 「智能接管 → 前置条件」。

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

## 跨机认人：为什么不能只看 key

云端凭证池（`cred-broker`）用一个 `key` 做跨机身份锚点：`区域:` + （手机号 → 昵称 → 本地 id）。
这一版之前**只看它**，于是踩出一个真实事故（2026-09-19）：账号列表里凭空多出一条
「国际版、同 token、无手机号、从没签过到」的重复账号。

事故由三个可独立复现的缺陷叠成：

1. **服务端不存区域**：`cred-broker` 的 `normalizeItem` 返回值里根本没有 `region` 字段，
   池里所有条目的区域都是空的 → 客户端被 `#[serde(default)]` 兜成**国际版**。
   于是一条国内版凭证只要在池里被当成新账号收养一次，就变成「国际版」的重复账号。
2. **key 会漂移**：上传时账号还没补上手机号（手机号是导入后 `fill_phone_if_missing` 补的），
   key 落在昵称上（`cn:nick0494015252`）；补上手机号后本机 key 变成 `cn:19174256652`
   → **池里那条旧 key 再也认不出本机账号** → 收养成新账号。
3. **合并只比 key 字符串**，从不看 token：同一份凭证在池里以多个 key 存在时，
   本机会变成多条记录；而它们区域不同（cn / global），之后再怎么导入都不会合并。

现在的规则（`src-tauri/src/broker.rs` 的 `claims` / `is_drifted_duplicate`）：

| 判据 | 用途 | 为什么 |
|---|---|---|
| `item_key_of(a) == normalize_pool_key(item.key)` | 同区域同人 | 续签轮换 token 后仍认得出，是最精确的一级 |
| `same_credential(item, a)`（access / refresh token 任一相等） | **同一份凭证** | key 会漂移、token 不会；它兜住「手机号后补 / 昵称被改」的窗口 |
| `item_region(item)`：key 的 `xx:` 前缀 → `region` 字段 | 判定条目属于哪套部署 | 老数据的区域只剩前缀这一条线索 |
| `union_pool` 丢掉「同凭证、另一个 key」的副本 | 池内自净 | 留着它，别的机器每同步一次就多收养一个账号 |
| `accounts::load_accounts` 的 `dedupe_by_credential` | 本机自净 | 盘上的幽灵条目不会自己消失；保留信息最全的那条（**有签到结果 = 该区域标签被证实过**） |

服务端侧同步修复：`normalizeItem` 必须原样往返 `region`（缺省时按 key 前缀回填），
并且**读出口也走一遍规范化** —— D1 的 `payload` 是不透明 JSON，老条目在读的那一刻就自愈。

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

- 推送内容形如：`【Qoder 助手】签到完成：成功 2 / 已签 1 / 失败 1（共 4 个账号）`，
  有失败时附上前 5 条「账号名（手机号）：失败原因」明细——这才是推送里最有价值的信息。
- **品牌前缀**：三款助手（WorkBuddy / TraeWork / Qoder）常把通知接到同一个通道上，
  因此每条推送都带 `【Qoder 助手】`。前缀在 `src-tauri/src/notify.rs` 的 `BRAND`
  常量里定义一次、由 `notify::send` 统一施加，**调用点不要自己拼**（改名只改那一行）。
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
# 请求链路（已落地）：
# Qoder 桌面端 → 每次会话 spawn 一次性 `--print` 推理进程（跑完即退，没有长驻 host）
#            → https://127.0.0.1:8789（本应用反代，**终止 TLS**）→ 接管区域的模型网关
#              国际版 https://api2-v2.qoder.sh · 国内版 https://gateway.qoder.com.cn
#
# 杠杆是那个推理进程读到的**环境变量** QODERCN_SERVER_ENDPOINT（国内版，覆盖全部 purpose）；
# 客户端把 scheme 写死成 https，所以反代必须真的能完成 TLS 握手。
```

### 它怎么真正生效的

1. **注入点 = 真正被执行的那个文件。** Qoder 每次会话起的一次性进程，argv 指向
   `…/Contents/Resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-cn-agent-sdk/dist/_worker/qoder-worker-runtime.obf.mjs`
   —— 它在 **asar 之外**（不受完整性校验），也正是 worker 实际执行的入口。
   我们在**文件头**插入一段注入（`/*qoder-assistant-takeover:begin … end*/`）：先
   `process.env.QODERCN_SERVER_ENDPOINT = "https://127.0.0.1:8789"`，再把官方原文原样接在后面。
2. **TLS 用「只对回环地址」的本地 CA 解决，不动系统信任库。** 注入段 monkeypatch
   `node:tls` 的 `connect` / `createSecureContext`：**只有当目标 host 是 `127.0.0.1` /
   `localhost` / `::1`** 时才塞入随注入一起携带的自签 CA 并关掉 `rejectUnauthorized`。
   证书由本应用自签（CA + 只签给 `127.0.0.1`、`localhost` 的叶证书，私钥 `0600`），
   落在应用数据目录的 `certs/` 下 —— **不需要管理员、不改系统信任、不装证书到钥匙串**。
   两个参数都是 `{ca, rejectUnauthorized}` 的既有形状，改的是值而不是协议，所以不碰其它流量。
3. **官方更新会覆盖它 → 指纹自愈。** 每次心跳（5s）比对文件头：不是我们的注入段就重打一遍。
   备份只在「当前文件确实是官方原版」时刷新，所以备份永远是**最新那版官方文件**，
   不会出现「还原把用户刚更新的客户端降级」。
4. **摘除是精确剥离，不是回滚备份。** 关闭开关 = 把注入段逐字节剥掉、还原成官方原文
   （有单测逐字节比对），而不是拿备份覆盖 —— 备份可能已经不是当前那版了。
   关掉后客户端**下一次会话**就恢复直连，**全程不重启 Qoder、不影响正在进行的对话**。
5. **换区域 = 换一个客户端接管。** 装新区域前先按旧租约把旧区域的注入卸干净，
   否则旧客户端会一直指向一个已经没人监听的端口。

### 真正要动的是 `COSY` 凭据（**不是** Bearer token）

接管唯一要做的事，是让上游**认为这个请求来自选中的扣费账号**。但 Qoder 客户端的业务接口
（对话、模型清单、data policy…）**根本不发 `Bearer <token>`**，而是发一个自包含凭据：

```
Authorization: Bearer COSY.<payload_b64>.<md5hex>
Cosy-User: <uid>      Cosy-Key: <rsa密文>      Cosy-Date: <unix秒>
```

凭据本体（账号 token）封在 payload 里、是**密文**；`Cosy-Key` 是解开它的对称密钥，
由客户端内嵌的 RSA 公钥加密。这决定了两种错法都不可行：

| 做法 | 后果 |
|---|---|
| **原样透传**（早期版本） | 上游永远看到客户端登录的那个账号 —— 就是「接管开着，额度却扣第一个账号」 |
| **换成扣费账号的 `Bearer dt-…`**（上一版） | 上游解不开 → `{"code":"101","message":"Signature invalid"}` → 模型清单拉不到 → `no_models_available`，客户端**起不来** |

唯一出路是**按同一算法重签**（[`src/cosy.rs`](src-tauri/src/cosy.rs)）：

```
A    = 16 个 ASCII 字符（8 随机字节的 hex；同时当 AES 密钥与 IV）—— 必须是文本
info = base64( AES-128-CBC(A, A)( JSON{uid, aid, name, email, security_oauth_token} ) )
key  = base64( RSA_PKCS1v15(内嵌公钥, A) )
n    = base64( JSON{version:"v1", requestId, info, cosyVersion, ideVersion} )
sig  = md5( n \n key \n ts \n body \n path )     // path 剥 query、剥 `/algo` 前缀
```

四个必踩的坑（都实测过；**前三个的症状是一模一样的 `101 Signature invalid`**）：

- **`uid` 是 Qoder 侧的账号 id**（`019eb647-…` 这种），**不是**本应用内部那个 UUID。
  填错的表现是 `{"code":"105","message":"Login expired"}` —— 看着像 token 过期，
  其实与 token 毫无关系。首次用到时用 token 调 `/api/v3/user/status` 取回并落盘。
- **签名覆盖 `body` 与 `path`**，所以只能在「已拿到完整请求体」的位置重算，
  且 path 要剥掉 `/algo` 前缀（客户端签名时就是这么剥的）。
- **明文的 JSON 字段顺序要照抄客户端**。`serde_json::json!` 落 `BTreeMap`、按字母序输出，
  明文一变密文全变 → `101`。**长度校验查不出来**：错版与对版都是 143B 明文 / 144B 密文 /
  192 字符 base64，肉眼与断言都过。
- **对称密钥 `A` 必须是 16 个可打印 ASCII 字符**。服务端把它当**字符串**用，16 个裸随机
  字节会被弄坏 → 同样 `101`。实测：任意二进制 16B 必挂；任意可打印 ASCII 16 字符
  （连 `7f7f7f7f…` 这种都行）全过。

定位这一类问题只有一招：**用固定密钥跑整套头**，把 AES / RSA / 签名三段分别与 JS 版对撞 ——

```bash
cargo test --lib -- --ignored --nocapture cosy_probe   # 打印固定密钥下的整包头
```

随机密钥下只能看到「整包头不行」，分不清是 AES、RSA 还是签名错（本轮就在「Rust AES 有差异」
这个假象上绕了很久 —— 真相是明文顺序不同，密文自然不同，而 AES 实现本来就是对的）。

`Cosy-User` / `Cosy-Key` / `Cosy-Date` 与 `Authorization` 必须**同时**替换：只换 Authorization，
上游仍拿旧的 Key/Date 去校验，等于没换。重签失败的每一处都**回落成原样透传** ——
宁可这个请求没换号，也绝不发一个半改的请求出去。

排障一眼看：每个推理 / `/algo` 请求都会在**调试日志**里留一行 ——

```
2026-09-19 17:48:02 [debug] proxy_auth  | 收到 POST /algo/api/v2/service/pro/sse/agent_chat_generation
                                        | 鉴权 Bearer COSY.eyJ…（1975B） | X-Client-Timestamp 无
                                        | → 已重签 COSY（换成扣费账号） | 头: host=gateway.qoder.com.cn …
```

它把「客户端到底发了什么」与「我们怎么处理」钉在同一时间点上，还顺带记下**完整请求头**
（那正是「会话 id 头到底有没有」这类问题的答案，以前只能靠反推，代价是一整轮排障）。

### 日志分两份：界面只讲对客通知，细节进日志文件

| 文件 | 给谁看 | 内容 |
|---|---|---|
| `takeover-journal.jsonl` | **用户**（接管页「接管动态」）| 开启/关闭接管、**本次对话由账号 X 提供**、限流切换、连不上上游 / 响应中断 |
| `takeover-debug.log` | **排障**（界面「查看日志」按钮定位）| 上面那些的副本 + 每个请求的路径、鉴权形态、完整请求头、上游状态码、连接在哪一步断的 |

分级在**写入侧**完成（`journal_append` 对客 / `debug_append` 仅日志），不是让界面去过滤 ——
否则以后每加一条技术事件都得记得同步改前端。调试日志是**超集**，`[user ]` / `[debug]`
前缀标明受众，所以排障时看一个文件就够。两份同锁写入，时间线逐条对齐。

> 曾经的反例：`proxy_auth` 走的是对客通道，于是「接管动态」整屏都是
> 「反代收到 POST /algo/…（鉴权：Bearer COSY.eyJ…）」，而用户真正要看的
> 「这轮对话扣的是哪个账号」被淹在中间、**一条都没有**（`route_start` 依赖
> `X-Conversation-Id`，而客户端实测根本不带这个头）。现在那一条由 `session_start`
> 承担，按「同账号同模型 5 分钟内只报一次」去重。

调试日志同样与**一次接管会话**绑定（开启接管时随 journal 一起重置），并带 4 MB 软上限
（超了从头部裁掉旧内容）。界面上的「清空」只清对客那份 —— 调试日志是排查材料，
误点一下就丢掉全部请求级细节是不可接受的代价。

### 前置条件：应用必须是**签名**的（macOS「App 管理」）

写别人的应用包受 macOS 的 TCC「App 管理」管辖，而**授权记在代码身份上**：

| 签名状态 | designated requirement | 重新构建 / 自动更新后 |
|---|---|---|
| 未签名 / ad-hoc（Tauri 默认 `adhoc,linker-signed`）| 只剩 `cdhash H"…"`，连标识符都是随机构造的 `qoder_assistant-<hash>` | **失配** → 系统把它当成新应用 |
| 固定证书签名（本应用现状）| `identifier "com.waxilo.qoder-assistant" and certificate leaf = H"…"` | **仍然匹配** |

失配的**表现很有欺骗性**：系统不是弹权限框，而是**静默拒绝写入**（`EPERM`），
于是端口在听、租约在跳，客户端的产物却**一个字节都没改** —— 用户看到的正是「接管没生效」。
（加 `sudo`、把应用装到别处都没用，这不是权限位问题。）

因此 `tauri.conf.json` 里写死了身份：

```json
"bundle": { "macOS": { "signingIdentity": "QoderAssistant Self-Signed", "hardenedRuntime": false } }
```

身份由 `scripts/make-signing-cert.sh` 生成并导入登录钥匙串（自签证书，不需要 Apple 开发者账号）：

```bash
# 本机现状：复用已有的本机自签 CA 签发一张 Qoder 专用叶证书。
# 共用 CA ⇒ 叶证书导入即可用，**不用再授权一次「信任设置」**；而 identifier 不同，
# 两个应用在 TCC 里各记各的条目，互不干扰。
CA_DIR=~/.traework-signing bash scripts/make-signing-cert.sh
```

自检（DR 里必须是 `certificate leaf` 而不是 `cdhash`）：

```bash
codesign -dvv /Applications/QoderAssistant.app | grep Authority
codesign -d -r- /Applications/QoderAssistant.app
```

> **换机器**：把凭据目录连同 CA 一起拷过去、重跑脚本的「导入 + 信任」两步，
> **不要重新生成** —— 新 CA 就是新身份，已授的权限全部作废。

### 授权：**系统不会弹窗**，必须手动开

这一点反直觉，但实测如此（2026-09-19，tccd 日志原文）：

```text
Failed to match existing code requirement for subject com.waxilo.qoder-assistant
  and service kTCCServiceSystemPolicyAppBundles
Service kTCCServiceSystemPolicyAppBundles does not allow prompting for unentitled
  binaries; returning denied.
AUTHREQ_RESULT: authValue=0, authReason=2          # 直接判拒，没有任何框
```

「unentitled」= 不是 Apple 签发的证书（自签就属于这类）。所以**别去等弹窗**，
它永远不会出现，只有一条 deny 被记进 tccd。手动开是唯一的路：

1. 打开 **系统设置 → 隐私与安全性 → App 管理**
   （接管页的说明文字里有一个一键直达入口「打开「App 管理」设置」）；
2. 把 **QoderAssistant** 的开关打开（首次请求后系统已替它建好条目，默认是关的；
   若列表里没有，点左下角 **+** 从 `/Applications` 添加）；
3. **重启 QoderAssistant**（授权对已运行的进程不追溯）。

> ⚠️ 如果条目**本来就在**、开关也开着，却仍然被拒：tccd 里那条记录存的是**首次申请那一刻**
> 的 requirement。若那条是在还挂着 ad-hoc 签名（DR 只有 cdhash）时建起来的，换成证书签名
> 之后永远匹配不上 —— 就是上面第 573 行那句 `Failed to match existing code requirement`。
> 此时拨开关没用，要在 **App 管理**里点 **−** 删掉旧条目、再 **+** 重新添加，让系统按
> 当前的 DR 重建记录，然后再走第 3 步。

授权记在代码身份上，证书签名让这个身份**跨构建稳定** ⇒ 上面这条重建之后不再复发。

### 第二道拦截：`com.apple.provenance`（与授权无关，代码自己绕）

macOS 15+ 还在文件上记一份 provenance：**这个 inode 归哪个 cdhash 写**。于是
「上一版构建注入的产物，这一版 `fs::write` 被拒」—— 授权明明是开着的，照样
`Operation not permitted`。这一层 TCC 开关治不了，所以 `patch::write_artifact` 撞
`EPERM` 时**换 inode 重写**：先写同目录的 `.qoderassistant-new` 临时文件（新建的文件由
当前身份取得归属），再 `rename` 顶掉目标，并把原来的 `0755` 权限位搬过去。
顺序不能反 —— 先 `remove_file` 再写，一旦第二次写失败就把客户端的产物整个删掉了。

### 没授权时长什么样：开关自己弹回去，并告诉你原因

授权没开时点「开启接管」，**开关会在几秒后自己关回去** —— 这不是 bug，是刻意的：
设置与磁盘上的事实绝不能相反（「设置说已开启、产物里却没有端点」会让此后这一页的
每次保存都被拓扑守卫拒掉，用户报的「切换账号失败」正是它的下游症状）。所以失败即整体
回滚，并把**注入失败的真实原因原样端到弹窗上**，例如：

```text
接管没能开启（设置已回滚）。
写入 worker 产物失败（…/qoder-worker-runtime.obf.mjs）：Operation not permitted (os error 1)。
去「系统设置 → 隐私与安全性 → App 管理」把「QoderAssistant」的开关打开（/Applications/QoderAssistant.app）。
这个服务不会弹授权框（系统只会在 tccd 里记一条拒绝），必须手动开、别等弹窗；
列表里若没有本应用，点左下角「+」从 /Applications 添加。
如果条目已经在、开关也开着却仍然被拒，要先点「−」删掉旧条目再「+」重新添加。
任一种改动之后都要重启本应用才生效。
```

> **关闭**接管走同一条 fail-closed 判断（注入还在就把反代停掉 = 客户端连向一个没人监听
> 的端口）。代价是「关不掉」这种死结，所以 `commands::disable_blocked` 会连着给出
> 一条能直接粘进终端的还原命令（`cp '<产物>.qoderassistant-orig' '<产物>'`，路径已引
> 好；备份不在时不给命令 —— 拿旧版客户端的原版覆盖新版比不给命令更糟）。

> 这条路径上有个**顺序陷阱**（已修）：真实原因活在租约的 `last_error` 里，而回滚的第一
> 步 `uninstall` 会删掉租约 —— 取晚了就只剩一句与事实无关的「端口是否被占用」，
> 把用户送去查一个没坏的东西。所以 `apply_settings` 在摘除**之前**先把原因取走
> （`stealth::last_error`）。

排查手法（比猜快）：

```bash
# ① 我们到底有没有被拦、被谁拦
/usr/bin/log show --last 5m --predicate 'eventMessage CONTAINS "qoder-assistant"' \
  | grep 'System Policy'                 # 有 deny 行 = App 管理没开
# ② 拦住之后系统是怎么判的
/usr/bin/log show --last 5m --predicate 'process == "tccd"' \
  | grep -E 'SystemPolicyAppBundles|AUTHREQ_RESULT'
# ③ 界面上直接看：接管页会显示「接管没能生效：注入 … 失败：Operation not permitted」
#    —— 这句就是本条症状的原文，照着第 1~3 步做即可
# ④ 退避是否生效：未授权时重试间隔是 30s（不是 2s），所以 deny 不该刷屏
/usr/bin/log show --last 2m --predicate 'eventMessage CONTAINS "System Policy"' | grep -c deny
```

### 区域支持现状

| 区域 | 生效的键 | 协议 | 覆盖范围 | 本应用 |
|---|---|---|---|---|
| 国内版 | `QODERCN_SERVER_ENDPOINT` | **必须 https**，只能是 origin（可含端口）| 全部 purpose（inference / center / openapi / base）| ✅ 支持 |
| 国际版 | `QODER_CENTER_ENDPOINT` | http / https 均可 | **仅 center**，推理端点还要靠代答 `/api/v3/service/region/endpoints` | ⛔ 暂不支持 |

另有 `QODER_MODEL_SERVER_HOST`（两区域通用，只给 host，路径写死
`/model/v1/chat/completions` 且 scheme 写死 https）。**国际版被显式拒绝**而不是静默空转：
`region::endpoint_env_key()` 对它返回 `None`，`stealth::install` 直接报「尚未支持」。

- ✅ **端口可用，不用占 443**：`QODERCN_SERVER_ENDPOINT` 只做 `new URL(v).origin` 校验，
  而 origin **含端口** ⇒ `https://127.0.0.1:8789` 合法。
- **键名在源码里没有字面量，别用 grep 判死刑**：键是 `Rr(name) = ${prefix}${name}` 拼出来的，
  国内版构建里 `prefix = "QODERCN"`（`Ja = ("cn" == "cn")` 硬编码）→ 搜不到原字符串。
  扫二进制要按 **bytes** 计数（`strings` / `grep` 会因编码漏掉）。
- ***为什么不能走 `settings.json`***：`~/.qoder[-cn]/settings.json` 的 `env` 块
  **没有任何消费者**（asar 与 worker bundle 里都找不到「把 settings.env 灌进 `process.env`」的
  代码）；进程 env 由桌面端 spawn 时构造，**不继承桌面端自己的 `process.env`**。
  旧版本写的 `env.CODEBUDDY_BASE_URL` 是 WorkBuddy/CodeBuddy 时代的残留键，Qoder 两个客户端
  都不读 —— 那正是「配置写成功、界面显示已开启、端口在听，但对话依旧直连官方」的原因。
- **怎么自己验证「覆盖有没有被读到」**：读客户端自己的运行日志
  `~/.qoder[-cn]/logs/runs/<时间戳>-p<桌面端pid>/qodercli.log`，看
  `[config-service] Initialising { baseUrl: … }` 与 `[endpoints] Elected inference endpoint:`。
  只要还是 `(SDK default, …)` + 官方域 ⇒ 没读到；应变成 `https://127.0.0.1:8789`。
  反代侧同时会收到 `/model/v1/chat/completions`。

- **Qoder 专用**：只监听 `127.0.0.1`、无鉴权 Key（不对外提供通用代理能力）；
  开启时把端点注入**接管区域**那个客户端的 worker 产物，
  关闭 / 换端口 / **换区域** / 应用退出时自动安全摘除（含原子端点切换，不留死端口；
  **全程不动任何客户端进程**，见本节末尾那两条）。
- **接管目标区域**：控制条上的「区域」选择器决定三件事 —— 端点注入哪套客户端的产物、
  请求转发到哪个模型网关、以及扣费账号**从哪个池里选**（跨区域的 token 在对方网关上无效，
  所以扣费池与模型清单都只列该区域）。开启期间换区域会走安全切换流程
  （摘旧区域的注入 → 装进新区域），并把扣费池重置为「全部」；
  关闭期间换区域只是把设置存下来。**两种情况都不动进程。**
  两套部署可以同时装着，而「现在该接管哪一个」是用户的意图、不是能从磁盘猜出来的事实，
  所以它是一次**显式选择**（`settings.takeover_region`）。选中区域**一个账号都没有**时，
  控制条下方会出现一行提示 + 一键切到有账号的那个区域（只提示，**不替用户改设置**）。
  换区域还要**先确认那个客户端真的装了**：worker 产物找不到时直接报错并回滚，
  而不是留下一个「设置说开着、磁盘上什么都没有」的半成品。
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
- **`Authorization` 有两种形态，必须分开对待**（2026-09-19 实测；这一条曾让接管「看起来开了、
  却完全不能用」）：推理与用户信息走 `Bearer <token>` —— **换成选中扣费账号的凭证**，这就是接管的
  落点；而 `/algo/*`（模型清单、data policy）走 **`Signature <hmac>` 请求签名**，
  密钥是 `sha256(secret:productVersion:machineToken)`、签名串含
  `method / path / requestId / machineToken / 时间戳 / sha256(body)` —— 它绑的是**机器**、
  与选哪个账号无关，反代**原样透传，一个字都不改**。覆盖它的后果不是「换号失败」，
  而是客户端**整个起不来**：网关回 `{"code":"101","message":"Signature invalid"}`
  → 拉不到模型清单 → `HeadlessSession initialize failed: no_models_available`
  → 会话初始化失败、进程退出。判据只在 `proxy::is_bearer_credential` 一处。
- **请求体支持分块传输**：正文长度一度只从 `Content-Length` 读，于是
  `Transfer-Encoding: chunked` 的请求被当成「没有正文」**转发空体** → 上游 400。
  失败形态是静默的（时间线上只有一行「上游返回 400」，看上去像是上游的毛病）：
  Qoder 的 OTLP 遥测（`/otel/v1/*`，80 KB 级）正是分块上传，每次心跳刷一行 400。
  对照实测：同一份 89 KB 正文带 `Content-Length` 过反代 → 上游 200，改成分块 → 400。
  现在分块正文先解开再转发（`proxy::dechunk`），**没解开就不转发**（继续读，不当空体）。
- **模型目录第 1 层的真实失败原因**（原来只记到「503 是阿里云 ALB 的 HTML 页」）：
  本工具的请求打的是 `gateway.qoder.com.cn/api/v2/model/list` → **503（ALB 空路由）**；
  而反代实测收到客户端那一侧的目录请求是 **`/algo/api/v2/model/list?Encode=1`**，
  **多了一段 `/algo` 前缀**（客户端 SDK 日志把它显示成 `path=/api/v2/model/list`，
  与它签名时 `path.slice(5)` 剥掉前缀的行为一致）。带 `/algo` 的这条路由**是存在的**，
  但要求机器签名：不带认证 / 带 Bearer / 带假签名一律
  `403 {"code":"101","message":"Signature invalid"}`。所以这一层不是「域不通」，
  而是**路径少了一段 + 缺签名**；真要补上，得先复现那份签名
  （`machineToken` + `productVersion` + 密钥）—— 在能验证之前不猜着写。
- **不再改写路径**（此条为订正）：这里原来写「CLI 请求裸 `/chat/completions`，
  代理转发前自动补 `/v2`」——**代码里这条改写已经删掉**，因为实测在 Qoder 上讲不通：
  客户端真正发的是 `/model/v1/chat/completions`（前缀对不上，改写本来也不会触发），
  而旧改写指向的 `/v2/chat/completions` 在国际版网关上是 **404（路由不存在）**、
  `/model/v1/chat/completions` 是 **401（路由存在、只是没认证）**。
  **透传代理不替网关发明路径** —— 没有正面证据要求改写时，原样转发才与「客户端自己直连」等价。
- **接管日志分两份**：对客通知进 `takeover-journal.jsonl`（界面「接管动态」读它），
  请求级细节进 `takeover-debug.log`（界面「查看日志」定位）。**都不设条数上限**：
  它们与「一次接管会话」绑定 —— 开启接管时整份重置，会话之内一条不丢
  （页面上「清空」只清对客那份，调试日志留着自己重置；调试日志另有 4 MB 软上限）。
- **网络急救**：设置 →「一键诊断 / 一键恢复」。会扫描配置文件、launchd 全局变量、
  shell 启动脚本，外加**两个客户端 worker 产物里的接管注入**，一键恢复并自动备份被改文件。
  （原先还有一条「桌面端仍持有已摘除端点」的判定，因前提不成立（没有长驻 host）
  且会误报成 `block` 并诱导用户去重启客户端，已删除。配置文件里的 `CODEBUDDY_*` 端点键
  现在**只剩历史残留价值** —— Qoder 不读它们，所以清它属于清理，不是修复连通性。
  真正会「断网」的是**产物里留着指向本机端口、而反代已经不在**的僵尸注入，
  诊断会把它单独报成 `block` 并告诉你清掉即可 —— 详见 `netfix.rs` 模块文档。）

> **关键机制（2026-09-19 三轮更正）**：
>
> 1. 原来这里写的是「桌面端与其长驻 CLI host 只在启动时读一次端点配置，所以开关接管要
>    自动重启客户端」——**两半都不成立**。Qoder 没有「长驻 CLI host」：它的推理进程是
>    **每次会话按需 spawn 的一次性 `--print` 进程**（证据：
>    `~/.qoder[-cn]/logs/runs/<时间戳>-p<桌面端pid>/manifest.json` 的 `argv`，以及
>    `runs/` 目录每个会话新增一份），跑完即退，没有可重启的东西。
> 2. 但「改配置就等于生效」也**不成立**：那个一次性进程的 env 由**桌面端在 spawn 时构造**，
>    而 `~/.qoder[-cn]/settings.json` 的 `env` 块**没有任何消费者** —— 所以旧实现写的
>    端点键从来没被读过，接管是**空转**（配置写成功、界面说已开启、端口在听，对话依旧直连官方）。
> 3. 现在改的**不是配置，而是那个一次性进程要执行的那份产物本身**：在 `app.asar.unpacked`
>    里的 worker 产物头部注入端点与本地 CA（见本节开头）。产物每次会话重新读，所以
>    「改完下一次对话即生效」，既不需要重启客户端，也不再依赖任何配置文件。
>    代价是官方更新会覆盖 → 靠心跳比对文件头指纹自愈；摘除是**精确剥离**，不是拿备份回滚。
>
> 删掉那套进程操作还顺手消掉了一个真实故障：`open -a` 紧跟在 AppleScript `quit` 之后
> 会撞上 LaunchServices 的注册竞态返回 `exit status: 1`，而旧实现在这一步失败后
> **不回滚**，于是磁盘说「已开启」、界面说「已关闭」—— 用户报的「接管报错、随后又显示
> 接管成功、切扣费账号却保存失败」整条链路都源于此。现在 `apply_settings` 的每一步
> 要么全成、要么整体回滚（落盘 → 等端点就位 → 失败则连设置一起退回）。

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
| `open_app_management` | 打开 macOS「App 管理」授权面板（深链写死在后端、不收前端参数：系统私有 scheme 不放行给 `open_external`）。接管页说明里那个「打开「App 管理」设置」就是它 |
| `broker_upload` / `broker_link` / `broker_unbind` / `broker_state` | 云端凭证池：上传成一个池并拿 uuid / 绑定别处的 uuid / 解绑 / 只读状态。四个都是**池级**命令，不带账号 id |
| `get_settings` | 读全局设置（含**接管目标区域** `takeover_region`） |
| `regions` | 区域清单（国际版 / 国内版的中文名与说明）：界面上「区域」的**唯一来源** |
| `save_settings` | 保存设置（校验定时时刻与 webhook）；拓扑类字段（启停 / 端口 / 区域）在接管开启时会被拒，必须走 `apply_settings` |
| `apply_settings` | 原子应用设置；接管启停 / 换端口 / **换区域**时走安全切换流程（摘端点 + 装到新区域），**全程不动客户端进程**；任一步失败整体回滚 |
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
