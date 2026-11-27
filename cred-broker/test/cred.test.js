import { test, after } from "node:test";
import assert from "node:assert/strict";
import { handle, normalizeItem, normalizeItems, commitPool, MAX_ITEMS, MAX_KEY_LEN } from "../src/cred.js";
import { migrateLegacyPayload } from "../src/schema.js";
import { createEnv, closeEnv } from "./mysql-env.js";

after(() => closeEnv());   // 连接池不关，进程会挂在退出流程上

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

test("health 暴露闸参数与服务器时间（给运维核对部署；三个客户端都不读它）", async () => {
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

// ── 续签设备（一池一个组合 id：多端使用、一端续签） ────────────────────────

test("建池时可带续签设备，取件 / 抢闸都原样带回（含被拒的那条路）", async () => {
  const env = createEnv();
  const renewer = "workbuddy-assistant:8f14e45f-ceea-467f-a8b0-1f0e2d3c4b5a";
  const created = await call(env, "POST", "/v1/pool", {
    body: { items: [item("a")], renewer },
  });
  assert.equal(created.status, 201);
  assert.equal(created.data.renewer, renewer);

  const uuid = created.data.uuid;
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}`)).data.renewer, renewer);
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}/renewer`)).data.renewer, renewer);

  const first = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  assert.equal(first.data.renewer, renewer);
  // 闸在 m1 手里：m2 被拒，但也要知道这一池的续签设备是谁
  const second = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  assert.equal(second.data.granted, false);
  assert.equal(second.data.renewer, renewer);
});

test("老池没有续签设备时读出来是 null（不是空串，也不猜一台机器）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const r = await call(env, "GET", `/v1/pool/${uuid}/renewer`);
  assert.equal(r.status, 200);
  assert.equal(r.data.renewer, null);
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}`)).data.renewer, null);
});

test("认领续签设备：空着或本来就是自己都能成功，重复认领幂等", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const me = "workbuddy2api:mac-mini";

  const first = await call(env, "PUT", `/v1/pool/${uuid}/renewer`, {
    body: { renewer: me },
    actor: "mac-mini",
  });
  assert.equal(first.status, 200);
  assert.equal(first.data.renewer, me);

  // 10 秒回落 / 手滑连点都会重放这一步，必须成功
  const again = await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: me } });
  assert.equal(again.status, 200);
  assert.equal(again.data.renewer, me);
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}/renewer`)).data.renewer, me);
});

test("别的设备已认领时新设备被拒（409），池上的值一点不动", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")], "old-mac");
  const old = "workbuddy-assistant:old";
  const fresh = "workbuddy2api:new";
  await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: old }, actor: "old-mac" });

  const r = await call(env, "PUT", `/v1/pool/${uuid}/renewer`, {
    body: { renewer: fresh },
    actor: "new-mac",
  });
  assert.equal(r.status, 409);
  assert.equal(r.data.error, "renewer_taken");
  assert.equal(r.data.renewer, old);

  const current = await call(env, "GET", `/v1/pool/${uuid}/renewer`);
  assert.equal(current.data.renewer, old, "冲突失败不能顺带把旧设备挤掉");
});

test("只有当前续签设备能清空；清空后新设备才能认领（换设备的唯一路径）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const old = "workbuddy-assistant:old";
  const fresh = "workbuddy-assistant:new";
  await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: old } });

  const denied = await call(env, "POST", `/v1/pool/${uuid}/renewer/release`, {
    body: { renewer: fresh },
  });
  assert.equal(denied.status, 409);
  assert.equal(denied.data.error, "not_renewer");
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}/renewer`)).data.renewer, old);

  const released = await call(env, "POST", `/v1/pool/${uuid}/renewer/release`, {
    body: { renewer: old },
  });
  assert.equal(released.status, 200);
  assert.equal(released.data.renewer, null);
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}/renewer`)).data.renewer, null);

  // 重复清空（10 秒回落前的重试）也成功
  assert.equal(
    (await call(env, "POST", `/v1/pool/${uuid}/renewer/release`, { body: { renewer: old } })).status,
    200
  );

  const claimed = await call(env, "PUT", `/v1/pool/${uuid}/renewer`, {
    body: { renewer: fresh },
  });
  assert.equal(claimed.status, 200);
  assert.equal(claimed.data.renewer, fresh);
});

test("续签设备标识不合法时 400：空、非字符串、超长都不猜也不截断", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);

  assert.equal((await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: {} })).status, 400);
  assert.equal(
    (await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: "   " } })).status,
    400
  );
  assert.equal(
    (await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: 42 } })).status,
    400
  );
  assert.equal(
    (await call(env, "POST", `/v1/pool/${uuid}/renewer/release`, { body: {} })).status,
    400
  );
  assert.equal(
    (await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: "x".repeat(256) } }))
      .status,
    400
  );
  // 建池时同样不接受超长，避免一台机器的 id 在库里被截成别人的
  assert.equal(
    (
      await call(env, "POST", "/v1/pool", {
        body: { items: [item("a")], renewer: "x".repeat(256) },
      })
    ).status,
    400
  );
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}/renewer`)).data.renewer, null);
});

test("删池之后续签设备的读取 / 认领 / 清空都 404 gone", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  assert.equal((await call(env, "DELETE", `/v1/pool/${uuid}`)).status, 200);
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}/renewer`)).status, 404);
  assert.equal(
    (await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: "x" } })).status,
    404
  );
  assert.equal(
    (await call(env, "POST", `/v1/pool/${uuid}/renewer/release`, { body: { renewer: "x" } }))
      .status,
    404
  );
});

test("续签设备的认领 / 释放 / 被拒都进审计事件表（纯旁路，不影响判定）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: "m1" }, actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}/renewer`, { body: { renewer: "m2" }, actor: "m2" });
  await call(env, "POST", `/v1/pool/${uuid}/renewer/release`, {
    body: { renewer: "m1" },
    actor: "m1",
  });

  const { results } = await env.DB
    .prepare("SELECT kind FROM pool_events WHERE uuid = ? ORDER BY id")
    .bind(uuid)
    .all();
  const kinds = results.map((r) => r.kind);
  assert.ok(kinds.includes("renewer_claimed"));
  assert.ok(kinds.includes("renewer_denied"));
  assert.ok(kinds.includes("renewer_released"));
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

  const { results } = await env.DB
    .prepare("SELECT kind FROM pool_events WHERE uuid = ? ORDER BY id")
    .bind(uuid)
    .all();
  const kinds = results.map((r) => r.kind);
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

// ── 区域：曾经被整条丢掉，后果是「凭空多一个国际版重复账号」 ──────────────

test("normalizeItem：显式 region 原样保留（两套部署的条目必须各自带区域）", () => {
  const cn = normalizeItem({ key: "cn:191", access_token: "t", region: "cn" });
  const gl = normalizeItem({ key: "global:191", access_token: "t", region: "global" });
  assert.equal(cn.region, "cn");
  assert.equal(gl.region, "global");
});

test("normalizeItem：没有 region 字段时按 key 的 xx: 前缀回填（老数据的唯一线索）", () => {
  assert.equal(normalizeItem({ key: "cn:191", access_token: "t" }).region, "cn");
  assert.equal(normalizeItem({ key: "global:191", access_token: "t" }).region, "global");
  // 两套部署出现之前的裸 key 只能是国际版
  assert.equal(normalizeItem({ key: "191", access_token: "t" }).region, "global");
});

test("normalizeItem：显式 region 非法 / 大小写不规范时也不丢区域", () => {
  assert.equal(normalizeItem({ key: "cn:k", access_token: "t", region: "CN" }).region, "cn");
  assert.equal(normalizeItem({ key: "cn:k", access_token: "t", region: "火星版" }).region, "cn");
});

test("落库的旧条目在读出口就有区域（老客户端上传时没带 region，靠 key 前缀回填）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [
    { key: "cn:19174256652", name: "n", phone: "19174256652", access_token: "at", refresh_token: "rt" },
  ]);
  const r = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(r.status, 200);
  assert.equal(r.data.items[0].region, "cn");
});

// ── 邮箱：纯展示标识，随池搬运但**不进锚点**（2026-09-21） ─────────────────

test("normalizeItem：email 原样透传并抹掉空白", () => {
  const it = normalizeItem({ key: "global:k", access_token: "t", email: "  a@b.c  " });
  assert.equal(it.email, "a@b.c");
});

test("normalizeItem：email 不进 key（锚点仍是 key → 手机号 → 昵称 → 本地 id）", () => {
  assert.equal(normalizeItem({ key: "global:k", access_token: "t", email: "a@b.c" }).key, "global:k");
  // 没有显式 key 时也只兜到手机号，不会被邮箱顶替
  assert.equal(normalizeItem({ phone: "138", access_token: "t", email: "a@b.c" }).key, "138");
});

test("normalizeItem：老条目没有 email → 空串（不是错误，客户端按缺字段处理）", () => {
  assert.equal(normalizeItem({ key: "global:k", access_token: "t" }).email, "");
});

test("email 跟着池走一个来回（别的机器收养时就有，不必自己再打接口）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [
    { key: "global:a@b.c", name: "n", email: "a@b.c", access_token: "at", refresh_token: "rt" },
  ]);
  const r = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(r.status, 200);
  assert.equal(r.data.items[0].email, "a@b.c");
});

// ── 引擎语义：这几条是「迁到 MySQL 之后才可能错」的地方，D1 时代不存在 ──────

test("适配层：匹配到但值没变的 UPDATE 仍报 changes=1（靠 mysql2 的 FOUND_ROWS 标志）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });

  // 写一个与当前值完全相同的 lease_owner：SQLite/D1 报 1（它数的是 WHERE 匹配到的行），
  // MySQL 默认报 0（它数的是真正改变值的行）。本项目所有关键判定都是 changes === 1，
  // 少那个标志的后果是「闸已经写给这台机器了，它却被告知没抢到」。
  const r = await env.DB.prepare("UPDATE pools SET lease_owner = ? WHERE uuid = ?")
    .bind("m1", uuid)
    .run();
  assert.equal(r.meta.changes, 1);
});

test("同一台机器在租约到期后能重新拿到自己的闸（值相同也不能判成没抢到）", async () => {
  const env = createEnv({ LEASE_MS: "1" });
  const uuid = await newPool(env, [item("a")]);
  assert.equal((await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" })).data.granted, true);
  await sleep(5);
  const r = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  assert.equal(r.data.granted, true);
});

test("整池 200 条真实长度的 token 原样往返（token 列必须是 LONGTEXT，TEXT 的 64KB 装不下）", async () => {
  const env = createEnv();
  const many = Array.from({ length: MAX_ITEMS }, (_, i) =>
    item(`k${i}`, { access_token: "at".repeat(300), refresh_token: "rt".repeat(300) })
  );
  const created = await call(env, "POST", "/v1/pool", { body: { items: many } });
  assert.equal(created.status, 201);
  assert.equal(created.data.count, MAX_ITEMS);

  const got = await call(env, "GET", `/v1/pool/${created.data.uuid}`);
  assert.equal(got.data.items.length, MAX_ITEMS);
  const last = got.data.items.find((i) => i.key === `k${MAX_ITEMS - 1}`);
  assert.equal(last.access_token, "at".repeat(300));
});

test("uuid 与 key 按字节比较：换大小写不等于同一个键（与 D1/SQLite 一致）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  // 库级排序规则若写成默认的 *_ci，这一条会读回整池 —— 身份锚点被悄悄放宽，
  // 而表现为「某个客户端读到了一份它本不该看到的凭证」。
  const r = await call(env, "GET", `/v1/pool/${uuid.toUpperCase()}`);
  assert.equal(r.status, 404);
  assert.equal(r.data.error, "gone");
});

// ── 行表 + 逻辑删除（2026-11-27 拆表） ─────────────────────────────────────

test("提交时本轮没出现的条目标成墓碑：items 只给活的，deleted 给墓碑", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a"), item("b")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  assert.equal(lease.data.items.length, 2);
  assert.deepEqual(lease.data.deleted, []);

  const done = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a", { access_token: "rotated" })] },
  });
  assert.equal(done.status, 200);
  assert.equal(done.data.count, 1, "count 只数活条目");

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["a"]);
  assert.equal(got.data.items[0].deleted_at, null);
  assert.equal(got.data.deleted.length, 1);
  assert.equal(got.data.deleted[0].key, "b");
  assert.ok(got.data.deleted[0].deleted_at > 0);
  // 墓碑带着原来的 token：别的机器要靠 JWT sub / RT 认出「这是同一条账号」
  assert.equal(got.data.deleted[0].refresh_token, "rt-b");

  // 抢闸也带同一份墓碑 —— 客户端靠它剪掉本机那份云端账号
  const next = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  assert.equal(next.data.granted, true);
  assert.deepEqual(next.data.items.map((i) => i.key), ["a"]);
  assert.equal(next.data.deleted[0].key, "b");
  // 墓碑不参与版本 / 条目数
  assert.equal(next.data.version, 1);
});

test("墓碑粘住：旧客户端把 key 原样带回来也不复活，必须显式 revive", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a"), item("b")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a")] },
  });
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}`)).data.deleted[0].key, "b");

  // 另一台机器还留着旧副本：整池 6 项原样提交（不带 revive）
  const lease2 = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  const back = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m2",
    body: {
      version: lease2.data.version,
      items: [item("a"), item("b", { access_token: "resurrected" })],
    },
  });
  assert.equal(back.status, 200);
  assert.equal(back.data.count, 1, "count 只数真正写进池的活条目");
  assert.equal(back.data.ignored, 1, "被墓碑挡下的条数要如实回报");

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["a"]);
  assert.equal(got.data.deleted[0].key, "b");
  assert.equal(got.data.deleted[0].access_token, "at-b", "墓碑的令牌不能被旧副本盖掉");
});

test("显式 revive 才复活墓碑（恢复 / 重新登录走这条路）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a"), item("b")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a")] },
  });

  const lease2 = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  const back = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: {
      version: lease2.data.version,
      items: [item("a"), item("b", { access_token: "new" })],
      revive: ["b"],
    },
  });
  assert.equal(back.status, 200);
  assert.equal(back.data.count, 2);
  assert.equal(back.data.ignored, 0);

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["a", "b"]);
  assert.equal(got.data.items[1].access_token, "new");
  assert.deepEqual(got.data.deleted, [], "复活之后墓碑必须消失");
});

test("revive 只对墓碑生效：活条目照常提交，未点名的墓碑互不影响", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a"), item("b"), item("c")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a"), item("b")] },
  });

  const lease2 = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  // 一个不在墓碑里的 key 点名复活：无害；c 仍留在墓碑
  const r = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: {
      version: lease2.data.version,
      items: [item("a"), item("b"), item("c")],
      revive: ["a", "no-such-key"],
    },
  });
  assert.equal(r.status, 200);
  assert.equal(r.data.count, 2, "没点名的 c 不能跟着复活");
  assert.equal(r.data.ignored, 1);
  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["a", "b"]);
  assert.deepEqual(got.data.deleted.map((i) => i.key), ["c"]);
});

// ── 同身份聚合（打码手机号 ↔ 完整号码）─────────────────────────────────────

test("同身份聚合：打码 key 的重新登录并进完整号码墓碑复活，不另起一行", async () => {
  const env = createEnv();
  // 生产里的墓碑 name/phone/key 都是完整号码，这里照原样构造
  const uuid = await newPool(env, [item("a"), item("17674785984", { name: "17674785984" })]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a")] },
  });
  assert.equal((await call(env, "GET", `/v1/pool/${uuid}`)).data.deleted[0].key, "17674785984");

  // 重新登录的那台机器只有打码手机号：key 与 phone 都是打码形态
  const lease2 = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m2" });
  const back = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m2",
    body: {
      version: lease2.data.version,
      items: [
        item("a"),
        item("176****5984", { phone: "176****5984", access_token: "relogin" }),
      ],
    },
  });
  assert.equal(back.status, 200);
  assert.equal(back.data.count, 2);

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["17674785984", "a"]);
  assert.equal(got.data.items[0].access_token, "relogin", "新票并进墓碑那条行");
  assert.equal(got.data.items[0].phone, "17674785984", "完整号码不被打码的盖掉");
  assert.equal(got.data.items[0].name, "17674785984");
  assert.deepEqual(got.data.deleted, [], "聚合 = 墓碑复活，同一个账号不再出现在已删除里");
});

test("现网遗留形态自愈：打码活行 + 完整号码墓碑并存，一次提交后聚合到完整号码、残留墓碑清掉", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  // 直接造出修复前的数据形态（老服务端留下的）：打码 key 活行 + 完整号码 key 墓碑
  const at = Date.now();
  await env.DB.prepare(
    "INSERT INTO pool_items (pool_uuid, item_key, region, name, phone, email, access_token, refresh_token, expires_at, rt_expires_at, updated_at, deleted_at) " +
      "VALUES (?, '176****5984', 'global', '176****5984', '176****5984', '', 'at-masked', 'rt-masked', NULL, NULL, ?, NULL)"
  )
    .bind(uuid, at)
    .run();
  await env.DB.prepare(
    "INSERT INTO pool_items (pool_uuid, item_key, region, name, phone, email, access_token, refresh_token, expires_at, rt_expires_at, updated_at, deleted_at) " +
      "VALUES (?, '17674785984', 'global', '17674785984', '17674785984', '', 'at-old', 'rt-old', NULL, NULL, ?, ?)"
  )
    .bind(uuid, at, at)
    .run();

  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  const back = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: {
      version: lease.data.version,
      items: [
        item("a"),
        item("176****5984", { phone: "176****5984", access_token: "relogin" }),
      ],
    },
  });
  assert.equal(back.status, 200);

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["17674785984", "a"]);
  assert.equal(got.data.items[0].access_token, "relogin");
  assert.equal(got.data.items[0].phone, "17674785984");
  assert.deepEqual(got.data.deleted, [], "打码 key 的残留墓碑也要清掉，已删除里不留同账号");
});

test("聚合不越界：手机号不同身份的墓碑不并入也不移除", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [
    item("a"),
    item("17674785984", { name: "17674785984" }),
    item("19908449648", { name: "19908449648" }),
  ]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  // 只回传 a：两个完整号码账号都成墓碑
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a")] },
  });

  // 打码重新登录只认得 176 那一条；199 的墓碑谁也不像，原样保留
  const lease2 = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  const back = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: {
      version: lease2.data.version,
      items: [
        item("a"),
        item("176****5984", { phone: "176****5984", access_token: "relogin" }),
      ],
    },
  });
  assert.equal(back.status, 200);

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.deepEqual(got.data.items.map((i) => i.key), ["17674785984", "a"]);
  assert.deepEqual(got.data.deleted.map((i) => i.key), ["19908449648"], "不同号码的墓碑不许动");
});

test("revive 不是字符串数组时 400（不静默当成空）", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  const r = await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a")], revive: "a" },
  });
  assert.equal(r.status, 400);
  assert.equal(r.data.error, "bad_revive");
});

test("整池提交是一笔事务：条目写到一半失败时，版本号与旧条目都回滚", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await assert.rejects(
    commitPool(env.DB, uuid, "m1", lease.data.version, [item("x".repeat(MAX_KEY_LEN + 1))], [], Date.now()),
    "超长 key 在库里必须报错（1406），不能静默截断"
  );

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(got.data.version, lease.data.version, "CAS 的版本 +1 也必须回滚");
  assert.deepEqual(got.data.items.map((i) => i.key), ["a"]);
  assert.deepEqual(got.data.deleted, [], "旧条目不能被这次失败的提交标成墓碑");
});

test("条目 key 超长在路由层就 400，不落到 500（客户端会把 5xx 当成管家不可达）", async () => {
  const env = createEnv();
  const r = await call(env, "POST", "/v1/pool", {
    body: { items: [item("k".repeat(MAX_KEY_LEN + 1))] },
  });
  assert.equal(r.status, 400);
  assert.equal(r.data.error, "bad_item_key");
});

test("删池把条目行（含墓碑）一并删掉，不留孤儿行", async () => {
  const env = createEnv();
  const uuid = await newPool(env, [item("a"), item("b")]);
  const lease = await call(env, "POST", `/v1/pool/${uuid}/lease`, { actor: "m1" });
  await call(env, "PUT", `/v1/pool/${uuid}`, {
    actor: "m1",
    body: { version: lease.data.version, items: [item("a")] },
  });
  assert.equal((await call(env, "DELETE", `/v1/pool/${uuid}`)).status, 200);
  const row = await env.DB
    .prepare("SELECT COUNT(*) AS n FROM pool_items WHERE pool_uuid = ?")
    .bind(uuid)
    .first();
  assert.equal(Number(row.n), 0);
});

test("老 payload 迁移：一池一段 JSON 摊成行表，成功后删掉 payload 列；重跑是空操作", async () => {
  const env = createEnv();
  // 模拟拆表前的库：加回 payload 列，插一行老数据（同 key 重复，验证后到覆盖先到）
  await env.DB.prepare("ALTER TABLE pools ADD COLUMN payload LONGTEXT NULL").run();
  const uuid = "33333333-3333-3333-3333-333333333333";
  const legacy = [
    { key: "cn:191", name: "老账号", phone: "191", access_token: "at-old", refresh_token: "rt-old" },
    { key: "cn:191", name: "重复", access_token: "at-dup" },
  ];
  await env.DB
    .prepare(
      "INSERT INTO pools (uuid, payload, version, lease_until, retry_after, created_at, updated_at) VALUES (?, ?, 0, 0, 0, 0, 0)"
    )
    .bind(uuid, JSON.stringify(legacy))
    .run();

  const r = await migrateLegacyPayload(env.DB);
  assert.equal(r.skipped, false);
  assert.ok(r.pools >= 1, "本文件前面建的池也会被扫一遍（它们的 payload 是 ALTER 补出来的 NULL）");

  const got = await call(env, "GET", `/v1/pool/${uuid}`);
  assert.equal(got.status, 200);
  assert.equal(got.data.items.length, 1);
  assert.equal(got.data.items[0].region, "cn");
  assert.equal(got.data.items[0].access_token, "at-dup");
  assert.equal(got.data.items[0].name, "重复");
  assert.equal(got.data.items[0].deleted_at, null);

  const col = await env.DB
    .prepare(
      "SELECT COUNT(*) AS n FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = 'pools' AND column_name = 'payload'"
    )
    .first();
  assert.equal(Number(col.n), 0, "迁移完成后 payload 列必须删掉（不能留两份真相）");

  const again = await migrateLegacyPayload(env.DB);
  assert.equal(again.skipped, true);
});
