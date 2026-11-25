# Responses API 转换层

Codex 从 2026-02 起**只认 Responses API**：`wire_api = "chat"` 这个选项被从 Codex
里删除了，`config.toml` 里只剩 `wire_api = "responses"` 一条路。

而上游（`www.workbuddy.cn`）只有 Chat Completions：

| 上游路由 | 实测结果（2026-11-24） |
| --- | --- |
| `POST /v2/chat/completions` | 正常，流式 |
| `POST /v2/responses` | **404** |
| `GET /v1/models`、`GET /v2/models` | 404（清单在 `/v2/enterprises/personal/models`） |

所以 `/v1/responses` 是网关**自己实现**的，不是转发。实现分两半：
`gateway::responses::to_chat`（请求方向）与 `gateway::responses::Translate`（响应方向）。

## 请求方向

| Responses | Chat Completions | 备注 |
| --- | --- | --- |
| `instructions` | 最前面一条 `role:"system"` | Codex 的系统提示 |
| `input`（字符串） | 一条 `role:"user"` | 规范允许直接给字符串 |
| `input[].type="message"` | 一条 message | content 里的 `input_text` → `text` |
| `input[].type="function_call"` | 一条 `assistant` 带 `tool_calls` | `call_id` 必须原样带回 |
| `input[].type="function_call_output"` | 一条 `role:"tool"` | `output` 可能是 part 数组 |
| `input[].type="reasoning"` | **丢弃** | 上游不认识，带上就是 400 |
| `tools[].type="function"`（扁平） | `tools[].function`（嵌套） | |
| `tools[].type=web_search/local_shell/…` | **丢弃** | 上游不支持 |
| `tool_choice` 对象 | `"auto"` | 上游该字段是 string |
| `max_output_tokens` | `max_tokens` | |
| `store`/`include`/`reasoning`/`text`/`metadata`/`prompt_cache_key` | **丢弃** | 上游没有对应字段 |

一律**白名单构造**，不做"透传 + 删几个"：上游是 Go 严格反序列化，
多一个它不认识的字段就可能整条 400（`tool_choice` 传对象被拒就是这么发现的）。

## 响应方向

上游 Chat SSE 逐帧翻译成 Responses 事件。下面是本机实测的 kiro2api
（Codex 认可的实现）的帧序列，翻译器输出与它同构：

### 纯文本

```
event: response.created
data: {"response":{...,"status":"in_progress","output":[],"usage":null},"sequence_number":0,"type":"response.created"}

event: response.in_progress
data: {...}

event: response.output_item.added
data: {"item":{"id":"msg_…","type":"message","status":"in_progress","role":"assistant","content":[]},"output_index":0}

event: response.content_part.added
data: {"part":{"type":"output_text","text":"","annotations":[]},"content_index":0,...}

event: response.output_text.delta        ← N 次，每次一个 token 片段
data: {"delta":"Hi","content_index":0,"logprobs":[]}

event: response.output_text.done
data: {"text":"Hi, I'm Codex."}

event: response.content_part.done
data: {"part":{"type":"output_text","text":"Hi, I'm Codex.","annotations":[]}}

event: response.output_item.done
data: {"item":{"id":"msg_…","type":"message","status":"completed","role":"assistant","content":[{...}]}}

event: response.completed
data: {"response":{...,"status":"completed","output":[…],"usage":{...}}}
```

### 工具调用

文本事件全部消失，换成：

```
event: response.output_item.added
data: {"item":{"id":"fc_…","type":"function_call","status":"in_progress","arguments":"","call_id":"call_…","name":"shell"},"output_index":0}

event: response.function_call_arguments.delta     ← N 次，拼出完整 JSON
data: {"delta":"{\"command"}

event: response.function_call_arguments.done
data: {"arguments":"{\"command\":[\"echo\",\"hello\"]}"}

event: response.output_item.done
data: {"item":{"type":"function_call","status":"completed","arguments":"{…}","call_id":"call_…","name":"shell"}}

event: response.completed
data: {"response":{...,"output":[{function_call 项}]}}
```

## 四个踩过的坑

**① 响应头必须延迟写。**
上游偶尔回 200 但 body 是一段普通 JSON（不是 SSE）。如果先写 200 再翻译，
客户端会收到"成功但空"的响应，比报错更难排查。现在先读第一块、确认真的解析出帧，
才写响应头；一帧都没有就回 502 并把上游原文塞进错误消息。

**② `sequence_number` 必须密集递增。**
Codex 用它判流有没有丢帧。跳号会被当成流损坏。

**③ 工具调用时 `output` 里不能有空的 message 项。**
只发 `type:"message"` 的 item 却没有文本，Codex 会渲染出一个空气泡。
翻译器只在真有文本时才开 message item。

**④ `reasoning_content` 丢掉。**
上游会返回思维链（`delta.reasoning_content`）。实测 kiro2api 收到
`include:["reasoning.encrypted_content"]` 也**完全不发** reasoning 事件，Codex 照常工作。
发 reasoning item 需要 `encrypted_content` 或完整的 summary 结构，凭空合成一个
不合规的 item 会让 Codex 整轮校验失败。宁可不显示思考过程。

## 降级行为

| 情况 | 行为 |
| --- | --- |
| 上游 200 但非 SSE | 502 + 上游原文前 300 字 |
| 流中途断开 | 照样补 `response.completed`（已收到多少算多少），日志记「上游读流失败」 |
| `finish_reason=length` | `status:"incomplete"` + `incomplete_details.reason="max_output_tokens"` |
| `finish_reason=content_filter` | `status:"incomplete"` + `reason:"content_filter"` |
| 下游非流式 | 收全后直接把最终的 response 对象当 JSON 返回 |

## 接入客户端

两条路，按客户端分：

| 客户端 | 怎么接 |
| --- | --- |
| **Codex**（桌面端 / CLI） | 页面上点「接入 Codex」，自动写 `config.toml` + 模型目录 |
| Cursor、Cline、Continue、Cherry Studio、自己的脚本 | 手动填三项（页面提供只读信息） |

### Codex：一键接入

写进 `~/.codex/config.toml` 的只有这几处，其余字节不动（外科手术式文本编辑）：

```toml
model_provider = "workbuddy2api"
model_catalog_json = "workbuddy2api-model-catalog.json"
model = "<你选的默认模型>"

[model_providers.workbuddy2api]
name = "WorkBuddy2API"
base_url = "http://127.0.0.1:<端口>/v1"
wire_api = "responses"
experimental_bearer_token = "sk-wb-…"

[features]
apps = false
tool_suggest = false
remote_plugin = false

[analytics]
enabled = false
```

同时生成模型目录 `workbuddy2api-model-catalog.json`：勾了几个模型就有几条，
`slug` / `display_name` / `priority`（默认模型 `0`，其余 `1000+i`）/ `context_window`
`128000`；`supported_reasoning_levels` 给空数组 —— Codex 就不显示 effort 选择器、
也不会发 `reasoning.effort`。

### 这四行分别省掉什么（Codex 桌面端 26.1117.41037 / Windows，2026-11-24、11-25）

- **`[features] apps = false`**：Codex 内置的 `codex_apps` MCP 服务要从
  `https://chatgpt.com/backend-api/*` 拉 apps/connectors 目录，连不上时 `rmcp` 每 10 秒重试，
  `world_state.build` 一直等它才降级（日志里那句
  `omitting MCP server without an exact ready client server_name=codex_apps`）。
  不关它，`codex debug prompt-input` 30 秒都走不完；关掉后 6.0~6.1 秒。
- **`tool_suggest = false` 与 `remote_plugin = false`**：各自还有一条要 ChatGPT 账号的
  插件发现/目录刷新（`list_tool_suggest_discoverable_plugins`、
  `POST chatgpt.com/backend-api/ps/plugins/installed`），同样打在注定失败的端点上。
- **`[analytics] enabled = false`**：它不是 feature 而是官方文档里明写的表
  （app-server 默认关、桌面端显式开）。不关时它每 10 秒往
  `chatgpt.com/backend-api/codex/analytics-events` 打一次、每次等满超时。
- **不要写 `disable_response_storage`**（0.171 已废弃，只会进 `unrecognized configuration
  setting` 列表），**也不要写 `[cloud.skills]`**（0.171 里根本没这个键，关它零收益）。

⚠️ **这四行不等于解决「思考很久」。** 下面第二节才是本机几十秒~两分钟的真正来源。

### 「思考很久」的真正来源：受管 UserPromptSubmit hook 被重复下发

装了企业 EDR 的 Windows 机器上（本机是微步 Threatbook，`C:\Program Files (x86)\Threatbook\Agent\Kuafu.exe`），
Codex 会从**机器级受管文件**读 hook：

```
C:\ProgramData\OpenAI\Codex\requirements.toml
```

本机这份文件 1965 行 / 65 KB，里面是 **392 条完全相同的块**：

```toml
[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
command = '& "C:\Program Files (x86)\Threatbook\Agent\Kuafu.exe" codex_userpromptsubmit'
type = 'command'
```

Codex 对每条都执行一次，而且是在模型请求**之前**跑完：

| 证据 | 数值 |
| --- | --- |
| 单次 `Kuafu.exe codex_userpromptsubmit` | 527 ms（PowerShell 计时） |
| 一次 turn 里 `hook/started` 事件数（`codex debug app-server send-message-v2`） | **392** |
| 桌面端首轮：`task_started` → 真正发出 `POST /v1/responses` | **118 秒**（11:30 那轮） |
| 同上，`apps=false` 已生效、app-server 已重启 | **153 秒**（14:07 那轮） |
| 同一轮网关自身耗时 | 1~3 秒 |

时间与条数严格成正比（118 / 392 ≈ 0.30 s、153 / 392 ≈ 0.39 s），而日志特征也一样：一整段
零日志空白，然后几百条 `hook/completed` 在同一秒刷出来，紧接着才发模型请求。

**用户配置改不掉它**：`requirements.toml` 是机器级受管层（`%ProgramData%`），里面还写着
`[features] hooks = true`（`featureRequirements: {hooks: true}`），`hooks` 开关不允许用户关。
唯一的修法是把那份文件**去重成 1 条**（需要管理员权限，且要跟 IT 确认），或让 IT 修 EDR 的
注入逻辑（它每同步一次策略就 append 一段，从不查重）。去重后这一项从 100 多秒降到 0.5 秒。

### 量化手段（都**不需要开桌面端**）

```bash
# A. 测「注定失败的联网等待」这一段：这条路径不跑 hook，所以只反映 apps 等开关
codex debug prompt-input "你好"          # 用 `timeout 40 …` 包一下看退出码；关 apps 后恒为 ~6s

# B. 测完整 turn 准备（含 hook）：会打印 app-server 的 JSON-RPC 通知流
codex debug app-server send-message-v2 "你好"
#   数一下 "hook/started" 出现几次、从第一次到最后一次隔多久，就是 hook 的净开销
```

⚠️ 别拿 `codex exec` 做计时：本机安全策略会在它开始请求时把进程杀掉。
⚠️ 也**别**只看 `debug prompt-input` 的 6 秒就以为「已经好了」——它不跑 hook，
在装 EDR 的机器上会和真实体感差 100 秒以上。

排查同款「正在思考」时按这个顺序看：

```bash
# 1. 网关有没有真的收到请求（没有 ⇒ 卡在 Codex 侧，与网关无关）
tail -n 20 "$APPDATA/com.waxilo.workbuddy2api/app-logs.jsonl"

# 2. 受管 hook 有没有被重复下发（本机踩过的坑，一眼就能看出来）
grep -c codex_userpromptsubmit "/c/ProgramData/OpenAI/Codex/requirements.toml"   # 正常应为 1

# 3. Codex 自己的日志里那一轮走到哪了（Windows：%USERPROFILE%\.codex\logs_2.sqlite 的 logs 表）
#    按 id 倒序看：world_state.build 之后长时间零日志、随后几百条 hook/completed 齐刷
#    ⇒ 就是 hook 重复

# 4. chatgpt.com 是不是「连得上但不回包」
curl -s -o /dev/null -m 40 -w 'code=%{http_code} connect=%{time_connect} total=%{time_total}\n' \
  'https://chatgpt.com/backend-api/ps/plugins/installed?limit=200'
#    connect 很快、code=000 且 total≈10s ⇒ 被代理黑洞
```

去重（**管理员 PowerShell**，先备份；EDR 有篡改保护时可能被拦或下次同步又写回来）：

```powershell
$p = 'C:\ProgramData\OpenAI\Codex\requirements.toml'
Copy-Item $p "$p.bak" -Force
$t = [IO.File]::ReadAllText($p)
$i = $t.IndexOf('[[hooks.UserPromptSubmit]]')
$one = "[[hooks.UserPromptSubmit]]`r`n[[hooks.UserPromptSubmit.hooks]]`r`n" +
       "command = '& `"C:\Program Files (x86)\Threatbook\Agent\Kuafu.exe`" codex_userpromptsubmit'`r`n" +
       "type = 'command'`r`n"
[IO.File]::WriteAllText($p, $t.Substring(0, $i).TrimEnd() + "`r`n`r`n" + $one, (New-Object Text.UTF8Encoding $false))
Select-String -Path $p -Pattern 'codex_userpromptsubmit' | Measure-Object | % Count   # 期望 1
```

改完要**重启 app-server**（桌面端彻底退出再打开；有 daemon 时 `codex app-server daemon restart`）。

首次接入把原文件备份到 `<app_data>/codex-config.bak.toml`。**断开**会删掉 provider 段、
把 `model_provider` / `model_catalog_json` / `model`、`[features]` 里的
`apps` / `tool_suggest` / `remote_plugin` 与 `[analytics] enabled` 恢复成备份里的原值
（原本没有的键就删掉，表空了连表头一起删），并删掉我们生成的模型目录。

**Codex 只在启动时读一次配置**，所以接入/断开后要重启才生效。页面上的「重启 Codex」只结束
「有窗口 + 进程名 `codex`/`chatgpt` + 路径属 OpenAI 桌面应用」的进程 —— 你在终端里跑的
`codex` CLI 不会被误杀；拉起走 `explorer shell:appsFolder\<AUMID>`（MSIX 的 exe 在 ACL
保护的 WindowsApps 下，直接 spawn 不可靠）。

**如果 Codex 卡在启动**：它启动时会 `GET /api/codex/settings/user` 打 `chatgpt.com`，
那一步取不到就一直等。这是 Codex 自己的启动路径，与本网关无关 —— 确认这台机器能访问
`chatgpt.com` 即可。

### 其它客户端：手动填

| 字段 | 值 |
| --- | --- |
| Base URL | `http://127.0.0.1:<端口>/v1` |
| API Key | 页面上的 `sk-wb-…` |
| 模型 | 清单里的 slug（每行右侧可复制） |

**`base_url` 用 `127.0.0.1` 而不是 `localhost`**：Windows 上 `localhost` 可能先解析到
`::1`，而网关只绑 IPv4，会连不上。

### Cursor 为什么不做一键接入

Cursor 的 AI 设置不在文件里，而是 `%APPDATA%\Cursor\User\globalStorage\state.vscdb`
这个 SQLite 的 `ItemTable`：`cursorAuth/openAIKey` 与
`...persistentStorage.applicationUser`（JSON，含 `useOpenAIKey` / `openAIBaseUrl` /
`aiSettings.userAddedModels`）。写它有三个硬前提 —— Cursor 必须**完全退出**（运行中写会被
内存态覆盖）、必须**留备份**（那里面是用户全部自定义模型）、键名**随版本漂移**。
这类「改别人私有状态」的动作风险与收益不成比例，暂不做。
