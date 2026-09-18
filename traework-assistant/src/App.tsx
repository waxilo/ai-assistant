import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import {
  listAccounts,
  checkinStatus,
  checkinAll,
  checkinOne,
  removeAccount,
  getSettings,
  saveSettings,
  refreshAccountProfiles,
  brokerState,
  brokerUpload,
  brokerLink,
  brokerUnbind,
  BRIEFING_SEALED_EVENT,
} from "./api";
import type { AcctStatus, Account, BrokerStatus, Page, Settings } from "./types";
import { accountLabel, type ConfirmReq, type Toast } from "./common";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { BrokerBindModal, PoolIssuedModal } from "./components/BrokerModals";
import {
  IconCheck,
  IconUsers,
  IconSwap,
  IconList,
  IconGear,
  IconUserPlus,
  IconRefresh,
  IconClock,
  IconActivity,
} from "./components/Icons";
import AccountsPage from "./pages/AccountsPage";
import TakeoverPage from "./pages/TakeoverPage";
import BriefingPage from "./pages/BriefingPage";
import LogsPage from "./pages/LogsPage";
import SettingsPage from "./pages/SettingsPage";
import AddAccountModal from "./pages/AddAccountModal";
import {
  checkAndInstall,
  downloadProgress,
  formatBytes,
  nextUpdateNotice,
  probeUpdate,
  type UpdateNotice,
  type UpdateProgress,
} from "./updater";

/**
 * 应用外壳：左侧导航 + 悬浮玻璃页头 + 内容区。
 *
 * ## 为什么「切 tab 卡」——以及这里怎么解决
 *
 * 早期实现把 4 个页面**全部常驻挂载**、只用 CSS 切可见性。那有四个叠加的代价：
 * 1. 每次点导航都会 `setPage` → `App` 重渲染 → **4 个页面全部重渲染**（表格、日志列表
 *    逐行 reconcile，哪怕它们是 `display:none`）；
 * 2. 隐藏页面里的定时器仍在跑（接管页轮询一次 IPC）；
 * 3. `display:none → block` 会让浏览器重新 layout 整棵子树；
 * 4. 页面切换动画用了 `transform`，给一个很大的子树建了合成层。
 *
 * 现在：**只渲染当前页**（其余页面根本不在 DOM 里，没有 reconcile、没有轮询、
 * 没有 layout），并把**跨页共享的数据上提到这里**（`accounts` / `statuses` /
 * `statusText`）—— 这样重新挂载一个页面时不会重新拉数据，「切回来」是零成本的。
 * 页面组件都用 `React.memo` 包了：toast 之类的外层状态变化不会再带着整页一起重渲染。
 *
 * ⚠️ 前端只能解决「重渲染 / 布局」这一半。真正把窗口卡住的是**后端**那条同步命令：
 * 切到接管页会调 `takeover_status`，而它跑在主线程上、内部还会 spawn `tasklist`
 * （见 `src-tauri/src/commands.rs` 与 `proc.rs` 的说明）。这一半在 Rust 侧修掉了。
 *
 * ## 页头为什么提到 App
 *
 * 页头承载「这一页是什么 + 这一页现在什么状态 + 这一页能做什么」。它与页面内容是
 * **同一层语义**，但页面主体是「列表/表单」，页头是「工具条」—— 放在页面内部会让
 * 每个页面各写一套标题样式与按钮排布（旧版就是这样：账号页把四个按钮塞进卡片标题行，
 * 接管页又没有标题）。所以动作的实现留在 App，页面只负责渲染主体。
 */
const NAV: { key: Page; label: string; Icon: (p: { size?: number }) => ReactNode }[] = [
  { key: "accounts", label: "账号与签到", Icon: IconUsers },
  { key: "takeover", label: "智能接管", Icon: IconSwap },
  { key: "briefing", label: "积分简报", Icon: IconActivity },
  { key: "logs", label: "签到日志", Icon: IconList },
  { key: "settings", label: "设置", Icon: IconGear },
];

const PAGE_TITLES: Record<Page, string> = {
  accounts: "账号与签到",
  takeover: "智能接管",
  briefing: "积分简报",
  logs: "签到日志",
  settings: "设置",
};

/** 页头图标与侧栏图标同源 —— 两处各写一份必然分叉 */
const PAGE_ICON: Record<Page, (p: { size?: number }) => ReactNode> = {
  accounts: IconUsers,
  takeover: IconSwap,
  briefing: IconActivity,
  logs: IconList,
  settings: IconGear,
};

// 后台轮询新版本：启动延迟 + 间隔。只为点亮侧栏那颗小红点，不求实时，间隔放宽到 6 小时。
// 首次延迟 8s，让首屏先渲染完再悄悄去问 GitHub。
const UPDATE_FIRST_DELAY_MS = 8_000;
const UPDATE_INTERVAL_MS = 6 * 60 * 60 * 1000;

export default function App() {
  const [page, setPage] = useState<Page>("accounts");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [version, setVersion] = useState("");
  /** 后台轮询发现的新版本（驱动侧边栏「设置」上的小红点） */
  const [updateNotice, setUpdateNotice] = useState<UpdateNotice>(null);
  /**
   * 应用更新：**全局任务状态**。持在 App 而非设置页 —— 下载是整机动作，切页不能丢；
   * 侧边栏底部常驻一条进行中的进度条（见 .sidebar-update），error / no-update 走 toast。
   */
  const [updateStatus, setUpdateStatus] = useState<UpdateProgress | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [toast, setToast] = useState<Toast>(null);
  const [confirmReq, setConfirmReq] = useState<ConfirmReq | null>(null);
  const [adding, setAdding] = useState(false);
  /**
   * 凭证池的绑定状态（只读）。
   *
   * 真相在后端进程里（`broker::status()` 读的是内存中的运行时状态），前端只负责展示与
   * 触发动作。放在 App 而不是账号页：那一栏在「账号为空」时也要画得出来，
   * 而空态是在页面自己那里提前 return 的。
   */
  const [brokerStatus, setBrokerStatus] = useState<BrokerStatus | null>(null);
  /** 上传 / 绑定 / 解绑都在打网络，三者互斥：一个开关统一禁用 */
  const [brokerBusy, setBrokerBusy] = useState(false);
  const [brokerModal, setBrokerModal] = useState<
    | null
    | { type: "brokerLink" }
    | { type: "poolIssued"; uuid: string; message: string }
  >(null);

  // ↓ 跨页共享数据（上提到这里，页面重新挂载时不重新拉取）
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [statuses, setStatuses] = useState<Record<string, AcctStatus>>({});
  const [statusText, setStatusText] = useState("");
  /** 正在签到的账号 id；"all" 表示批量签到在跑 */
  const [busy, setBusy] = useState<{ ids: Set<string>; all: boolean; refresh: boolean }>({
    ids: new Set(),
    all: false,
    refresh: false,
  });

  const showToast = useCallback((t: Toast) => {
    setToast(t);
    if (t) window.setTimeout(() => setToast(null), 3200);
  }, []);

  /**
   * 自研确认框：不使用 window.confirm —— Tauri 的 WebView 未实现原生 confirm 面板，
   * 调用会静默返回 false，导致删除/清空这类操作永远不执行。
   */
  const askConfirm = useCallback(
    (opts: Omit<ConfirmReq, "resolve">) =>
      new Promise<boolean>((resolve) => setConfirmReq({ ...opts, resolve })),
    []
  );
  const resolveConfirm = useCallback(
    (ok: boolean) => {
      confirmReq?.resolve(ok);
      setConfirmReq(null);
    },
    [confirmReq]
  );

  /** 拉取每个账号的签到状态与积分（每个账号一次请求，属于重活，故只在必要时调用） */
  const refreshStatus = useCallback(async () => {
    setStatusText("查询中…");
    try {
      const st = await checkinStatus();
      setStatuses(Object.fromEntries(st.map((s) => [s.id, s])));
      setStatusText(st.length ? `已刷新 ${st.length} 个账号状态` : "未取到任何账号状态");
    } catch (e) {
      setStatusText("查询失败: " + e);
    }
  }, []);

  const reloadAccounts = useCallback(async () => setAccounts(await listAccounts()), []);

  useEffect(() => {
    getSettings().then(setSettings).catch(() => {});
    getVersion().then(setVersion).catch(() => {});
    listAccounts().then(setAccounts).catch(() => {});
    brokerState().then(setBrokerStatus).catch(() => {});
    // 账号资料补全：真昵称（GetUserInfo.ScreenName）与脱敏手机号（NonPlainTextMobile）都只能
    // 从服务端查，所以启动后按需回源一次 —— 把「浏览器登录账号」/手机号当名字这类占位值换成
    // 真名，并给缺手机号的账号补上。不阻塞首屏：先渲染本地列表，拿到结果再覆盖
    //（后端只对资料不全的账号发请求，离线时静默返回原列表）。
    refreshAccountProfiles().then(setAccounts).catch(() => {});
    void refreshStatus();
    // 简报每小时固化时后端会顺手把采样回写进 accounts.json（见 `commands::fetch_samples`），
    // 这里跟着重拉一遍账号列表：简报页的「当前剩余」与账号页的积分快照都读
    // `credit_snapshot`，不刷新就会停在进入应用时的旧值上。
    const un = listen(BRIEFING_SEALED_EVENT, () => {
      listAccounts().then(setAccounts).catch(() => {});
    });
    return () => {
      void un.then((f) => f());
    };
  }, [refreshStatus]);

  // 后台轮询要判断「用户此刻在不在设置页」，但不能把 page 写进依赖（会让定时器反复重建）
  const pageRef = useRef(page);
  useEffect(() => {
    pageRef.current = page;
  }, [page]);

  // 后台定时检查新版本，只为点亮侧边栏那颗小红点。
  // 窗口被隐藏时 webview 仍在跑（点红按钮只是 hide、不销毁窗口），所以常驻期间定时器一直有效。
  useEffect(() => {
    let alive = true;
    const run = async () => {
      try {
        const v = await probeUpdate();
        if (!alive) return;
        setUpdateNotice((prev) =>
          nextUpdateNotice(prev, v, pageRef.current === "settings")
        );
      } catch (e) {
        // 轮询失败一律静默：网络抖动、代理不通都是常态，不该弹提示打扰用户。
        // 想看明确报错就到设置页手动点「检查更新」。
        console.warn("后台检查更新失败：", e);
      }
    };
    const first = window.setTimeout(run, UPDATE_FIRST_DELAY_MS);
    const timer = window.setInterval(run, UPDATE_INTERVAL_MS);
    return () => {
      alive = false;
      window.clearTimeout(first);
      window.clearInterval(timer);
    };
  }, []);

  /** 小红点：后台查到了新版本，且用户还没看过 */
  const showUpdateDot = updateNotice !== null && !updateNotice.seen;

  /** 用户进设置页就算看过这条提醒（设置页里仍写明有新版本，信息不丢） */
  const markUpdateSeen = useCallback(() => {
    setUpdateNotice((n) => (n && !n.seen ? { ...n, seen: true } : n));
  }, []);

  /**
   * 触发检查并安装 —— 实现与状态都在这一层，下载是整机动作，切页既不会中断也不会丢进度。
   * 手动检查的结论同时回写 updateNotice：新版本点亮红点（用户已在看，算已读）、已是最新则清掉。
   */
  const runUpdate = useCallback(async () => {
    if (updateBusy) return;
    setUpdateBusy(true);
    setUpdateStatus({ status: "checking", message: "正在检查更新…" });
    await checkAndInstall((p) => {
      setUpdateStatus(p);
      if (p.status === "error") showToast({ kind: "err", text: p.message });
      if (p.status === "no-update") showToast({ kind: "info", text: p.message });
      if (p.status === "available")
        setUpdateNotice({ version: p.version ?? "", seen: true });
      else if (p.status === "no-update") setUpdateNotice(null);
    });
    setUpdateBusy(false);
  }, [showToast, updateBusy]);

  /** 设置改动：乐观写内存 + 落盘，失败弹 toast（设置页不弹成功提示，避免每次改动都刷屏） */
  const update = useCallback(
    (patch: Partial<Settings>) => {
      setSettings((prev) => {
        const next = { ...(prev ?? ({} as Settings)), ...patch };
        saveSettings(next).then(setSettings).catch((e) => showToast({ kind: "err", text: "保存失败：" + e }));
        return next;
      });
    },
    [showToast]
  );

  const runCheckinAll = useCallback(async () => {
    setBusy((s) => ({ ...s, all: true }));
    try {
      const res = await checkinAll();
      await reloadAccounts();
      const ok = res.filter((r) => r.success && !r.already).length;
      const already = res.filter((r) => r.already).length;
      const fail = res.length - ok - already;
      showToast({
        kind: fail > 0 ? "err" : "ok",
        text: `全部完成：成功 ${ok} / 已签 ${already} / 失败 ${fail}`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "批量签到失败：" + e });
    } finally {
      setBusy((s) => ({ ...s, all: false }));
      void refreshStatus();
    }
  }, [reloadAccounts, refreshStatus, showToast]);

  const runCheckinOne = useCallback(
    async (id: string) => {
      setBusy((s) => ({ ...s, ids: new Set(s.ids).add(id) }));
      try {
        const r = await checkinOne(id);
        await reloadAccounts();
        const kind = r.already || r.success ? "ok" : "err";
        const tag = r.already ? "已签" : r.success ? "成功" : "失败";
        showToast({ kind, text: `[${tag}] ${r.message}` });
      } catch (e) {
        showToast({ kind: "err", text: "签到失败：" + e });
      } finally {
        setBusy((s) => {
          const ids = new Set(s.ids);
          ids.delete(id);
          return { ...s, ids };
        });
        void refreshStatus();
      }
    },
    [reloadAccounts, refreshStatus, showToast]
  );

  const runRefresh = useCallback(async () => {
    setBusy((s) => ({ ...s, refresh: true }));
    await refreshStatus();
    await reloadAccounts().catch(() => {});
    setBusy((s) => ({ ...s, refresh: false }));
  }, [refreshStatus, reloadAccounts]);

  const removeOne = useCallback(
    async (a: Account) => {
      const ok = await askConfirm({
        title: "删除账号",
        body: `确认删除「${accountLabel(a.name, a.phone)}」？删除后需要重新导入或登录才能恢复。`,
        okText: "删除",
        danger: true,
      });
      if (!ok) return;
      try {
        setAccounts(await removeAccount(a.id));
        showToast({ kind: "ok", text: `已删除 ${accountLabel(a.name, a.phone)}` });
      } catch (e) {
        showToast({ kind: "err", text: "删除失败：" + e });
      }
    },
    [askConfirm, showToast]
  );

  const onImported = useCallback(
    (list: Account[]) => {
      setAccounts(list);
      void refreshStatus();
    },
    [refreshStatus]
  );

  /** 重拉凭证池绑定状态（上传 / 绑定 / 解绑 / 同步后都调用，让那一栏跟上后端） */
  const refreshBroker = useCallback(async () => {
    try {
      setBrokerStatus(await brokerState());
    } catch {
      /* 状态拉取失败不打扰用户：下一次动作会再拉 */
    }
  }, []);

  /**
   * 把本机这一批账号整体上传到凭证管家：管家颁发一串 uuid 并当场绑定。
   *
   * 这是**整台机器**的动作，不是「某个账号」的 —— 所以它不收账号参数，也不去改任何一条
   * 账号。绑定后本地与云端是并集，账号本身若因合并而有变化，交给 `reloadAccounts` 整体刷新。
   *
   * ⚠️ **只在未绑定时可点**：已绑定时界面把按钮禁掉，后端也会拒绝（`broker::upload_guard`）。
   * 服务端建池只会新建、不会覆盖，放行一次就会在云端留下第二池、让两台机器各持一把闸。
   */
  const runBrokerUpload = useCallback(async () => {
    setBrokerBusy(true);
    try {
      const op = await brokerUpload();
      // 用弹窗而不是 toast：uuid 是唯一需要被**搬到别的机器**上去的东西，
      // 一条 3 秒就消失的提示等于没给（详见 PoolIssuedModal 的注释）
      setBrokerModal({ type: "poolIssued", uuid: op.uuid, message: op.message });
      await reloadAccounts();
      await refreshBroker();
    } catch (e) {
      showToast({ kind: "err", text: "上传失败：" + String(e) });
    } finally {
      setBrokerBusy(false);
    }
  }, [reloadAccounts, refreshBroker, showToast]);

  /**
   * 绑定别处复制过来的 uuid。
   *
   * **失败要往上抛**，由弹窗就地显示原因 —— 这一步最常见的错法是把 uuid 抄漏了几位，
   * 把弹窗关掉再让用户自己回忆哪里错了，等于把错误藏起来。
   */
  const submitBrokerLink = useCallback(
    async (uuid: string) => {
      setBrokerBusy(true);
      try {
        const op = await brokerLink(uuid);
        showToast({ kind: "ok", text: op.message });
        await reloadAccounts();
        await refreshBroker();
      } finally {
        setBrokerBusy(false);
      }
    },
    [reloadAccounts, refreshBroker, showToast]
  );

  /**
   * 解绑：摘掉本机 uuid，**云端那一池保留**（可能还绑着别的机器），
   * 本机「与云端一致」的账号从本地删除，token 在本机停用。
   *
   * 不可逆：删掉的本地账号不会自己回来（重新绑定同一 uuid 可拉回云端那批）。
   * 所以必须走一次确认，并且把后果原话说明。
   */
  const runBrokerUnbind = useCallback(async () => {
    const ok = await askConfirm({
      title: "解除本机绑定？",
      body: `本机将摘掉凭证池绑定（${
        brokerStatus?.uuid?.slice(0, 8) ?? ""
      }…），云端那一池保留，其他绑定过同一 uuid 的机器不受影响。本机与云端一致的账号（已上传的那批）会从本机删除，token 在本机停用；重新绑定同一 uuid 可拉回云端那批。此操作不可恢复。`,
      okText: "解除绑定",
      danger: true,
    });
    if (!ok) return;
    setBrokerBusy(true);
    try {
      const st = await brokerUnbind();
      setBrokerStatus(st);
      await reloadAccounts();
      showToast({
        kind: "ok",
        text: `已解绑（云端池保留）${
          st.removed_local ? `，本机删除了 ${st.removed_local} 个账号` : ""
        }`,
      });
    } catch (e) {
      showToast({ kind: "err", text: String(e) });
    } finally {
      setBrokerBusy(false);
    }
  }, [askConfirm, brokerStatus?.uuid, reloadAccounts, showToast]);

  const checkedToday = useMemo(
    () => accounts.filter((a) => statuses[a.id]?.checked_in).length,
    [accounts, statuses]
  );

  /** 页头右侧：当前页的只读状态 + 该页的动作。有状态才显示，没状态就留空。 */
  const pagebarRight = () => {
    if (page === "accounts") {
      return (
        <>
          <div className="status-group">
            {settings?.checkin_enabled && (
              <span className="status-pill" title="应用保持运行时才会触发；可在「设置」里修改">
                <i className="dot" />
                每日 {settings.checkin_time || "10:00"} 自动签到
              </span>
            )}
            {accounts.length > 0 && (
              <span className="status-pill" title="来自服务端的今日真实签到状态">
                <i className="dot" />
                今日已签 {checkedToday}/{accounts.length}
              </span>
            )}
          </div>
          <span className="spacer" />
          <div className="action-group">
            <button className="btn ghost" title="扫描本机登录态或走浏览器授权登录" onClick={() => setAdding(true)}>
              <IconUserPlus size={15} />
              添加账号
            </button>
            <button
              className="btn ghost"
              disabled={busy.refresh || accounts.length === 0}
              title="重打签到状态与积分（每个账号一次请求）"
              onClick={() => void runRefresh()}
            >
              <IconRefresh size={15} className={busy.refresh ? "spin" : undefined} />
              刷新状态
            </button>
            <button
              className="btn primary"
              disabled={busy.all || accounts.length === 0}
              title="对全部账号依次签到"
              onClick={() => void runCheckinAll()}
            >
              {busy.all ? "签到中…" : "全部签到"}
            </button>
          </div>
        </>
      );
    }
    if (page === "takeover") {
      return (
        <div className="status-group">
          <span className={`status-pill${settings?.takeover_enabled ? "" : " warn"}`}>
            <i className="dot" />
            {settings?.takeover_enabled ? "接管生效中" : "未接管，应用直连官方"}
          </span>
        </div>
      );
    }
    if (page === "settings") {
      return (
        <div className="status-group">
          <span className="status-pill" title="当前运行的应用版本">
            <IconClock size={13} />
            v{version || "…"}
          </span>
        </div>
      );
    }
    // 签到日志页的工具条在页面内部（筛选 + 清空与列表状态强相关），页头留空
    return null;
  };

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="logo">
            <IconCheck size={19} />
          </span>
          <div className="brand-text">
            <span className="brand-name">TraeWork Assistant</span>
            <span className="brand-ver">v{version || "…"}</span>
          </div>
        </div>

        <nav className="nav">
          {NAV.map(({ key, label, Icon }) => (
            <button
              key={key}
              className={"nav-item" + (page === key ? " active" : "")}
              onClick={() => {
                setPage(key);
                // 进设置页 = 看到了更新提醒，熄灭小红点
                if (key === "settings") markUpdateSeen();
              }}
              title={label}
            >
              <span className="nav-icon">
                <Icon />
              </span>
              {label}
              {key === "takeover" && settings?.takeover_enabled && (
                <span className="nav-dot" title="接管生效中" />
              )}
              {key === "settings" && showUpdateDot && (
                <span
                  className="nav-dot err"
                  title={`有新版本 v${updateNotice?.version} 可更新`}
                />
              )}
            </button>
          ))}
        </nav>

        {/* 全局更新状态：检查/下载/安装进行中时在侧边栏底部常驻，切页不丢进度。
            error / no-update 已由 runUpdate 弹 toast，不在这里占位。 */}
        {updateStatus &&
          (updateStatus.status === "checking" ||
            updateStatus.status === "downloading" ||
            updateStatus.status === "installing" ||
            updateStatus.status === "updated") && (
            <div className="sidebar-update">
              <div className="sidebar-update-title">应用更新</div>
              <div className="upd-status">{updateStatus.message}</div>
              {(() => {
                const dl = downloadProgress(updateStatus);
                if (!dl) return null;
                return (
                  <div className="upd-row">
                    {dl.percent !== null && (
                      <div className="upd-progress-wrap">
                        <div
                          className="upd-progress-bar"
                          style={{ width: `${dl.percent}%` }}
                        />
                      </div>
                    )}
                    <div className="upd-progress-text">
                      {dl.percent === null
                        ? `已下载 ${formatBytes(dl.downloaded)}`
                        : `${formatBytes(dl.downloaded)} / ${formatBytes(dl.total)} · ${dl.percent}%`}
                    </div>
                  </div>
                );
              })()}
            </div>
          )}
      </aside>

      <div className="main">
        <header className="pagebar">
          <div className="pagebar-left">
            <span className="pagebar-icon">
              {(() => {
                const I = PAGE_ICON[page];
                return <I size={17} />;
              })()}
            </span>
            <h2>{PAGE_TITLES[page]}</h2>
          </div>
          {pagebarRight()}
        </header>

        <main className="content">
          {/* 只渲染当前页；`key` 让重新进入该页时重播一次入场动画 */}
          <div className="panel-page" key={page}>
            {page === "accounts" && (
              <AccountsPage
                accounts={accounts}
                statuses={statuses}
                statusText={statusText}
                busyIds={busy.ids}
                onCheckinOne={(id) => void runCheckinOne(id)}
                onRemove={(a) => void removeOne(a)}
                brokerStatus={brokerStatus}
                brokerBusy={brokerBusy}
                onBrokerUpload={() => void runBrokerUpload()}
                onBrokerLink={() => setBrokerModal({ type: "brokerLink" })}
                onBrokerUnbind={() => void runBrokerUnbind()}
              />
            )}
            {page === "takeover" && (
              <TakeoverPage
                settings={settings}
                update={update}
                notify={showToast}
                accounts={accounts}
              />
            )}
            {page === "briefing" && settings && (
              <BriefingPage
                accounts={accounts}
                settings={settings}
                askConfirm={askConfirm}
                onSettings={setSettings}
                onToast={showToast}
              />
            )}
            {page === "logs" && (
              <LogsPage askConfirm={askConfirm} onToast={showToast} />
            )}
            {page === "settings" && (
              <SettingsPage
                settings={settings}
                update={update}
                version={version}
                updateStatus={updateStatus}
                updateBusy={updateBusy}
                onRunUpdate={() => void runUpdate()}
                updateVersion={updateNotice?.version ?? null}
              />
            )}
          </div>
        </main>
      </div>

      {toast && <div className={`toast toast-${toast.kind}`}>{toast.text}</div>}

      {adding && (
        <AddAccountModal
          onClose={() => setAdding(false)}
          onImported={onImported}
          notify={showToast}
        />
      )}
      {brokerModal?.type === "brokerLink" && (
        <BrokerBindModal
          onBind={submitBrokerLink}
          onClose={() => setBrokerModal(null)}
        />
      )}
      {brokerModal?.type === "poolIssued" && (
        <PoolIssuedModal
          uuid={brokerModal.uuid}
          message={brokerModal.message}
          onClose={() => setBrokerModal(null)}
          onToast={showToast}
        />
      )}
      {confirmReq && <ConfirmDialog req={confirmReq} onDone={resolveConfirm} />}
    </div>
  );
}
