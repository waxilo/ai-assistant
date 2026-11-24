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
```

同时生成模型目录 `workbuddy2api-model-catalog.json`：勾了几个模型就有几条，
`slug` / `display_name` / `priority`（默认模型 `0`，其余 `1000+i`）/ `context_window`
`128000`；`supported_reasoning_levels` 给空数组 —— Codex 就不显示 effort 选择器、
也不会发 `reasoning.effort`。

**两个实测必需的细节**（codex-cli 0.171.0-alpha.13 / Windows MSIX，2026-11-24）：

- **`[features] apps = false` 必写**。不写时 Codex 每 10 秒去
  `POST chatgpt.com/backend-api/ps/mcp` 重试（它内置的 apps/connectors 后台），连不上就一直重试，
  一个 turn 永远走不到结束 —— 表现为「卡住不动」。
- **不要写 `disable_response_storage`**。0.171 已废弃这个键，写进去日志会报 `is ignored`。

首次接入把原文件备份到 `<app_data>/codex-config.bak.toml`。**断开**会删掉 provider 段、
把 `model_provider` / `model_catalog_json` / `model` / `features.apps` 恢复成备份里的原值
（原本没有的键就删掉，`[features]` 表空了连表头一起删），并删掉我们生成的模型目录。

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
