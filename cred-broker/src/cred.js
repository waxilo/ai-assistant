/**
 * cred-broker —— 跨机器共用同一批账号凭证的「池 + 闸」。
 *
 * ## 为什么要有它
 *
 * 官方续签是**单链轮换**：续签会返回一个新的 refresh token，服务端按账号只认最新那一条。
 * 多台机器各持同一份 RT 的副本时，谁先续签谁赢，其余副本连旧 AT 一起被踢；而「已用过的
 * refresh token 再次出现」还会被判成凭证泄露，把整条链作废。
 *
 * 所以问题不是「怎么同步凭证」，而是**怎么保证同一时刻只有一台机器在续签**。
 * 这里提供的就是那把闸，加上「一整池凭证」的存放处。
 *
 * ## 粒度是「一整池」，不是「一个账号」
 *
 * 服务端颁发一个 uuid 代表一池；复制到别的机器绑定，那一池账号就全部并进本地。
 * ⚠️ 按账号分粒度是错的：两台机器可以各自拿着不同账号的闸、同时提交同一批账号，
 * 于是出现「一半新一半旧」这种**谁也没签错**的错状态。一池一把闸就堵死了这条路。
 *
 * ## 三步握手（缺一步就退化成「两边同时续签」）
 *
 *   GET  /v1/pool/:uuid         取件 —— 拿到这一池的最新凭证
 *   POST /v1/pool/:uuid/lease   抢闸 —— 抢到的人**同时拿到那一刻的池内容**（别自己再 GET）
 *   PUT  /v1/pool/:uuid         提交 —— CAS 整池写回，版本 +1
 *   POST /v1/pool/:uuid/abort   失败 —— 释放闸 + 记一段冷却
 *
 * 抢到闸时响应里带着 `items`，**客户端必须用它去签**：本地那份 refresh token 可能早就被
 * 别的机器换掉了，再自己 GET 一次读到的是同一份，只会多一次往返、多一个出错的机会。
 *
 * ## 没有任何鉴权，uuid 就是凭据
 *
 * 知道 uuid 等于有全部权限（能读整池的 token）。这是刻意的：加一层 key 只会制造
 * 「探活显示连接正常、到续签那一刻才 403」这种延迟暴露的坑，而它挡不住真拿到 uuid 的人。
 *
 * ## 原子性靠 SQL，不靠「先查再写」
 *
 * 准入判据只有 `UPDATE … WHERE lease_until <= ? AND retry_after <= ?` 的 `changes === 1`；
 * 提交是 `version = version + 1 WHERE version = ? AND lease_owner = ?`。
 * **不许写成「先 SELECT 判断、再 UPDATE」** —— 那中间就是双死窗口。
 */

export const DEFAULT_LEASE_MS = 180_000;
export const DEFAULT_FAIL_COOLDOWN_MS = 300_000;
/** 一池最多塞多少条账号：既是防呆，也是防「一次请求写爆 payload」 */
export const MAX_ITEMS = 200;
/** uuid 形态校验：只用于挡住明显不是 uuid 的路径，不解释它从哪来 */
export const UUID_RE =
  /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

// ── 小工具 ────────────────────────────────────────────────────────────────

export function json(data, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

export function asInt(v, fallback) {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? Math.trunc(n) : fallback;
}

/** 取干净的字符串（空串与纯空白一律归零） */
function str(v) {
  return typeof v === "string" ? v.trim() : "";
}

/** 取毫秒时间戳；非有限数一律 null（**不写 0**：0 会被读成 1970 年，是比 null 更坏的数据） */
function ms(v) {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? Math.trunc(n) : null;
}

/** D1 的 `meta.changes`：只有它等于 1 才算「这一条语句真的改到了那一行」 */
function changes(res) {
  return Number(res?.meta?.changes ?? res?.changes ?? 0);
}

async function readJson(request) {
  try {
    const body = await request.json();
    return body && typeof body === "object" ? body : null;
  } catch {
    return null;
  }
}

/** 谁在操作：机器标识只是审计用，不参与任何判断 */
function actorOf(request, body) {
  const h = request.headers.get("x-cred-actor");
  return str(h) || str(body?.actor) || "unknown";
}

// ── 条目 ──────────────────────────────────────────────────────────────────

/**
 * 规范化一条账号凭证。
 *
 * **两个 token 都为空时整条丢弃**：一个只剩昵称、没有凭证的条目搬过去毫无意义，
 * 却会让「这一池有多少条」这个数字失真。
 *
 * `key` 是跨机身份锚点，选取顺序 = 显式 key → 手机号 → 昵称 → 本地 id。
 * 客户端的合并是**并集**，全靠这个 key 认人；本地独有的账号一条都不会被删。
 */
export function normalizeItem(raw) {
  if (!raw || typeof raw !== "object") return null;
  const access = str(raw.access_token);
  const refresh = str(raw.refresh_token);
  if (!access && !refresh) return null;
  const phone = str(raw.phone);
  const name = str(raw.name);
  const localId = str(raw.local_id);
  const key = str(raw.key) || phone || name || localId;
  if (!key) return null;
  return {
    key,
    name,
    phone,
    access_token: access,
    refresh_token: refresh,
    expires_at: ms(raw.expires_at),
    rt_expires_at: ms(raw.rt_expires_at),
    updated_at: ms(raw.updated_at),
  };
}

/** 整池规范化：按 key 去重（后到的覆盖先到的），并截断到 MAX_ITEMS */
export function normalizeItems(list) {
  if (!Array.isArray(list)) return [];
  const byKey = new Map();
  for (const raw of list) {
    const item = normalizeItem(raw);
    if (item) byKey.set(item.key, item);
  }
  return [...byKey.values()].slice(0, MAX_ITEMS);
}

function parseItems(payload) {
  try {
    const parsed = JSON.parse(payload ?? "[]");
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

/** 审计事件：纯旁路，**不参与任何判断**。写不进去也不该让请求失败 */
async function logEvent(db, uuid, kind, actor, note, at) {
  try {
    await db
      .prepare(
        "INSERT INTO pool_events (uuid, at, kind, actor, note) VALUES (?, ?, ?, ?, ?)"
      )
      .bind(uuid, at, kind, actor ?? null, note ?? null)
      .run();
  } catch {
    /* 审计失败不影响主流程 */
  }
}

// ── 业务 ──────────────────────────────────────────────────────────────────

/** 新建一池：uuid 由服务端颁发（客户端不自己推导，否则两台机器会各造一个） */
export async function createPool(db, rawItems, actor, at) {
  const items = normalizeItems(rawItems);
  const uuid = crypto.randomUUID();
  await db
    .prepare(
      "INSERT INTO pools (uuid, payload, version, lease_owner, lease_until, retry_after, created_at, updated_at) " +
        "VALUES (?, ?, 0, NULL, 0, 0, ?, ?)"
    )
    .bind(uuid, JSON.stringify(items), at, at)
    .run();
  await logEvent(db, uuid, "created", actor, `${items.length} 项`, at);
  return { uuid, version: 0, count: items.length };
}

/** 取件：只读，不动闸 */
export async function fetchPool(db, uuid, actor, at) {
  const row = await db
    .prepare("SELECT payload, version FROM pools WHERE uuid = ?")
    .bind(uuid)
    .first();
  if (!row) return { kind: "gone" };
  await logEvent(db, uuid, "fetched", actor, null, at);
  return { kind: "ok", items: parseItems(row.payload), version: Number(row.version) };
}

/**
 * 抢闸：**唯一入口是那条带 WHERE 的 UPDATE**。
 *
 * 抢到的人同时拿到这一池的内容 —— 这是刻意的，见文件头「三步握手」。
 */
export async function leasePool(db, uuid, actor, at, leaseMs) {
  const until = at + leaseMs;
  const res = await db
    .prepare(
      "UPDATE pools SET lease_owner = ?, lease_until = ?, updated_at = ? " +
        "WHERE uuid = ? AND lease_until <= ? AND retry_after <= ?"
    )
    .bind(actor, until, at, uuid, at, at)
    .run();

  const row = await db
    .prepare("SELECT payload, version, lease_owner, lease_until, retry_after FROM pools WHERE uuid = ?")
    .bind(uuid)
    .first();
  if (!row) return { kind: "gone" };

  if (changes(res) !== 1) {
    const retryAfter = Number(row.retry_after) || 0;
    await logEvent(
      db,
      uuid,
      "lease_denied",
      actor,
      retryAfter > at
        ? `冷却中，${Math.ceil((retryAfter - at) / 1000)}s 后可再试`
        : `闸在 ${row.lease_owner ?? "?"} 手里`,
      at
    );
    return {
      kind: "denied",
      retry_after: retryAfter,
      lease_owner: row.lease_owner ?? null,
      lease_until: Number(row.lease_until) || 0,
    };
  }

  await logEvent(db, uuid, "leased", actor, `闸保持到 ${until}`, at);
  return {
    kind: "granted",
    items: parseItems(row.payload),
    version: Number(row.version),
    lease_until: until,
  };
}

/** 提交整池（CAS）：版本 +1，同时归还闸 */
export async function commitPool(db, uuid, actor, version, rawItems, at) {
  const items = normalizeItems(rawItems);
  const res = await db
    .prepare(
      "UPDATE pools SET payload = ?, version = version + 1, lease_owner = NULL, lease_until = 0, updated_at = ? " +
        "WHERE uuid = ? AND version = ? AND lease_owner = ?"
    )
    .bind(JSON.stringify(items), at, uuid, version, actor)
    .run();

  if (changes(res) === 1) {
    await logEvent(db, uuid, "committed", actor, `v${version} → v${version + 1}，${items.length} 项`, at);
    return { kind: "ok", version: version + 1, count: items.length };
  }

  const row = await db
    .prepare("SELECT version, lease_owner FROM pools WHERE uuid = ?")
    .bind(uuid)
    .first();
  if (!row) return { kind: "gone" };
  await logEvent(
    db,
    uuid,
    "commit_rejected",
    actor,
    `声称 v${version}，实际 v${Number(row.version)}（闸在 ${row.lease_owner ?? "?"} 手里）`,
    at
  );
  return { kind: "stale", version: Number(row.version), lease_owner: row.lease_owner ?? null };
}

/**
 * 放弃 + 记冷却。
 *
 * 不要求闸还在自己手里：凭证真死的时候，需要的是**所有人都停手**，
 * 而不是让持有闸的那台机器继续敲同一条已死的链。
 */
export async function abortPool(db, uuid, note, at, cooldownMs) {
  const retryAfter = at + cooldownMs;
  const res = await db
    .prepare(
      "UPDATE pools SET lease_owner = NULL, lease_until = 0, retry_after = ?, updated_at = ? WHERE uuid = ?"
    )
    .bind(retryAfter, at, uuid)
    .run();
  if (changes(res) !== 1) return { kind: "gone" };
  await logEvent(db, uuid, "aborted", null, note ?? `冷却到 ${retryAfter}`, at);
  return { kind: "ok", retry_after: retryAfter };
}

/** 删池 = 解绑。绑过同一 uuid 的其他机器之后会拿到 gone，需要重新上传或重新绑定 */
export async function deletePool(db, uuid, actor, at) {
  const res = await db.prepare("DELETE FROM pools WHERE uuid = ?").bind(uuid).run();
  if (changes(res) !== 1) return { kind: "gone" };
  await logEvent(db, uuid, "deleted", actor, null, at);
  return { kind: "ok" };
}

// ── 路由 ──────────────────────────────────────────────────────────────────

export async function handle(request, env) {
  const at = Date.now();
  const method = request.method.toUpperCase();
  const seg = new URL(request.url).pathname.split("/").filter(Boolean);
  const db = env.DB;
  const leaseMs = asInt(env.LEASE_MS, DEFAULT_LEASE_MS);
  const cooldownMs = asInt(env.FAIL_COOLDOWN_MS, DEFAULT_FAIL_COOLDOWN_MS);

  if (seg[0] !== "v1") return json({ error: "not_found" }, 404);

  if (seg[1] === "health") {
    if (method !== "GET") return json({ error: "method_not_allowed" }, 405);
    return json({ ok: true, now: at, lease_ms: leaseMs, fail_cooldown_ms: cooldownMs });
  }

  if (seg[1] !== "pool") return json({ error: "not_found" }, 404);

  // POST /v1/pool —— 新建一池
  if (seg.length === 2) {
    if (method !== "POST") return json({ error: "method_not_allowed" }, 405);
    const body = await readJson(request);
    if (!body) return json({ error: "bad_json" }, 400);
    const created = await createPool(db, body.items, actorOf(request, body), at);
    return json({ ...created, now: at }, 201);
  }

  const uuid = seg[2];
  if (!UUID_RE.test(uuid)) return json({ error: "bad_uuid" }, 400);
  const tail = seg[3] ?? "";

  // /v1/pool/:uuid
  if (!tail && seg.length === 3) {
    const body = method === "GET" || method === "DELETE" ? null : await readJson(request);
    const actor = actorOf(request, body);

    if (method === "GET") {
      const r = await fetchPool(db, uuid, actor, at);
      if (r.kind === "gone") return json({ error: "gone", now: at }, 404);
      return json({ items: r.items, version: r.version, now: at });
    }

    if (method === "PUT") {
      if (!body) return json({ error: "bad_json" }, 400);
      // **要求真的是数字**：`Number("1")` 也能过，把字符串版本号放进来等于默许两边对
      // 「版本」的理解不一致（客户端若把版本号当字符串存过一版，就会一直提交不上却看不出原因）
      const version = body.version;
      if (typeof version !== "number" || !Number.isInteger(version) || version < 0) {
        return json({ error: "bad_version" }, 400);
      }
      const r = await commitPool(db, uuid, actor, version, body.items, at);
      if (r.kind === "gone") return json({ error: "gone", now: at }, 404);
      if (r.kind === "stale") {
        return json({ error: "stale", version: r.version, lease_owner: r.lease_owner, now: at }, 409);
      }
      return json({ ok: true, version: r.version, count: r.count, now: at });
    }

    if (method === "DELETE") {
      const r = await deletePool(db, uuid, actor, at);
      if (r.kind === "gone") return json({ error: "gone", now: at }, 404);
      return json({ ok: true, now: at });
    }

    return json({ error: "method_not_allowed" }, 405);
  }

  // POST /v1/pool/:uuid/lease
  if (tail === "lease" && seg.length === 4) {
    if (method !== "POST") return json({ error: "method_not_allowed" }, 405);
    const body = (await readJson(request)) ?? {};
    const r = await leasePool(db, uuid, actorOf(request, body), at, leaseMs);
    if (r.kind === "gone") return json({ error: "gone", now: at }, 404);
    if (r.kind === "denied") {
      return json({
        granted: false,
        retry_after: r.retry_after,
        lease_owner: r.lease_owner,
        lease_until: r.lease_until,
        now: at,
      });
    }
    return json({
      granted: true,
      items: r.items,
      version: r.version,
      lease_until: r.lease_until,
      now: at,
    });
  }

  // POST /v1/pool/:uuid/abort
  if (tail === "abort" && seg.length === 4) {
    if (method !== "POST") return json({ error: "method_not_allowed" }, 405);
    const body = (await readJson(request)) ?? {};
    const r = await abortPool(db, uuid, str(body.note) || null, at, cooldownMs);
    if (r.kind === "gone") return json({ error: "gone", now: at }, 404);
    return json({ ok: true, retry_after: r.retry_after, now: at });
  }

  return json({ error: "not_found" }, 404);
}
