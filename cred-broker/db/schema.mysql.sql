-- cred-broker 落库（MySQL 方言，本机自托管的表结构唯一真源）。
-- 与 migrations/0001_init.sql（Cloudflare D1 那份）在语义上等价；下面每处写法差异都注明了原因，
-- 因为 src/cred.js 里的 SQL 是两个引擎共用的，改表结构时必须同时确认那边还能跑。
--
-- 全局两点：
--   * 排序规则一律 utf8mb4_bin（库级也设成同一个）：D1/SQLite 的 TEXT 按字节比较，
--     **大小写敏感**。用默认的 *_ci 会让「同一池 uuid 换个大小写」被认成同一个键，
--     也会让 lease_owner 的 CAS 判等比 D1 宽松 —— 这两种都表现为偶发、且日志里看不出来。
--   * 所有时间列都是 epoch 毫秒整数，没有 DATETIME，因此不涉及时区（服务端 TZ 与数据无关）。

-- 凭证池：一行 = 一池，uuid 即地址也是唯一凭据。
CREATE TABLE IF NOT EXISTS pools (
  uuid        VARCHAR(36)  NOT NULL,
  payload     LONGTEXT     NOT NULL,
  version     BIGINT       NOT NULL DEFAULT 0,
  lease_owner TEXT,
  lease_until BIGINT       NOT NULL DEFAULT 0,
  retry_after BIGINT       NOT NULL DEFAULT 0,
  created_at  BIGINT       NOT NULL DEFAULT 0,
  updated_at  BIGINT       NOT NULL DEFAULT 0,
  PRIMARY KEY (uuid),
  KEY idx_pools_retry_after (retry_after)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- uuid 定长 36 是有依据的，不是估的：handle() 在查库前用 UUID_RE 钉死了路径形态，
-- 新建池的 uuid 由服务端 crypto.randomUUID() 颁发，两条路都不可能塞进更长的值。
--
-- payload 用 LONGTEXT 而不是 TEXT：一池上限 MAX_ITEMS=200 条，每条带两个 token，
-- 现实里就见过单池几十 KB 的数据。SQLite 的 TEXT 无长度上限，MySQL 的 TEXT 上限 65535 字节
-- —— 用 TEXT 会在某次正常提交时报 1406 Data too long，而这正好发生在「续签完写回」那一刻。
-- （MySQL 的 TEXT/BLOB 系列不能写 DEFAULT，所以 D1 那句 DEFAULT '[]' 在这里去掉了；
--   唯一的写入路径 createPool 始终显式给值，行为不变。）
--
-- lease_owner / actor 用 TEXT：值是客户端 x-cred-actor 头带来的，长度由客户端说了算，
-- D1 侧是无界 TEXT。换成 VARCHAR(64) 等于给一个「超长机器名」制造 500，而它只是审计字段。
--
-- version / lease_until / retry_after 用 BIGINT：epoch 毫秒（约 1.76e12）远超 INT32 上限，
-- SQLite 的 INTEGER 本来就是 64 位。

-- 索引一律写在 CREATE TABLE 里面，不用独立的 CREATE INDEX 语句：
-- MySQL 没有 `CREATE INDEX IF NOT EXISTS`（那是 SQLite/MariaDB 的写法），独立语句第二次执行
-- 就报 1061 重复键名，于是 db-init.sh 就不再可重复执行。
-- idx_pools_retry_after 沿用 D1 那份的索引清单（按冷却期捞池时用）。

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
