import { useCallback, useEffect, useMemo, useState } from "react";
import type { Account, ModelReport, JournalEvent, Settings, StealthStatus } from "../types";
import {
  applySettings,
  takeoverEvents,
  clearTakeoverEvents,
  revealDebugLog,
  saveSettings,
  stealthStatus,
  freeModels,
  openAppManagement,
} from "../api";
import { AccountCell, accountIdent, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";
import {
  IconAlertTriangle,
  IconBolt,
  IconFile,
  IconInfo,
  IconRefresh,
  IconShield,
  IconTrash,
  IconUser,
} from "../components/Icons";
import { Row, Toggle } from "../components/SettingsControls";
import Switch from "../components/Switch";
import { Dialog } from "../components/Dialog";

/**
 * 事件类型 → 界面标签与配色。
 *
 * 这张表**只认对客通知**。请求级细节（`proxy_auth` / `proxy_request` /
 * `proxy_bad_request`…）压根不会送到前端 —— 它们在写入侧就分流去了调试日志文件。
 * 如果哪天这里又收到一个技术事件，说明后端的分流漏了，label 会退化成「事件」。
 */
function eventKind(e: JournalEvent): {
  label: string;
  cls: "on" | "off" | "route" | "failover" | "restart" | "err";
} {
  switch (e.event) {
    case "install":
      return { label: "开启接管", cls: "on" };
    case "uninstall":
      return { label: "关闭接管", cls: "off" };
    // 「本次对话由账号 X 提供」—— 整个页面最要紧的一条：它回答「这轮对话扣的是谁」
    case "session_start":
      return { label: "本次对话", cls: "route" };
    case "route_start":
      return { label: "开始使用账号", cls: "route" };
    case "failover":
      return { label: "限流切换", cls: "failover" };
    case "restart_qoder":
      return { label: "重启 Qoder", cls: "restart" };
    // 真故障：连不上上游 / 响应中断。用户能感知，所以它必须出现在这里。
    case "proxy_upstream_error":
    case "proxy_stream_error":
      return { label: "接管异常", cls: "err" };
    default:
      return { label: "事件", cls: "restart" };
  }
}

/**
 * 全选归一：勾选集覆盖**整池**时存空（= 默认全选，之后新增的账号自动可扣费）。
 */
function normalizeBilling(list: string[], pool: string[]): string[] {
  return pool.length > 0 && list.length === pool.length ? [] : list;
}

/**
 * 时间列只保留**必要的精度**：今天的事件给 `HH:MM:SS`，跨天的才带上 `MM-DD`。
 * 后端 `at` 已是本地时间串，`at_ms` 用来判断是不是今天。
 */
function shortTime(e: JournalEvent): string {
  const hms = e.at.length >= 19 ? e.at.slice(11, 19) : e.at;
  const isToday = new Date(e.at_ms).toDateString() === new Date().toDateString();
  return isToday ? hms : `${e.at.slice(5, 10)} ${hms.slice(0, 5)}`;
}

/**
 * 账号的积分摘要（弹窗每行右侧）。积分来自台账快照，与账号页显示的是同一个数 ——
 * 选号时不必再切回账号页去对。
 */
function creditMeta(a: Account): string {
  const snap = a.credits;
  if (!snap) return "积分未知";
  return snap.credits === null ? "积分未知" : `积分 ${formatCredits(snap.credits)}`;
}

/**
 * 模型清单来源的展示文案，与后端那三层一一对应。
 *
 * 用表而不是嵌套三元：来源现在有四档，三元链会写成连自己都数不清的缩进。
 */
const SOURCE_LABEL: Record<ModelReport["source"], string> = {
  fetched: "在线清单",
  cache: "缓存清单",
  local: "仅本机用过的模型",
  empty: "暂不可用",
};

/**
 * 控制条那颗胶囊上的**短**来源后缀。
 *
 * `fetched` 故意缺席：那是正常态，不必在胶囊里复述一遍。
 * 其余三档必须说出来 —— 否则胶囊上那串数字看着和「刚拉到的」一模一样，
 * 而实际上少得多（本机记录只含用过的模型）。
 */
const SOURCE_SHORT: Partial<Record<ModelReport["source"], string>> = {
  cache: "缓存",
  local: "本机",
};

/**
 * 「智能接管」页：顶部一张主控制卡（大开关 + 端口 + 两颗弹窗入口），
 * 下方是接管动态时间轴。机制说明收进标题旁的悬浮层，首屏只留控件与状态。
 * 多选类配置一律收进弹框、不在页面上直接铺开：
 * 开关拨动立即应用；端口仅在关闭时可改（失焦即存）；
 * 扣费账号、限流切换模型在各自弹框里点「保存」才落库生效。
 */
export function TakeoverPage({
  settings,
  accounts,
  askConfirm,
  onSettings,
  onReloadSettings,
  onToast,
}: {
  settings: Settings;
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 保存后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  /** 从磁盘重读设置：任何一次操作失败后都要用它把界面拉回与后端一致 */
  onReloadSettings: () => Promise<void>;
  onToast: (t: Toast) => void;
}) {
  // ⚠️ 这一页曾经有 6 个镜像 state（`proxyOn` / `proxyPort` / `region` / `billing` /
  // `rlModels` / `rlFailover`，逐个 `useState(settings.x)`）。它们是这页最贵的 bug：
  // 镜像只在挂载时初始化、**永不跟 props 同步**，而组装请求时又无条件把它们盖回去 ——
  // 只要后端在任何一次操作里改写了设置（或某次失败后回滚），界面就会拿着**过期的**
  // `proxy_enabled` 去 `save_settings`，被后端的拓扑守卫拒成「必须使用安全切换流程」，
  // 而且此后这一页的任何保存都会一直失败，直到切页重挂载或重启应用。
  //
  // 现在事实源只有一个：`settings`（props）。本地只留两类**草稿** ——
  // 弹窗里尚未提交的勾选、端口输入框的中间态。
  const [portDraft, setPortDraft] = useState<string | null>(null);
  const billing = settings.billing_account_ids;
  const rlModels = settings.rate_limit_models;
  const rlFailover = settings.failover_on_rate_limit;
  const proxyOn = settings.proxy_enabled;
  const proxyPort = portDraft ?? String(settings.proxy_port || 8789);
  const [pickerOpen, setPickerOpen] = useState(false);
  // 弹框草稿：打开时复制当前生效值，点「保存」才落库生效
  const [draft, setDraft] = useState<string[] | null>(null);
  const [stealth, setStealth] = useState<StealthStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [busy, setBusy] = useState(false);
  // 只覆盖「开关接管」这一步：它比别的保存慢得多（要等客户端优雅退出再拉起来，
  // 最长约半分钟），副文案要专门说明「在等什么」，不能让用户以为界面卡死了
  const [switching, setSwitching] = useState(false);
  const [err, setErr] = useState("");
  // 接管动态手动刷新：独立于 15s 自动轮询，点按即重拉状态与事件流
  const [feedBusy, setFeedBusy] = useState(false);
  // 限流切换模型弹窗（草稿制：打开复制当前值，点「保存」才落库生效）
  const [mdlOpen, setMdlOpen] = useState(false);
  const [mdlDraft, setMdlDraft] = useState<string[] | null>(null);
  // 模型清单：三层来源（Qoder 目录 / 落盘快照 / 本机痕迹），弹窗列表与控制条摘要共用
  const [fm, setFm] = useState<ModelReport | null>(null);
  const [fmBusy, setFmBusy] = useState(false);
  const [mdlFailover, setMdlFailover] = useState<boolean | null>(null);

  /** 全部账号的 id：扣费池的合法取值范围（设置里残留的已删除账号 id 由 effective 清掉） */
  const accountIds = useMemo(() => accounts.map((a) => a.id), [accounts]);

  /**
   * 实际生效的扣费池。语义没变（空设置 = 全选）；设置里残留的已删除账号 id
   * 在这里清掉，别让弹窗里的勾选状态与真实扣费池对不上。
   */
  const effective = useMemo(() => {
    const scoped = billing.filter((id) => accountIds.includes(id));
    return scoped.length === 0 ? accountIds : scoped;
  }, [billing, accountIds]);

  /** 刷新接管状态与事件流（15 秒自动轮询） */
  const refreshStealth = useCallback(async () => {
    try {
      const [s, ev] = await Promise.all([stealthStatus(), takeoverEvents()]);
      setStealth(s);
      setEvents(ev);
    } catch (e) {
      onToast({ kind: "err", text: "读取接管状态失败：" + String(e) });
    }
  }, [onToast]);

  useEffect(() => {
    void refreshStealth();
    const t = window.setInterval(() => void refreshStealth(), 15000);
    return () => window.clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /**
   * 拉取模型列表（控制条摘要 + 限流切换弹窗共用）：打开这一页时拉一次。
   *
   * 刻意不把 `loadFreeModels` 写进依赖：它还依赖 `onToast` 的引用，而那个引用只要外层
   * 重渲染就会变，会把「进页拉一次」变成「每次重渲染都拉一次」。
   */
  useEffect(() => {
    void loadFreeModels(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 手动刷新接管动态（重拉状态与事件流） */
  const doRefreshFeed = useCallback(async () => {
    setFeedBusy(true);
    try {
      await refreshStealth();
    } finally {
      setFeedBusy(false);
    }
  }, [refreshStealth]);

  /** 清空接管动态（不可恢复），清完刷新本地列表 */
  const doClearEvents = async () => {
    const ok = await askConfirm({
      title: "清空接管动态",
      body: "将删除界面上的全部通知（开启/关闭/每次对话的账号/异常），此操作不可恢复。调试日志不受影响。继续？",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearTakeoverEvents();
      setEvents([]);
      onToast({ kind: "ok", text: "接管动态已清空" });
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
    }
  };

  /**
   * 在文件管理器里定位调试日志。
   *
   * 界面上只留对客通知，于是「这个请求到底怎么被处理的」必须有地方可查 —— 就是它。
   * 排查时先看这里，别指望界面上有。
   */
  const doRevealLog = async () => {
    try {
      await revealDebugLog();
    } catch (e) {
      onToast({ kind: "err", text: "打开调试日志失败：" + String(e) });
    }
  };

  /** 拉取限流切换支持的模型列表（refresh=true 忽略缓存强制重拉） */
  const loadFreeModels = useCallback(
    async (refresh: boolean) => {
      setFmBusy(true);
      try {
        setFm(await freeModels(refresh));
      } catch (e) {
        onToast({ kind: "err", text: "拉取模型列表失败：" + String(e) });
      } finally {
        setFmBusy(false);
      }
    },
    [onToast]
  );

  /** 弹窗里勾/去勾某个付费模型（只改草稿；免费模型恒生效不可点） */
  const toggleDraftModel = (id: string) =>
    setMdlDraft((list) => {
      const base = list ?? rlModels;
      return base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
    });

  /** 打开「限流切换模型」弹窗：草稿复制当前生效值，模型清单缺失则先拉取 */
  const openModelPicker = () => {
    setMdlDraft(rlModels);
    setMdlFailover(rlFailover);
    setMdlOpen(true);
    if (!fm) void loadFreeModels(false);
  };

  /** 关掉弹窗并丢弃全部草稿（点遮罩 / 点「取消」共用一个出口，避免只清一半草稿） */
  const closeModelPicker = () => {
    setMdlDraft(null);
    setMdlFailover(null);
    setMdlOpen(false);
  };

  /**
   * 组装一份**只改动指定字段**的设置：其余字段一律以 props 为准。
   *
   * 刻意不再把界面上的 `proxy_enabled` / `proxy_port` 之类「盖」回去 ——
   * 后端才是拓扑字段的事实源，前端盖回去等于拿旧值把新事实顶掉。
   */
  const patched = (over: Partial<Settings>): Settings => ({ ...settings, ...over });

  /**
   * 出错后把界面拉回与磁盘一致。
   *
   * 一次失败的 `apply_settings` 可能已经落盘、也可能整段回滚了 —— 结果的判断权在后端，
   * 前端唯一正确的动作是**重读**，而不是自己猜。猜错就是那个「开关与事实相反、
   * 此后这一页每次保存都被拒」的死结。
   */
  const resync = async (e: unknown) => {
    setErr(String(e));
    try {
      await onReloadSettings();
    } catch {
      /* 重读本身失败就算了：下一次轮询或重挂载还会再拉一次 */
    }
  };

  /**
   * 开关即拨即用：客户端每次会话都新起一个 worker 线程并**从磁盘重读产物**，
   * 所以改动在下一次对话自然生效，不需要重启任何东西（2026-09-24 活体标记实验裁决）。
   */
  const doToggle = async (next: boolean) => {
    const action = next ? "开启接管" : "关闭接管";
    const ok = await askConfirm({
      title: action,
      body: next
        ? "开启后，Qoder 的下一次对话起按备选账号扣费。现在开启吗？"
        : "关闭后，Qoder 的下一次对话起恢复官方直连。现在关闭吗？",
      okText: action,
    });
    if (!ok) return; // 取消：开关状态不动
    setBusy(true);
    setSwitching(true);
    setErr("");
    try {
      const saved = await applySettings(patched({ proxy_enabled: next }));
      onSettings(saved);
      onToast({
        kind: "ok",
        text: next
          ? "接管已开启：Qoder 的下一次对话起按备选账号扣费（无需重启）"
          : "接管已关闭：Qoder 的下一次对话起恢复直连（无需重启）",
      });
      await refreshStealth();
    } catch (e) {
      await resync(e);
    } finally {
      setSwitching(false);
      setBusy(false);
    }
  };

  /** 端口只在接管关闭时可改；失焦时若变了就立即落盘（纯配置，不动任何进程） */
  const onPortBlur = async () => {
    const port = Number(proxyPort) || 8789;
    if (proxyOn || port === settings.proxy_port) {
      setPortDraft(null);
      return;
    }
    try {
      const saved = await saveSettings(patched({ proxy_port: port }));
      onSettings(saved);
      setPortDraft(null);
      onToast({ kind: "ok", text: "端口已保存" });
    } catch (e) {
      onToast({ kind: "err", text: "端口保存失败：" + String(e) });
      setPortDraft(null);
      await onReloadSettings().catch(() => undefined);
    }
  };

  /** 打开扣费账号弹窗：草稿复制当前生效池，「取消」原样丢弃 */
  const openBilling = () => {
    setDraft([...effective]);
    setPickerOpen(true);
  };

  /** 关掉扣费账号弹窗（点遮罩 / 取消 / 右上角 × 共用） */
  const closeBilling = () => {
    setDraft(null);
    setPickerOpen(false);
  };

  /** 弹框「保存」：草稿落库立即生效（纯账号池调整，不需要重启） */
  const doSaveBilling = async () => {
    if (draft == null) return;
    // 空池在设置里代表「全选」，无法表达「一个都不用」——所以这里必须拦住
    if (draft.length === 0) {
      onToast({ kind: "err", text: "至少保留一个账号参与扣费" });
      return;
    }
    const next = normalizeBilling(draft, accountIds);
    setBusy(true);
    setErr("");
    try {
      const saved = await saveSettings(patched({ billing_account_ids: next }));
      onSettings(saved);
      setDraft(null);
      setPickerOpen(false);
      onToast({ kind: "ok", text: "扣费账号已生效" });
    } catch (e) {
      onToast({ kind: "err", text: "保存失败：" + String(e) });
      await onReloadSettings().catch(() => undefined);
    } finally {
      setBusy(false);
    }
  };

  /** 限流切换弹框「保存」：草稿落库立即生效（纯模型白名单 + 换号开关，不需要动进程） */
  const doSaveModels = async () => {
    if (mdlDraft == null) return;
    const failover = mdlFailover ?? rlFailover;
    setBusy(true);
    setErr("");
    try {
      const saved = await saveSettings(
        patched({ rate_limit_models: mdlDraft, failover_on_rate_limit: failover })
      );
      onSettings(saved);
      setMdlDraft(null);
      setMdlFailover(null);
      setMdlOpen(false);
      onToast({ kind: "ok", text: "限流切换设置已生效" });
    } catch (e) {
      onToast({ kind: "err", text: "保存限流设置失败：" + String(e) });
      await onReloadSettings().catch(() => undefined);
    } finally {
      setBusy(false);
    }
  };

  /** 扣费账号弹框草稿里勾/去勾（基准：当前草稿；没开过弹窗时以生效池为准） */
  const toggleDraft = (id: string) =>
    setDraft((list) => {
      const base = list ?? effective;
      return base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
    });

  const live = stealth?.installed && stealth.alive;

  /**
   * 限流切换摘要（控制条上的按钮）：免费模型恒生效，付费模型按勾选数。
   * 关掉「限流时换号」时补一个后缀——那是一个会改变 429 行为的关键状态，
   * 不该只藏在弹窗里。
   *
   * 另外把**清单来源**也缀上去（`fetched` 除外，那是正常态）：按钮上那几个数字
   * 在「刚拉到」和「只有本机记录」两种情况下长得一模一样，而后者少得多。
   */
  const freeCount = fm?.models.filter((m) => m.free).length ?? 0;
  const modelSummary =
    fm == null
      ? "加载中…"
      : fm.source === "empty"
      ? "清单不可用"
      : (rlModels.length === 0
          ? `${freeCount} 个免费（默认）`
          : `${freeCount} 免费 · 付费 ${rlModels.length}`) +
        (SOURCE_SHORT[fm.source] ? ` · ${SOURCE_SHORT[fm.source]}` : "") +
        (rlFailover ? "" : " · 不换号");

  /**
   * 状态副文案。只说盘上为真的事：端点写没写进去、反代在不在听。
   * 「哪一次对话开始生效」只陈述机制（下一次会话重读产物），不替客户端下结论。
   */
  const stateText = switching
    ? "正在生效…"
    : live
    ? "已开启 · 下一次对话生效"
    : stealth?.installed
    ? "状态异常，请关闭后重新开启"
    : proxyOn
    ? "正在生效…"
    : "开启后按备选账号扣费";

  /**
   * 连续相同（类型 + 内容都一样）的事件聚合为一条，附重复次数。
   * 事件流是「新的在前」，相邻即时间连续——重启风暴、心跳重复这类刷屏只会占一行。
   * 不再截断末尾：后端已把日志限定在「一次接管会话」内，整段历史都值得看。
   */
  const groupedEvents = useMemo(() => {
    const out: { e: JournalEvent; count: number }[] = [];
    for (const e of events) {
      const last = out[out.length - 1];
      if (last && last.e.event === e.event && last.e.detail === e.detail) {
        last.count += 1;
      } else {
        out.push({ e, count: 1 });
      }
    }
    return out;
  }, [events]);

  // 弹窗里实际勾选的账号；打开那一刻从生效池复制，之后只改草稿
  const draftSel = draft ?? effective;
  const allPicked = accounts.length > 0 && draftSel.length === accounts.length;
  const somePicked = draftSel.length > 0 && !allPicked;
  // 摘要与 trae 同口径：全选时报总数，细选时报「已选 / 总数」
  const billingValue =
    billing.length === 0
      ? `全部 ${accounts.length}`
      : `已选 ${effective.length} / ${accounts.length}`;

  const isMac = /Mac/.test(navigator.userAgent);

  return (
    <section className="panel-page">
      {/* ── 主控制卡：一个开关 + 一个端口 + 两个弹窗入口。纯配置，不在页面上铺列表 ── */}
      <section className={`card hero${live ? " live" : ""}`}>
        <div className="hero-top">
          <div className="hero-copy">
            <span className="hero-icon">
              <IconShield size={19} />
            </span>
            <div className="hero-text">
              <div className="hero-title">
                <h2>接管 Qoder 请求</h2>
                {/* 常驻一整段机制说明会把标题区撑成文档；收进悬浮层，要核对时再展开 */}
                <span
                  className="rule-hint"
                  role="note"
                  tabIndex={0}
                  aria-label="选号规则：每个会话绑定一个账号；新会话按剩余积分从多到少选；触发限流时临时换号"
                >
                  <IconInfo size={16} />
                  <span className="rule-hint-pop" role="tooltip">
                    <strong>选号与生效规则</strong>
                    <span>每个会话固定一个账号；新会话按剩余积分从多到少选。</span>
                    <span>触发限流时临时换号重试，不改变当前会话。</span>
                    <span>改动在下一次对话生效，无需重启。</span>
                  </span>
                </span>
              </div>
              <span className={`hero-sub${live ? " ok" : ""}`} title={stealth?.note}>
                {stateText}
              </span>
            </div>
          </div>
          <Switch
            size="lg"
            checked={proxyOn}
            disabled={busy}
            title={
              proxyOn
                ? "关闭接管：Qoder 的下一次对话起恢复官方直连（无需重启）"
                : "开启接管：Qoder 的下一次对话起按备选账号扣费（无需重启）"
            }
            onChange={(v) => void doToggle(v)}
          />
        </div>

        <div className="hero-settings">
          <div className="hero-field">
            <span className="field-label">端口</span>
            <input
              className="port-input"
              type="number"
              inputMode="numeric"
              min={1024}
              max={65535}
              value={proxyPort}
              disabled={proxyOn || busy}
              title={
                proxyOn
                  ? "接管开启期间不允许修改端口；请先关闭接管"
                  : "本地反代监听端口"
              }
              onChange={(e) => setPortDraft(e.target.value)}
              onBlur={() => void onPortBlur()}
              onKeyDown={(e) => {
                if (e.key === "Enter") e.currentTarget.blur();
              }}
            />
          </div>

          {accounts.length > 0 && (
            <div className="hero-field">
              <span
                className="field-label"
                title="未勾选的账号不参与扣费；新会话按剩余积分从多到少选"
              >
                参与扣费
              </span>
              <button
                className="billing-trigger"
                disabled={busy}
                title="选择参与扣费的账号"
                onClick={openBilling}
              >
                <IconUser size={14} />
                {billingValue}
                <span className="billing-caret" />
              </button>
            </div>
          )}

          <div className="hero-field">
            <span
              className="field-label"
              title="0 积分模型默认享受 429 无感换号；付费模型勾选后同样生效"
            >
              限流切换
            </span>
            <button
              className="billing-trigger"
              disabled={busy}
              title="选择触发限流时自动换号重试的模型"
              onClick={openModelPicker}
            >
              <IconBolt size={14} />
              {modelSummary}
              <span className="billing-caret" />
            </button>
          </div>
        </div>

        {/* 「App 管理」是 macOS 专有授权：写官方客户端的产物要它放行，而本应用是
            **固定自签证书**签的 —— 系统只拦截、**永远不弹授权框**。
            非 macOS 不显示：那张面板只存在于 macOS。 */}
        {isMac && (
          <p className="hero-note">
            <IconAlertTriangle size={14} />
            <span>
              macOS 首次开启需授权：到「系统设置 → 隐私与安全性 → App 管理」打开本应用，再重启。
              <button
                type="button"
                className="link-btn"
                onClick={() => {
                  openAppManagement().catch((e) => {
                    onToast({ kind: "err", text: String(e) });
                  });
                }}
              >
                打开「App 管理」设置
              </button>
            </span>
          </p>
        )}
      </section>

      {err && <p className="form-err">{err}</p>}

      {/* ── 接管动态：只说用户需要知道的（开关动作、每次对话的账号、异常） ── */}
      <section className="card">
        <div className="card-head">
          <h3>接管动态</h3>
          <span className="spacer" />
          <button
            className="icon-btn"
            title="查看调试日志：每个请求的路径、鉴权、上游状态都记在里面"
            onClick={() => void doRevealLog()}
          >
            <IconFile size={15} />
          </button>
          <button
            className="icon-btn"
            disabled={feedBusy}
            title="重新拉取接管状态与动态"
            onClick={() => void doRefreshFeed()}
          >
            <IconRefresh size={15} />
          </button>
          <button
            className="icon-btn icon-danger"
            disabled={events.length === 0}
            title="清空界面上的通知（调试日志不受影响）"
            onClick={() => void doClearEvents()}
          >
            <IconTrash size={15} />
          </button>
        </div>

        {events.length === 0 ? (
          <div className="feed-empty">暂无动态。开启接管并对话后，这里会显示每次对话使用的账号。</div>
        ) : (
          <ul className="feed">
            {groupedEvents.map(({ e, count }, i) => {
              const k = eventKind(e);
              return (
                <li
                  key={`${e.at_ms}-${i}`}
                  className={`feed-item k-${k.cls}`}
                  title={count > 1 ? `相同事件连续出现 ${count} 次` : undefined}
                >
                  <span className="feed-dot" />
                  <span className="feed-time">{shortTime(e)}</span>
                  <span className="feed-label">{k.label}</span>
                  {count > 1 && <span className="feed-count">×{count}</span>}
                  <span className="feed-detail">{e.detail}</span>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      {/* ── 扣费账号弹框（草稿制：点「保存」才生效） ── */}
      {pickerOpen && (
        <Dialog
          size="md"
          icon={<IconUser size={16} />}
          title="参与扣费"
          subtitle="未勾选的账号不参与扣费；全选则新账号自动参与。"
          label="选择参与扣费的账号"
          onClose={closeBilling}
          tools={
            <div className="billing-tools">
              <label className="billing-all">
                <input
                  type="checkbox"
                  checked={allPicked}
                  disabled={accounts.length === 0}
                  ref={(el) => {
                    if (el) el.indeterminate = somePicked;
                  }}
                  onChange={() =>
                    setDraft(allPicked ? [] : accountIds)
                  }
                />
                全选
              </label>
              <span className="billing-count">
                已选 {draftSel.length} / {accounts.length}
              </span>
            </div>
          }
          footer={
            <>
              <button className="btn" onClick={closeBilling}>
                取消
              </button>
              <button
                className="btn primary"
                disabled={busy || draft == null || draftSel.length === 0}
                title={
                  draftSel.length === 0 ? "至少保留一个账号参与扣费" : undefined
                }
                onClick={() => void doSaveBilling()}
              >
                {busy ? "保存中…" : "保存"}
              </button>
            </>
          }
        >
          {accounts.length === 0 ? (
            <p className="empty">
              还没有账号。先到「账号签到」页登录或导入。
            </p>
          ) : (
            <div className="billing-list">
              {accounts.map((a) => {
                const picked = draftSel.includes(a.id);
                return (
                  <label
                    key={a.id}
                    className={"billing-row" + (picked ? " picked" : "")}
                  >
                    <input
                      type="checkbox"
                      checked={picked}
                      onChange={() => toggleDraft(a.id)}
                    />
                    {/* 用全应用统一的账号单元格（头像 + 名称 + 展示标识）——
                        与表格里的账号列长得一样，才不用在名单里重新认人 */}
                    <AccountCell
                      name={a.name}
                      ident={accountIdent(a.phone, a.email)}
                    />
                    <span className="billing-meta">{creditMeta(a)}</span>
                  </label>
                );
              })}
            </div>
          )}
        </Dialog>
      )}

      {/* ── 限流切换模型弹框（草稿制：点「保存」才生效） ── */}
      {mdlOpen && (
        <Dialog
          size="md"
          icon={<IconBolt size={16} />}
          title="限流切换"
          subtitle="选中模型触发限流时自动换号重试，不改变当前会话。"
          label="限流切换"
          fixedBodyHeight={420}
          onClose={closeModelPicker}
          tools={
            <button
              className="btn small"
              disabled={fmBusy}
              onClick={() => void loadFreeModels(true)}
            >
              {fmBusy ? "刷新中…" : "刷新"}
            </button>
          }
          footer={
            <>
              <button className="btn" onClick={closeModelPicker}>
                取消
              </button>
              <button
                className="btn primary"
                disabled={busy || mdlDraft == null}
                onClick={() => void doSaveModels()}
              >
                {busy ? "保存中…" : "保存"}
              </button>
            </>
          }
        >
          <Row
            title="限流时临时换号重试"
            desc="关闭后限流响应原样透传，不冷却、不换号。"
            ctrl={
              <Toggle
                checked={mdlFailover ?? rlFailover}
                onChange={setMdlFailover}
                title="会话内不换号 = 调用凭证稳定"
              />
            }
          />
          <p className="note">
            <IconInfo size={14} />
            <span>免费模型默认生效且不可取消；付费模型需勾选。</span>
          </p>
          {fm && <p className="modal-meta">来源：{SOURCE_LABEL[fm.source]}</p>}
          {fm == null ? (
            <p className="empty">加载中…</p>
          ) : fm.models.length === 0 ? (
            <p className="empty">
              暂未获取到模型，请确认 Qoder 已登录并运行过对话，再点「刷新」。
            </p>
          ) : (
            <ul className="pick-list">
              {fm.models.map((m) => {
                const checked = m.free || (mdlDraft ?? rlModels).includes(m.id);
                return (
                  <li
                    key={m.id}
                    className={
                      "pick-item" +
                      (m.free ? " locked" : checked ? " picked" : "")
                    }
                    title={
                      m.free
                        ? "0 积分免费模型，恒享受限流切换，不可取消"
                        : "勾选后该付费模型也享受 429 无感换号"
                    }
                    onClick={() => {
                      if (!m.free) toggleDraftModel(m.id);
                    }}
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      disabled={m.free}
                      onChange={() => {
                        if (!m.free) toggleDraftModel(m.id);
                      }}
                      onClick={(e) => e.stopPropagation()}
                    />
                    <span className="pick-main">
                      {/* 有显示名就把名字放主行、id 退到副行；没有名字时只显示 id
                          —— 缺名字就是缺，不编一个出来（后端取不到 name 会留空串） */}
                      <span className={"pick-name" + (m.name ? "" : " mono")}>
                        {m.name || m.id}
                      </span>
                      {m.name && <div className="pick-sub mono">{m.id}</div>}
                    </span>
                    <span className="pick-tail">
                      {/* 倍率拿不到时说「倍率未知」，不说「付费」—— 本机痕迹那层给不出倍率，
                          谎称付费会让人以为这些模型是收费的 */}
                      <span
                        className={"pick-state " + (m.free ? "ok" : "warn")}
                      >
                        {/* 活动价要把原价划出来：光看 x0.2 分不清常价还是错峰折扣 */}
                        {m.original_multiplier && (
                          <s className="pick-was">{m.original_multiplier}</s>
                        )}
                        {m.free ? "免费 · 默认" : m.multiplier || "倍率未知"}
                      </span>
                    </span>
                  </li>
                );
              })}
            </ul>
          )}
        </Dialog>
      )}
    </section>
  );
}
