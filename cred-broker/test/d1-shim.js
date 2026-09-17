/**
 * 把 node 内置的 `node:sqlite` 包成 D1 的最小接口（prepare / bind / run / first / all）。
 *
 * 用**真 SQL 引擎**而不是手写 fake，是因为这里最要紧的东西恰恰在 SQL 里：
 * 抢闸的准入判据是 `WHERE lease_until <= ? AND retry_after <= ?` 的 `changes === 1`，
 * 提交是 `version = ? AND lease_owner = ?` 的 CAS。自己写个「假装原子」的 fake，
 * 测的就只是 fake 自己的假设 —— 等于什么都没测。
 */
import { DatabaseSync } from "node:sqlite";
import { readFileSync } from "node:fs";

class Stmt {
  constructor(db, sql) {
    this.db = db;
    this.sql = sql;
    this.args = [];
  }
  bind(...args) {
    // D1 的 bind 不接受 undefined；SQLite 也不接受，统一归成 null
    this.args = args.map((a) => (a === undefined ? null : a));
    return this;
  }
  async run() {
    const r = this.db.prepare(this.sql).run(...this.args);
    return {
      success: true,
      meta: { changes: Number(r.changes), last_row_id: Number(r.lastInsertRowid) },
    };
  }
  async first() {
    const row = this.db.prepare(this.sql).get(...this.args);
    return row === undefined ? null : { ...row };
  }
  async all() {
    return {
      results: this.db.prepare(this.sql).all(...this.args).map((r) => ({ ...r })),
    };
  }
}

/** 建一个干净的内存库，并执行 migrations 下的建表语句 */
export function createDb() {
  const sql = readFileSync(new URL("../migrations/0001_init.sql", import.meta.url), "utf8");
  const db = new DatabaseSync(":memory:");
  db.exec(sql);
  return {
    prepare: (s) => new Stmt(db, s),
    /** 测试里直查用（如检查事件表） */
    raw: db,
  };
}

/** 造一个签名的 env（Worker 侧传来的 vars 都是字符串） */
export function createEnv(overrides = {}) {
  return {
    DB: createDb(),
    LEASE_MS: "180000",
    FAIL_COOLDOWN_MS: "300000",
    ...overrides,
  };
}
