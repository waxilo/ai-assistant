# cred-broker

跨机器共用同一批账号凭证的 **「池 + 闸」**。线上地址
`https://cred-broker.sloan.dpdns.org`（经网关；桌面客户端的编译期常量也是这一域名 ——
容器不在桌面端那台机器上，旧回环常量 `http://127.0.0.1:7003` 只在部署机上成立，2026-09-30 改）。

本机 Docker 容器（Node 24 + 共享 MySQL，见 `../../mysql-server`），公网域名由共享网关
`../../gw` 按 Host 转发进来。服务端只做「存凭证 + 发续签闸」，**续签本身始终在客户端
本机执行**，不出站、不跑任何后台任务。

> **迁移已完成（2026-09-26）**：Cloudflare 上的 Worker 与 D1 库都已删除，本仓库里不再有任何
> CF 配置（`wrangler.toml`、`migrations/` 一并删掉）。`https://cred-broker.sloan.dpdns.org`
> 的解析仍然走 Cloudflare 的通配 DNS + 隧道，但**应答的是下面的本机容器**，MySQL 里那份
> 是唯一副本。历史搬迁过程见「数据搬迁（已完成）」。

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
| 凭证池存取 | 按 `uuid` 存 / 取，服务端只做规范化与透传，不解释业务含义 |
| 逐账号存储 | 每条账号是 `pool_items` 里的一行，绑池 uuid；池配置（`renewer` / 闸 / 版本）留在 `pools` 表。整池 JSON 不再落一列 |
| 逻辑删除 | 提交时本轮没出现的 key 记墓碑（`deleted_at`），随 `GET` / `lease` 的 `deleted` 发给客户端，避免「云端删了、别的机器又传回来」 |
| 互斥续签闸 | 同一时刻只有拿到闸的机器能提交；租约到期自动释放，**无需任何后台任务** |
| 一池一个续签设备 | 池上只存一个组合 id（客户端类型 + 机器唯一标识）；只有它续签云端账号，其余机器只同步。换设备必须旧设备先清空、新设备再认领 |
| 取件即最新 | 抢闸成功时**顺带返回那一刻的整池内容**，客户端不必再 GET，杜绝「读了旧副本去签」 |
| CAS 提交 | 提交带版本号 + 持闸人双重校验，版本 +1；旧副本整块盖回会被 409 拒绝 |
| 失败冷却 | `abort` 释放闸并记冷却期，凭证真死时所有机器停手，不轮流敲已死的链 |
| 条目规范化 | `region`、`key` 与展示标识（`phone` / `email`）都必须原样往返（区域丢过 → 客户端会凭空多出重复账号）；去重、去空白、缺失时间戳落 `null`；超上限截断 |
| 审计日志 | 每次操作（创建 / 取件 / 抢闸 / 提交 / 放弃 / 删除）全量落库，可回溯 |
| 无鉴权设计 | `uuid` 即凭据，不做二次 key（详见「它不做什么」） |
| 健康检查 | `/v1/health` 暴露闸参数与服务器时间，供运维核对部署；**当前三个客户端都不读它** |

---

## 它不做什么

- **不续签、不出站**：没有任何后台任务（Worker 时代 `triggers.crons` 刻意留空，容器里也
  就不装进程内定时器 —— 加一个空跑的 ticker 只会让人以为「管家会自己续签」）。
  续签始终在客户端本机 IP 上执行。
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

**闸只防「同时」，不防「轮流」**：两台机器错开时间各自抢闸续签，仍然会轮换同一条链，
把上一台手里的 refresh token 作废。所以池上再记一个 `renewer`（组合 id）：

```
   PUT  /v1/pool/:uuid/renewer          认领（renewer 为空或就是自己才成功；否则 409）
   POST /v1/pool/:uuid/renewer/release  旧设备清空（只有当前那台能清）
   GET  /v1/pool/:uuid/renewer          读当前续签设备（客户端 10 秒回落时用它）
```

客户端只让 `renewer == 自己` 的机器续签**云端同步账号**；本机登录账号不受这个字段约束，
手动导入的公用 token 永不续签。换设备的流程是「旧设备取消续签（置空）→ 新设备设为续签」，
服务端不提供强制覆盖 —— 旧设备没退位就让新设备接管，等于给自己留了一条两台同时续签的路。
旧设备清空后有 10 秒宽限：没有新设备认领就自动把自己写回去，避免整池停在「无人续签」。

**删除要能被别的机器看见**：池里的账号是**逻辑删除**（`pool_items.deleted_at`）。一次 `PUT`
是整池替换，本轮没出现的 key 就被记成墓碑；墓碑留在库里，随 `GET` / `lease` 的 `deleted`
数组一起发出去。客户端：

- 删账号时在本机 `broker.json` 的 `deleted_keys` 记一笔（同步成功后清空）—— 否则下一轮
  同步的并集会把这条账号从池里原样加回来，用户看到「删了又回来」；
- 同步时跳过这些 key、提交时不带它们（服务端据此记墓碑），同时按 `cloud_ids` 把
  `deleted` 里对应的云端账号从本机移除；
- 重新登录同一个账号后再上传，同一个 key 会**复活**墓碑（`deleted_at = NULL`）。

`cloud_ids` 仍是「哪些账号跟着池走」的本机标记；第一次升级上来的机器只登记、不删，
空快照（live 与墓碑都为空）一律不剪 —— 服务端坏 JSON 会被静默读成空池，不能据此删光本机账号。

**同账号冲突按来源定优先级**（客户端规则，服务端不参与）：本地登录 > 云端同步 > 手动导入。
高优先级那一份**可用**时，低优先级的不许覆盖它 —— 池里那张就算 `iat` / `exp` 更晚也一样；
低优先级只在「补空字段」时被采纳。反过来，**不可用就无条件换血**：本机这张已被判死、
续签链过期、或没链又已过期时，只要池里那份可用就直接替换（2026-10-08 死票锁死事故的抢救
通道）。同源之间（云端账号 ↔ 池里那份）仍按新序判：`iat` 更新或 `exp` 更晚才算更新。
**池里那份自己失效就直接跳过**：不为它建新账号、也不拿它补任何字段（手机号、到期、RT），
本机没有匹配账号时同样跳过 —— 接管一具尸体只会让本机多一条签不动的账号；要修这条链，
靠的是本机那份可用凭证在回写方向把池里那条压掉。
本地 → 云端方向对称执行：可用的本地登录账号永远回写并压过池里那份；云端账号只在严格更新时
回写；手动导入的公用 token 永不回写；任何不可用的票（判死 / 链过期）都不许进池。

---

## 接口

基础地址：客户端的编译期常量是 `https://cred-broker.sloan.dpdns.org`（经网关）；
部署机上还可直连容器发布的宿主回环调试端口 `http://127.0.0.1:7003`，两者同一套路径。

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/v1/health` | 探活，返回 `lease_ms` / `fail_cooldown_ms`（按设计**不查库**） |
| GET | `/healthz` | 容器健康检查，真的 `SELECT 1`；只给 compose 用，不在这张对外接口表的语义里 |
| POST | `/v1/pool` | 新建一池，body `{ items, renewer? }`（上传者通常就是第一个续签设备） |
| GET | `/v1/pool/:uuid` | 取件（只读，不动闸），带回 `renewer` |
| POST | `/v1/pool/:uuid/lease` | 抢闸（成功 / 被拒都带回 `renewer`） |
| PUT | `/v1/pool/:uuid/renewer` | 认领续签设备，body `{ renewer }` |
| POST | `/v1/pool/:uuid/renewer/release` | 清空续签设备，body `{ renewer }`（只有当前那台成功） |
| GET | `/v1/pool/:uuid/renewer` | 读当前续签设备（不记审计，10 秒回落会轮询） |
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

请求体：`{ "items": [ ...条目... ], "renewer": "workbuddy-assistant:<机器标识>" }`

```json
// 201
{ "uuid": "3f2c1a9e-...", "version": 0, "count": 2, "renewer": "workbuddy-assistant:<机器标识>", "now": 1760000000000 }
```

`uuid` 由**服务端**颁发（客户端不自己推导，否则两台机器会各造一个）。版本从 0 起。
`renewer` 可省略（老客户端）→ `null`：云端账号谁都不续签，直到有设备认领。

### GET /v1/pool/:uuid

```json
// 200：items 只有活条目；deleted 是云端已逻辑删除的账号（墓碑）
{ "items": [ ...整池活条目... ], "deleted": [ ...墓碑... ], "version": 0, "renewer": "workbuddy-assistant:<机器标识>", "now": 1760000000000 }

// 404（池不存在，客户端据此判定「池在别处被解绑」）
{ "error": "gone", "now": 1760000000000 }
```

### POST /v1/pool/:uuid/lease

```json
// 200 抢到：items 是那一刻的整池活条目，客户端必须用它去签；deleted 同样带上
{ "granted": true, "items": [ ... ], "deleted": [ ... ], "version": 0, "lease_until": 1760000180000, "renewer": "workbuddy-assistant:<机器标识>", "now": 1760000000000 }

// 200 没抢到：告知持闸人 / 冷却期 / 何时能再试
{ "granted": false, "retry_after": 1760000000000, "lease_owner": "mac-mini", "lease_until": 1760000180000, "renewer": "workbuddy-assistant:<机器标识>", "now": 1760000000000 }
```

被拒也带 `renewer`：续不续云端账号只看它，与这次有没有抢到闸无关。

### PUT /v1/pool/:uuid/renewer

请求体：`{ "renewer": "workbuddy2api:<机器标识>" }`

```json
// 200（空着 / 本来就是自己；重复认领幂等）
{ "ok": true, "renewer": "workbuddy2api:<机器标识>", "now": 1760000000000 }

// 409 已经被别的设备占用（换设备要先让旧设备 release）
{ "error": "renewer_taken", "renewer": "workbuddy-assistant:<另一台机器>", "now": 1760000000000 }
```

### POST /v1/pool/:uuid/renewer/release

请求体：`{ "renewer": "workbuddy-assistant:<旧机器>" }`

```json
// 200（含重复清空；清空后云端账号暂停续签，客户端 10 秒内没新设备就自己写回）
{ "ok": true, "renewer": null, "now": 1760000000000 }

// 409 当前不是这台（只有本人能清，服务端不做强制覆盖）
{ "error": "not_renewer", "renewer": "workbuddy-assistant:<当前设备>", "now": 1760000000000 }
```

### GET /v1/pool/:uuid/renewer

```json
// 200
{ "renewer": "workbuddy-assistant:<机器标识>", "now": 1760000000000 }
// 或 { "renewer": null, "now": 1760000000000 }
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

`items` 是**整池替换**：本轮没出现的 key 会被记成墓碑（`deleted_at = now`），但行不删 ——
墓碑要随 `GET` / `lease` 的 `deleted` 发出去，别的机器才能把本地那份云端账号移除。
本轮出现的条目即使原来有墓碑，也会被 upsert 复活（`deleted_at = NULL`）。

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

删池是**硬删**（连 `pool_items` 整片清掉，不留墓碑）：它的语义是「解绑」，留墓碑只会让
客户端误以为账号是被逐个删掉的。墓碑只用于池内的单条删除。

### 错误码汇总

| 状态 | `error` | 含义 |
|---|---|---|
| 400 | `bad_json` | body 不是合法 JSON |
| 400 | `bad_uuid` | 路径不是合法 uuid 形态（不去查库） |
| 400 | `bad_version` | 提交时 `version` 不是非负整数 |
| 400 | `bad_renewer` | 续签设备标识为空 / 非字符串 / 超过 255 字符 |
| 400 | `bad_item_key` | 条目的 `key` 超过 512 字符（`pool_items.item_key` 的列宽，提前拒绝，不留到 INSERT 变 500） |
| 404 | `gone` | 池不存在或已被删除 |
| 404 | `not_found` | 路径不存在 / 前缀不是 `v1` |
| 405 | `method_not_allowed` | 方法不对（如对 health 发 PUT） |
| 409 | `stale` | CAS 失败：版本或持闸人不符 |
| 409 | `renewer_taken` | 这一池已有别的续签设备（先让旧设备 release） |
| 409 | `not_renewer` | 清空请求不是当前续签设备发出的 |
| 500 | `internal` | 未捕获异常（带 message，供客户端区分「代码写错」和「网络不通」） |

---

## 为什么抢闸要顺带返回 `items`

客户端拿到闸之后如果自己再 `GET` 一次，读到的是同一份内容 —— 多一次往返、多一个出错的机会。
更要紧的是：本地那份 refresh token **可能早就被别的机器换掉了**，而闸带回来的永远是最新的。

---

## 原子性

准入判据**只有** `UPDATE … WHERE lease_until <= ? AND retry_after <= ?` 的 `changes === 1`；
提交的准入是 `version = version + 1 WHERE version = ? AND lease_owner = ?`（同样是单条语句）。
**不要写成「先 SELECT 判断、再 UPDATE」** —— 那中间就是双死窗口。

抢到闸**之后**的那组写才是多条语句（CAS 改 pools → 整片标墓碑 → upsert 本轮条目），
它们包在 `DB.transaction()` 里一起生一起死。否则 CAS 成功、条目写到一半失败，池会停在
「版本号已进、内容还是旧的」这种谁也提交不上的状态。事务**只包这组写，抢闸本身仍是单条
条件 UPDATE、不进事务** —— 准入判据没有因此变成「先查再写」。

存储要的不是 D1，而是**「一条语句就能做完条件更新」**的引擎：抢闸与提交都必须是
服务端原子的 read-free CAS。当年排除掉的 KV（全球最终一致、同 key 每秒只能写 1 次，
抢闸恰好撞上）、Cache API（没有 CAS）、R2（只有 `If-Match`，没有 read-modify-write）
今天仍然不合格；D1/SQLite 与 **MySQL/InnoDB** 都合格 —— 后者对 `UPDATE` 持行锁、
autocommit 下单条语句即原子，所以迁到本机共享的 MySQL 没有削弱任何保证。
（真正需要盯的是方言边界，见下面那条 FOUND_ROWS。）

⚠️ **MySQL 侧必须开 `mysql2` 的 `FOUND_ROWS` 标志**（已在 `src/db.js` 里，并有测试钉住）：
MySQL 的 `UPDATE` 默认上报「真正改变了值的行数」，而 SQLite/D1 上报「WHERE 匹配到的行数」。
本服务**所有**关键判定都是 `changes === 1`，少这个标志就会出现「那条 UPDATE 已经把闸写给
这台机器，它却被告知没抢到」—— 于是客户端不续签，而闸白占到租约超时。

实测：D1 时代查询 2.65ms、端到端 1.1–3.3s（TLS 与跨境握手占大头）。本机 MySQL 的同一批
语句在 0.2–3ms 量级，端到端慢的那部分整个消失了 —— 迁移的收益在延迟，不在原子性。

---

## 条目结构

服务端只做规范化与透传，不解释业务含义。每个条目在库里是 `pool_items` 的一行：
`(pool_uuid, item_key)` 是主键，`region` 单独落列，`name` / `phone` / `email` 用 `TEXT`
（保持原来「不设上限」的语义），两个 token 用 `LONGTEXT`，时间戳是 `BIGINT NULL`。
拆行的目的不是查询性能，而是**删除只影响一条记录**：整段 JSON 改写式的删除无法区分
「被删了」与「还没同步」，客户端一提交就把它原样带回来。

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
  老 payload 里的区域只存在于 key 前缀中，所以**启动迁移也走同一套规范化**
  （`schema.js` 的 `migrateLegacyPayload` 摊平进 `pool_items`），老条目在迁移的那一刻自愈。
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

## 部署与运维

```
cred-broker/
├── src/cred.js          全部业务：路由 + 规范化 + 池条目 / 闸 / CAS 的 SQL
├── src/index.js         请求边界：把异常收成 500（保留 fetch(request, env) 签名）
├── src/server.js        容器入口：node:http → fetch；/healthz 真查库
├── src/db.js            MySQL → 与 D1 同形的 env.DB（prepare/bind/run/first/all + meta.changes + transaction）
├── src/schema.js        启动时建表、补列、把老 payload 摊平进 pool_items（幂等）
├── db/schema.mysql.sql  表结构（唯一真源，镜像里也会带上）
└── scripts/             db-init / deploy / gw-join
```

```bash
../../mysql-server/scripts/start.sh   # 共享数据库容器
npm install                          # 只装 mysql2（测试与迁移脚本在宿主机上跑）
./scripts/db-init.sh                 # 一次性：建库建用户 + 建表 + 生成 .env / .env.test
./scripts/deploy.sh                  # npm test 闸门 → 构建 → 起容器 → 探活
curl -s http://127.0.0.1:7003/v1/health
```

容器启动时会先跑一次幂等 schema 迁移（建表 → 补 `renewer` 列 → 老 `payload` 摊平进
`pool_items` → 全部成功才 `DROP COLUMN payload`）。迁移失败直接退出，不会带着半迁移的表
对外服务。日志里看 `schema_migrated`。

应用只听 `127.0.0.1:7003`（容器内监听 `80`；宿主 `7001`–`7004` 是各应用各自的回环调试端口），公网域名由
`../../gw` 注入容器名 `cred-broker:80`，本项目自己不开公网端口、不碰证书。

```bash
./scripts/gw-join.sh                                    # 登记/校对 gw 上的 vhost（幂等）
docker exec gw wget -qO- http://cred-broker:80/healthz  # 从网关那一侧验上游
```

⚠️ 探活必须**从 `gw` 容器里发**，不能只信本机的 `curl`：`127.0.0.1:7003` 只发布在宿主回环，
容器网络里根本连不通。管理台把上游写成过 `cred-broker:7003`，于是**本机所有应用打这个域名全部
502**，而 `/etc/hosts` 的接管行又让本机自己的 curl 一切正常 —— 只有这一侧的探测能暴露它。
`gw-join.sh` 现在会检测上游端口并自动重写。

```bash
docker compose logs -f cred-broker                   # 运行日志（request_failed / health_db_failed）
docker inspect -f '{{.State.Health.Status}}' cred-broker
docker compose stop                                  # 只停不删（数据全在 MySQL 里）
```

看某一池最近发生了什么（排查「为什么这台机器说池没了」）：

```bash
docker exec -i mysql-server mysql -u"$DB_USER" -p"$DB_PASSWORD" cred_broker -e \
  "SELECT FROM_UNIXTIME(at/1000) t, kind, actor, note FROM pool_events WHERE uuid='<uuid>' ORDER BY id DESC LIMIT 30"
```
（`.env` 里那对凭证由 `db-init.sh` 生成；`--default-character-set=utf8mb4` 手工加在 mysql 参数上更稳。）

**删池是不可逆的**：绑过同一 uuid 的其他机器之后会拿到 `gone`，需要重新上传或重新绑定。

### 测试

```bash
npm test        # 对 cred_broker_test 库跑，60 项
```

跑在**真实 MySQL** 上，不再用内存 SQLite：迁移之后生产引擎就是 MySQL，而这里最要紧的东西
恰恰在引擎语义里。留在 SQLite 上只会得到「全绿但测的不是生产方言」，那比红更没用。
每次运行先 DROP 本库全部表、再按 `db/schema.mysql.sql` 重建，所以测的就是那份 schema 本身；
`test/mysql-env.js` 里有「DB_NAME 必须以 `_test` 结尾」的闸门，防止手滑清掉生产池。

覆盖：健康检查、建池（颁发 uuid / 丢弃无 token 条目 / 去重 / 截断）、路径校验、取件、
抢闸（互斥 / 租约过期接管 / 冷却）、CAS 提交（版本不符 / 持闸人不符 / version 类型）、
abort（不要求持闸）、删池后全部 404、审计事件、**续签设备的认领 / 释放 / 冲突 / 非法值 / 删除后 404 / 审计**、actor 不参与判断、纯函数规范化、
**区域往返（显式字段 / key 前缀回填）**、**邮箱往返（透传 / 不进 key / 老条目落空串）**、
**墓碑（items / deleted 分组、复活、整笔提交失败时版本与条目一起回滚、超长 key 走 400）**、
**删池连 `pool_items` 一起清**、**老 `payload` 迁移（摊平 / DROP 列 / 重跑幂等）**。

迁移新增的四条（都是「D1 时代不存在、换引擎后才可能错」的地方）：

| 断言 | 钉住的东西 |
|---|---|
| 匹配到但值没变的 UPDATE 仍报 `changes=1` | `src/db.js` 的 `FOUND_ROWS` 标志 |
| 同一台机器租约到期后重拿自己的闸 | 上一条在生产路径上的后果 |
| 200 条真实长度 token 原样往返 | `access_token` / `refresh_token` 必须是 LONGTEXT（TEXT 的 64KB 装不下） |
| uuid 换大小写读不到同一池 | 库表排序规则必须是 `utf8mb4_bin` |

---

## 数据搬迁（已完成，2026-09-26）

云上 Cloudflare Worker 与 D1 库均已删除，本仓库里不再有任何 CF 配置。这段留的是**为什么**
这样做，以及下次再要搬迁时的顺序约束。

1. **先搬数据、再切流量、最后拆旧的**。三件事之间都不能留双跑窗口：两边各读各的库时，
   签出去的新 token 只落在一边，而单链轮换会让另一边的整条链作废。
2. 导出用 `wrangler d1 export --remote`，导入前**逐行逐字段回读校验**，并检查每个 `payload`
   都能 `JSON.parse` 成数组 —— 读出口对坏 JSON 是静默当空池，坏数据会伪装成「账号凭空全没了」。
   `uuid` 原样保留，所以客户端侧零改动。
3. `dump.sql` 是**整池 token 的明文**，校验通过就 `rm -rf .d1-export`，不进版本库。
4. 域名沿用不变是刻意的：三个应用把它写成编译期常量（`src-tauri/src/broker.rs` 的 `BASE`），
   换名等于改三处代码 + 重新构建 + 让用户重装。
5. 删 Worker 用 `wrangler delete`，它会连带摘掉 custom domain 路由（单独去控制台翻路由反而更绕）。
   删完之后 DNS 仍解析到 Cloudflare 边缘 —— 那是通配记录 + 隧道在兜，`--resolve` 钉边缘 IP
   探一次就能确认应答方已经换成本机容器。

**回滚窗口已经关闭**：云上不再有旧副本，MySQL 里那份是唯一副本。此后的备份责任落在
`../../mysql-server` 那侧（本项目不再有自己的异地副本），别再按「回滚到 Worker」来想问题。

---

## 配置项

运行参数都在 `.env`（`scripts/db-init.sh` 生成，`.env.example` 是它的说明版）；`src/server.js`
启动时缺 `DB_*` 任一项就直接退出，不会留到第一个请求才 500。

| 环境变量 | 默认值 | 含义 |
|---|---|---|
| `DB_HOST` / `DB_PORT` | `mysql` / `3306` | 共享 MySQL。容器内主机名固定是 `mysql`；宿主机上跑脚本要换 `127.0.0.1` |
| `DB_NAME` / `DB_USER` / `DB_PASSWORD` | `cred_broker` / 同 / 随机 | 专用库与专用账号，只授权这一个库 |
| `LEASE_MS` | `180000` | 续签闸的租约时长（毫秒）。**必须大于客户端「把这一池该签的都签掉 + 提交结果」的最坏耗时**，否则租约提前失效、第二台机器拿到闸，而单链轮换下两边都可能死。闸的作用域是整池，要容得下「一轮里连续签若干个账号」 |
| `FAIL_COOLDOWN_MS` | `300000` | 续签失败后的冷静期：凭证真死时，别让几台机器轮流敲同一条已死的链 |
| `PORT` / `HOST` | `80` / `0.0.0.0` | 容器内监听（compose 里显式设 `PORT=80`，源码兜底 `8787`）；改 `PORT` 要同步改 compose、healthcheck 与 gw 的上游 |
| `APP_BIND_ADDR` / `APP_PORT` | `127.0.0.1` / `7003` | 只有 compose 读：发布到宿主机的回环调试地址与端口（各应用占 `7001`–`7004`） |

正常路径上签完就立刻归还，`LEASE_MS` 只决定「出意外时闸被占多久」。

两个闸参数由 `.env` 提供，形态是**字符串**（`src/cred.js` 用 `asInt` 解析并兜默认值；
沿用字符串是因为它们最初来自 Worker 的 `[vars]`，`src/server.js` 原样把 `process.env` 交给它）。

改这两个值**不需要重编客户端**（三个应用都不读 `/v1/health`，也没把它们写成常量）：
客户端只用抢闸响应里的 `retry_after` 减去自己的本地时钟来算要等多久。真正要守的是
`LEASE_MS` 那条下限 —— 它必须大于客户端「把整池该签的签掉 + 提交」的最坏耗时，否则会
出现租约被第二台机器接管的窗口。

这里没有 JWT / 签名密钥类配置：凭据**就是池 uuid 本身**（见「它不做什么」）。所以这次迁移
不存在「换了签名密钥导致所有端掉线」那一步，客户端侧零改动。
