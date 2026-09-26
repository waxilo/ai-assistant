/**
 * 请求边界：容器进程（src/server.js）把每个请求交到这里。
 * 全部业务在 `cred.js`（那里可以直接被 node 测试跑），
 * 这里只负责把异常收成 500 —— 一个未捕获的 throw 会让客户端只看到
 * 一段错误页或半截响应，而它进了客户端的 `FailKind` 判定就是 `Unreachable`，
 * 会把「代码写错了」误判成「网络不通」，继续用本地凭证（看起来一切正常）。
 *
 * 形状保留为 `fetch(request, env)`：当初它是 Worker 入口，Cloudflare 那份已下线，
 * 但这层签名对容器来说同样自然（server.js 直接构造 Request 传进来）。
 */
import { handle } from "./cred.js";

export default {
  async fetch(request, env) {
    try {
      return await handle(request, env);
    } catch (e) {
      const message = String((e && e.message) || e);
      console.error("cred-broker 未捕获异常：", message);
      return new Response(JSON.stringify({ error: "internal", message }), {
        status: 500,
        headers: { "content-type": "application/json; charset=utf-8" },
      });
    }
  },
};
