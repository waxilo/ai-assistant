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
import { maskPhone } from "../common";
import type { Toast } from "../common";
import { Dialog } from "./Dialog";
import {
  IconAlertTriangle,
  IconClock,
  IconInfo,
  IconUserPlus,
} from "./Icons";

/**
 * 「添加账号」弹窗：把「导入本机账号 / 登录新账号」合成一个入口，
 * 两条通道用分段控件切换（参考 trae）。
 *
 * - 本机账号：读 WorkBuddy 写在本机的 auth/*.info，不需要它正在运行；
 * - 浏览器登录：官方 OAuth state 轮询，在系统浏览器完成登录。
 *
 * 凭据只留在本机这一份：不做「导出账户 / 从文件导入」，否则多台机器各持一份
 * refresh token 副本，而官方续签是单链轮换，谁先签就把别人踢下线。
 */

const DEFAULT_HOST = "https://www.workbuddy.cn";

type AddTab = "local" | "oauth";

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

  const waiting = phase === "waiting";
  const locked = importing || waiting;

  return (
    <Dialog
      size="lg"
      icon={<IconUserPlus size={16} />}
      title="添加账号"
      label="添加账号"
      subtitle="导入本机已登录的账号，或用浏览器登录新账号。"
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
      </div>
    </Dialog>
  );
}
