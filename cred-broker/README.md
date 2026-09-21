# cred-broker

跨机器共用同一批账号凭证的 **「池 + 闸」**。线上地址
`https://cred-broker.sloan.dpdns.org`（客户端把它写成编译期常量）。

一个 Cloudflare Worker（D1 存储）。服务端只做「存凭证 + 发续签闸」，**续签本身始终在客户端
本机执行**，不出站、不跑任何后台任务。

---

## 它解决什么问题

官方续签是**单链轮换**：续签返回一个新的 refresh token，服务端按账号只认最新那一条。
多台机器各持同一份 RT 的副本时，谁先续签谁赢，其余副本连旧 AT 一起被踢；而「已用过的
refresh token 再次出现」还会被判成凭证泄露，把整条链作废。

所以要解决的不是「怎么同步凭证」，而是**怎么保证同一时刻只有一台机器在续签**。
cred-broker 提供两样东西：

1. **池** —— 一整池凭证的集中存放处，跨机器共享，`uuid` 即地址；
2. **闸** —— 一把只发一份的续签许可。谁抢到闸，谁才有资格改这一池，改完 CAS 交回。

---

## 功能特性

| 能力 | 说明 |
|---|---|
| 凭证池存取 | 一整池账号凭证按 `uuid` 存 / 取，服务端只做规范化与透传，不解释业务含义 |
| 互斥续签闸 | 同一时刻只有拿到闸的机器能提交；租约到期自动释放，**无需任何后台任务** |
| 取件即最新 | 抢闸成功时**顺带返回那一刻的整池内容**，客户端不必再 GET，杜绝「读了旧副本去签」 |
| CAS 提交 | 提交带版本号 + 持闸人双重校验，版本 +1；旧副本整块盖回会被 409 拒绝 |
| 失败冷却 | `abort` 释放闸并记冷却期，凭证真死时所有机器停手，不轮流敲已死的链 |
| 条目规范化 | `region`、`key` 与展示标识（`phone` / `email`）都必须原样往返（区域丢过 → 客户端会凭空多出重复账号）；去重、去空白、缺失时间戳落 `null`；超上限截断 |
| 审计日志 | 每次操作（创建 / 取件 / 抢闸 / 提交 / 放弃 / 删除）全量落库，可回溯 |
| 无鉴权设计 | `uuid` 即凭据，不做二次 key（详见「它不做什么」） |
| 健康检查 | `/v1/health` 返回闸参数，客户端可核对与自身编译期常量是否一致 |

---

## 它不做什么

- **不续签、不出站**：`triggers.crons = []`，没有任何后台任务。续签始终在客户端本机 IP 上执行。
- **不鉴权**：凭据就是池 uuid 本身。知道 uuid 就等于有全部权限（能读整池 token）。
  加一层 key 只会制造「探活显示连接正常、到续签那一刻才 403」这种延迟暴露的坑。
- **不按账号分粒度**：粒度是**一整池**。按账号分粒度时，两台机器可以各自拿着不同账号的闸、
  同时提交同一批账号，于是出现「一半新一半旧」这种**谁也没签错**的错状态。

---

## 客户端工作流（三步握手）

```
         GET /v1/pool/:uuid          取件（可选，先看池里有什么）
         POST /v1/pool/:uuid/lease   抢闸 —— 抢到的人同时拿到那一刻的池内容
   ┌──── 成功 ────► 用闸带回的 items 在本机续签
   │
   ├─ 续签全成功 ─► PUT /v1/pool/:uuid    提交：version + 1，归还闸
   ├─ 续签有失败 ─► PUT 照常提交（已签的写回），失败的账号下次再试
   └─ 凭证已死  ─► POST /v1/pool/:uuid/abort  释放闸 + 记冷却，所有人都停手
```

> 抢到闸时响应里带着 `items`，**客户端必须用它去签**：本地那份 refresh token 可能早就被
> 别的机器换掉了，再自己 GET 一次读到的是同一份内容，只会多一次往返、多一个出错的机会。
>
> 三步**缺一步就退化成「两边同时续签」**。

---

## 接口

基础地址：`https://cred-broker.sloan.dpdns.org`

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/v1/health` | 探活，返回 `lease_ms` / `fail_cooldown_ms` |
| POST | `/v1/pool` | 新建一池，body `{ items }` |
| GET | `/v1/pool/:uuid` | 取件（只读，不动闸） |
| POST | `/v1/pool/:uuid/lease` | 抢闸（成功时带回整池） |
| PUT | `/v1/pool/:uuid` | 提交整池（CAS），body `{ version, items }` |
| POST | `/v1/pool/:uuid/abort` | 放弃 + 记冷却，body `{ note? }` |
| DELETE | `/v1/pool/:uuid` | 删池（= 解绑） |

机器标识走 `x-cred-actor` 头（或 body 里的 `actor`），**只进审计，不参与任何判断**。

### GET /v1/health

```json
// 200
{ "ok": true, "now": 1760000000000, "lease_ms": 180000, "fail_cooldown_ms": 300000 }
```

### POST /v1/pool

请求体：`{ "items": [ ...条目... ] }`

```json
// 201
{ "uuid": "3f2c1a9e-...", "version": 0, "count": 2, "now": 1760000000000 }
```

`uuid` 由**服务端**颁发（客户端不自己推导，否则两台机器会各造一个）。版本从 0 起。

### GET /v1/pool/:uuid

```json
// 200
{ "items": [ ...整池最新条目... ], "version": 0, "now": 1760000000000 }

// 404（池不存在，客户端据此判定「池在别处被解绑」）
{ "error": "gone", "now": 1760000000000 }
```

### POST /v1/pool/:uuid/lease

```json
// 200 抢到：items 是那一刻的整池内容，客户端必须用它去签
{ "granted": true, "items": [ ... ], "version": 0, "lease_until": 1760000180000, "now": 1760000000000 }

// 200 没抢到：告知持闸人 / 冷却期 / 何时能再试
{ "granted": false, "retry_after": 1760000000000, "lease_owner": "mac-mini", "lease_until": 1760000180000, "now": 1760000000000 }
```

### PUT /v1/pool/:uuid

请求体：`{ "version": 0, "items": [ ...续签后的整池... ] }`

```json
// 200
{ "ok": true, "version": 1, "count": 2, "now": 1760000000000 }

// 409 版本或持闸人不符（防「拿着旧副本整块盖回去」）
{ "error": "stale", "version": 1, "lease_owner": "mac-mini", "now": 1760000000000 }
```

`version` **必须是数字**：字符串版本号等于默许两边对「版本」的理解不一致，会一直提交不上
却看不出原因。

### POST /v1/pool/:uuid/abort

```json
// 200
{ "ok": true, "retry_after": 1760000300000, "now": 1760000000000 }
```

不要求闸还在自己手里：凭证真死的时候，需要的是**所有人都停手**，而不是让持有闸的那台
机器继续敲同一条已死的链。

### DELETE /v1/pool/:uuid

```json
// 200
{ "ok": true, "now": 1760000000000 }
```

### 错误码汇总

| 状态 | `error` | 含义 |
|---|---|---|
| 400 | `bad_json` | body 不是合法 JSON |
| 400 | `bad_uuid` | 路径不是合法 uuid 形态（不去查库） |
| 400 | `bad_version` | 提交时 `version` 不是非负整数 |
| 404 | `gone` | 池不存在或已被删除 |
| 404 | `not_found` | 路径不存在 / 前缀不是 `v1` |
| 405 | `method_not_allowed` | 方法不对（如对 health 发 PUT） |
| 409 | `stale` | CAS 失败：版本或持闸人不符 |
| 500 | `internal` | 未捕获异常（带 message，供客户端区分「代码写错」和「网络不通」） |

---

## 为什么抢闸要顺带返回 `items`

客户端拿到闸之后如果自己再 `GET` 一次，读到的是同一份内容 —— 多一次往返、多一个出错的机会。
更要紧的是：本地那份 refresh token **可能早就被别的机器换掉了**，而闸带回来的永远是最新的。

---

## 原子性

准入判据**只有** `UPDATE … WHERE lease_until <= ? AND retry_after <= ?` 的 `changes === 1`；
提交是 `version = version + 1 WHERE version = ? AND lease_owner = ?`。
**不要写成「先 SELECT 判断、再 UPDATE」** —— 那中间就是双死窗口。

存储必须是 D1：KV 全球最终一致、同 key 每秒只能写 1 次（抢闸恰好撞上）；Cache API 没有 CAS；
R2 只有 `If-Match`，没有 read-modify-write。真要换，唯一等价物是 Durable Objects。
实测 D1 查询 2.65ms、端到端请求 1.1–3.3s（握手占大头），换存储省不到 0.1%。

---

## 条目结构

服务端只做规范化与透传，不解释业务含义：

```json
{
  "region": "cn",
  "key": "cn:13800000000",
  "name": "昵称",
  "phone": "13800000000",
  "email": "a@b.c",
  "access_token": "…",
  "refresh_token": "…",
  "expires_at": 1760000000000,
  "rt_expires_at": 1770000000000,
  "updated_at": 1759900000000
}
```

规范化规则：

- `region`（`global` / `cn`）是**必存字段**，取值顺序 = 显式字段 → **key 的 `xx:` 前缀** → 国际版。
  ⚠️ 这一条曾经被整条丢掉（返回值里根本没有这个键），后果不是「少个字段」而是**多出账号**：
  客户端拿回任何条目都落成国际版，于是同一条国内版凭证在池里被当成新账号收养一次，
  界面上就凭空多出一个「国际版」的重复账号，且因为区域不同，之后再怎么导入都不会与真身合并。
  老 payload 里的区域只存在于 key 前缀中，所以**读出口也走一遍规范化**（`parseItems`），
  老条目在读到的那一刻就自愈，不需要迁移脚本。
- `key` 是**跨机身份锚点**（客户端按 `显式 key → 手机号 → 昵称 → 本地 id` 生成，带区域前缀）；
  客户端的合并是并集，全靠它认人，本地独有的账号一条都不会被删。同 key 重复时后到的覆盖先到的。
  ⚠️ 它**会漂移**（手机号是后来才补上的、昵称会被改），所以客户端认人是「key 相等 **或**
  同一份 token」，并且会把「同一凭证、另一个 key」的副本从池里清掉 —— 详见
  `qoder-assistant/src-tauri/src/broker.rs` 的 `claims` / `is_drifted_duplicate`。
- `phone` / `email` 是**展示标识**，同 `region` 一样**必须原样往返回去**，但它们**不进 `key`、
  不参与认人**：客户端按「区域选首选、另一项回落」显示（国内版看手机号、国际版看邮箱）。
  有了 `email`，别的机器收养这条账号时当场就显示得出标识，不必自己再打一次 `/api/v1/userinfo`。
  老条目没有这个字段 → 空串，客户端按「缺这个字段」处理（回落到手机号或刷新时补上）。
- 两个 token 都为空 → 整条丢弃（只剩昵称的条目会让「这一池有几条」失真）。
  只有一个 token 的条目保留（access 缺失但 refresh 在，仍可续签）。
- 缺失的时间戳落成 `null`，**不写 0**：0 会被读成 1970 年，是比 null 更坏的数据。
- 前后空白被抹掉，避免「同一个 key 因为空格被当成两个账号」。
- 超上限时截断到 `MAX_ITEMS = 200`。

---

## 开发与部署

```bash
export HTTPS_PROXY=http://127.0.0.1:7897 HTTP_PROXY=http://127.0.0.1:7897   # 本机直连 CF 会超时

node test/cred.test.js          # 用 node 内置 sqlite 跑真 SQL，34 项
npx wrangler deploy
npx wrangler d1 execute cred-broker --remote --command "<单条 SQL>"
npx wrangler tail
```

⚠️ `wrangler d1 execute --file` 在**本机恒失败**（POST /import 返回 200 后，轮询导入结果的
请求被掐断 → `fetch failed`）。D1 会把整批自动回滚，可安全重试。绕过办法是把 SQL 按 `;`
拆成单条、逐条 `--command`（只走 /query，没有轮询）。

### 测试

测试用 node 内置的 `node:sqlite` + **真 SQL**（`migrations/0001_init.sql` 建表），不用手写
fake —— 因为最要紧的东西恰恰在 SQL 里：抢闸的 `WHERE lease_until <= ? AND retry_after <= ?`、
提交的 `version = ? AND lease_owner = ?`。自己写个「假装原子」的 fake，测的就只是 fake
自己的假设，等于什么都没测。

覆盖：健康检查、建池（颁发 uuid / 丢弃无 token 条目 / 去重 / 截断）、路径校验、取件、
抢闸（互斥 / 租约过期接管 / 冷却）、CAS 提交（版本不符 / 持闸人不符 / version 类型）、
abort（不要求持闸）、删池后全部 404、审计事件、actor 不参与判断、纯函数规范化、
**区域往返（显式字段 / key 前缀回填 / 老 payload 在读出口自愈）**、**邮箱往返（透传 / 不进 key / 老条目落空串）**。

---

## 运维

```bash
curl https://cred-broker.sloan.dpdns.org/v1/health
```

```bash
# 看某一池最近发生了什么（排查「为什么这台机器说池没了」）
npx wrangler d1 execute cred-broker --remote \
  --command "SELECT datetime(at/1000,'unixepoch','localtime') t, kind, actor, note FROM pool_events WHERE uuid='<uuid>' ORDER BY id DESC LIMIT 30"
```

**删池是不可逆的**：绑过同一 uuid 的其他机器之后会拿到 `gone`，需要重新上传或重新绑定。

---

## 配置项

| 环境变量 | 默认值 | 含义 |
|---|---|---|
| `LEASE_MS` | `180000` | 续签闸的租约时长（毫秒）。**必须大于客户端「把这一池该签的都签掉 + 提交结果」的最坏耗时**，否则租约提前失效、第二台机器拿到闸，而单链轮换下两边都可能死。闸的作用域是整池，要容得下「一轮里连续签若干个账号」 |
| `FAIL_COOLDOWN_MS` | `300000` | 续签失败后的冷静期：凭证真死时，别让几台机器轮流敲同一条已死的链 |

正常路径上签完就立刻归还，`LEASE_MS` 只决定「出意外时闸被占多久」。
