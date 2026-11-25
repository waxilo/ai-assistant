import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  Account,
  ImportItem,
  ImportReport,
  LocalAccount,
  OAuthPoll,
} from "../types";
import {
  discoverLocalAccounts,
  oauthPoll,
  oauthStart,
  openExternal,
} from "../api";
import { maskPhone, copyText } from "../common";
import type { Toast } from "../common";
import { Dialog } from "./Dialog";
import {
  IconAlertTriangle,
  IconClock,
  IconInfo,
  IconUserPlus,
} from "./Icons";

/**
 * 「添加账号」弹窗：把「导入本机账号 / 登录新账号 / JSON」合成一个入口，
 * 三条通道用分段控件切换（参考 trae）。
 *
 * - 本机账号：读 WorkBuddy 写在本机的 auth/*.info，不需要它正在运行；
 * - 浏览器登录：官方 OAuth state 轮询，在系统浏览器完成登录；
 * - JSON：粘贴别处转来的账号（或本应用导出的内容），**只有 access token**。
 *
 * 前两条是完整续签链（带 refresh token），凭据只留在本机这一份 —— 多台机器各持
 * 一份副本会被官方的单链轮换互相踢下线，跨机器共享走凭证池。
 * JSON 通道是明确的例外：导入的一律标记成「公用 token」，不续签、不上传凭证池，
 * 导出也只写 access token（绝不带 refresh token）。
 */

const DEFAULT_HOST = "https://www.workbuddy.cn";

type AddTab = "local" | "oauth" | "json";

/**
 * 解析导入的 JSON。顶层可以是账号数组，也可以是本应用导出的
 * `{ accounts: [...] }`；每条只认 token（必填）与几项展示信息。
 */
function parseJsonAccounts(text: string): { items: ImportItem[]; error: string } {
  const raw = text.trim();
  if (!raw) return { items: [], error: "" };
  let data: unknown;
  try {
    data = JSON.parse(raw);
  } catch (e) {
    return { items: [], error: "JSON 解析失败：" + String(e) };
  }
  const rows = Array.isArray(data)
    ? data
    : data &&
        typeof data === "object" &&
        Array.isArray((data as { accounts?: unknown }).accounts)
      ? (data as { accounts: unknown[] }).accounts
      : null;
  if (!rows) {
    return { items: [], error: "没找到账号数组：顶层数组或 { accounts: [...] }" };
  }
  const str = (v: unknown) =>
    typeof v === "string" && v.trim() ? v.trim() : null;
  const items: ImportItem[] = [];
  const seen = new Set<string>();
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    if (!row || typeof row !== "object") {
      return { items: [], error: `第 ${i + 1} 项不是对象` };
    }
    const r = row as Record<string, unknown>;
    const token = str(r.token);
    if (!token) return { items: [], error: `第 ${i + 1} 项缺少 token` };
    if (seen.has(token)) continue;
    seen.add(token);
    const exp = Number(r.expires_at);
    items.push({
      token,
      name: str(r.name),
      phone: str(r.phone),
      host: str(r.host) ?? str(r.base_url),
      expires_at: Number.isFinite(exp) && exp > 0 ? exp : null,
      // 这一类导入不续签、不上传凭证池（见文件头注释）
      shared: true,
    });
  }
  if (items.length === 0) {
    return {
      items: [],
      error: rows.length === 0 ? "账号数组是空的" : "没有有效账号（token 都重复）",
    };
  }
  return { items, error: "" };
}

export function AddAccountModal({
  accounts,
  onImport,
  onClose,
  onToast,
}: {
  accounts: Account[];
  onImport: (items: ImportItem[]) => Promise<ImportReport>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [tab, setTab] = useState<AddTab>("local");
  const [importing, setImporting] = useState(false);

  // ── JSON 通道 ─────────────────────────────────────────────
  const [jsonText, setJsonText] = useState("");
  const parsed = useMemo(() => parseJsonAccounts(jsonText), [jsonText]);

  // ── 本机账号 ──────────────────────────────────────────────
  const [list, setList] = useState<LocalAccount[]>([]);
  const [loading, setLoading] = useState(true);

  const scan = useCallback(async () => {
    setLoading(true);
    try {
      // `?? []` 不是多余的：这个返回值会直接喂给下面的 `list.filter`，
      // 一旦 IPC 回了 null（命令名改了 / 后端没注册），崩的是整个 React 树 ——
      // 表现成「打开导入弹窗，整个应用白屏」，而不是「这个弹窗里没有账号」。
      setList((await discoverLocalAccounts()) ?? []);
    } catch {
      setList([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void scan();
  }, [scan]);

  const addedTokens = useMemo(
    () => new Set(accounts.map((a) => a.token)),
    [accounts]
  );
  // 已存在的账号（同 token）会被跳过而不是重复添加
  const pending = list.filter((d) => !addedTokens.has(d.token));

  const toItem = (d: LocalAccount): ImportItem => ({
    token: d.token,
    host: d.host,
    name: d.nickname || d.phone,
    phone: d.phone,
    // 不带这两个字段的话，导入的账号永远无法自动续签
    refresh_token: d.refresh_token,
    expires_at: d.expires_at,
  });

  // ── 浏览器登录 ────────────────────────────────────────────
  // 本助手只支持 WorkBuddy 国内版，接口域固定，不再给用户选。
  const [phase, setPhase] = useState<"idle" | "waiting" | "done" | "error">(
    "idle"
  );
  const [uri, setUri] = useState("");
  const [result, setResult] = useState<OAuthPoll | null>(null);
  const [oauthErr, setOauthErr] = useState("");

  const timer = useRef<number | null>(null);
  const polling = useRef(false);

  const stop = useCallback(() => {
    if (timer.current !== null) {
      window.clearInterval(timer.current);
      timer.current = null;
    }
    polling.current = false;
  }, []);
  useEffect(() => stop, [stop]);

  const begin = async () => {
    stop();
    setResult(null);
    setOauthErr("");
    setUri("");
    setPhase("waiting");
    try {
      const s = await oauthStart(DEFAULT_HOST);
      setUri(s.verification_uri);
      try {
        await openExternal(s.verification_uri);
      } catch {
        onToast({ kind: "info", text: "未能自动打开浏览器，请手动点「重新打开」" });
      }
      const startedAt = Date.now();
      const limitMs = (s.expires_in || 600) * 1000;
      timer.current = window.setInterval(() => {
        const elapsed = Date.now() - startedAt;
        if (elapsed > limitMs) {
          stop();
          setOauthErr("登录超时，请重新发起");
          setPhase("error");
          return;
        }
        // 上一次轮询还没回来就跳过这一拍，避免请求叠加
        if (polling.current) return;
        polling.current = true;
        void (async () => {
          try {
            const r = await oauthPoll(s.login_id);
            if (!r.done) return;
            stop();
            if (r.error || !r.token) {
              setOauthErr(r.error ?? "授权完成但未返回 token");
              setPhase("error");
            } else {
              setResult(r);
              setPhase("done");
            }
          } catch (e) {
            stop();
            setOauthErr(String(e));
            setPhase("error");
          } finally {
            polling.current = false;
          }
        })();
      }, 2000);
    } catch (e) {
      setOauthErr(String(e));
      setPhase("error");
    }
  };

  const reset = () => {
    stop();
    setPhase("idle");
    setUri("");
    setResult(null);
    setOauthErr("");
  };

  /** 两条通道共用：导入成功后关窗；没有新增也没有更新时按各自的文案提示 */
  const finishImport = async (
    items: ImportItem[],
    emptyText: string,
    closeOnEmpty: boolean
  ) => {
    if (items.length === 0) return;
    // JSON 通道更新的是公用 token：它不补续签信息，反而会停掉原来的续签
    const shared = items.every((i) => i.shared);
    setImporting(true);
    try {
      const { added, updated } = await onImport(items);
      if (added > 0 || updated > 0) {
        onToast({
          kind: "ok",
          text:
            added > 0 && updated > 0
              ? `已导入 ${added} 个账号，更新 ${updated} 个已有账号的凭证`
              : added > 0
              ? `已导入 ${added} 个账号`
              : shared
              ? `已更新 ${updated} 个账号（按公用 token 处理：不续签、不上传凭证池）`
              : `已更新 ${updated} 个账号的凭证（补全续签信息）`,
        });
        onClose();
      } else {
        onToast({ kind: "info", text: emptyText });
        if (closeOnEmpty) onClose();
      }
    } catch (e) {
      onToast({ kind: "err", text: "导入失败：" + String(e) });
    } finally {
      setImporting(false);
    }
  };

  /**
   * 导出当前账号为 JSON：只写 access token 与展示信息，
   * **绝不带 refresh token** —— 那份副本会让两台机器互相踢下线。
   * 同时填进输入框并复制：剪贴板不可用时还能手动全选。
   */
  const exportJson = async () => {
    const payload = {
      app: "workbuddy2api",
      version: 1,
      exported_at: new Date().toISOString(),
      accounts: accounts.map((a) => ({
        name: a.name,
        phone: a.phone,
        host: a.base_url,
        expires_at: a.expires_at,
        token: a.token,
      })),
    };
    const text = JSON.stringify(payload, null, 2);
    setJsonText(text);
    const ok = await copyText(text);
    onToast(
      ok
        ? {
            kind: "ok",
            text: `已复制 ${accounts.length} 个账号的 JSON（不含续签凭证）`,
          }
        : {
            kind: "err",
            text: "复制失败：JSON 已填进输入框，可手动全选复制",
          }
    );
  };

  const waiting = phase === "waiting";
  const locked = importing || waiting;

  return (
    <Dialog
      size="lg"
      icon={<IconUserPlus size={16} />}
      title="添加账号"
      label="添加账号"
      subtitle="导入本机已登录的账号、用浏览器登录，或粘贴 JSON（公用 token）。"
      onClose={onClose}
      tools={
        tab === "local" ? (
          <button
            className="btn small"
            disabled={loading || importing}
            onClick={() => void scan()}
          >
            重新读取
          </button>
        ) : undefined
      }
      footer={
        tab === "local" ? (
          <>
            <button className="btn ghost" onClick={onClose} disabled={importing}>
              关闭
            </button>
            <button
              className="btn primary"
              disabled={importing || pending.length === 0}
              onClick={() =>
                void finishImport(
                  pending.map(toItem),
                  "没有需要导入的账号",
                  false
                )
              }
            >
              {importing ? "导入中…" : `全部导入（${pending.length}）`}
            </button>
          </>
        ) : tab === "json" ? (
          <>
            <button className="btn ghost" onClick={onClose} disabled={importing}>
              关闭
            </button>
            <button
              className="btn"
              disabled={accounts.length === 0 || importing}
              onClick={() => void exportJson()}
            >
              导出当前账号
            </button>
            <button
              className="btn primary"
              disabled={importing || parsed.items.length === 0}
              onClick={() =>
                void finishImport(parsed.items, "没有可导入的账号", false)
              }
            >
              {importing ? "导入中…" : `导入（${parsed.items.length}）`}
            </button>
          </>
        ) : phase === "done" && result?.token ? (
          <>
            <button className="btn ghost" onClick={onClose} disabled={importing}>
              关闭
            </button>
            <button className="btn ghost" onClick={reset} disabled={importing}>
              再登一个
            </button>
            <button
              className="btn primary"
              disabled={importing}
              onClick={() =>
                void finishImport(
                  [
                    {
                      token: result.token as string,
                      host: result.host,
                      name: result.nickname || result.phone,
                      phone: result.phone,
                      refresh_token: result.refresh_token,
                      expires_at: result.expires_at,
                    },
                  ],
                  "该账号已在列表中",
                  true
                )
              }
            >
              添加为账号
            </button>
          </>
        ) : waiting ? (
          <button className="btn" onClick={reset}>
            取消
          </button>
        ) : (
          <>
            <button className="btn ghost" onClick={onClose}>
              关闭
            </button>
            <button className="btn primary" onClick={() => void begin()}>
              打开授权页并开始
            </button>
          </>
        )
      }
    >
      <div className="seg">
        <button
          className={tab === "local" ? "active" : ""}
          disabled={locked}
          onClick={() => setTab("local")}
        >
          本机账号
        </button>
        <button
          className={tab === "oauth" ? "active" : ""}
          disabled={locked}
          onClick={() => setTab("oauth")}
        >
          浏览器登录
        </button>
        <button
          className={tab === "json" ? "active" : ""}
          disabled={locked}
          onClick={() => setTab("json")}
        >
          JSON
        </button>
      </div>

      <div className="add-body">
        {tab === "local" && (
          <>
            <p className="note">
              <IconInfo size={14} />
              <span>
                直接读取 WorkBuddy 写在本机的登录信息，不需要它正在运行。仅读取，不外传。
              </span>
            </p>

            {loading ? (
              <p className="empty">读取中…</p>
            ) : list.length === 0 ? (
              <p className="empty">
                未找到登录信息。请先在 WorkBuddy 桌面端登录一次。
              </p>
            ) : (
              <ul className="pick-list">
                {list.map((d) => {
                  const added = addedTokens.has(d.token);
                  return (
                    <li
                      key={d.file}
                      className={"pick-item static" + (added ? " locked" : "")}
                    >
                      <div className="pick-main">
                        <div className="pick-name">
                          {d.nickname || d.uid?.slice(0, 8) || "未命名账号"}
                          {d.phone && (
                            <span className="ac-phone">{maskPhone(d.phone)}</span>
                          )}
                          {d.is_current && (
                            <span className="badge badge-ok">当前登录</span>
                          )}
                          {added && <span className="badge badge-idle">已添加</span>}
                        </div>
                        <div className="ac-meta">
                          {d.host && <span className="tag">{d.host}</span>}
                        </div>
                      </div>
                      <span className="pick-tail">
                        <button
                          className="btn small"
                          disabled={added || importing}
                          onClick={() =>
                            void finishImport(
                              [toItem(d)],
                              "没有需要导入的账号",
                              false
                            )
                          }
                        >
                          {added ? "已添加" : "导入"}
                        </button>
                      </span>
                    </li>
                  );
                })}
              </ul>
            )}
          </>
        )}

        {tab === "oauth" && (
          <>
            <p className="note">
              <IconInfo size={14} />
              <span>
                在系统浏览器完成登录，本应用自动取得凭证；不重启 WorkBuddy，也不改动本机登录文件。
              </span>
            </p>

            {waiting && (
              <>
                <p className="note info">
                  <IconClock size={14} />
                  <span>
                    请在浏览器中完成登录（10 分钟内有效），完成后会自动继续。
                  </span>
                </p>
                {uri && (
                  <div className="oauth-uri">
                    <button
                      className="btn small"
                      onClick={() => void openExternal(uri)}
                    >
                      重新打开授权页
                    </button>
                  </div>
                )}
              </>
            )}

            {phase === "done" && result?.token && (
              <div className="pick-item static locked">
                <div className="pick-main">
                  <div className="pick-name">
                    {result.nickname || result.uid?.slice(0, 8) || "新账号"}
                    {result.phone && (
                      <span className="ac-phone">{maskPhone(result.phone)}</span>
                    )}
                  </div>
                  <div className="ac-meta">
                    {result.host && <span className="tag">{result.host}</span>}
                  </div>
                </div>
                <span className="pick-tail">
                  <span className="pick-state ok">授权成功</span>
                </span>
              </div>
            )}

            {phase === "error" && (
              <p className="note danger">
                <IconAlertTriangle size={14} />
                <span>授权失败：{oauthErr}</span>
              </p>
            )}
          </>
        )}

        {tab === "json" && (
          <>
            <p className="note">
              <IconInfo size={14} />
              <span>
                粘贴账号 JSON（顶层数组，或{" "}
                <code className="mono">{"{ accounts: [...] }"}</code>
                ）。这类账号是别处转来的<b>公用 token</b>：能签到、能走网关，但
                <b>不会自动续签</b>，也不会传进凭证池 —— 替共享票续签会把真正持有它
                的一方踢下线。已经在本机、且是正常登录过的同账号，再按 JSON
                导入会停掉它的自动续签，直到重新从本机 / 浏览器登录导入一次。
              </span>
            </p>

            <label>
              账号 JSON
              <textarea
                rows={9}
                spellCheck={false}
                value={jsonText}
                placeholder={
                  '{\n  "accounts": [\n    { "name": "示例账号", "token": "…" }\n  ]\n}'
                }
                onChange={(e) => setJsonText(e.target.value)}
              />
            </label>

            {parsed.error ? (
              <p className="note danger">
                <IconAlertTriangle size={14} />
                <span>{parsed.error}</span>
              </p>
            ) : parsed.items.length > 0 ? (
              <p className="note info">
                <IconInfo size={14} />
                <span>
                  解析到 {parsed.items.length} 个账号，导入后都会标记为公用 token
                  （不续签、不上传凭证池）。
                </span>
              </p>
            ) : null}
          </>
        )}
      </div>
    </Dialog>
  );
}
