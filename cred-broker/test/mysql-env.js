/**
 * 测试库：真实 MySQL，连接参数取自 .env.test（scripts/db-init.sh 生成的那份）。
 *
 * 为什么不再是 node:sqlite 内存库：迁移之后**生产跑的引擎就是 MySQL**，
 * 而这里最要紧的东西恰恰在引擎语义里 —— 抢闸的准入判据是
 * `UPDATE … WHERE lease_until <= ? AND retry_after <= ?` 的 `changes === 1`，
 * 提交是 `version = ? AND lease_owner = ?` 的 CAS。
 * 留在 SQLite 上只会得到「全绿但测的不是生产方言」，那比红更没用
 * （SQLite 报匹配行数、MySQL 默认报改变行数，正是这一类差异）。
 *
 * 每次运行都 DROP 全部表、再按 db/schema.mysql.sql 重建：测的就是那份 schema 本身，
 * 改了 src 的 SQL 却没同步 schema 会立刻红，而不是等线上第一个请求发现。
 * 只允许跑在 *_test 库上：这里删的是本库所有表，DB_NAME 手滑写成生产库就是一次删库事故。
 *
 * 各 test 之间不需要清表：每个用例都自己 POST /v1/pool 拿一个新颁发的 uuid，
 * 之后所有读写都以这个 uuid 为键 —— 别人的行既查不到也改不动。
 */
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createDatabase, databaseConfigFromEnv } from '../src/db.js';

const HERE = dirname(fileURLToPath(import.meta.url));

// 优先环境变量，其次 .env.test，最后退回 .env —— 退回后 DB_NAME 是生产库名，
// 会立刻被下面那道 _test 闸门拦下。这样 npm test 不用每条命令前手写 DB_*，
// 也不可能顺手连上生产库。
if (!process.env.DB_HOST) {
  for (const p of ['../.env.test', '../../.env.test', '../.env']) {
    try {
      process.loadEnvFile(join(HERE, p));
      break;
    } catch {
      /* 换下一个候选路径 */
    }
  }
}

const cfg = databaseConfigFromEnv(process.env);
if (!/_test$/.test(cfg.database)) {
  console.error(
    `DB_NAME="${cfg.database}" 不是测试库（必须以 _test 结尾）—— 本测试会 DROP 全部表，拒绝执行`
  );
  process.exit(1);
}

const { DB, close } = createDatabase(cfg);

// mysql2 关掉了多语句执行（multipleStatements: false，杜绝 ; 拼出第二条语句），
// 所以建表要按语句拆开逐条跑。schema 里的注释行以 -- 开头，先剔掉再拆。
const schemaStatements = () =>
  readFileSync(join(HERE, '..', 'db', 'schema.mysql.sql'), 'utf8')
    .split('\n')
    .filter((l) => !/^\s*--/.test(l))
    .join('\n')
    .split(';')
    .map((s) => s.trim())
    .filter(Boolean);

// 连不上 / 建不起来就硬失败退出，绝不跳过：静默的绿是假的安全感。
try {
  for (const t of ['pool_events', 'pools']) {
    await DB.prepare(`DROP TABLE IF EXISTS \`${t}\``).run();
  }
  for (const stmt of schemaStatements()) await DB.prepare(stmt).run();
} catch (err) {
  console.error(`测试库初始化失败（${cfg.host}:${cfg.port}/${cfg.database}）：${err.message}`);
  process.exit(1);
}

/**
 * 造一个 env（Worker 侧传来的 vars 都是字符串，所以这里同样给字符串）。
 * 整个测试文件共用一个连接池：闸的判定是单条语句内的行锁，用例之间按 uuid 隔离，
 * 不存在「必须各开一池」的前提；开 30 个池反而会让 MySQL 的 connection 上限先炸。
 */
export function createEnv(overrides = {}) {
  return {
    DB,
    LEASE_MS: '180000',
    FAIL_COOLDOWN_MS: '300000',
    ...overrides,
  };
}

/** 连接池不关，node 进程就不会退出。测试文件末尾用 node:test 的 after() 调它。 */
export async function closeEnv() {
  await close();
}
