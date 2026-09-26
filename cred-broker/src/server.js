// 容器进程入口：把 node:http 适配成 fetch(request, env) 这个形状。
//
// index.js（异常收成 500）与 cred.js（路由 + 全部业务）一行不改 —— 它们只用了 Web 标准 API
// （Request / Response / URL / crypto.randomUUID），Node 24 全部原生具备。
// 这份代码原先跑在 Cloudflare 上（现已下线），两处平台依赖在本机各自换了实现：
//   1) D1 绑定   → src/db.js 的 env.DB（同形接口）
//   2) 自定义域名 → 由 ../gw 共享网关按 Host 转发过来；应用只听 http，不碰证书
//
// 没有第三处：这个服务**没有后台任务**（原来 wrangler.toml 里 crons 就刻意是空的），
// 所以不需要 notify-hub 那种进程内定时器 —— 加一个空跑的 ticker 只会让人以为「管家会自己续签」。
//
// 两个健康检查各有各的人，不能合并：
//   /healthz    → compose 的 healthcheck，真的 SELECT 1（接口活着但连不上库要能被看出来）
//   /v1/health  → 对外探活，回答的是闸参数（lease_ms / fail_cooldown_ms），供运维核对部署；
//                 当前三个客户端都不读它（只用抢闸响应里的 retry_after 算等待）。按设计不查库。
import { createServer } from 'node:http';
import worker from './index.js';
import { createDatabase, databaseConfigFromEnv } from './db.js';

const PORT = Number(process.env.PORT || 8787);
const HOST = process.env.HOST || '0.0.0.0';
// 一池上限 200 条、每条两个 token，正常提交是几十 KB 量级。4MB 已远超任何合法请求，
// 而没这道闸的话，一个 500MB 的 body 会先被完整读进内存再 JSON.parse。
const MAX_BODY_BYTES = 4_000_000;

// LEASE_MS / FAIL_COOLDOWN_MS 就沿用到处的字符串形态（Worker 的 vars 本来就是字符串，
// cred.js 用 asInt 解析并兜默认值 —— 两侧同形，不必在这里先转一遍数字再期望它认得数字）。
const env = { ...process.env };

/** 回给 cred.js 的 URL：它只取 pathname 做路由，origin 在这里没有任何语义。 */
function requestUrl(req) {
  const host = req.headers.host || `127.0.0.1:${PORT}`;
  return `http://${host}${req.url || '/'}`;
}

async function readBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_BODY_BYTES) {
      const err = new Error('payload too large');
      err.statusCode = 413;
      throw err;
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

async function toWebRequest(req) {
  const headers = new Headers();
  for (const [k, v] of Object.entries(req.headers)) {
    if (v === undefined) continue;
    for (const item of Array.isArray(v) ? v : [v]) headers.append(k, item);
  }
  const method = req.method || 'GET';
  const init = { method, headers };
  if (method !== 'GET' && method !== 'HEAD') {
    const body = await readBody(req);
    if (body.length) init.body = body;
  }
  return new Request(requestUrl(req), init);
}

async function handleApi(req, res) {
  const request = await toWebRequest(req);
  const response = await worker.fetch(request, env);
  const headers = Object.fromEntries(response.headers.entries());
  res.writeHead(response.status, headers);
  if (request.method === 'HEAD' || response.status === 204) return res.end();
  const buf = Buffer.from(await response.arrayBuffer());
  res.end(buf);
}

async function handleHealth(res) {
  let dbOk = false;
  try {
    await env.DB.prepare('SELECT 1 AS ok').first();
    dbOk = true;
  } catch (err) {
    console.error('health_db_failed', String(err && err.message ? err.message : err));
  }
  res.writeHead(dbOk ? 200 : 503, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify({ status: dbOk ? 'ok' : 'error', database: dbOk }));
}

function assertConfig() {
  try {
    databaseConfigFromEnv(env);
  } catch (err) {
    console.error('startup_misconfigured', JSON.stringify({ problem: String(err.message) }));
    process.exit(1);
  }
}

assertConfig();
const { DB, close: closeDb } = createDatabase(databaseConfigFromEnv(env));
env.DB = DB;

const server = createServer((req, res) => {
  let pathname = '/';
  try {
    pathname = new URL(requestUrl(req)).pathname;
  } catch { /* 非法请求行按 / 处理，交给路由表回 404 */ }
  const route = (async () => {
    if (pathname === '/healthz') return handleHealth(res);
    return handleApi(req, res);   // 其余全部走 cred.js 的路由表（含 /v1/health 与 404/405）
  })();
  route.catch((err) => {
    const status = err && err.statusCode === 413 ? 413 : 500;
    console.error('request_failed', JSON.stringify({ method: req.method, path: pathname, status, err: String(err) }));
    if (!res.headersSent) res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
    res.end(JSON.stringify({ error: status === 413 ? 'payload_too_large' : 'internal', now: Date.now() }));
  });
});

server.listen(PORT, HOST, () => {
  console.log('cred-broker listening', JSON.stringify({ port: PORT, host: HOST, db: env.DB_NAME }));
});

// docker stop 发 SIGTERM：停止接单、等在途请求收尾，再关连接池。
// 这里尤其不能硬切：客户端三步握手最怕「提交请求发出去了但服务端没落库」，
// 那种情况下持闸机器只能等租约超时（LEASE_MS），期间所有机器都不续签。
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    console.log('shutdown', JSON.stringify({ signal: sig }));
    server.close(() => closeDb().catch(() => {}).then(() => process.exit(0)));
    setTimeout(() => process.exit(0), 8000).unref();
  });
}
