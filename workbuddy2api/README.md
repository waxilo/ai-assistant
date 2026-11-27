# WorkBuddy2API（Tauri 桌面端）

把 **WorkBuddy 账号池**变成一个**可对外使用的 API**：本地起网关，给 Key，
OpenAI Chat Completions / Responses 兼容，任何能填自定义 Base URL 的客户端都能直接接上。

> 本目录由 `workbuddy-assistant` 整体复制而来（同为 Tauri v2 + Rust + React），
> 但**定位不同、独立迭代**：
>
> | | `workbuddy-assistant` | `workbuddy2api` |
> | --- | --- | --- |
> | 定位 | 桌面助手：签到、续签、积分、接管官方客户端 | API 网关：把账号池的额度转成标准 API |
> | 对官方客户端 | 接管（改 `CODEBUDDY_BASE_URL`） | 不接管 |
> | 对外端口 | 8787（无鉴权、仅本机） | 8790（带 Key、可配置暴露范围） |
> | 数据目录 | `com.waxilo.workbuddy-assistant` | `com.waxilo.workbuddy2api` |
> | 更新通道 | `workbuddy-latest` | `workbuddy2api-latest` |

两者的账号池、设置、台账、日志**完全隔离**（不同签名、不同 AppData 目录），可以同时运行。

---

## 为什么上游不需要协议翻译

腾讯网关本身就是 OpenAI Chat Completions 兼容的（`POST /v2/chat/completions`，
标准 SSE chunk、标准 `tool_calls` 增量、标准 `usage`）。本项目要补的是**网关外壳**：

- Key 鉴权、监听范围、限速
- `/v1/models` 模型清单（上游没有这个路由）
- **非流式**响应（上游明确拒绝 `stream:false`，必须内部聚合）
- **`/v1/responses`**（只认 Responses API 的客户端用；上游没有该路由，网关双向翻译）

方案与实测证据见 [`docs/api-gateway-plan.md`](docs/api-gateway-plan.md)。

---

## 本地开发

前置：Node 20+、Rust 1.77+、WebView2（Windows，通常已预装）。

```bash
npm install
npm run tauri dev
```

后端单测（纯本地）：

```bash
cd src-tauri && cargo test --offline
```

> 本机 `crates.io` 走代理会 502，**必须带 `--offline`**。

## 本地构建

```bash
npx tauri icon src-tauri/icons/icon-source.svg   # 只有改了图标才需要
npm run tauri build
```

## 发布

打 tag `workbuddy2api-v<版本>` 并推送，`release-workbuddy2api.yml` 会构建
macOS universal + Windows NSIS 并更新 `workbuddy2api-latest` 通道。

```bash
git tag workbuddy2api-v0.2.10
git push origin workbuddy2api-v0.2.10
```

---

## 状态

- [x] 骨架从 `workbuddy-assistant` 复制、身份隔离（productName / identifier / 包名 / 更新通道 / 数据目录）
- [x] 网关：`/v1/chat/completions`、`/v1/models`、Key 鉴权、非流式聚合
- [x] 上游加固：错误分级（12 类）、两档冷却、熔断、`developer` 角色归一
- [x] 网关：`/v1/responses` 转换层（[说明](docs/responses-wire.md)）
- [x] API 接入页：开关、Key、监听设置、扣费备选账号、接入 Codex（一键写入 config.toml + 模型目录）
- [x] 端口只在网关关闭时可改：开着网关时输入框锁定，先关网关再改端口
- [x] 关网关自动收尾：断开 Codex 接入、按备份还原 `config.toml`，并重启正在运行的 Codex
- [x] 紫色应用图标：与同门的 WorkBuddy Assistant（深绿底）在 Dock / 任务栏里一眼分开
- [x] 添加账号支持 JSON 导入导出：导出的 JSON 只含 access token；导入的账号标记为**公用 token**——照常签到、走网关，但不自动续签、不上传凭证池（替共享票续签会把真正持链的一方踢下线）；重新从本机登录文件 / 浏览器登录导入同一账号即可恢复续签
- [x] 同账号合并优先级：本地登录 > 云端同步 > 手动导入。低优先级的副本不覆盖可用的高优先级账号（只补空字段）；账号不可用（判死 / 续签链过期 / 无链且过期）时任何来源直接覆盖。导入账号被云端副本接管后转回正常续签规则，公用 token 本身永不回写池
- [x] 一池一个**续签设备**：云端同步账号只有池上登记的那台机器续签（组合 id = 客户端类型 + 机器标识）；本地登录账号无条件续签，JSON 导入的公用 token 永不续签。换设备先在旧设备「取消本机续签」（10 秒内没有新设备接手会自动恢复），再在新设备「设为本机续签」
- [x] 主表与云端列表分开操作：主表展示本机登录 / 云端副本 / JSON 导入三类账号，删除只动本机这一份（删云端副本也不动云端池）；云端账号的编辑（只改展示字段、不碰 token）、逻辑删除、从「已删除」恢复都在池栏「云端账号」弹窗里，操作的是云端池。与续签设备身份无关

---

## 两条客户端路线

| 客户端 | 用哪个路由 | 备注 |
| --- | --- | --- |
| Cursor、Cline、Cherry Studio、Continue、自己的脚本 | `POST /v1/chat/completions` | 原样转发 + 非流式聚合 |
| 新版 OpenAI SDK 等只认 Responses 的客户端 | `POST /v1/responses` | 网关双向翻译（上游没有这个路由） |

**Codex 可以在页面上「一键接入」**：点一次把本网关写进它的 `config.toml` 与模型目录，
之后在 Codex 里正常对话即可。只改网关相关的那几个键，断开时按备份原样还原
（含 `[features]` 里 `apps = false` / `tool_suggest = false` / `remote_plugin = false`
与 `[analytics] enabled = false` —— 不关这几个，Codex 会在 turn 开始前去打 `chatgpt.com`
的 apps/connectors、插件目录与统计上报，连不上就一直重试）。

⚠️ 装了企业 EDR 的 Windows 机器上，如果仍然「思考很久」，先看一眼机器级受管文件
`C:\ProgramData\OpenAI\Codex\requirements.toml` 里 `codex_userpromptsubmit` 出现了几次：
本机曾累积 **392 条完全相同的 UserPromptSubmit hook**，每个 turn 都要跑完它们才发请求
（实测 118~153 秒），而这一层是用户配置改不掉的。排查与去重步骤见
[`docs/responses-wire.md`](docs/responses-wire.md) 的「思考很久」一节。

其它客户端（Cursor、Cline、Cherry Studio、Continue、自己的脚本）**由你自己填**：
把上面的 Base URL 与 API Key 填进「自定义供应商 / 覆盖 OpenAI Base URL」，模型名照清单挑一个。
Cursor 仍未做自动接入 —— 它的 AI 设置只存在自己的 SQLite（`state.vscdb`）里，写库必须等它
完全退出、键名还随版本漂移，风险与收益不成比例。

细节见 [`docs/responses-wire.md`](docs/responses-wire.md)。

网关只监听 `127.0.0.1`，且**每个请求都必须带 Key**（没有本机豁免）。
