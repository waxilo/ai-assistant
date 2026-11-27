// 启动时的库结构自检 + 一次性数据迁移。
//
// 两件事都必须在 listen 之前做完：否则第一波请求会撞上 `Unknown column` / `Table doesn't exist`，
// 而客户端把 5xx 记成「管家不可达」，看起来像网络问题（见 server.js 的注释）。
//
// 表结构的唯一真源是 db/schema.mysql.sql（镜像里也带着这份，Dockerfile 的 COPY db/）：
// 这里只是把它逐条执行一遍。全部是 CREATE TABLE IF NOT EXISTS，重复执行安全；
// 已存在的表不会补列，所以「后加的列」仍走下面的列探测。
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { insertItems, normalizeItems } from './cred.js';

const HERE = dirname(fileURLToPath(import.meta.url));

/** schema 文件按 ; 拆成语句（驱动关了 multipleStatements；注释行先剔掉）。 */
function schemaStatements() {
  return readFileSync(join(HERE, '..', 'db', 'schema.mysql.sql'), 'utf8')
    .split('\n')
    .filter((l) => !/^\s*--/.test(l))
    .join('\n')
    .split(';')
    .map((s) => s.trim())
    .filter(Boolean);
}

async function hasColumn(db, table, column) {
  const row = await db
    .prepare(
      "SELECT COUNT(*) AS n FROM information_schema.columns " +
        "WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?"
    )
    .bind(table, column)
    .first();
  return Number(row?.n ?? 0) > 0;
}

/**
 * 老库升级：`pools.renewer` 是后加的列，而 `CREATE TABLE IF NOT EXISTS` 不会补列。
 *
 * 不加 `AFTER payload`：payload 在拆表后会被删掉，写死位置会让「没有 payload 的库」
 * 在这一句上直接失败。列序没有任何语义。
 */
async function ensureRenewerColumn(db, log) {
  if (await hasColumn(db, 'pools', 'renewer')) return;
  await db.prepare('ALTER TABLE pools ADD COLUMN renewer VARCHAR(255) NULL').run();
  log('schema_migrated', { table: 'pools', column: 'renewer' });
}

/**
 * 老库升级：`pools.payload`（一池一段 JSON）→ `pool_items`（一行一条）。
 *
 * 幂等：条目用 INSERT IGNORE 写入（重复执行不会拿旧 JSON 盖掉已经写进来的新行），
 * **只有全部池都处理完才 DROP payload** —— 中途崩了下次启动会重做一遍，不会丢数据。
 * 解析不出来的 payload 按空池处理（与旧的读出口一致：坏 JSON 不等于一池凭证消失）。
 *
 * 返回 `{ skipped: true }` 表示这个库已经是行表结构（没有 payload 列）。
 */
export async function migrateLegacyPayload(db, log = () => {}) {
  if (!(await hasColumn(db, 'pools', 'payload'))) return { skipped: true, pools: 0, items: 0 };

  const { results } = await db.prepare('SELECT uuid, payload FROM pools').all();
  let items = 0;
  for (const row of results) {
    let parsed = [];
    try {
      parsed = normalizeItems(JSON.parse(row.payload ?? '[]'));
    } catch {
      parsed = [];
    }
    await db.transaction(async (tx) => {
      await insertItems(tx, row.uuid, parsed, { revive: false });
    });
    items += parsed.length;
  }
  await db.prepare('ALTER TABLE pools DROP COLUMN payload').run();
  log('schema_migrated', { table: 'pools', column: 'payload', pools: results.length, items });
  return { skipped: false, pools: results.length, items };
}

/** 建表（幂等）→ 补列 → 老 payload 摊平。任何一步失败都向上抛，由 server.js 退出。 */
export async function ensureSchema(db, log = () => {}) {
  for (const stmt of schemaStatements()) await db.prepare(stmt).run();
  await ensureRenewerColumn(db, log);
  await migrateLegacyPayload(db, log);
}