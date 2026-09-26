// MySQL 驱动，对外暴露与 Cloudflare D1 完全同形的 env.DB
// （prepare(sql).bind(...).run()/first()/all() + meta.changes / meta.last_row_id），
// 所以 src/cred.js 里的 SQL 与全部业务判据一行都不用改 —— 迁移动了引擎，没动语义。
//
// 只实现真正用到的那一面：没有 batch()、没有显式事务。原子性从来不靠事务，
// 靠的是单条语句：抢闸是 `UPDATE … WHERE lease_until <= ? AND retry_after <= ?` 的
// `changes === 1`，提交是 `version = version + 1 WHERE version = ? AND lease_owner = ?`。
// InnoDB 对 UPDATE 持行锁，autocommit 下单条语句即原子 —— 与 D1 当时给的保护同级。
// 把「先查再写」请回来才是事故（那中间就是双死窗口，见 src/cred.js 文件头）。
//
// 三处参数是 D1 语义与 MySQL 默认行为对不上导致的，少一个都会静默改变判定：
//   * flags: ['FOUND_ROWS'] —— MySQL 的 UPDATE 默认上报「实际改变了值的行数」，
//     而 SQLite/D1 上报「WHERE 匹配到的行数」。本项目**全部**关键判定都是 changes === 1：
//     不开这个标志，「同一台机器在租约到期后原样续闸」这类匹配到但值没变的情形会被判成
//     没抢到闸 —— 而那条 UPDATE 其实已经把闸写给它了。于是客户端拿着 denied 不续签，
//     闸却永久占在自己手里，直到租约超时。这是全项目最需要一条标志来保命的地方。
//   * decimalNumbers: true —— 聚合列（COUNT 等）在 MySQL 回 DECIMAL，不转会变字符串。
//     运维查池、测试数事件条数都会经过它；转不动就是 JSON 里 "3" 与 3 的区别。
//   * 刻意不开 supportBigNumbers：它会把超过 2^53 的 BIGINT 变成字符串，从而让 version、
//     lease_until 这类值在响应 JSON 里从数字变字符串。这里的时间戳与版本号远小于 2^53，
//     不需要那层精度保护，宁可保持类型不变（客户端是按数字比对租约到期的）。
//
// 绑定参数遇 undefined 一律抛错，与 D1 的 D1_TYPE_ERROR 同形：
// 「SQL 加了占位符、变量却忘了定义」必须当场红，静默归成 NULL 等于把双死窗口写进库里。
import mysql from 'mysql2/promise';

const lastOf = (s) => String(s).split('/').pop();

function normalize(sql, params) {
  for (let i = 0; i < params.length; i++) {
    if (params[i] === undefined) {
      throw new TypeError(`D1_TYPE_ERROR: Type 'undefined' not supported for binding (arg #${i}) [${lastOf(sql).slice(0, 60)}]`);
    }
  }
}

function metaOf(result) {
  if (Array.isArray(result)) return { changes: result.length, last_row_id: 0 };
  return { changes: result?.affectedRows ?? 0, last_row_id: result?.insertId ?? 0 };
}

function createStatement(exec, sql, params) {
  const bound = (...more) => {
    const next = [...params, ...more];
    normalize(sql, next);
    return createStatement(exec, sql, next);
  };
  return {
    bind: bound,
    async all() {
      const [rows] = await exec(sql, params);
      return { results: Array.isArray(rows) ? rows : [], success: true, meta: metaOf(rows) };
    },
    async first() {
      const [rows] = await exec(sql, params);
      return Array.isArray(rows) ? (rows[0] ?? null) : null;
    },
    async run() {
      const [result] = await exec(sql, params);
      return { success: true, meta: metaOf(result) };
    },
  };
}

/**
 * @param {{host:string,port:number,user:string,password:string,database:string,connectionLimit?:number}} cfg
 * @returns {{ DB: { prepare(sql: string): object }, close(): Promise<void> }}
 */
export function createDatabase(cfg) {
  const pool = mysql.createPool({
    host: cfg.host,
    port: cfg.port,
    user: cfg.user,
    password: cfg.password,
    database: cfg.database,
    waitForConnections: true,
    connectionLimit: cfg.connectionLimit ?? 10,
    queueLimit: 0,
    flags: ['FOUND_ROWS'],
    decimalNumbers: true,
    // 一条语句一次往返，杜绝 ; 拼接出第二条语句执行
    multipleStatements: false,
    charset: 'utf8mb4_bin',
  });

  const exec = (sql, params) => pool.execute(sql, params);

  return {
    DB: { prepare: (sql) => createStatement(exec, sql, []) },
    close: () => pool.end(),
  };
}

/** 从环境变量读连接参数（缺失即启动失败，不留到第一个请求才 500）。 */
export function databaseConfigFromEnv(env) {
  const missing = ['DB_HOST', 'DB_USER', 'DB_PASSWORD', 'DB_NAME'].filter((k) => !env[k]);
  if (missing.length) throw new Error(`missing env: ${missing.join(', ')}`);
  return {
    host: env.DB_HOST,
    port: Number(env.DB_PORT || 3306),
    user: env.DB_USER,
    password: env.DB_PASSWORD,
    database: env.DB_NAME,
  };
}
