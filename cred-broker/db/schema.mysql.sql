-- cred-broker 落库（MySQL 方言，本机自托管的表结构唯一真源）。
-- 与 migrations/0001_init.sql（Cloudflare D1 那份）在语义上等价；下面每处写法差异都注明了原因，
-- 因为 src/cred.js 里的 SQL 是两个引擎共用的，改表结构时必须同时确认那边还能跑。
--
-- 全局两点：
--   * 排序规则一律 utf8mb4_bin（库级也设成同一个）：D1/SQLite 的 TEXT 按字节比较，
--     **大小写敏感**。用默认的 *_ci 会让「同一池 uuid 换个大小写」被认成同一个键，
--     也会让 lease_owner 的 CAS 判等比 D1 宽松 —— 这两种都表现为偶发、且日志里看不出来。
--   * 所有时间列都是 epoch 毫秒整数，没有 DATETIME，因此不涉及时区（服务端 TZ 与数据无关）。

-- 凭证池：一行 = 一池。uuid 即地址也是唯一凭据；**只放池自身的配置与闸状态**，
-- 账号条目在 pool_items 里一行一条（2026-11-27 从 payload LONGTEXT 拆出来）。
CREATE TABLE IF NOT EXISTS pools (
  uuid        VARCHAR(36)  NOT NULL,
  -- 这一池唯一的「续签设备」标识 = 客户端类型 + 机器唯一标识（客户端自己拼，服务端只存不解释）。
  -- NULL = 还没有设备认领：云端账号一律不续签，本地登录账号不受影响。
  -- 换设备必须由旧设备先清空、新设备再认领（CAS 在 src/cred.js 的 claim/release 里）。
  renewer     VARCHAR(255) NULL,
  version     BIGINT       NOT NULL DEFAULT 0,
  lease_owner TEXT,
  lease_until BIGINT       NOT NULL DEFAULT 0,
  retry_after BIGINT       NOT NULL DEFAULT 0,
  created_at  BIGINT       NOT NULL DEFAULT 0,
  updated_at  BIGINT       NOT NULL DEFAULT 0,
  PRIMARY KEY (uuid),
  KEY idx_pools_retry_after (retry_after)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- 池里的账号条目：一行 = 一个账号（token 对）**绑定一个池 uuid**。
--
-- 为什么从「一池一段 JSON」拆成行：整池提交曾是一条 `UPDATE pools SET payload=?`，
-- 于是「删掉一个账号」只能靠改写整段 JSON 表达 —— 云端删掉后，任何还攥着旧副本的
-- 客户端下一次整池回写就把它又带回来了。拆成行之后，删除是**逻辑删除**（deleted_at），
-- 墓碑留在表里、随 GET/lease 一起发给客户端，客户端据此移除本地那份云端账号；
-- 客户端提交时不带墓碑，服务端按「本轮没出现 = 标记删除」处理。
--
-- item_key 是跨机身份锚点（手机号 → 昵称 → 本地 id），与池 uuid 一起做主键：
-- 同一个池里同一个 key 只有一行。VARCHAR(512) 是防呆上限（客户端真实 key 是手机号 /
-- 昵称 / uuid，远小于它）；超长 key 在路由层直接 400，不会走到这里变成 1406。
--
-- name / phone / email 用 TEXT 而不是 VARCHAR：它们是客户端来的展示字段、没有长度契约，
-- 与拆表前 JSON 的「无界」语义保持一致。token 用 LONGTEXT（与拆表前同一理由：单条
-- 刷新票可以很长，TEXT 的 64KB 上限会在「续签完写回」那一刻报 1406）。
--
-- deleted_at = 逻辑删除时间（epoch 毫秒）。NULL = 活条目；非 NULL = 墓碑，仍会发给客户端。
CREATE TABLE IF NOT EXISTS pool_items (
  pool_uuid     VARCHAR(36)  NOT NULL,
  item_key      VARCHAR(512) NOT NULL,
  region        VARCHAR(16)  NOT NULL DEFAULT 'global',
  name          TEXT         NOT NULL,
  phone         TEXT         NOT NULL,
  email         TEXT         NOT NULL,
  access_token  LONGTEXT     NOT NULL,
  refresh_token LONGTEXT     NOT NULL,
  expires_at    BIGINT       NULL,
  rt_expires_at BIGINT       NULL,
  updated_at    BIGINT       NULL,
  deleted_at    BIGINT       NULL,
  PRIMARY KEY (pool_uuid, item_key),
  -- 「这一池的活条目」是每次 GET / lease / 提交都要走的形状；墓碑靠 deleted_at 过滤。
  KEY idx_pool_items_live (pool_uuid, deleted_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- uuid 定长 36 是有依据的，不是估的：handle() 在查库前用 UUID_RE 钉死了路径形态，
-- 新建池的 uuid 由服务端 crypto.randomUUID() 颁发，两条路都不可能塞进更长的值。
--
-- lease_owner / actor 用 TEXT：值是客户端 x-cred-actor 头带来的，长度由客户端说了算，
-- D1 侧是无界 TEXT。换成 VARCHAR(64) 等于给一个「超长机器名」制造 500，而它只是审计字段。
--
-- version / lease_until / retry_after 用 BIGINT：epoch 毫秒（约 1.76e12）远超 INT32 上限，
-- SQLite 的 INTEGER 本来就是 64 位。

-- 索引一律写在 CREATE TABLE 里面，不用独立的 CREATE INDEX 语句：
-- MySQL 没有 `CREATE INDEX IF NOT EXISTS`（那是 SQLite/MariaDB 的写法），独立语句第二次执行
-- 就报 1061 重复键名，于是 db-init.sh 就不再可重复执行。

-- 审计事件：纯旁路，写失败不影响主流程（src/cred.js 的 logEvent 吞异常）。
-- 它的唯一用途是回答「这台机器为什么说自己没拿到闸 / 池为什么没了」。
CREATE TABLE IF NOT EXISTS pool_events (
  id    BIGINT      NOT NULL AUTO_INCREMENT,
  uuid  VARCHAR(36) NOT NULL,
  at    BIGINT      NOT NULL,
  kind  TEXT        NOT NULL,
  actor TEXT,
  note  TEXT,
  PRIMARY KEY (id),
  -- (uuid, at DESC)：查「某一池最近发生了什么」的那条运维语句的形状。
  -- MySQL 8.0 支持真降序索引（5.7 会解析但忽略），这台是 8.0。
  KEY idx_pool_events_uuid_at (uuid, at DESC)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;