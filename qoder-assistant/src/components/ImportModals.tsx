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
import { baseName, copyText, maskPhone, maskToken } from "../common";
import type { Toast } from "../common";
import { Dialog } from "./Dialog";
import {
  IconAlertTriangle,
  IconClock,
  IconCloud,
  IconFile,
  IconInfo,
  IconLink,
  IconUserPlus,
} from "./Icons";

/**
 * 账号导入的两条通道（都保留弹窗形态——它们是「做完即走」的任务流）：
 * - 导入本机账号：读 Qoder 写在本机的 auth/*.info；
 * - 登录新账号：官方 OAuth state 轮询，在系统浏览器完成登录。
 *
 * 原来的「导出账号 / 从文件导入」已拆掉：它把 token 与 refresh token 原样写进文件，
 * 于是几台机器各持一份 refresh token 的副本 —— 而官方续签是单链轮换，
 * 谁先签就把别人踢下线。凭据只留在本机这一份，不进任何共享通路。
 */

/**
 * 「导入本机账号」：直接读 Qoder 写在本机的登录信息文件（`auth/*.info`）。
 *
 * 这是最省事的一条路——不需要 Qoder 正在运行、不用改启动方式，
 * 而且一次就能拿到 token + 昵称 + 手机号（导入时自动带上手机号）。
 * 代价是它只能拿到**已经在本机登录过**的账号；要收新账号请用「登录新账号」。
 */
export function LocalAccountsModal({
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
  const [list, setList] = useState<LocalAccount[]>([]);
  const [loading, setLoading] = useState(true);
  const [importing, setImporting] = useState(false);

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
    name: d.nickname || d.phone,
    phone: d.phone,
    // 不带这三个字段的话，导入的账号永远无法自动续签（前两个换来新 token，
    // 第三个让界面能显示「续签链还能撑多久」）
    refresh_token: d.refresh_token,
    expires_at: d.expires_at,
    rt_expires_at: d.rt_expires_at,
  });

  const doImport = async (items: ImportItem[]) => {
    if (items.length === 0) return;
    setImporting(true);
    try {
      const { added, updated } = await onImport(items);
      if (added > 0 && updated > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号，更新 ${updated} 个已有账号的凭证` });
        onClose();
      } else if (added > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号` });
        onClose();
      } else if (updated > 0) {
        onToast({ kind: "ok", text: `已更新 ${updated} 个账号的凭证（补全续签信息）` });
        onClose();
      } else {
        onToast({ kind: "info", text: "没有需要导入的账号" });
      }
    } catch (e) {
      onToast({ kind: "err", text: "导入失败：" + String(e) });
    } finally {
      setImporting(false);
    }
  };

  return (
    <Dialog
      size="lg"
      icon={<IconFile size={16} />}
      title="导入本机账号"
      label="导入本机账号"
      onClose={onClose}
      tools={
        <button
          className="btn small"
          disabled={loading}
          onClick={() => void scan()}
        >
          重新读取
        </button>
      }
      footer={
        <>
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
          <button
            className="btn primary"
            disabled={importing || pending.length === 0}
            onClick={() => void doImport(pending.map(toItem))}
          >
            {importing ? "导入中…" : `全部导入（${pending.length}）`}
          </button>
        </>
      }
    >
      <p className="note">
        <IconInfo size={14} />
        <span>
          Qoder 登录后会把账号与凭证写到本机
          <code>CodeBuddyExtension/Data/Public/auth/*.info</code>，这里直接读取它 ——
          <b>不需要 Qoder 正在运行，也不用改启动方式</b>，而且能一次拿到昵称与手机号。
          仅读取、不外传。
        </span>
      </p>

      {loading ? (
        <p className="empty">读取中…</p>
      ) : list.length === 0 ? (
        <p className="empty">
          未找到登录信息文件。请先在 Qoder 桌面端登录一次（本工具只读，不会改动它）。
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
                    <code className="tok">{maskToken(d.token)}</code>
                    <span className="tag">{baseName(d.file)}</span>
                    {d.uid && <span className="tag">uid {d.uid}</span>}
                  </div>
                </div>
                <span className="pick-tail">
                  <button
                    className="btn small"
                    disabled={added || importing}
                    onClick={() => void doImport([toItem(d)])}
                  >
                    {added ? "已添加" : "导入"}
                  </button>
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </Dialog>
  );
}

/**
 * 「登录新账号」：官方设备授权流（无感登录）。
 *
 * 独立于「导入本机账号」——后者只能拿到**已经登录过**的账号，
 * 这条通道能主动把新账号签发进来，且不重启、不打断当前 Qoder、不改本机登录文件。
 *
 * **没有「选域」这一步**：Qoder 只有一套 Global 域，地址由后端 `qoder_api` 唯一决定。
 * 旧版这里有个下拉框，列的是 CodeBuddy 时代的四个域（`codebuddy.cn` / `codebuddy.ai` /
 * `qoder.cn` / `qoder.ai`），而那个参数在后端从来就被忽略 —— 一个不起作用的选项比没有更糟，
 * 它让人以为换个域就能解决登录问题。
 */
export function OAuthModal({
  onImport,
  onClose,
  onToast,
}: {
  onImport: (items: ImportItem[]) => Promise<ImportReport>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [importing, setImporting] = useState(false);
  const doImport = async (items: ImportItem[]) => {
    if (items.length === 0) return;
    setImporting(true);
    try {
      const { added, updated } = await onImport(items);
      if (added > 0 && updated > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号，更新 ${updated} 个已有账号的凭证` });
        onClose();
      } else if (added > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号` });
        onClose();
      } else if (updated > 0) {
        onToast({ kind: "ok", text: `已更新 ${updated} 个账号的凭证（补全续签信息）` });
        onClose();
      } else {
        onToast({ kind: "info", text: "该账号已在列表中" });
        onClose();
      }
    } catch (e) {
      onToast({ kind: "err", text: "导入失败：" + String(e) });
    } finally {
      setImporting(false);
    }
  };

  // 弹窗外壳由 OAuthPanel 自己渲染：底部按钮要随授权阶段（等待中 / 已授权 / 失败）
  // 变化，而阶段状态就在面板里。为了把按钮塞进 footer 而把整套轮询状态提上来，
  // 只会让这个组件变成两个都想管状态的容器。
  return (
    <OAuthPanel
      importing={importing}
      onImport={doImport}
      onToast={onToast}
      onClose={onClose}
    />
  );
}

/**
 * 「登录新账号」面板：走官方设备授权流，在系统浏览器里完成一次登录。
 *
 * 轮询期服务端回 404 是**正常等待态**（用户还没点完），不是错误。
 */
function OAuthPanel({
  importing,
  onImport,
  onToast,
  onClose,
}: {
  importing: boolean;
  onImport: (items: ImportItem[]) => Promise<void>;
  onToast: (t: Toast) => void;
  onClose: () => void;
}) {
  const [phase, setPhase] = useState<"idle" | "waiting" | "done" | "error">("idle");
  const [uri, setUri] = useState("");
  const [result, setResult] = useState<OAuthPoll | null>(null);
  const [err, setErr] = useState("");
  const [waited, setWaited] = useState(0);

  const timer = useRef<number | null>(null);
  const busy = useRef(false);

  const stop = useCallback(() => {
    if (timer.current !== null) {
      window.clearInterval(timer.current);
      timer.current = null;
    }
    busy.current = false;
  }, []);
  useEffect(() => stop, [stop]);

  const begin = async () => {
    stop();
    setResult(null);
    setErr("");
    setWaited(0);
    setUri("");
    setPhase("waiting");
    try {
      const s = await oauthStart();
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
        setWaited(Math.round(elapsed / 1000));
        if (elapsed > limitMs) {
          stop();
          setErr("登录超时，请重新发起");
          setPhase("error");
          return;
        }
        // 上一次轮询还没回来就跳过这一拍，避免请求叠加
        if (busy.current) return;
        busy.current = true;
        void (async () => {
          try {
            const r = await oauthPoll(s.login_id);
            if (!r.done) return;
            stop();
            if (r.error || !r.token) {
              setErr(r.error ?? "授权完成但未返回 token");
              setPhase("error");
            } else {
              setResult(r);
              setPhase("done");
            }
          } catch (e) {
            stop();
            setErr(String(e));
            setPhase("error");
          } finally {
            busy.current = false;
          }
        })();
      }, 2000);
    } catch (e) {
      setErr(String(e));
      setPhase("error");
    }
  };

  const reset = () => {
    stop();
    setPhase("idle");
    setUri("");
    setResult(null);
    setErr("");
    setWaited(0);
  };

  return (
    <Dialog
      size="lg"
      icon={<IconUserPlus size={16} />}
      title="登录新账号"
      label="登录新账号"
      onClose={onClose}
      footer={
        phase === "done" && result?.token ? (
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
                void onImport([
                  {
                    token: result.token as string,
                    name: result.nickname || result.phone,
                    phone: result.phone,
                    refresh_token: result.refresh_token,
                    expires_at: result.expires_at,
                    rt_expires_at: result.rt_expires_at,
                  },
                ])
              }
            >
              添加为账号
            </button>
          </>
        ) : phase === "waiting" ? (
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
      <p className="note">
        <IconInfo size={14} />
        <span>
          走官方<b>设备授权流</b>：在<b>系统浏览器</b>里完成一次登录（扫码即可），
          本工具轮询取得该账号的凭证 ——
          <b>不重启、不打断当前 Qoder，也不改动本机登录文件</b>。
          适合把第二个 / 第三个账号收进来。
        </span>
      </p>

      {phase === "waiting" && (
        <>
          <p className="note info">
            <IconClock size={14} />
            <span>
              请在弹出的浏览器窗口中完成登录 / 扫码…… 已等待 {waited}s（10 分钟内有效）。
              完成后这个窗口会自己跳到下一步，不用你回来点任何东西。
            </span>
          </p>
          {uri && (
            <div className="oauth-uri">
              <code>{uri}</code>
              <button
                className="btn small"
                onClick={() => void openExternal(uri)}
              >
                重新打开
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
              <code className="tok">{maskToken(result.token)}</code>
              {result.uid && <span className="tag">uid {result.uid}</span>}
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
          <span>授权失败：{err}</span>
        </p>
      )}
    </Dialog>
  );
}

/**
 * 「绑定云端凭证池」：把**这台机器**接到别处那一池上。
 *
 * 跨机器共用走凭证池，不走「导出凭证文件」—— 那条路会把 refresh token 原样写进文件，
 * 于是几台机器各持一份副本，而官方续签是单链轮换：谁先签就把别人踢下线。
 *
 * ⚠️ 粒度是**整台机器**，不是单个账号：所以这个弹窗不收 `Account`。
 */
export function BrokerBindModal({
  onBind,
  onClose,
}: {
  onBind: (uuid: string) => Promise<void>;
  onClose: () => void;
}) {
  const [uuid, setUuid] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const submit = async () => {
    const v = uuid.trim();
    if (!v) {
      setErr("请先粘贴 uuid");
      return;
    }
    setBusy(true);
    setErr("");
    try {
      await onBind(v);
      onClose();
    } catch (e) {
      // 失败就留在弹窗里显示原因：绑定这一步最常见的错法就是 uuid 抄漏了几位，
      // 关掉弹窗再让用户从别的地方找回来，等于把错误藏了
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      onClose={onClose}
      size="md"
      icon={<IconLink size={18} />}
      title="绑定云端凭证池"
      subtitle="把在另一台机器上传的那一池接到本机"
      footer={
        <>
          <button className="btn ghost" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button
            className="btn primary"
            onClick={() => void submit()}
            disabled={busy}
          >
            {busy ? "绑定中…" : "绑定"}
          </button>
        </>
      }
    >
      <label className="set-field">
        池 uuid
        <input
          value={uuid}
          autoFocus
          spellCheck={false}
          placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
          onChange={(e) => {
            setUuid(e.target.value);
            setErr("");
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") void submit();
          }}
        />
      </label>
      <p className="note">
        <IconInfo size={14} />
        <span>
          绑定后本机账号与那一池「取并集」：池里的并进来，本机独有的一个都不会删，
          下一轮同步时再一起推上去。
        </span>
      </p>
      {err && (
        <p className="note danger">
          <IconAlertTriangle size={14} />
          <span>{err}</span>
        </p>
      )}
    </Dialog>
  );
}

/**
 * 上传成功后摊开那串 uuid。
 *
 * **用弹窗而不是 toast**：uuid 是唯一需要被「搬到别的机器」上去的东西 ——
 * 一条 3 秒就消失的提示等于没给。这里还要能一键复制，并把「它等于密码」
 * 这件事当面说清楚，而不是塞进一句会自动消失的提示里。
 */
export function PoolIssuedModal({
  uuid,
  message,
  onClose,
  onToast,
}: {
  uuid: string;
  message: string;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    const ok = await copyText(uuid);
    setCopied(ok);
    onToast(
      ok
        ? { kind: "ok", text: "uuid 已复制到剪贴板" }
        : { kind: "err", text: "复制失败，请手动选中下面这串" }
    );
  };

  return (
    <Dialog
      onClose={onClose}
      size="md"
      tone="ok"
      icon={<IconCloud size={18} />}
      title="这一池的 uuid"
      subtitle="复制到其他机器的「绑定云端凭证池」里"
      tools={
        <button className="btn small" onClick={() => void copy()}>
          {copied ? "已复制" : "复制 uuid"}
        </button>
      }
      footer={
        <button className="btn primary" onClick={onClose}>
          我已保存好
        </button>
      }
    >
      <p className="note">
        <IconInfo size={14} />
        <span>{message}</span>
      </p>
      {/* 点一下即全选：即使剪贴板两条路都不通，用户也能手动 Ctrl/Cmd+C */}
      <code className="pool-uuid">{uuid}</code>
      <p className="note warn">
        <IconAlertTriangle size={14} />
        <span>
          拿到这串 uuid 就等于拿到这一池账号的完整权限，所以请把它当密码保存 ——
          别发进聊天群、别截图外传。
        </span>
      </p>
    </Dialog>
  );
}
