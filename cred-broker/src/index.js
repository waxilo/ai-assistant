/**
 * Worker 入口。全部业务在 `cred.js`（那里可以直接被 node 测试跑），
 * 这里只负责把异常收成 500 —— 一个未捕获的 throw 会让客户端只看到
 * Cloudflare 的 HTML 错误页，而它进了客户端的 `FailKind` 判定就是 `Unreachable`，
 * 会把「代码写错了」误判成「网络不通」，继续用本地凭证（看起来一切正常）。
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
