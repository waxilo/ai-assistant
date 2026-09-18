import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import type {
  Account,
  BrokerStatus,
  ImportItem,
  ImportReport,
  Settings,
} from "./types";
import {
  listAccounts,
  importAccounts,
  removeAccount,
  checkinOne,
  checkinAll,
  refreshAll,
  getSettings,
  saveSettings as saveSettingsApi,
  appVersion,
  brokerState,
  brokerUpload,
  brokerLink,
  brokerUnbind,
} from "./api";
import { accountLabel, tally, formatBytes, type ConfirmReq, type Toast } from "./common";
import { bindCredits, seedCredits } from "./credits";
import {
  checkAndInstall,
  downloadProgress,
  nextUpdateNotice,
  probeUpdate,
  type UpdateNotice,
  type UpdateProgress,
} from "./updater";
import { AccountsPage } from "./pages/AccountsPage";
import { TakeoverPage } from "./pages/TakeoverPage";
import { BriefingPage } from "./pages/BriefingPage";
import { LogsPage } from "./pages/LogsPage";
import { SettingsPage } from "./pages/SettingsPage";
import {
  IconCheck,
  IconSwap,
  IconActivity,
  IconList,
  IconGear,
  IconUserPlus,
  IconRefresh,
} from "./components/Icons";
import {
  BrokerBindModal,
  LocalAccountsModal,
  OAuthModal,
  PoolIssuedModal,
} from "./components/ImportModals";
import { ConfirmDialog } from "./components/ConfirmDialog";

/**
 * 应用外壳：左侧导航栏 + 右侧内容区。
 *
 * 页面（tab）承载常驻功能：账号签到 / 智能接管 / 签到日志 / 设置
 * （网络急救已作为一张卡并入设置页，见 NetfixCard）；
 * 弹窗只留给「做完即走」的任务流（登录新账号、导入本机账号、危险操作确认）。
 */
type Page = "accounts" | "takeover" | "briefing" | "logs" | "settings";

type Modal =
  | { type: "local" }
  | { type: "oauth" }
  | { type: "brokerLink" }
  /** 上传成功：摊开那一池的 uuid 等用户搬走（详情见 PoolIssuedModal） */
  | { type: "poolIssued"; uuid: string; message: string }
  | null;

const NAV: { key: Page; label: string }[] = [
  { key: "accounts", label: "账号签到" },
  { key: "takeover", label: "智能接管" },
  { key: "briefing", label: "积分简报" },
  { key: "logs", label: "签到日志" },
  { key: "settings", label: "设置" },
];

const PAGE_ICON: Record<Page, ReactNode> = {
  accounts: <IconCheck />,
  takeover: <IconSwap />,
  briefing: <IconActivity />,
  logs: <IconList />,
  settings: <IconGear />,
};

const PAGE_TITLES: Record<Page, string> = {
  accounts: "账号签到",
  takeover: "智能接管",
  briefing: "积分简报",
  logs: "签到日志",
  settings: "设置",
};

/**
 * 后台检查更新的节奏：启动后先等 8 秒（避开启动期的签到/续签抢网络），此后每 6 小时一次。
 * 定成常驻轮询是因为应用是「后台常驻」形态——没人会天天主动去点「检查更新」。
 */
const UPDATE_FIRST_DELAY_MS = 8_000;
const UPDATE_INTERVAL_MS = 6 * 60 * 60 * 1000;

export default function App() {
  const [page, setPage] = useState<Page>("accounts");
  /** 日志页的初始账号筛选（从账号条目点「日志」跳转时带上） */
  const [logsInitialId, setLogsInitial] = useState<string | null>(null);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [version, setVersion] = useState("");
  const [loading, setLoading] = useState(true);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [busyAll, setBusyAll] = useState(false);
  const [busyRefresh, setBusyRefresh] = useState(false);
  /** 后台轮询发现的新版本（驱动侧边栏「设置」上的小红点） */
  const [updateNotice, setUpdateNotice] = useState<UpdateNotice>(null);
  /**
   * 应用更新：**全局任务状态**。持在 App 而非设置页 —— 下载是整机动作，切页不能丢；
   * 侧边栏底部常驻一条进行中的进度条（见 .sidebar-update），error / no-update 走 toast。
   */
  const [updateStatus, setUpdateStatus] = useState<UpdateProgress | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [modal, setModal] = useState<Modal>(null);
  const [toast, setToast] = useState<Toast>(null);
  const [confirmReq, setConfirmReq] = useState<ConfirmReq | null>(null);
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

  // 自研确认框：不使用 window.confirm —— Tauri 的 WKWebView 未实现原生 confirm 面板，
  // 调用会静默返回 false，导致删除/清空这类操作永远不执行。
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

  const showToast = useCallback((t: Toast) => {
    setToast(t);
    if (t) window.setTimeout(() => setToast(null), 3200);
  }, []);

  const load = useCallback(async () => {
    try {
      const [acc, set, ver, bkr] = await Promise.all([
        listAccounts(),
        getSettings(),
        appVersion(),
        brokerState(),
      ]);
      setAccounts(acc);
      // 列表里带着后端的台账投影，顺手灌进**全局积分对象**（见 src/credits.ts）：
      // 账户页与简报页读的都是它，所以这里播一次种，两页从一开始就是同一个数
      seedCredits(acc);
      setSettings(set);
      setVersion(ver);
      setBrokerStatus(bkr);
    } catch (e) {
      showToast({ kind: "err", text: "加载失败：" + String(e) });
    } finally {
      setLoading(false);
    }
  }, [showToast]);

  useEffect(() => {
    load();
  }, [load]);

  // 全局积分对象接上后端广播（幂等，只注册一次）。
  //
  // 采集是**后端**做的，而且页面没打开时也在做（整点采样跑在后台线程里）：把结果落进
  // 模块级的全局对象，而不是某个组件的 state，才谈得上「一次采集、两页同时更新」——
  // 页面切走再回来也不会各自再去拉一份不同时刻的数。
  useEffect(() => {
    bindCredits();
  }, []);

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
        const version = await probeUpdate();
        if (!alive) return;
        setUpdateNotice((prev) =>
          nextUpdateNotice(prev, version, pageRef.current === "settings")
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

  const reloadSettings = useCallback(async () => {
    setSettings(await getSettings());
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

  // 启动时若开启“自动签到”，则对全部账号执行一次
  useEffect(() => {
    if (!loading && settings?.auto_checkin_on_start && accounts.length > 0) {
      void runCheckinAll();
      // 仅在首次装配完成后触发一次
      // eslint-disable-next-line react-hooks/exhaustive-deps
      setSettings((s) => (s ? { ...s, auto_checkin_on_start: false } : s));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loading]);

  // 定时签到由后端调度线程触发（应用需保持运行），完成后刷新列表并提示
  useEffect(() => {
    const un = listen<{ stage: string; count?: number; message?: string }>(
      "checkin-scheduled",
      (e) => {
        const p = e.payload;
        if (p.stage === "done") {
          void load();
          showToast({
            kind: "info",
            text: `定时签到完成（${p.count ?? 0} 个账号）`,
          });
        } else if (p.stage === "error") {
          showToast({ kind: "err", text: "定时签到异常：" + (p.message ?? "") });
        }
      }
    );
    return () => {
      void un.then((f) => f());
    };
  }, [load, showToast]);

  // 自动续签由后端调度线程完成（有效期不足 48h 触发），完成后刷新列表
  useEffect(() => {
    const un = listen<{ count?: number }>("auto-refreshed", (e) => {
      void load();
      showToast({
        kind: "info",
        text: `已自动续签 ${e.payload.count ?? 0} 个账号的凭证`,
      });
    });
    return () => {
      void un.then((f) => f());
    };
  }, [load, showToast]);

  // 积分简报由后端调度线程每小时固化一次，事件由「积分简报」页自己监听并刷新列表。
  //
  // 这里**故意不弹提示**：每小时弹一次就是噪音，而简报本来就是「回来看历史」的东西，
  // 用户不需要在写别的代码时被通知「又攒了一条时条目」。真正值得打扰的是每日推送
  // （走 webhook），那部分由后端负责。

  const runCheckinOne = useCallback(
    async (id: string) => {
      setBusyIds((s) => new Set(s).add(id));
      try {
        const updated = await checkinOne(id);
        setAccounts((list) => list.map((a) => (a.id === id ? updated : a)));
        // 签到顺手读到的余额也在返回值里，并进全局对象（后端此刻也会广播一次，两者等价）
        seedCredits([updated]);
        if (updated.last?.already) {
          showToast({ kind: "info", text: `${updated.name} 今日已签` });
        } else if (updated.last?.success) {
          showToast({ kind: "ok", text: `${updated.name} 签到成功` });
        } else if (updated.last?.inactive) {
          showToast({ kind: "info", text: `${updated.name} 活动未开启` });
        } else {
          showToast({
            kind: "err",
            text: `${updated.name} 签到失败：${updated.last?.message ?? ""}`,
          });
        }
      } catch (e) {
        showToast({ kind: "err", text: "签到异常：" + String(e) });
      } finally {
        setBusyIds((s) => {
          const n = new Set(s);
          n.delete(id);
          return n;
        });
      }
    },
    [showToast]
  );

  const runCheckinAll = useCallback(async () => {
    setBusyAll(true);
    try {
      const updated = await checkinAll();
      setAccounts(updated);
      seedCredits(updated);
      const { ok, already, fail } = tally(
        updated.map((a) => a.last).filter((r): r is NonNullable<typeof r> => r != null)
      );
      showToast({
        kind: fail > 0 ? "err" : "ok",
        text: `全部完成：成功 ${ok} / 已签 ${already} / 失败 ${fail}`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "批量签到异常：" + String(e) });
    } finally {
      setBusyAll(false);
    }
  }, [showToast]);

  // 一键刷新：重拉积分事实（余额 + 最早过期时间 + 逐包明细 → 写进后端积分台账）
  // 与真实签到状态，不触发签到（已签账号再打签到接口只会拿到 400）；后端返回最新账号列表一次到位。
  const runRefresh = useCallback(async () => {
    setBusyRefresh(true);
    try {
      const updated = await refreshAll();
      setAccounts(updated);
      // 刷新是「手工触发的一次采集」：读数并进全局对象，两个页面跟着一起变
      seedCredits(updated);
      const got = updated.filter((a) => a.credits?.credits != null).length;
      const st = updated.filter((a) => a.checked_today === true).length;
      showToast({
        kind: "ok",
        text: `已刷新 ${updated.length} 个账号（${got} 个取到积分，当前 ${st} 个今日已签）`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "刷新失败：" + String(e) });
    } finally {
      setBusyRefresh(false);
    }
  }, [showToast]);

  const removeOne = useCallback(
    async (a: Account) => {
      const ok = await askConfirm({
        title: "删除账号",
        body: `确认删除「${accountLabel(a.name, a.phone)}」？该账号的签到日志也会一并删除。`,
        okText: "删除",
        danger: true,
      });
      if (!ok) return;
      try {
        await removeAccount(a.id);
        setAccounts((l) => l.filter((x) => x.id !== a.id));
        showToast({ kind: "ok", text: `已删除 ${accountLabel(a.name, a.phone)}` });
      } catch (e) {
        showToast({ kind: "err", text: "删除失败：" + String(e) });
      }
    },
    [askConfirm, showToast]
  );

  // 批量导入：后端按「手机号或 token」识别已有账号并合并补全凭证，不会产生重复条目
  const importItems = useCallback(
    async (items: ImportItem[]): Promise<ImportReport> => {
      const report = await importAccounts(items);
      if (report.added > 0 || report.updated > 0) {
        await load();
        // 导入新账号后补查真实状态（只读查询，持久化），列表直接反映服务端真相。
        // 结果必须收回来：`void refreshAll()` 会把返回的账号（含刚查到的 checked_today
        // 与台账读数）整包丢掉，列表就只剩导入那一刻的陈旧状态。
        void refreshAll().then((list) => {
          setAccounts(list);
          seedCredits(list);
        });
      }
      return report;
    },
    [load]
  );

  /**
   * 把本机这一批账号整体上传到凭证管家：管家颁发一串 uuid 并当场绑定。
   *
   * 这是**整台机器**的动作，不是「某个账号」的 —— 所以它不收账号参数，也不去改任何一条
   * 账号。绑定后本地与云端是并集，账号本身若因合并而有变化，交给 `load()` 整体刷新。
   *
   * ⚠️ **只在未绑定时可点**：已绑定时界面把按钮禁掉，后端也会拒绝（`broker::upload_guard`）。
   * 服务端建池只会新建、不会覆盖，放行一次就会在云端留下第二池、让两台机器各持一把闸。
   * 而且已绑定后新增的账号由整池同步自动带上去，这个动作本来也就多余了。
   */
  const runBrokerUpload = useCallback(async () => {
    setBrokerBusy(true);
    try {
      const op = await brokerUpload();
      // 用弹窗而不是 toast：uuid 是唯一需要被**搬到别的机器**上去的东西，
      // 一条 3 秒就消失的提示等于没给（详见 PoolIssuedModal 的注释）
      setModal({ type: "poolIssued", uuid: op.uuid, message: op.message });
      await load();
    } catch (e) {
      showToast({ kind: "err", text: "上传失败：" + String(e) });
    } finally {
      setBrokerBusy(false);
    }
  }, [load, showToast]);

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
        await load();
      } finally {
        setBrokerBusy(false);
      }
    },
    [load, showToast]
  );

  /**
   * 解绑：摘掉本机 uuid，**云端那一池保留**；本机会移除与云端重复的凭证，只留本机独有的。
   *
   * 不影响其他绑定同一 uuid 的机器。会动本机账号，所以先确认一次。
   */
  const runBrokerUnbind = useCallback(async () => {
    const ok = await askConfirm({
      title: "解除绑定？",
      body: `解绑后本机回到单机运行：与云端那一池（${
        brokerStatus?.uuid?.slice(0, 8) ?? ""
      }…）重复的凭证会从本机移除，本机独有的账号保留；云端那一池保留，其他机器不受影响。`,
      okText: "解除绑定",
      danger: true,
    });
    if (!ok) return;
    setBrokerBusy(true);
    try {
      setBrokerStatus(await brokerUnbind());
      await load();
      showToast({ kind: "ok", text: "已解绑，本机与云端重复的凭证已移除" });
    } catch (e) {
      // 拿不到云端那一池时后端会拒绝，并保留本地绑定 —— 照实说，别让用户以为已经解绑了
      showToast({ kind: "err", text: String(e) });
    } finally {
      setBrokerBusy(false);
    }
  }, [askConfirm, brokerStatus?.uuid, load, showToast]);

  const saveSettings = useCallback(async (s: Settings) => {
    // 设置页已改为「改动自动保存」，这里只负责落盘并刷新内存中的 settings，
    // 不再弹成功 toast（每次改动都弹会刷屏）。失败提示由设置页兜底。
    const saved = await saveSettingsApi(s);
    setSettings(saved);
  }, []);

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="logo">
            <IconCheck size={18} />
          </span>
          <div className="brand-text">
            <span className="brand-name">Qoder 助手</span>
            <span className="brand-ver">v{version}</span>
          </div>
        </div>

        <nav className="nav">
          {NAV.map((n) => (
            <button
              key={n.key}
              className={`nav-item ${page === n.key ? "active" : ""}`}
              /* 窄窗口下侧栏收成图标轨道、文案被隐藏，title 是那时**唯一**能说明
                 「这个图标是什么」的东西；宽窗口下它只是重复一遍可见文案，无害。 */
              title={n.label}
              onClick={() => {
                setPage(n.key);
                // 从导航进入日志页时不带账号预设（只有从账号条目跳转才带）
                if (n.key === "logs") setLogsInitial(null);
                // 进设置页 = 看到了更新提醒
                if (n.key === "settings") markUpdateSeen();
              }}
            >
              <span className="nav-icon">{PAGE_ICON[n.key]}</span>
              {/* 文案必须包成元素：侧栏在窄窗口下收成图标轨道时要单独隐藏它 */}
              <span className="nav-label">{n.label}</span>
              {n.key === "takeover" && settings?.proxy_enabled && (
                <span className="nav-dot" title="接管生效中" />
              )}
              {n.key === "settings" && showUpdateDot && (
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
            <span className="pagebar-icon">{PAGE_ICON[page]}</span>
            <h2>{PAGE_TITLES[page]}</h2>
          </div>
          <span className="spacer" />
          {page === "accounts" && (
            <>
              <div className="status-group">
                {settings?.schedule_enabled && (
                  <span
                    className="status-pill"
                    title={
                      settings.schedule_window_minutes > 0
                        ? `应用保持运行时才会触发；今天的时刻在 ${settings.schedule_time} 之后的 ${settings.schedule_window_minutes} 分钟内随机挑定，挑定后会推送一条通知告知；可在「设置」里修改`
                        : "应用保持运行时才会触发；可在「设置」里修改"
                    }
                  >
                    <i className="dot" />
                    <span className="pill-text">
                      {settings.schedule_window_minutes > 0
                        ? `每日 ${settings.schedule_time} 起 ${settings.schedule_window_minutes} 分钟内随机签到`
                        : `每日 ${settings.schedule_time} 自动签到`}
                    </span>
                  </span>
                )}
                {settings?.stagger_checkin && settings.stagger_max_seconds > 0 && (
                  <span
                    className="status-pill warn"
                    title="批量签到时账号之间随机间隔，降低同 IP 触发风控的概率"
                  >
                    <i className="dot" />
                    <span className="pill-text">
                      风控间隔 ≤{settings.stagger_max_seconds}s
                    </span>
                  </span>
                )}
              </div>
              <div className="action-group">
                <button
                  className="btn ghost"
                  title="用系统浏览器扫码登录新账号"
                  onClick={() => setModal({ type: "oauth" })}
                >
                  <IconUserPlus size={15} />
                  登录新账号
                </button>
                <button
                  className="btn ghost"
                  title="读取本机 Qoder 登录信息自动添加账号"
                  onClick={() => setModal({ type: "local" })}
                >
                  导入本机账号
                </button>
                <button
                  className="btn ghost"
                  disabled={busyRefresh || accounts.length === 0}
                  title="重拉全部账号的积分快照 / 签到状态 / 积分余量并持久化"
                  onClick={runRefresh}
                >
                  {busyRefresh ? (
                    <>
                      <IconRefresh size={15} className="spin" />
                      刷新中
                    </>
                  ) : (
                    <>
                      <IconRefresh size={15} />
                      刷新
                    </>
                  )}
                </button>
                <button
                  className="btn primary"
                  disabled={busyAll || accounts.length === 0}
                  onClick={runCheckinAll}
                >
                  {busyAll ? "签到中…" : "全部签到"}
                </button>
              </div>
            </>
          )}
          {/* 页头右侧只放「当前页面的只读状态」：有状态才显示，没状态就留空。
              日志页原先挂的是一句说明文案（「按账号筛选查看签到记录」），
              既不承载状态、又与账号页那组状态胶囊长得像，已移除。 */}
        </header>

        <main className="content">
          {page === "accounts" && (
            <AccountsPage
              accounts={accounts}
              loading={loading}
              busyIds={busyIds}
              brokerStatus={brokerStatus}
              brokerBusy={brokerBusy}
              onBrokerUpload={() => void runBrokerUpload()}
              onBrokerLink={() => setModal({ type: "brokerLink" })}
              onBrokerUnbind={() => void runBrokerUnbind()}
              onCheckinOne={(id) => void runCheckinOne(id)}
              onRemove={(a) => void removeOne(a)}
              onOpenLogs={(id) => {
                // 从账号条目进日志页：锁定该账号
                setLogsInitial(id);
                setPage("logs");
              }}
            />
          )}
          {page === "takeover" && settings && (
            <TakeoverPage
              settings={settings}
              accounts={accounts}
              askConfirm={askConfirm}
              onSettings={setSettings}
              onToast={showToast}
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
            <LogsPage
              accounts={accounts}
              initialAccountId={logsInitialId ?? undefined}
              askConfirm={askConfirm}
              onToast={showToast}
            />
          )}
          {page === "settings" && settings && (
            <SettingsPage
              version={version}
              settings={settings}
              onSave={async (s) => {
                try {
                  await saveSettings(s);
                } catch (e) {
                  showToast({ kind: "err", text: "保存失败：" + String(e) });
                }
              }}
              onToast={showToast}
              askConfirm={askConfirm}
              onReloadSettings={reloadSettings}
              updateStatus={updateStatus}
              updateBusy={updateBusy}
              onRunUpdate={() => void runUpdate()}
              updateVersion={updateNotice?.version ?? null}
            />
          )}
        </main>
      </div>

      {toast && <div className={`toast toast-${toast.kind}`}>{toast.text}</div>}

      {modal?.type === "local" && (
        <LocalAccountsModal
          accounts={accounts}
          onImport={importItems}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}
      {modal?.type === "oauth" && (
        <OAuthModal
          onImport={importItems}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}
      {modal?.type === "brokerLink" && (
        <BrokerBindModal onBind={submitBrokerLink} onClose={() => setModal(null)} />
      )}
      {modal?.type === "poolIssued" && (
        <PoolIssuedModal
          uuid={modal.uuid}
          message={modal.message}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}

      {confirmReq && (
        <ConfirmDialog req={confirmReq} onDone={resolveConfirm} />
      )}
    </div>
  );
}
