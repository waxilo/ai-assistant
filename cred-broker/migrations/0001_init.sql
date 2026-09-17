-- cred-broker 落库：池 + 审计事件。
--
-- **没有 keys 表**：凭据就是池 uuid 本身。谁能提供 uuid，谁就能取整池凭证 ——
-- 所以 uuid 必须当密码保存。服务端不做鉴权，也就不存在「只读 key 泄露」这类中间态。

CREATE TABLE IF NOT EXISTS pools (
  uuid        TEXT    PRIMARY KEY,
  payload     TEXT    NOT NULL DEFAULT '[]',
  version     INTEGER NOT NULL DEFAULT 0,
  lease_owner TEXT,
  lease_until INTEGER NOT NULL DEFAULT 0,
  retry_after INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_pools_retry_after ON pools (retry_after);

CREATE TABLE IF NOT EXISTS pool_events (
  id    INTEGER PRIMARY KEY AUTOINCREMENT,
  uuid  TEXT    NOT NULL,
  at    INTEGER NOT NULL,
  kind  TEXT    NOT NULL,
  actor TEXT,
  note  TEXT
);

CREATE INDEX IF NOT EXISTS idx_pool_events_uuid_at ON pool_events (uuid, at DESC);
