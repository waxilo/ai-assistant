import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  Account,
  ImportItem,
  ImportReport,
  LocalAccount,
  LocalProbe,
  OAuthPoll,
} from "../types";
import {
  discoverLocalAccounts,
  oauthPoll,
  oauthStart,
  openExternal,
} from "../api";
import { accountIdent, baseName, copyText, maskPhone, maskToken } from "../common";
import { regionLabel, useRegions } from "../regions";
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
 * - 导入本机账号：读 Qoder 写在本机的 `auth.v1.dat`（Chromium safeStorage 加密）；
 * - 登录新账号：官方 OAuth state 轮询，在系统浏览器完成登录。
 *
 * 原来的「导出账号 / 从文件导入」已拆掉：它把 token 与 refresh token 原样写进文件，
 * 于是几台机器各持一份 refresh token 的副本 —— 而官方续签是单链轮换，
 * 谁先签就把别人踢下线。凭据只留在本机这一份，不进任何共享通路。
 */

/**
 * 「导入本机账号」：直接读 Qoder 写在本机的凭据文件（`auth.v1.dat`，Chromium safeStorage 加密）。
 *
 * 这是最省事的一条路——不需要 Qoder 正在运行、不用改启动方式，
 * 而且一次就能拿到 token + 昵称 + 手机号 / 邮箱（导入时一并带上）。
 * 代价是它只能拿到**已经在本机登录过**的账号；要收新账号请用「登录新账号」。
 *
 * 两套部署的目录都会扫（`com.qoder.app.stable` / `com.qodercn.app.stable`），
 * 所以每条结果都带着「来自哪个区域」—— 同一条凭据在两边是完全不同的账号。
 */
export function LocalAccountsModal({
  accounts,
  region,
  onImport,
  onClose,
  onToast,
}: {
  accounts: Account[];
  /** 当前区域（左下角全局选择器）；null = 还没读到 settings（首屏那一瞬），此时展示全部 */
  region: string | null;
  onImport: (items: ImportItem[]) => Promise<ImportReport>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [list, setList] = useState<LocalAccount[]>([]);
  const [probes, setProbes] = useState<LocalProbe[]>([]);
  const [loading, setLoading] = useState(true);
  const [importing, setImporting] = useState(false);
  // 区域清单：后端给的「国际版 / 国内版」中文名，列表里那个标签要用
  const regionOpts = useRegions();

  const scan = useCallback(async () => {
    setLoading(true);
    try {
      // `?.` 与 `?? []` 都不是多余的：这个返回值会直接喂给下面的 `list.filter`，
      // 一旦 IPC 回了 null（命令名改了 / 后端没注册），崩的是整个 React 树 ——
      // 表现成「打开导入弹窗，整个应用白屏」，而不是「这个弹窗里没有账号」。
      const result = await discoverLocalAccounts();
      setList(result?.accounts ?? []);
      setProbes(result?.probes ?? []);
    } catch {
      setList([]);
      setProbes([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void scan();
  }, [scan]);

  // 「已添加」的判据是**区域 + token**：只看 token 会在极端情况下把另一个区域里的
  // 凭据也算成「已添加」—— 而两套部署签发的 token 本来就互不相通，区域是这条凭据的一半身份。
  // 这里故意不做手机号 / 邮箱匹配（后端合并会认）：本机文件里的 token 一旦轮换过，
  // 那条正是需要导进来刷新凭证的，判成「已添加」反而把该做的事挡在了门外。
  const addedKeys = useMemo(
    () => new Set(accounts.map((a) => `${a.region}\n${a.token}`)),
    [accounts]
  );
  const isAdded = (d: LocalAccount) =>
    addedKeys.has(`${d.region}\n${d.token}`);

  // 数据隔离：只展示当前区域（左下角全局选择器）的本机账号。两套部署的凭据互不相通，
  // 后端确实扫了全部目录（`discover_local_accounts` 一次给全量），但在这里**不混**，
  // 只把当前区域那几条列出来、也只导入它们 —— 另一区域切过去再看。
  // region 为 null（settings 未就绪）时退回展示全部，避免弹窗白屏。
  const visible = region ? list.filter((d) => d.region === region) : list;
  const probesShown = region
    ? probes.filter((p) => p.region === region)
    : probes;
  // 已存在的账号会被跳过而不是重复添加
  const pending = visible.filter((d) => !isAdded(d));

  const toItem = (d: LocalAccount): ImportItem => ({
    // 区域必须带上：登录文件本身不写区域，而下游每个请求都要靠它选域
    region: d.region,
    token: d.token,
    name: d.nickname || d.phone,
    phone: d.phone,
    // 邮箱与手机号同为识别键（国内版看手机号、国际版看邮箱），带着它导入才能
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
          Qoder 登录后会把账号与凭证写到本机的 <code>auth.v1.dat</code>（Chromium safeStorage
          加密；macOS 的密钥在系统钥匙串里，首次读取若弹出授权，点「始终允许」以后就不再问）。
          这里直接读取它，<b>只扫「当前区域」那个目录</b>（跟随左下角的区域选择器）；
          <b>不需要 Qoder 正在运行，也不用改启动方式</b>，而且能一次拿到昵称与手机号 / 邮箱。
          仅读取、不外传。另一区域的账号切到左下角再看。
        </span>
      </p>

      {/* 当前区域的读取情况：一行。这一段是该弹窗里最该被看见的东西 ——
          「没读到」的原因必须写在脸上，否则用户只能去猜，
          而最容易猜错的结论就是「我是不是没登录」。 */}
      {!loading && probesShown.length > 0 && (
        <ul className="probe-list">
          {probesShown.map((p) => (
            <li
              key={p.region}
              className={"probe-item" + (p.found ? " ok" : "")}
            >
              <span className={"badge " + (p.found ? "badge-ok" : "badge-idle")}>
                {regionLabel(regionOpts, p.region) ?? p.region}
              </span>
              <span className="probe-detail">{p.detail}</span>
            </li>
          ))}
        </ul>
      )}

      {loading ? (
        <p className="empty">读取中…</p>
      ) : visible.length === 0 ? (
        <p className="empty">
          当前区域没有可导入的账号 —— 具体原因见上面那一行
          （本工具只读，不会改动 Qoder 的登录文件）。
        </p>
      ) : (
        <ul className="pick-list">
          {visible.map((d) => {
            const added = isAdded(d);
            const rg = regionLabel(regionOpts, d.region);
            // 展示标识按区域选（国内版手机号 / 国际版邮箱）：与导入后账号页看到的是同一个
            const ident = accountIdent(d.region, d.phone, d.email);
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
                    {/* 这条凭据来自哪套部署：两个目录都会扫到，不标出来就分不清 */}
                    {rg && <span className="ac-region">{rg}</span>}
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
 * **跟随当前区域**：国际版与国内版是两套**互不相通**的部署（账号 / 积分 / 活动各自独立），
 * 而授权链接本身**不含**区域信息 —— 只有发起方知道用户点的是哪个入口，
 * 所以不是让弹窗里再选一遍，而是直接取左下角那个全局区域选择器的值（见 `oauth_start` 的 `region`）。
 * 要登到另一个区域，先在左下角切过去再打开这里。
 */
export function OAuthModal({
  region,
  onImport,
  onClose,
  onToast,
}: {
  /** 当前区域（左下角全局选择器）；null = 还没读到 settings，此时取清单第一个兜底 */
  region: string | null;
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
      defaultRegion={region}
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
  defaultRegion,
  importing,
  onImport,
  onToast,
  onClose,
}: {
  /** 当前区域（左下角全局选择器）；null = 还没读到 settings，此时取清单第一个兜底 */
  defaultRegion: string | null;
  importing: boolean;
  onImport: (items: ImportItem[]) => Promise<void>;
  onToast: (t: Toast) => void;
  onClose: () => void;
}) {
  const [phase, setPhase] = useState<"idle" | "waiting" | "done" | "error">("idle");
  // 这一轮授权**实际**用的区域（后端会话里记的那个）：导入时以它为准，
  // 而不是以界面此刻的显示为准 —— 那才是这次授权的真实身份。
  const [sessionRegion, setSessionRegion] = useState("");
  const [uri, setUri] = useState("");
  const [result, setResult] = useState<OAuthPoll | null>(null);
  const [err, setErr] = useState("");
  const [waited, setWaited] = useState(0);

  // 直接跟随左下角全局区域选择器：不再让弹窗里自己挑一遍区域（数据隔离）。
  // 兜底只取清单第一个，用于 settings 还没读到的首帧那一瞬。
  const regionOpts = useRegions();
  const region = defaultRegion ?? regionOpts[0]?.key ?? "";
  const selLabel = regionLabel(regionOpts, region);
  // 授权成功那条预览的展示标识（国内版手机号 / 国际版邮箱）：按本轮**实际**授权的区域选
  const doneIdent = result
    ? accountIdent(sessionRegion || region, result.phone, result.email)
    : null;

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
      const s = await oauthStart(region);
      setSessionRegion(s.region);
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
                    region: sessionRegion || region,
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
          <button className="btn" onClick={reset}>
            取消
          </button>
        ) : (
          <>
            <button className="btn ghost" onClick={onClose}>
              关闭
            </button>
            {/* 区域清单还没到货时不能开始：那时 region 是空串，
                后端会把「认不出的区域标识」当成一次失败而不是默认放行 */}
            <button
              className="btn primary"
              disabled={!region}
              onClick={() => void begin()}
            >
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

      {phase === "idle" && (
        <>
          <p className="modal-meta">
            将登入<b>{selLabel ?? "当前区域"}</b> —— 跟随左下角的区域选择器。
            要登到另一个区域，请先在左下角切过去，再打开这里。
          </p>
          <p className="note">
            <IconInfo size={14} />
            <span>
              两个版本是<b>两套互不相通的部署</b>：账号、积分、签到活动各自独立，
              所以新账号会收进<b>当前区域</b>，不会混到另一个版本去，
              <b>也不影响本机已经登录的那个客户端</b>。
            </span>
          </p>
        </>
      )}

      {phase === "waiting" && (
        <>
          <p className="note info">
            <IconClock size={14} />
            <span>
              正在登入<b>{selLabel ?? "所选区域"}</b>。
              请在弹出的浏览器窗口中完成登录 / 扫码…… 已等待 {waited}s（10 分钟内有效）。
              完成后这个窗口会自己跳到下一步，不用你回来点任何东西。
              浏览器那一页<b>不会</b>唤起 Qoder 客户端，跑完直接关掉即可。
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
              {regionLabel(regionOpts, sessionRegion) && (
                <span className="ac-region">
                  {regionLabel(regionOpts, sessionRegion)}
                </span>
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
