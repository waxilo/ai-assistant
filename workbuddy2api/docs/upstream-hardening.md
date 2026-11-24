# 上游加固：错误分级、冷却档位、出站改写

> 起因：参考开源项目 `linguo2625469/workbuddy2api-panel`（Go，2026-10 已停维护）的
> 失败模式记录，把它写在注释里的「踩坑结论」落到本项目。**只抄结论，不抄架构**——
> 它的选号是加权随机，与本项目「纯排序」的既定口径冲突，那条明确不引入。

## 一、修掉的真问题

### 1. 非 429 错误既不罚号也不换号

`gateway::serve_chat` 早期只识别 `429`，其余状态码走 `relay` 原样返回。后果很具体：

- 某号余额耗尽 → 402，冷却表没记 → 下一轮排序照样选中它；
- 某号对该模型无权限 → `400 {"code":11102,"msg":"model [...] service info not found"}`，
  同样不记 → 每次请求都撞同一个号同一个错。

用户侧表现：「网关一直在报错，但账号列表里那几个号看着都正常」。

### 2. `developer` 角色被上游拒绝

实测（2026-11-24，真实账号 + 免费模型）：

```
POST /v2/chat/completions  {"messages":[{"role":"developer",...}]}
→ 400 {"code":11128,"msg":"Illegal API invocation from an unapproved channel",
       "displayTips":{"zh":"这次调用没通过渠道校验，可能不是官方客户端发出的。请用官方版本重新发送。"}}
```

同一請求把 `developer` 换成 `system` → 正常 SSE。`developer` 是 OpenAI 新规范里
`system` 的别名（Codex 与新版 SDK 会发），所以归一不改变语义。
**只改这一个值**，其余 role（含未知值）原样保留。

### 3. 下游客户端的 UA 被透传给上游

`reqwest` 的同名头以后写的为准，所以把下游的 `user-agent` 透传过去，等于把
我们自己设的 `WorkBuddy2Api/x.y.z` 覆盖掉。下游发的是 `OpenAI/Python 1.55.0`、
`codex_cli_rs/...` 这类一眼不像官方客户端的串；上游对 UA 有白名单校验，
送错轻则被记成可疑设备，重则 `403 code 11140 "request illegal"`。

现在一律剥掉：`user-agent`、`x-stainless-*`（OpenAI 官方 SDK 的调试头）、
`referer`、`origin`、`x-title`，以及我们自己的 `x-api-key` / `x-gateway-key`。

### 4. `max_completion_tokens` 被上游忽略

OpenAI 已把 `max_tokens` 标为 deprecated、让新 SDK 发 `max_completion_tokens`，
上游只认 `max_tokens`。别名原样透传会被忽略，然后回落到默认输出上限
（表现为「设了 4096 却只出几百 token」）。现在翻译过去，显式 `max_tokens` 优先。

## 二、错误分级（`routing::classify`）

**业务码优先，HTTP 状态兜底**——上游把语义写在响应体的 `code` 里，HTTP 状态可能统一是 400。

| `code` | 分类 | 处置 |
| --- | --- | --- |
| 11101 | `BadRequest` | 不罚号、**不换号**（换号得到同样的错），透传 |
| 11115 | `BadRequest` | 同上 |
| 11102 / 11158 | `ModelBlocked` | (账号, 模型) 负缓存 6h 起 ×2 封顶 24h；换号 |
| 11128 | `ChannelRefused` | **不罚号**（账号健康），但可换号试 |
| 11140 + `request illegal` | `Restricted` | 账号级硬冷却 24h（只能官方申诉，重登无效） |
| 11140 + rate-limiting | `RateAccount` | 账号级限流冷却 |
| 12153 | `SessionDead` | 账号级硬冷却 24h（refresh token 已废） |
| 6004 | `RateModel` | **模型级**冷却（换模型即豁免） |
| 402 / 余额关键词 | `CreditExhausted` | 账号级硬冷却 6h |
| 429（无业务码） | `RateAccount` | 账号级限流冷却 |
| 403 | `Waf` | 账号级 60s（信号比 429 轻） |
| 404 | `NotFound` | 账号级 60s，**不按限流退避**（偶发路径缺失不是限流信号） |
| 5xx | `Server` | 熔断计数，连败 3 次才罚 |

「硬冷却」= 调了必定失败（余额耗尽 / 会话失效 / 被限制），全冷却兜底时**跳过**。

## 三、冷却档位

两档必须分开存，合成一档会给两种行为都带上错误后果：

| 键 | 场景 | 为什么 |
| --- | --- | --- |
| `Account(account_id)` | 余额耗尽、会话失效、被限制、账号级限流、WAF、5xx 熔断 | 换模型也救不回来 |
| `Model(account_id, model)` | 6004 模型级限流、11102 无此模型 | 按账号整体记会把本来还能服务的模型一起赶走 |

实测依据（2026-09-14）：同一账号 `hy3` 吃 429 的同一时刻，主模型
`deepseek-v4.1-flash` 依然 200 —— 限流本来就是按「账号 × 模型」算的。

### 两条容易写错的细节

- **有上游重置时刻时绝不做指数堆加**。上游给的是权威墙钟
  （`"将在 2026-09-14 19:35:25 UTC+8 重置"`），堆加只会让号更久不可用。
- **已经在冷却中再撞一次 429，不推进退避、不延长冷却**。用户重试或并发兜底探测
  每撞一次就把 streak +1 指数翻倍，是「全池被推到 2h 封顶」的元凶。

## 四、数值一览（调参见此表）

| 项 | 值 | 位置 |
| --- | --- | --- |
| 有界退避基数 | 10 分钟 | `routing::SOFT_BASE_MS` |
| 有界退避封顶 | 2 小时 | `routing::SOFT_CAP_MS` |
| 冷却时长夹取范围 | 30 秒 ~ 24 小时 | `routing::COOLDOWN_MIN_MS` / `MAX_MS` |
| 5xx 熔断阈值 | 连败 3 次 | `routing::BREAKER_THRESHOLD` |
| 5xx 熔断时长 | 30 分钟 × 2ⁿ 封顶 6 小时 | `BREAKER_BASE_MS` / `BREAKER_CAP_MS` |
| WAF 冷却 | 60 秒 | `routing::WAF_MS` |
| 404 冷却 | 60 秒 | `routing::NOT_FOUND_MS` |
| 会话失效 / 被限制 | 24 小时（硬） | `SESSION_DEAD_MS` / `RESTRICTED_MS` |
| 余额耗尽 | 6 小时（硬） | `CREDIT_EXHAUSTED_MS` |
| 模型负缓存 | 6h 起 ×2 封顶 24h | `MODEL_BLOCK_BASE_MS` / `CAP_MS` |
| 单请求最多换几个号 | 3 | `routing::FAILOVER_MAX_TRIES` |
| 选择顺序 | 到期自然日 → 当日待过期积分（纯排序） | `routing::score` |

## 五、全冷却兜底

无健康候选时，不再直接 503，而是选「解禁最早」的那个（硬冷却号排除）。
直接 503 会让客户端以为网关挂了，实际只是暂时没号；而兜底选硬冷却号则是拿
必败的请求去撞 402，只会把「没号」变成一条误导性的余额错误。

## 六、明确不引入的（开源项目有，本项目不抄）

| 机制 | 为什么不抄 |
| --- | --- |
| 加权随机选号（`credits 比例 × 10 + 闲置补偿`，Top5 抽签） | 与本项目「每条请求对当前可用集纯排序」的既定口径冲突，用户已明确定过 |
| 三种调度策略（weighted_random / round_robin / sequential_exhaust） | 同一套随机语义的子机制 |
| 成本分层 tier0/1/2 + 条件探索改道 | 同上 |
| 提示词注入（`defaultprompt.md`） | 会改变用户请求的语义 |
| 多 realm（cn/global）分流、企业版 session | 本项目只支持 WorkBuddy 国内版一个域 |
| `paused` 轻量暂停态 | 它解决面板多用户运维，桌面端用「禁用」就够 |
| `max_in_flight` 单号在途上限（默认 3 / global 2） | 桌面单机并发低；等真有并发压力再加 |
| `credit_floor` 积分保底（曾实现，2026-11-24 按用户要求移除） | 选号只按到期日/待过期积分排序；保底需要模型倍率缓存参与判定，等于让「排序」依赖一份可能过期的展示数据 |

`X-Machine-ID` / `X-Session-ID` 按账号稳定派生（开源项目用它防多号被设备指纹识别）
暂未引入：它属于「主动伪装」，与当前 `WorkBuddy2Api/<ver>` 自报身份的策略不一致，
要加得先定清楚这一层的口径。
