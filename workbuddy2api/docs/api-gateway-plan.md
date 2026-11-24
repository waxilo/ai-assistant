# WorkBuddy Assistant · 对外 API 网关方案（探索稿）

> 目标：让 workbuddy-assistant 不只是一个「接管客户端」的反代，而是一个**可对外提供 API 的网关**，
> 形态对齐 kiro2api（本地跑、给 Key、OpenAI 兼容、能被 Codex 直接接上）。
> 本文只做方案与决策，不含代码改动。
>
> **状态（2026-11-24）**：P0/P1 均已落地。第六节「一键接入 Codex」曾一度撤销
> （当时的误判是「Codex 只接受 GPT 系模型」），**现已恢复并按其真实机制实现**：
> 它支持 `model_catalog_json` 指向自定义模型目录，slug 完全由我们给，
> `model_providers.<id>` 只认 `wire_api = "responses"` —— 实测（0.171 MSIX）能接上。
> Cursor 仍然不做（它的设置只能写自己的 SQLite）。
> 现状与两个实测必需的细节见 [`responses-wire.md`](responses-wire.md) 末节。

---

## 一、结论先说

**可行，且比 kiro2api 的处境好得多。**

kiro2api 要把 Kiro 的**私有协议**翻译成 OpenAI 格式；而腾讯网关本身就是 **OpenAI Chat Completions 兼容**的
（`POST /v2/chat/completions`，标准 SSE chunk、标准 `tool_calls` 增量、标准 `usage`），
现有 `proxy.rs` 已经在做同协议透传。所以要补的不是"协议翻译"，而是**网关外壳**：

| 缺口 | 性质 | 工作量 |
| --- | --- | --- |
| 鉴权（现在端口无 Key、任何本机进程可用） | 新增 | 小 |
| `/v1/chat/completions` + `/v1/models` 的对外面向（路径改写、模型清单合成） | 新增 | 小 |
| **非流式**响应（上游明确拒绝 `stream:false`） | 新增：内部流式收全再聚合 | 中 |
| **`/v1/responses`（Responses API）** —— Codex 2026-02 起**只认这个** | 新增：双向转换层 | **大（主要工作量）** |
| 一键写 `~/.codex/config.toml` | 新增 | 中 |

**关键约束（已实测，不是推测）：**

1. 上游 `POST /v2/chat/completions` 在 `stream:false` 时返回
   `400 code:11101 "Non-stream chat request is currently not supported"` ⇒ 非流式必须由网关聚合。
2. 上游**没有** `/v2/responses`、`/v1/models`、`/v2/models`（全部 `404 Route Not Found`）⇒
   Codex 的 Responses 请求**只能**在助手侧转换；模型清单也只能自己合成。
3. 上游 `tool_choice` **只接受字符串**（`auto`/`none`/`required`），传对象报
   `cannot unmarshal object into Go struct field Request.tool_choice of type string`
   ⇒ Codex 的 `tool_choice: {"type":"function",...}` 必须降级。

---

## 二、实测证据（2026-11-24，真实账号、真实网关）

| 探测项 | 结果 |
| --- | --- |
| `GET /v2/enterprises/personal/models` | 200，36 个模型，含 `hy4flash-free`(x0.00) 等免费项 |
| `POST /v2/chat/completions` 流式 + `tools` | 200，delta 内为标准 `tool_calls` 数组（`id`/`function.name`/`arguments` 增量/`index`） |
| 流末帧 `usage` | 存在，标准 `prompt_tokens`/`completion_tokens`/`total_tokens`/`completion_tokens_details`，另带私有 `credit`/`quota` |
| `stream:false` | 400 / 11101（不支持） |
| `tool_choice` 传对象 | 400（Go 结构体只收 string） |
| `tool_choice:"required"` + 工具 | 200，`finish_reason:"tool_calls"`，首帧带 `id`，后续帧 `name` 空、`arguments` 逐段 |
| 请求体 186KB | 200（Codex 的 `instructions` + tools schema 体积无压力） |
| 多轮 `role:"tool"` + `tool_call_id` | 200 |
| `content` 传数组（`[{type:"text"}]`） | 200（多模态段被接受） |
| 未知字段（`input`/`reasoning`/`store`/`include`） | 被忽略，不报错 |
| `/v2/responses`、`/v1/models` | 404 |

**本地已有的参照物**（这是最大的确定性来源）：本机 Codex 现在就通过 kiro2api 在跑，

```toml
model_provider = "kiro2api"
model_catalog_json = "kiro2api-model-catalog.json"
[model_providers.kiro2api]
base_url = "http://127.0.0.1:5680/openai/v1"   # ← 带 /v1
wire_api = "responses"
experimental_bearer_token = "k2a-..."          # ← Key 可直接内联，不需要环境变量
```

并且我已经从它身上抓到了 **Codex 认可的完整事件序列**（文本支线 + 工具调用支线，见第四节），
等于拿到了转换层的验收基准。

---

## 三、目标形态

```
                     ┌──────────────────────────────────────────┐
  Codex CLI ────────►│ :8790  API 网关（新增，可关、带 Key）      │
  OpenAI SDK ───────►│   /v1/responses   → 转换层 ─┐             │
  Cline/Cherry ─────►│   /v1/chat/completions ────┤             │
  Claude Code ──────►│   /v1/messages    (P2)     │             │
                     └────────────────────────────┼─────────────┘
                                                  │
                     ┌────────────────────────────▼─────────────┐
  WorkBuddy 客户端 ─►│ :8787  接管反代（现有，无鉴权、仅本机）    │
  （CODEBUDDY_BASE_URL）│  任意路径 → 原样透传                    │
                     └────────────────────────────┬─────────────┘
                                                  │  共用
                                    ┌─────────────▼──────────────┐
                                    │ routing（选号/粘滞/冷却）   │ ← 从 proxy.rs 抽出
                                    │ upstream（加票、发请求）    │
                                    └─────────────┬──────────────┘
                                                  ▼
                                    https://copilot.tencent.com
```

**为什么新开一个端口，而不是复用 8787：**

- 8787 的语义是「装扮成官方网关给 WorkBuddy 客户端用」——**不能加 Key 鉴权**，CLI 带的是官方票，
  不是我们自定义的 Key；加鉴权会直接打断接管。
- 8787 目前无鉴权，只靠"仅监听 127.0.0.1"兜底。对外 API 要可控地开放（局域网/分享），
  两种信任模型混在一个端口里迟早出事。
- 独立端口可以独立开关、独立限速、独立日志，关掉不影响接管。

**选号逻辑完全复用现有语义**：每条请求对「当前模型可用集」纯排序 `(到期自然日, −待过期积分)`，
冷却按「账号 × 模型」。**不引入任何新的粘滞规则**；网关只是给 `choose_account` 提供
一个 `conv` key（`prompt_cache_key` → input 首条消息 hash → 5 分钟 TTL），避免同一会话跨账号。

---

## 四、协议转换规格（核心）

### 4.1 请求侧：Responses → Chat Completions

| Responses 字段/项 | 转成 Chat | 规则 |
| --- | --- | --- |
| `instructions`（字符串） | `messages[0] = {role:"system"}` | 非空才插；Codex 的系统提示就在这 |
| `input` 为字符串 | `{role:"user", content:str}` | |
| `input[] item.type="message"` | `{role, content}` | `developer` → **`system`**；`assistant` 原样 |
| └ `content[] part` | `input_text`→`text`；`input_image`→`image_url` | 其它 part 丢弃 |
| `input[] item.type="function_call"` | 并入**同一条** `assistant` 的 `tool_calls[]`（`id=call_id`） | 连续多条合并，不拆成多条 assistant |
| `input[] item.type="function_call_output"` | `{role:"tool", tool_call_id:call_id, content:output}` | `output` 是数组时取文本段拼接 |
| `input[] item.type="reasoning"` | **丢弃** | 配套把 catalog 的 `supports_reasoning_summaries` 设 false，让它别发 |
| `tools[]`（扁平 `{type:"function",name,...}`） | `{type:"function",function:{name,description,parameters}}` | 非 `function` 类型（`local_shell`/`web_search`）**丢弃** |
| `tool_choice`（对象 `{type:"function",name}`） | `"auto"` | **必须降级**，上游只收字符串 |
| `tool_choice`（字符串） | 原样 | |
| `parallel_tool_calls` | 原样 | 上游接受 |
| `max_output_tokens` | `max_tokens` | |
| `temperature` / `top_p` | 原样 | |
| `stream` | **恒置 true** | 上游不支持非流式 |
| `store`/`include`/`reasoning`/`text`/`truncation`/`prompt_cache_key`/`previous_response_id` | 丢弃 | 本地无状态转换，历史全在 `input` 里 |

### 4.2 响应侧：Chat chunk → Responses 事件

必须按下面的顺序发（每帧 `sequence_number` 自增，`event:` 行与 `data.type` 同名）：

```
: keepalive                     ← 可选，长思考时保活

response.created                ← 空 response 骨架（status=in_progress, output=[]）
response.in_progress

# 文本支线
response.output_item.added      ← {type:"message", id:"msg_...", role:"assistant", content:[]}
response.content_part.added     ← {part:{type:"output_text", text:"", annotations:[]}}
response.output_text.delta  ×N
response.output_text.done
response.content_part.done
response.output_item.done       ← {content:[{type:"output_text", text:"..."}], status:"completed"}

# 工具调用支线（每个 tool_call index 一组，output_index 顺延）
response.output_item.added      ← {type:"function_call", id:"fc_...", call_id:<上游 tc.id>, name, arguments:"", status:"in_progress"}
response.function_call_arguments.delta ×N
response.function_call_arguments.done
response.output_item.done       ← {status:"completed", arguments:"完整JSON"}

response.completed              ← 完整 response：output=[所有 item]、status、usage
```

`usage` 从上游**流末帧**取（`prompt_tokens`→`input_tokens`、`completion_tokens`→`output_tokens`、
`completion_tokens_details.reasoning_tokens`→`output_tokens_details.reasoning_tokens`、
`prompt_tokens_details.cached_tokens`→`input_tokens_details.cached_tokens`）。

出错时发 `response.failed`（带 `error:{code,message}`），**不要**直接断流——Codex 会卡住而不是报错。

### 4.3 非流式

上游只给流，所以 `/v1/chat/completions` 收到 `stream:false` 时：内部用流式请求 → 聚合 content 与
`tool_calls`（按 index 拼接 arguments）→ 一次性返回标准 `chat.completion` JSON（含 `usage`）。
`/v1/responses` 收到 `stream:false` 同理，返回完整 `response` 对象。

---

## 五、代码落点（重构式，不做缝补）

现状问题：`proxy.rs` 2356 行，一个文件同时扛 HTTP 解析、选号、上游转发、SSE 透传、模型缓存。
对外网关要用其中三样，直接调私有函数会让「网关」依赖「接管」，以后改一边必踩另一边。

| 动作 | 文件 | 说明 |
| --- | --- | --- |
| 抽出 | `src-tauri/src/routing.rs` | `choose_account` / `cooldown` / `sticky` / `score` / `expiry_day` / `expiring_credits` / `pick_index` / `billing_candidates` 整体搬迁（含单测），`proxy.rs` 改为调用 |
| 抽出 | `src-tauri/src/upstream.rs` | `upstream_url` / `upstream_path` / `hop_by_hop` / 加票发请求 |
| 新增 | `src-tauri/src/gateway/mod.rs` | 独立监听线程（非阻塞 accept + 配置轮询），与 `proxy::spawn` 同构 |
| 新增 | `gateway/auth.rs` | API Key 校验、常量时间比较、来源判定 |
| 新增 | `gateway/chat.rs` | `/v1/chat/completions`（流式透传 + 非流式聚合） |
| 新增 | `gateway/responses.rs` | `/v1/responses` 转换层（本方案最大一块） |
| 新增 | `gateway/models.rs` | `/v1/models`：`/v2/enterprises/personal/models` → OpenAI 结构，1h 缓存 |
| 新增 | `gateway/codex.rs` | 一键接入 / 撤销接入（读写 `~/.codex`） |
| 前端 | `src/pages/ApiPage.tsx` | 新导航项「API 接入」：开关、端口、Key 展示/复制/重置、Base URL、一键接入 Codex、其它客户端示例 |
| 设置 | `accounts.rs::Settings` | 新增 `gateway_enabled` / `gateway_port`(默认 8790) / `gateway_api_key` / `gateway_lan` / `gateway_local_no_auth`，**同步进 `migrate_settings()`** |

`Settings` 新字段一律 `#[serde(default)]`，且必须落进 `migrate_settings()`——
否则老机器上"默认值只在字段缺失时生效"的坑会重演。

---

## 六、一键接入 Codex（不破坏现有 kiro2api 配置）

写入目标：`%USERPROFILE%\.codex\config.toml`。**只增不改**，用文本段插入 + 存在性检测
（不引 `toml_edit`，原因见风险 5）。

```toml
[model_providers.workbuddy]
name = "WorkBuddy"
base_url = "http://127.0.0.1:8790/v1"
wire_api = "responses"
experimental_bearer_token = "sk-wb-xxxxxxxx"

[profiles.workbuddy]
model_provider = "workbuddy"
model = "deepseek-v4-pro"
```

- 默认写 **profile**，用户跑 `codex --profile workbuddy`；顶层 `model_provider` 不动
  （用户现在指向 kiro2api，直接改会打断他现有用法）。UI 另给「设为默认」二级确认。
- `model_catalog_json` 是**全局单值**，直接改会顶掉 kiro2api 的 catalog。
  做法：读取现有 catalog JSON → 追加 WorkBuddy 模型条目 → 写 `workbuddy-model-catalog.json`
  → 顶层键指向合并后的文件；已有非本工具 catalog 且用户拒绝切换时**跳过**（Codex 用默认 200k 上下文）。
- 备份：写前复制 `config.toml.bak-<时间戳>`，保留最近 3 份；「撤销接入」按备份还原并移除本工具段落。

> 这一步会改用户个人目录下的文件，属于高风险写入：按钮上必须写清"会新增 2 个段落 + 1 个备份文件"，
> 并支持一键还原。

---

## 七、分阶段

| 阶段 | 内容 | 交付后能干什么 |
| --- | --- | --- |
| **P0** | 网关骨架 + Key + `/v1/chat/completions`（流式/非流式）+ `/v1/models` + API 页 | OpenAI SDK、Cline、Cherry Studio、Continue 直接可用 |
| **P1** | `/v1/responses` 转换层 + 一键接入 Codex + `--dump` 调试开关（落真实 payload 便于核对） | `codex --profile workbuddy` 可用 |
| **P2** | `/v1/messages`（Claude Code 直连）、多 Key、按 Key 的用量与限速 | 更接近 kiro2api 的完整形态 |

建议 P0 与 P1 之间先发一版，让 `.workbuddy` 里那些客户端先跑起来验证选号/冷却/额度消耗，
再打 Codex 这块硬骨头。

---

## 八、验收用例（实现前先落成测试）

| # | 场景 | 期望 |
| --- | --- | --- |
| 1 | 无 Key / 错 Key 访问 | 401，body 为 OpenAI 错误结构 |
| 2 | 正确 Key + 来源非本机且 `gateway_lan=false` | 拒绝（不监听即连接失败） |
| 3 | `GET /v1/models` | OpenAI 结构，`id` 为网关模型名，缓存 1h |
| 4 | `POST /v1/chat/completions stream=false` | 200 完整 JSON，含 `usage`（内部聚合） |
| 5 | `POST /v1/chat/completions stream=true` | 客户端收到完整 SSE 并以 `[DONE]` 结束 |
| 6 | 上游 429（免费模型） | 换号重发，客户端无感；日志有 `failover` 记录 |
| 7 | Responses 纯文本 | 事件序列 = 4.2 文本支线，`sequence_number` 连续 |
| 8 | Responses 工具调用 | `function_call` item + `arguments` 增量 + `completed.output` 内含该 item |
| 9 | Responses 多轮（含 `function_call_output`） | 正确转成 `role:"tool"`，上游 200 |
| 10 | `tool_choice` 传对象 | 降级为 `auto`，上游 200（不 400） |
| 11 | 上游 400/超长 | 发 `response.failed`，Codex 报错而非挂死 |
| 12 | 并发 4 路不同会话 | 各自独立选号，不串号 |
| 13 | 关闭开关 | 端口释放，连接被拒 |
| 14 | 端口被占 | UI 明确报错，不静默失败 |

纯函数（转换层）走真实响应原文当 fixture —— 上游 chunk 已抓到，可直接固化成测试数据。

---

## 九、风险与限制（必须先认账）

1. **合规/风控**：把桌面端积分当 API 对外提供，与客户端正常使用特征差异大（高频、无 UI、长连接）。
   可能触发风控或封号。建议：默认关闭、默认仅本机、默认限速，UI 上写明白。
2. **积分消耗**：Codex 每个 turn 反复发全量上下文（`instructions` + tools schema + 历史），
   prompt 缓存又不一定命中。6 个账号可能撑不住一天的密集使用。建议默认模型选低倍率强模型
   （`deepseek-v4-pro` x0.51 / `glm-5.3` x0.69 / `kimi-k3.1-pro` x0.97），并在 API 页显示实时消耗。
3. **模型能力**：Codex 依赖稳定的 `shell`/`apply_patch` 结构化调用。国产模型能跑但会翻车
   （实测免费模型 `hy4flash-free` 面对工具调用直接输出 ```json 文本）。
4. **上游限制**（已实测）：非流式不支持、`tool_choice` 只收字符串、无 `/responses`、无 `/v1/models`。
   前两条是转换层的硬约束，第三条决定了必须有转换层。
5. **新增依赖装不上**：本机 `cargo` 走 crates.io 会 502，需 `--offline`。
   ⇒ 接入 Codex 的 TOML 改写**不要引 `toml_edit`**，用纯文本段插入 + 简单存在性检测。
6. **`model_catalog_json` 全局单值**：与用户现有 kiro2api 配置冲突，必须合并而非覆盖。
7. **Codex 版本漂移**：2026-02 刚移除 `wire_api="chat"`，Responses 的 item/事件集合仍在演进。
   转换层要按"忽略未知字段、只发必要事件"实现，并留 `--dump` 便于对齐。

---

## 十、待你拍板的四个点

| # | 决策 | 选项 | 我的建议 |
| --- | --- | --- | --- |
| 1 | 端口 | ① 新开 8790（独立开关/鉴权） ② 复用 8787（少配一个端口） | **①** |
| 2 | 暴露范围 | ① 仅 127.0.0.1 ② 可开局域网（必须带 Key） | **① 起步，② 留开关** |
| 3 | Codex 接入 | ① 写 profile（`codex --profile workbuddy`） ② 改默认 `model_provider` | **①**，②做二次确认选项 |
| 4 | 首版范围 | ① 先 P0（通用 OpenAI 客户端） ② 直接冲 P1（含 Codex） | **① 先发一版验证，再上 Codex** |

> **定稿（2026-11-24）**：第 2 点最终选了**只有 ①**——「局域网开关」与「本机免 Key」
> 两个开关都不做，监听地址恒为 `127.0.0.1`、每个请求都必须带 Key（理由见
> `p0-changes.md` 里的删除说明）。第 3 点最终是 ②（直接改顶层 `model_provider`），
> 首版就把 P0/P1 一起发了。
