import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
  type MutableRefObject,
} from "react";
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
import { accountIdent, baseName, copyText, maskPhone, maskToken } from "../common";
import type { Toast } from "../common";
import { Dialog } from "./Dialog";
import {
  IconAlertTriangle,
  IconClock,
  IconCloud,
  IconInfo,
  IconLink,
  IconUserPlus,
} from "./Icons";

/**
 * 「添加账号」弹窗：两条导入通道收进同一张弹窗，用分段选项卡切换（对齐 trae 助手）：
 * - 本机账号：读 Qoder 写在本机的 `auth.v1.dat`（Chromium safeStorage 加密）；
 * - 浏览器登录：官方设备授权轮询，在系统浏览器完成登录。
 *
 * 页头因此只剩一个「添加账号」入口；两条通道的说明与能力原样保留。
 * 原来的「导出账号 / 从文件导入」已拆掉：它把 token 与 refresh token 原样写进文件，
 * 于是几台机器各持一份 refresh token 的副本 —— 而官方续签是单链轮换，
 * 谁先签就把别人踢下线。凭据只留在本机这一份，不进任何共享通路。
 */

/**
 * 本机账号选项卡的内容：直接读 Qoder 写在本机的凭据文件
 * （`auth.v1.dat`，Chromium safeStorage 加密）。
 *
 * 这是最省事的一条路——不需要 Qoder 正在运行、不用改启动方式，
 * 而且一次就能拿到 token + 昵称 + 手机号 / 邮箱（导入时一并带上）。
 * 代价是它只能拿到**已经在本机登录过**的账号；要收新账号请切「浏览器登录」。
 *
 * 只扫国内版那一套目录（`com.qodercn.app.stable`）；「读到了什么 / 为什么没读到」
 * 由后端写成一句人话一起带回来（[`LocalScan`] 的 `note`），界面原样摆出来。
 */
function LocalTab({
  accounts,
  importing,
  setImporting,
  onImport,
  onPendingChange,
  ctl,
}: {
  accounts: Account[];
  importing: boolean;
  setImporting: (v: boolean) => void;
  /** 导入成功后由弹窗层关弹窗、弹 toast */
  onImport: (items: ImportItem[]) => Promise<void>;
  /** 待导入数上抛：footer 的「全部导入（N）」要用 */
  onPendingChange: (n: number) => void;
  /** 「重新读取 / 全部导入」按钮在弹窗 footer 上，动作经这个句柄回传 */
  ctl: MutableRefObject<{ rescan: () => void; importAll: () => void } | null>;
}) {
  const [list, setList] = useState<LocalAccount[]>([]);
  // 「读到了什么 / 为什么没读到」：后端把原因写成一句人话（见 LocalScan.note）。
  // 不写这句，用户最容易猜错的结论就是「我是不是没登录」。
  const [note, setNote] = useState("");
  const [loading, setLoading] = useState(true);

  const scan = useCallback(async () => {
    setLoading(true);
    try {
      // `?.` 与 `?? []` 都不是多余的：这个返回值会直接喂给下面的 `list.filter`，
      // 一旦 IPC 回了 null（命令名改了 / 后端没注册），崩的是整个 React 树 ——
      // 表现成「打开导入弹窗，整个应用白屏」，而不是「这个弹窗里没有账号」。
      const result = await discoverLocalAccounts();
      setList(result?.accounts ?? []);
      setNote(result?.note ?? "");
    } catch {
      setList([]);
      setNote("");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void scan();
  }, [scan]);

  // 「已添加」的判据是 token：本机文件里同一串 token 就是同一条凭据。
  // 这里故意不做手机号 / 邮箱匹配（后端合并会认）：本机文件里的 token 一旦轮换过，
  // 那条正是需要导进来刷新凭证的，判成「已添加」反而把该做的事挡在了门外。
  const addedKeys = useMemo(
    () => new Set(accounts.map((a) => a.token)),
    [accounts]
  );
  const isAdded = (d: LocalAccount) => addedKeys.has(d.token);

  // 已存在的账号会被跳过而不是重复添加
  const pending = list.filter((d) => !isAdded(d));

  useEffect(() => {
    onPendingChange(pending.length);
  }, [pending.length, onPendingChange]);

  useImperativeHandle(ctl, () => ({
    rescan: () => void scan(),
    importAll: () => {
      void (async () => {
        if (pending.length === 0) return;
        setImporting(true);
        try {
          await onImport(pending.map(toItem));
        } finally {
          setImporting(false);
        }
      })();
    },
  }));

  const toItem = (d: LocalAccount): ImportItem => ({
    token: d.token,
    name: d.nickname || d.phone,
    phone: d.phone,
    // 邮箱与手机号同为识别键，带着它导入才能
    // 让「token 已轮换、手机号/邮箱没变」的账号并到同一条上，而不是新增一条
    email: d.email,
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
      await onImport(items);
    } finally {
      setImporting(false);
    }
  };

  return (
    <>
      <p className="note">
        <IconInfo size={14} />
        <span>读取本机已登录的 Qoder 账号，不会改动登录文件。</span>
      </p>

      {/* 读取情况一行（读到 / 没读到都非空）。「没读到」的原因必须写在脸上，
          否则用户只能去猜，而最容易猜错的结论就是「我是不是没登录」。 */}
      {!loading && note && <p className="modal-meta">{note}</p>}

      {loading ? (
        <p className="empty">读取中…</p>
      ) : list.length === 0 ? (
        <p className="empty">本机没有可导入的账号。</p>
      ) : (
        <ul className="pick-list">
          {list.map((d) => {
            const added = isAdded(d);
            const ident = accountIdent(d.phone, d.email);
            return (
              <li
                key={d.file}
                className={"pick-item static" + (added ? " locked" : "")}
              >
                <div className="pick-main">
                  <div className="pick-name">
                    {d.nickname || d.uid?.slice(0, 8) || "未命名账号"}
                    {ident && (
                      <span className="ac-ident">{maskPhone(ident)}</span>
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
    </>
  );
}

/**
 * 浏览器登录选项卡的内容：走官方设备授权流（无感登录）。
 *
 * 独立于「本机账号」——后者只能拿到**已经登录过**的账号，
 * 这条通道能主动把新账号签发进来，且不重启、不打断当前 Qoder、不改本机登录文件。
 *
 * 轮询期服务端回 404 是**正常等待态**（用户还没点完），不是错误。
 * 授权状态（phase / result）由弹窗层持有，footer 按钮要随阶段变化。
 */
function BrowserTab({
  phase,
  uri,
  result,
  err,
  waited,
}: {
  phase: "idle" | "waiting" | "done" | "error";
  uri: string;
  result: OAuthPoll | null;
  err: string;
  waited: number;
}) {
  // 授权成功那条预览的展示标识（手机号 / 邮箱）
  const doneIdent = result ? accountIdent(result.phone, result.email) : null;

  return (
    <>
      <p className="note">
        <IconInfo size={14} />
        <span>用系统浏览器扫码登录，完成后会自动添加账号。</span>
      </p>

      {phase === "waiting" && (
        <>
          <p className="note info">
            <IconClock size={14} />
            <span>
              请在浏览器中完成登录（10 分钟内有效），完成后会自动进入下一步。已等待 {waited}s。
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
              {doneIdent && (
                <span className="ac-ident">{maskPhone(doneIdent)}</span>
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
    </>
  );
}

/**
 * 「添加账号」：两条通道共用的一张弹窗。
 *
 * 「本机账号」选项卡打开即扫描本机登录态；「浏览器登录」选项卡走设备授权流。
 * 两条通道的导入动作都汇到同一个 `runImport`：成功 toast 并关弹窗，失败留在弹窗里。
 */
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
  const [tab, setTab] = useState<"local" | "browser">("local");

  // —— 本机账号选项卡 ——
  const [importing, setImporting] = useState(false);
  // 待导入数在 LocalTab 里算（判据是 token），这里只为 footer 的「全部导入」存一份
  const [pendingCount, setPendingCount] = useState(0);
  /** footer 上「重新读取 / 全部导入」的动作句柄，由 LocalTab 填充 */
  const localCtl = useRef<{ rescan: () => void; importAll: () => void } | null>(null);

  // —— 浏览器登录选项卡 ——
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
  // 弹窗卸载（关闭）时必须停掉轮询：否则循环还在跑，
  // 拿到的 token 会写进一个已经卸载的组件状态里（静默丢账号）
  useEffect(() => stop, [stop]);

  /** 两条通道共用的导入出口：成功 toast 并关弹窗，失败 toast 留在弹窗里 */
  const runImport = async (items: ImportItem[]) => {
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
      }
    } catch (e) {
      onToast({ kind: "err", text: "导入失败：" + String(e) });
    } finally {
      setImporting(false);
    }
  };

  /** 申请授权 → 打开系统浏览器 → 轮询回调 → 进入 done 态等用户确认 */
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

  const resetOAuth = () => {
    stop();
    setPhase("idle");
    setUri("");
    setResult(null);
    setErr("");
    setWaited(0);
  };

  const switchTab = (k: "local" | "browser") => {
    setTab(k);
    setErr("");
  };

  const browserBusy = phase === "waiting";

  return (
    <Dialog
      size="lg"
      icon={<IconUserPlus size={16} />}
      title="添加账号"
      label="添加账号"
      fixedBodyHeight={320}
      onClose={onClose}
      tools={
        tab === "local" ? (
          <button
            className="btn small"
            disabled={importing}
            onClick={() => localCtl.current?.rescan()}
          >
            重新读取
          </button>
        ) : null
      }
      footer={
        tab === "local" ? (
          <>
            <button className="btn ghost" onClick={onClose} disabled={importing}>
              关闭
            </button>
            <button
              className="btn primary"
              disabled={importing || pendingCount === 0}
              onClick={() => localCtl.current?.importAll()}
            >
              {importing ? "导入中…" : `全部导入（${pendingCount}）`}
            </button>
          </>
        ) : phase === "done" && result?.token ? (
          <>
            <button className="btn ghost" onClick={onClose} disabled={importing}>
              关闭
            </button>
            <button className="btn ghost" onClick={resetOAuth} disabled={importing}>
              再登一个
            </button>
            <button
              className="btn primary"
              disabled={importing}
              onClick={() =>
                void runImport([
                  {
                    token: result.token as string,
                    name: result.nickname || result.phone,
                    phone: result.phone,
                    // 邮箱与手机号同为识别键；登录接口一并返回，带上它导入才能
                    // 让轮换过 token 的同一个人并到既有账号上（见 merge_import）
                    email: result.email,
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
          <button className="btn" onClick={resetOAuth}>
            取消
          </button>
        ) : (
          <>
            <button className="btn ghost" onClick={onClose}>
              关闭
            </button>
            <button
              className="btn primary"
              onClick={() => void begin()}
            >
              打开授权页并开始
            </button>
          </>
        )
      }
    >
      <div className="seg">
        {(
          [
            ["local", "本机账号"],
            ["browser", "浏览器登录"],
          ] as ["local" | "browser", string][]
        ).map(([k, label]) => (
          <button
            key={k}
            className={tab === k ? "active" : ""}
            disabled={browserBusy}
            onClick={() => switchTab(k)}
          >
            {label}
          </button>
        ))}
      </div>

      {tab === "local" && (
        <LocalTab
          accounts={accounts}
          importing={importing}
          setImporting={setImporting}
          onImport={runImport}
          onPendingChange={setPendingCount}
          ctl={localCtl}
        />
      )}
      {tab === "browser" && (
        <BrowserTab
          phase={phase}
          uri={uri}
          result={result}
          err={err}
          waited={waited}
        />
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
        <span>绑定后与云端账号合并，本机独有的不会删除。</span>
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
        <span>这串 uuid 等于这一池账号的完整权限，请当密码保存，勿外传。</span>
      </p>
    </Dialog>
  );
}
