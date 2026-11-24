# workbuddy2api · P0 实施清单

> 目标：把复制来的助手骨架改造成「账号池 → OpenAI 兼容 API」网关，首版不含 Codex 的 Responses 转换。
> 范围（已确认）：保留账号/签到/台账/简报/日志，**移除智能接管**，新增 API 网关。

---

## 一、模块增删

### 新增

| 文件 | 职责 | 来源 |
| --- | --- | --- |
| `src-tauri/src/httpsrv.rs` | HTTP 服务器原语：请求解析、读头、写头、chunked 输出、hop-by-hop 过滤 | 从 `proxy.rs` 提取 |
| `src-tauri/src/routing.rs` | 选号：评分排序、冷却表、粘滞、台账补拉 | 从 `proxy.rs` 提取 |
| `src-tauri/src/upstream.rs` | 上游地址改写（`/v1/*` → `/v2/*`）、带票请求 | 从 `proxy.rs` 提取 |
| `src-tauri/src/gateway/mod.rs` | 网关监听线程 + 路由分发 + 访问日志 | 新写 |
| `src-tauri/src/gateway/auth.rs` | Key 校验（常量时间）、来源判定 | 新写 |
| `src-tauri/src/gateway/chat.rs` | `POST /v1/chat/completions`：流式透传 + 非流式聚合 | 新写 |
| `src-tauri/src/gateway/models.rs` | `GET /v1/models`：`/v2/enterprises/personal/models` → OpenAI 结构 | 新写 |
| `src/pages/ApiPage.tsx` | API 接入页 | 新写 |

### 停用（文件保留，不再声明 `mod` / 不再被 import）

| 文件 | 原因 |
| --- | --- |
| `src-tauri/src/proxy.rs` | 接管反代，职责已拆进 routing / upstream / gateway |
| `src-tauri/src/stealth.rs` | 接管端点的装卸与日记，新项目不接管官方客户端 |
| `src-tauri/src/netfix.rs` | 只为接管服务的网络急救 |
| `src/pages/TakeoverPage.tsx` | 同上 |

> 不物理删除的原因：本机 `rm` 被 DLP 钩子拦截，且这些文件是 `workbuddy-assistant` 的对照参考。
> 停用后 Rust 侧根本不会编译它们、前端也不会打包。

### 修改

| 文件 | 改动 |
| --- | --- |
| `lib.rs` | `mod` 列表、`invoke_handler` 注册 |
| `accounts.rs` | `Settings` 新增 5 个字段 + `migrate_settings()` 迁移 |
| `commands.rs` | 去掉接管相关命令调用；新增 `gateway_status` / `gateway_key_rotate` |
| `logs.rs` | `unified()` 不再合并 `takeover-journal.jsonl` |
| `tray.rs` | 去掉「接管生效」绿点 |
| `App.tsx` / `api.ts` / `types.ts` | 导航项与命令封装 |

---

## 二、设置项（`Settings` 新增，全部 `#[serde(default)]`）

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `gateway_enabled` | `false` | 网关总开关。**默认关**，与"接管默认关"一致 |
| `gateway_port` | `8790` | 监听端口（避开 8787/8788/8789） |
| `gateway_api_key` | 空 | 空 = 首次开启时生成 `sk-wb-<32hex>` |

> **2026-11-24 已删除**（原 P0 里的两个字段）：`gateway_lan`（允许局域网访问）与
> `gateway_local_no_auth`（本机免 Key）。理由：这条通道烧的是整个账号池的积分，
> `127.0.0.1` 上跑着什么程序用户未必全清楚，而丢一次 Key 只需要点一下「重新生成」。
> 现在监听地址恒为 `127.0.0.1`，鉴权对每个请求都生效（含健康检查以外的全部路由）。
> 老配置文件里的这两个键会被 serde 静默忽略，不需要迁移。

`migrate_settings()` 里补一段：`gateway_port == 0` → 8790；`gateway_api_key` 空且 `gateway_enabled` → 生成。

---

## 三、网关行为

| 路由 | 行为 |
| --- | --- |
| `POST /v1/chat/completions` | 上游恒用 `stream:true`；下游 `stream:false` 时聚合后返回完整 JSON |
| `GET /v1/models` | 上游 `/v2/enterprises/personal/models` 映射为 `{object:"list", data:[{id,object:"model",owned_by}]}`，1h 缓存 |
| `GET /healthz` | `200 ok`（给客户端做连通性探测，不鉴权） |
| 其它 | 404，OpenAI 错误结构 |

- 鉴权：`Authorization: Bearer <key>` 或 `x-api-key`，常量时间比较；`gateway_local_no_auth` 且来源为本机时放行。
- 选号：完全复用现有 `(到期自然日, −待过期积分)` 排序 + 「账号×模型」冷却。
- 上游错误：**不只 429**。按业务码分 12 类、各档冷却与「要不要换号」见
  [`upstream-hardening.md`](./upstream-hardening.md)；单请求最多换 3 个号。
- 访问日志：写 `app-logs.jsonl`（新类别 `api`，字段：模型/账号/状态/耗时），不落请求体。

---

## 四、测试场景（先写用例再改代码）

### 纯函数（`cargo test --offline`）

| # | 用例 | 断言 |
| --- | --- | --- |
| U1 | `path_map("/v1/chat/completions")` | → `/v2/chat/completions` |
| U2 | `path_map("/v1/models")` | → 标记为本地路由（不打上游） |
| U3 | `path_map("/v2/chat/completions")` | 原样（兼容旧客户端） |
| U4 | `bearer_key("Bearer sk-wb-abc")` | → `Some("sk-wb-abc")` |
| U5 | `bearer_key("bearer  sk-wb-abc ")` | 大小写不敏感、trim |
| U6 | `key_ok(expected, given)` | 相等 true；长度不同 false；空 expected 恒 false |
| U7 | `sse_to_completion(上游原始帧)` | 聚合成标准 `chat.completion`，content/tool_calls/usage 齐全 |
| U8 | `sse_to_completion` 含 tool_calls 多帧 | `arguments` 拼接完整、`index` 不串 |
| U9 | `sse_to_completion` 空流 | 返回错误而不是空 200 |
| U10 | `models_from_upstream(真实响应原文)` | 数量一致、`x0.00` 的标记为免费、id 保序（免费在前） |
| U11 | `choose_conv_key(无任何线索)` | 退回稳定 hash，5 分钟内同值 |

### 集成（手动/脚本，端口 8790）

| # | 场景 | 期望 |
| --- | --- | --- |
| I1 | 无 Key 访问 `/v1/models` | 401 + OpenAI 错误体 |
| I2 | 错误 Key | 401，且耗时恒定（常量时间比较） |
| I3 | 正确 Key，`stream:true` | 收到完整 SSE，以 `data: [DONE]` 结束 |
| I4 | 正确 Key，`stream:false` | 200 完整 JSON，含 `usage` |
| I5 | 用本机局域网 IP 访问 | 连接被拒（只绑 `127.0.0.1`） |
| I5b | 本机访问但**不带** Key | 401（没有本机豁免） |
| I6 | 关闭开关 | 端口释放，连接被拒 |
| I7 | 端口被占 | 页面明确报错，不静默 |
| I8 | 上游 429（免费模型打满） | 换号重发，客户端无感；日志有 failover 记录 |
| I9 | 并发 3 路不同模型 | 各自独立选号，日志账号不串 |

---

## 五、风险与已知限制（P0）

1. 上游不支持非流式 ⇒ 非流式请求要在网关内聚合，长回复会占满一个连接（沿用 chunked，无 Content-Length 预知）。
2. 上游 `tool_choice` 只接受字符串 ⇒ P0 对对象形式直接降级为 `auto`。
3. ~~无 `/v1/responses` ⇒ Codex 接不上~~ —— 已在 P1 补上，见 `responses-wire.md`。
4. 请求体上限沿用现有 `MAX_BODY`，Codex 的长上下文（实测 186KB）能过，但更大的要复核。
