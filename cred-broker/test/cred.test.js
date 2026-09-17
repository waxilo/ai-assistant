import { test } from "node:test";
import assert from "node:assert/strict";
import { handle, normalizeItem, normalizeItems, MAX_ITEMS } from "../src/cred.js";
import { createEnv } from "./d1-shim.js";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** 打一次接口：路径写相对段，返回 { status, data } */
async function call(env, method, path, opts = {}) {
  const headers = { "content-type": "application/json" };
  if (opts.actor) headers["x-cred-actor"] = opts.actor;
  const req = new Request(`https://cred-broker.test${path}`, {
    method,
    headers,
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
  });
  const res = await handle(req, env);
  const text = await res.text();
  return { status: res.status, data: text ? JSON.parse(text) : null };
}

const item = (key, over = {}) => ({
  key,
  name: `账号${key}`,
  phone: key,
  access_token: `at-${key}`,
  refresh_token: `rt-${key}`,
  expires_at: 1_760_000_000_000,
  rt_expires_at: 1_770_000_000_000,
  ...over,
});

/** 建一池，返回 uuid */
async function newPool(env, items, actor = "mac-mini") {
  const r = await call(env, "POST", "/v1/pool", { body: { items }, actor });
  assert.equal(r.status, 201);
  return r.data.uuid;
}

// ── 健康检查 ───────────────────────────────────────────────────────────────

test("health 返回闸参数，供客户端核对与自身常量是否一致", async () => {
  const env = createEnv();
  const r = await call(env, "GET", "/v1/health");
  assert.equal(r.status, 200);
  assert.equal(r.data.ok, true);
  assert.equal(r.data.lease_ms, 180_000);
  assert.equal(r.data.fail_cooldown_ms, 300_000);
  assert.ok(Number.isFinite(r.data.now));
});

test("未知路径与错误方法各自返回 404 / 405", async () => {
  const env = createEnv();
  assert.equal((await call(env, "GET", "/nope")).status, 404);
  assert.equal((await call(env, "GET", "/v1")).status, 404);
  assert.equal((await call(env, "GET", "/v1/health-nope")).status, 404);
  assert.equal((await call(env, "PUT", "/v1/health")).status, 405);
  assert.equal((await call(env, "GET", "/v1/pool")).status, 405);
});

// ── 建池 ──────────────────────────────────────────────────────────────────

test("建池颁发 uuid，版本从 0 起，条目数如实", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", {
    body: { items: [item("13800000000"), item("13900000000")] },
    actor: "mac-mini",
  });
  assert.equal(r.status, 201);
  assert.match(r.data.uuid, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  assert.equal(r.data.version, 0);
  assert.equal(r.data.count, 2);
});

test("两次建池拿到不同 uuid（服务端颁发，客户端不自己推导）", async () => {
  const env = createEnv();
  const a = await newPool(env, [item("a")]);
  const b = await newPool(env, [item("b")]);
  assert.notEqual(a, b);
});

test("两个 token 都为空的条目整条丢弃，不让「这一池有几条」失真", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", {
    body: {
      items: [
        item("keep"),
        { key: "no-token", name: "只有昵称" },
        { key: "blank", access_token: "   ", refresh_token: "" },
        null,
        "垃圾",
      ],
    },
  });
  assert.equal(r.data.count, 1);
});

test("只有一个 token 的条目保留（access 缺失但 refresh 在，仍可续签）", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", {
    body: { items: [{ key: "only-rt", refresh_token: "rt-x" }] },
  });
  assert.equal(r.data.count, 1);
  const got = await call(env, "GET", `/v1/pool/${r.data.uuid}`);
  assert.equal(got.data.items[0].refresh_token, "rt-x");
  assert.equal(got.data.items[0].access_token, "");
});

test("同一 key 重复出现时后到的覆盖先到的", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", {
    body: {
      items: [
        item("same", { access_token: "old" }),
        item("same", { access_token: "new" }),
      ],
    },
  });
  assert.equal(r.data.count, 1);
  const got = await call(env, "GET", `/v1/pool/${r.data.uuid}`);
  assert.equal(got.data.items[0].access_token, "new");
});

test("条目数超上限时截断到 MAX_ITEMS", async () => {
  const env = createEnv();
  const many = Array.from({ length: MAX_ITEMS + 20 }, (_, i) => item(`k${i}`));
  const r = await call(env, "POST", "/v1/pool", { body: { items: many } });
  assert.equal(r.data.count, MAX_ITEMS);
});

test("items 不是数组时当作空池，不报错", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", { body: { items: "nope" } });
  assert.equal(r.status, 201);
  assert.equal(r.data.count, 0);
});

test("body 不是合法 JSON 时返回 400", async () => {
  const env = createEnv();
  const req = new Request("https://cred-broker.test/v1/pool", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: "{ 坏掉的 json",
  });
  const res = await handle(req, env);
  assert.equal(res.status, 400);
  assert.equal((await res.json()).error, "bad_json");
});

// ── 路径参数 ──────────────────────────────────────────────────────────────

test("不是 uuid 的路径直接 400，不去查库", async () => {
  const env = createEnv();
  for (const bad of ["abc", "123", "not-a-uuid-here-xxxxx"]) {
    assert.equal((await call(env, "GET", `/v1/pool/${bad}`)).status, 400);
  }
});

test("形态合法但不存在的 uuid 返回 404 gone（客户端据此判定「池在别处被解绑」）", async () => {
  const env = createEnv();
  const r = await call(env, "GET", "/v1/pool/00000000-0000-0000-0000-000000000000");
  assert.equal(r.status, 404);
  assert.equal(r.data.error, "gone");
});

// ── 取件 ──────────────────────────────────────────────────────────────────

test("取件返回整池与版本，且不动闸", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a"), item("b")]);
  const r = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(r.status, 200);
  assert.equal(r.data.items.length, 2);
  assert.equal(r.data.version, 0);

  // 连着取两次都该成功：取件是只读的
  const again = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(again.status, 200);
});

test("两个 token 的过期时间原样往返（客户端靠它们判断要不要续签）", async () => {
  const env = createEnv();
  const at = 1_760_111_222_333;
  const rt = 1_770_444_555_666;
  const uuid = await newPool(env, [item("x", { expires_at: at, rt_expires_at: rt })]);
  const r = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(r.data.items[0].expires_at, at);
  assert.equal(r.data.items[0].rt_expires_at, rt);
});

test("没带过期时间的条目落成 null，不会变成 0", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [
    { key: "bare", access_token: "at" },
  ]);
  const r = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(r.data.items[0].expires_at, null);
  assert.equal(r.data.items[0].rt_expires_at, null);
});

// ── 抢闸 ──────────────────────────────────────────────────────────────────

test("抢闸成功时**同时带回那一池**（客户端必须用它去签，别再自己 GET）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const r = await call(env, "POST", `/v1/pool/${uuid}/lease`, {
    body: { actor: "mac-mini" },
    actor: "mac-mini",
  });
  assert.equal(r.status, 200);
  assert.equal(r.data.granted, true);
  assert.equal(r.data.items.length, 1);
  assert.equal(r.data.items[0].key, "a");
  assert.equal(r.data.version, 0);
  assert.ok(r.data.lease_until > r.data.now);
});

test("闸在别人手里时第二个请求被拒，且带回 retry_after / 持闸人", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "mac-mini" });

  const r = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "win-box" });
  assert.equal(r.status, 200);
  assert.equal(r.data.granted, false);
  assert.equal(r.data.lease_owner, "mac-mini");
  assert.ok(r.data.retry_after <= r.data.now); // 没冷却时 retry_after 只是「已经过去了」
  assert.ok(r.data.lease_until > r.data.now);
});

test("租约过期后第二台机器可以接管（不依赖任何后台任务）", async () => {
  const env = createEnv({ LEASE_MS: "1" });
  const uuid = await newPool(env, [item("a")]);
  assert.equal((await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" })).data.granted, true);
  await sleep(5);
  const r = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  assert.equal(r.data.granted, true);
});

// ── 提交（CAS） ────────────────────────────────────────────────────────────

test("提交成功：版本 +1、条目替换、闸归还，之后别人能抢到", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });

  const done = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: {
      actor: "m1",
      version: lease.data.version,
      items: [item("a", { access_token: "rotated", refresh_token: "rt-rotated" })],
    },
  });
  assert.equal(done.status, 200);
  assert.equal(done.data.ok, true);
  assert.equal(done.data.version, 1);

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(got.data.version, 1);
  assert.equal(got.data.items[0].access_token, "rotated");

  const next = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  assert.equal(next.data.granted, true);
  assert.equal(next.data.version, 1);
  assert.equal(next.data.items[0].refresh_token, "rt-rotated");
});

test("版本不对的提交被拒（409 stale）——防「拿着旧副本整块盖回去」", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);

  const first = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: first.data.version, items: [item("a", { access_token: "v1" })] },
  });

  // 第二台机器拿着过期的 version=0 来提交
  const stale = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m2",
    body: { version: 0, items: [item("a", { access_token: "v0-旧副本" })] },
  });
  assert.equal(stale.status, 409);
  assert.equal(stale.data.error, "stale");
  assert.equal(stale.data.version, 1);

  // 旧副本没有写进去
  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(got.data.items[0].access_token, "v1");
});

test("没抢到闸的人不能提交（lease_owner 不匹配同样 409）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });

  const r = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m2",
    body: { version: 0, items: [item("a", { access_token: "偷写" })] },
  });
  assert.equal(r.status, 409);
  assert.equal(r.data.error, "stale");
});

test("version 非整数时 400，不落到 CAS 上", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  for (const bad of ["1", -1, 1.5, null]) {
    const r = await call(env, "PUT", `/v1/pool/${uuid}`, {
      actor: "m1",
      body: { version: bad, items: [] },
    });
    assert.equal(r.status, 400, `version=${bad} 应 400`);
  }
});

test("提交到不存在的池返回 404 gone", async () => {
  const env = createEnv();
  const r = await call(env, "PUT", "/v1/pool/11111111-1111-1111-1111-111111111111", {
    body: { version: 0, items: [] },
  });
  assert.equal(r.status, 404);
  assert.equal(r.data.error, "gone");
});

// ── 失败与冷却 ────────────────────────────────────────────────────────────

test("abort 释放闸并记冷却，冷却期内谁抢都被拒", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });

  const ab = await call(env, "POST", `/v1/pool/${uuid}/abort`, {
    body: { actor: "m1", note: "refresh 被服务端拒绝" },
  });
  assert.equal(ab.status, 200);
  assert.ok(ab.data.retry_after > ab.data.now);

  const r = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  assert.equal(r.data.granted, false);
  assert.ok(r.data.retry_after > r.data.now);
});

test("冷却过期后可以重新抢闸", async () => {
  const env = createEnv({ FAIL_COOLDOWN_MS: "1" });
  const uuid = await newPool(env, [item("a")]);
  await call(env, "POST", `/v1/pool/${uuid}/abort`, { body: {} });
  await sleep(5);
  const r = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  assert.equal(r.data.granted, true);
});

test("abort 不要求闸还在自己手里（凭证真死时需要所有人都停手）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const r = await call(env, "POST", `/v1/pool/${uuid}/abort`, {
    body: { actor: "从来没抢过闸的机器" },
  });
  assert.equal(r.status, 200);
});

test("对不存在的池 abort 返回 404", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool/22222222-2222-2222-2222-222222222222/abort", {
    body: {},
  });
  assert.equal(r.status, 404);
});

// ── 解绑 ──────────────────────────────────────────────────────────────────

test("删池之后取件 / 抢闸 / 提交全变 404 gone", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const del = await call(env, "DELETE", `/v1/pool/${uuid}`, { actor: "m1" });
  assert.equal(del.status, 200);

  assert.equal((await call(env, "GET", `/v1/pool/${uuid}`)).status, 404);
  assert.equal((await call(env, "POST", `/v1/pool/${uuid}/lease`, { body: {} })).status, 404);
  assert.equal((await call(env, "PUT", `/v1/pool/${uuid}`, { body: { version: 0, items: [] } })).status, 404);
  assert.equal((await call(env, "DELETE", `/v1/pool/${uuid}`)).status, 404);
});

// ── 审计 ──────────────────────────────────────────────────────────────────

test("事件表记下了创建 / 取件 / 抢闸 / 提交 / 解绑，且与业务判断解耦", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")], "mac-mini");
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "mac-mini" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "mac-mini",
    body: { version: lease.data.version, items: [item("a")] },
  });
  await call(env, "DELETE", `/v1/pool/${uuid}`, { actor: "mac-mini" });

  const kinds = env.DB.raw
    .prepare("SELECT kind FROM pool_events WHERE uuid = ? ORDER BY id")
    .all(uuid)
    .map((r) => r.kind);
  assert.ok(kinds.includes("created"));
  assert.ok(kinds.includes("leased"));
  assert.ok(kinds.includes("committed"));
  assert.ok(kinds.includes("deleted"));
});

test("actor 只进审计，不参与任何判断（不带 actor 也能走完全流程）", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", { body: { items: [item("a")] } });
  const uuid = r.data.uuid;
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { body: {} });
  assert.equal(lease.data.granted, true);
  const done = await call(env, "PUT", `/v1/pool/${uuid}`, {
    body: { version: lease.data.version, items: [item("a")] },
  });
  assert.equal(done.status, 200);
});

// ── 纯函数 ────────────────────────────────────────────────────────────────

test("normalizeItem：key 的兜底顺序是 key → 手机号 → 昵称 → 本地 id", () => {
  const withToken = (over) => ({ access_token: "t", ...over });
  assert.equal(normalizeItem(withToken({ key: "k", phone: "138", name: "n" })).key, "k");
  assert.equal(normalizeItem(withToken({ phone: "138", name: "n" })).key, "138");
  assert.equal(normalizeItem(withToken({ name: "n", local_id: "id1" })).key, "n");
  assert.equal(normalizeItem(withToken({ local_id: "id1" })).key, "id1");
});

test("normalizeItem：既没有 key 也没有手机号昵称本地 id 时丢弃（无身份锚点）", () => {
  assert.equal(normalizeItem({ access_token: "t" }), null);
});

test("normalizeItems：非数组、全空、含垃圾都不抛错", () => {
  assert.deepEqual(normalizeItems(undefined), []);
  assert.deepEqual(normalizeItems(null), []);
  assert.deepEqual(normalizeItems("x"), []);
  assert.deepEqual(normalizeItems([null, 1, "s", {}]), []);
  assert.equal(normalizeItems([{ access_token: "t", key: "k" }]).length, 1);
});

test("前后空白被抹掉，避免「同一个 key 因为空格被当成两个账号」", () => {
  const it = normalizeItem({ key: "  k  ", access_token: "  at  ", name: " n " });
  assert.equal(it.key, "k");
  assert.equal(it.access_token, "at");
  assert.equal(it.name, "n");
});
