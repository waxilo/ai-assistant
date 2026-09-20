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
import { AccountCell } from "../common";
import { regionLabel, useRegions } from "../regions";
import type { ConfirmReq, Toast } from "../common";
import { IconBolt, IconInfo, IconUser } from "../components/Icons";
import { Row, Toggle } from "../components/SettingsControls";
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
 *
 * 池 = 当前区域的账号。设置本身按区域各存一份，切走再切回来勾选集原样还在，
 * 所以这里不需要（也不应该）在换区域时做任何清理。
 */
function normalizeBilling(list: string[], pool: string[]): string[] {
  return pool.length > 0 && list.length === pool.length ? [] : list;
}

/**
 * 模型清单来源的展示文案，与后端那三层一一对应。
 *
 * 用表而不是嵌套三元：来源现在有四档，三元链会写成连自己都数不清的缩进。
 */
const SOURCE_LABEL: Record<ModelReport["source"], string> = {
  fetched: "刚从 Qoder 模型目录拉取",
  cache: "落盘快照（上次成功拉取的结果）",
  local: "本机 Qoder 的记录（只含这台机器用过的模型）",
  empty: "三层都没拿到",
};

/**
 * 控制条那颗胶囊上的**短**来源后缀。
 *
 * `fetched` 故意缺席：那是正常态，不必在胶囊里复述一遍。
 * 其余三档必须说出来 —— 否则胶囊上那串数字看着和「刚拉到的」一模一样，
 * 而实际上少得多（本机记录只含用过的模型）。
 */
const SOURCE_SHORT: Partial<Record<ModelReport["source"], string>> = {
  cache: "快照",
  local: "本机记录",
};

/**
 * 「智能接管」页：顶部一条紧凑控制条（开关 + 状态 + 扣费账号 / 限流切换两颗摘要胶囊 + 端口），
 * 下方「接管动态」铺满剩余空间（列表内部滚动、滚动条隐藏）。
 * 多选类配置一律收进弹框、不在页面上直接铺开，避免把事件流挤没：
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
  onSwitchRegion,
}: {
  settings: Settings;
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 保存后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  /** 从磁盘重读设置：任何一次操作失败后都要用它把界面拉回与后端一致 */
  onReloadSettings: () => Promise<void>;
  onToast: (t: Toast) => void;
  /**
   * 请求切换「当前区域」—— 本页不再自己切：切区域是全局动作（左下角选择器的职责），
   * 且接管开启中是一律禁止的（产品决定）。这里只把空态提示那颗按钮转交给同一个
   * 处理器，让它带上全局的闸门（proxy 检查、防连点、失败回读）。
   */
  onSwitchRegion: (key: string) => void;
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
  const region = settings.takeover_region;
  const regionOpts = useRegions();
  const billing = settings.billing_account_ids;
  const rlModels = settings.rate_limit_models;
  const rlFailover = settings.failover_on_rate_limit;
  const proxyOn = settings.proxy_enabled;
  const proxyPort = portDraft ?? String(settings.proxy_port || 8789);
  const [pickerOpen, setPickerOpen] = useState(false);
  // 弹框草稿：打开时复制当前生效值，点「保存」才落库生效
  const [draft, setDraft] = useState<string[] | null>(null);
  const [query, setQuery] = useState("");
  const [stealth, setStealth] = useState<StealthStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [busy, setBusy] = useState(false);
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

  /**
   * 本区域的账号。反代只在这个区域里选号扣费（跨区域的 token 在对方网关上无效），
   * 所以扣费池与模型池的口径都按它算，界面上也就只该列它。
   */
  const regionAccounts = useMemo(
    () => accounts.filter((a) => a.region === region),
    [accounts, region]
  );
  const regionIds = useMemo(
    () => regionAccounts.map((a) => a.id),
    [regionAccounts]
  );
  /** 区域标识 → 中文名（清单还没到货时退回显示标识本身，不留空） */
  const labelOf = (key: string) => regionLabel(regionOpts, key) ?? key;
  /**
   * 实际生效的扣费池。
   *
   * 语义没变（空设置 = 全选），但口径收到**本区域**：设置里可能残留另一个区域的账号 id
   * （换区域之前选的），那些 id 对现在的代理毫无意义 —— 留着会让弹窗里的勾选状态
   * 与真实扣费池对不上。本区域一个都没指定时，同样按「全选本区域」处理。
   */
  const effective = useMemo(() => {
    const scoped = billing.filter((id) => regionIds.includes(id));
    return scoped.length === 0 ? regionIds : scoped;
  }, [billing, regionIds]);

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
   * 拉取模型列表（控制条摘要 + 限流切换弹窗共用）：打开这一页时拉，**换区域时重拉** ——
   * 两个区域的模型目录不在同一个域上，清单本来就该跟着区域走。
   *
   * 刻意不把 `loadFreeModels` 写进依赖：它还依赖 `onToast` 的引用，而那个引用只要外层
   * 重渲染就会变，会把「换区域时拉一次」变成「每次重渲染都拉一次」。
   */
  useEffect(() => {
    void loadFreeModels(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [region]);

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
        setFm(await freeModels(refresh, region));
      } catch (e) {
        onToast({ kind: "err", text: "拉取模型列表失败：" + String(e) });
      } finally {
        setFmBusy(false);
      }
    },
    [onToast, region]
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
   * 开关即拨即用。
   *
   * **不碰官方客户端进程**：Qoder 的推理进程是每次会话按需起的一次性 `--print` 进程，
   * 端点由它在启动时读客户端配置决定 —— 所以写配置就够了，正在登录的账号、
   * 正在进行的对话都不受影响（详见 `commands.rs` 顶部那段说明）。
   */
  const doToggle = async (next: boolean) => {
    const action = next ? "开启接管" : "关闭接管";
    const ok = await askConfirm({
      title: action,
      body: next
        ? "接管开启后，Qoder 的下一次对话将按备选账号扣费。" +
          "端点写进客户端配置即生效，**不需要重启或退出 Qoder**，" +
          "正在登录的账号与正在进行的对话都不受影响。现在开启吗？"
        : "接管关闭后，Qoder 的下一次对话恢复直连官方。" +
          "端点会从客户端配置里摘掉，同样**不需要重启 Qoder**。现在关闭吗？",
      okText: action,
    });
    if (!ok) return; // 取消：开关状态不动
    setBusy(true);
    setErr("");
    try {
      const saved = await applySettings(patched({ proxy_enabled: next }));
      onSettings(saved);
      onToast({
        kind: "ok",
        text: next
          ? "接管已开启，下一次对话生效"
          : "接管已关闭，下一次对话恢复直连",
      });
      await refreshStealth();
    } catch (e) {
      await resync(e);
    } finally {
      setBusy(false);
    }
  };

  // 这里曾有 `onChangeRegion`：本页自己切区域（开着接管走安全切换流程、关着走
  // saveSettings）。两个理由让它退役了：① 产品决定「接管开启中不允许切换区域」；
  // ② 设置按区域各存一份之后，「带着当前区域取值的视图存进另一区域」会把对方的
  // 设置整个盖掉 —— 后端已把 save_settings / apply_settings 两条路的改区域一律拒掉，
  // 切区域只剩左下角选择器一条路（`set_region`，只动全局指针不碰切片）。

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

  /** 弹框「保存」：草稿落库立即生效（纯账号池调整，不需要重启） */
  const doSaveBilling = async () => {
    if (draft == null) return;
    const next = normalizeBilling(draft, regionIds);
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

  /** 扣费账号弹框草稿里勾/去勾（基准：草稿为空视为「本区域全选」） */
  const toggleDraft = (id: string) =>
    setDraft((list) => {
      const base = list == null ? effective : list.length === 0 ? regionIds : list;
      return base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
    });

  const live = stealth?.installed && stealth.alive;

  /** 扣费账号摘要（控制条上的那颗胶囊按钮）。数字的口径是**本区域**（见 `effective`） */
  const billingSummary =
    regionAccounts.length === 0
      ? "暂无账号"
      : billing.length === 0
      ? `全部 ${regionAccounts.length} 个（默认）`
      : `已选 ${effective.length} · 未选 ${
          regionAccounts.length - effective.length
        }`;

  /**
   * 限流切换摘要（控制条上的胶囊按钮）：免费模型恒生效，付费模型按勾选数。
   * 关掉「会话内换号」时补一个后缀——那是一个会改变 429 行为的关键状态，
   * 不该只藏在弹窗里。
   *
   * 另外把**清单来源**也缀上去（`fetched` 除外，那是正常态）：胶囊上那几个数字
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
   * 「接管区域上还没有账号」的提示（没有则为 null）。
   *
   * 只在**当前区域 0 个账号、而另一个区域有**时给出来 —— 那正是「用户只登了一边、
   * 而设置里躺着缺省值」的形态：这一页于是什么都没得选（扣费池是空的，模型清单
   * 只能靠本机痕迹），看起来像功能坏了。
   *
   * 只提示 + 给一键切换，**不悄悄改设置**：接管区域决定端点写进哪一套客户端的配置，
   * 那是用户的意图（见 `Settings::takeover_region`），不该由「哪个区域碰巧有账号」去推断。
   */
  const regionNudge = useMemo(() => {
    if (regionAccounts.length > 0) return null;
    const n = (key: string) => accounts.filter((a) => a.region === key).length;
    return (
      regionOpts
        .filter((r) => r.key !== region && n(r.key) > 0)
        .map((r) => ({ key: r.key, label: r.label, n: n(r.key) }))
        // 两个区域都有账号时取多的那个：那是用户实际在用的部署
        .sort((a, b) => b.n - a.n)[0] ?? null
    );
  }, [regionAccounts.length, regionOpts, accounts, region]);

  /** 状态副文案。只说盘上为真的事：端点写没写进去、反代在不在听。 */
  const stateText = live
    ? "配置已就绪：端点已写入，本地反代正在监听"
    : stealth?.installed
    ? "状态异常：关闭开关即可把配置还原成直连"
    : proxyOn
    ? "应用中：正在写入端点配置（不重启 Qoder）"
    : "开启后会把端点写进客户端配置，按备选账号分流扣费";

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

  /** 弹框内按用户名 / 手机号过滤（范围内本来就只有本区域的账号） */
  const filteredAccounts = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return regionAccounts;
    return regionAccounts.filter(
      (a) =>
        a.name.toLowerCase().includes(q) || (a.phone ?? "").includes(q)
    );
  }, [regionAccounts, query]);

  const draftEffective =
    draft == null ? effective : draft.length === 0 ? regionIds : draft;

  return (
    <section className="panel-page tk-page">
      <p className="set-intro">
        <IconInfo size={14} />
        <span>
          开启后 Qoder 的对话请求由本地代理转发，按「积分最早过期优先」在账号间分配扣费；
          下方记录每一次开关、路由与异常。
          <b>接管作用于当前区域</b>（跟随后台左下角的全局区域选择器）：端点写进那一套客户端的 worker 产物，
          扣费也只在该区域的账号里选（跨区域的 token 在对方网关上无效）。
          <b>全程不重启 Qoder</b>：Qoder 每次会话自己起一次性推理进程，会重新读一遍产物，
          所以改完下一次对话就生效，正在登录的账号与正在进行的对话都不受影响。
          <b>本机 TLS</b>：端点被客户端强制成 https，所以反代会用一张只签给 127.0.0.1 的
          自签证书终止 TLS；这张 CA 随注入一起写进产物，<b>不改系统信任库、不需要管理员</b>。
          <b>官方客户端更新会覆盖注入</b>，本应用每次心跳都会复查文件指纹并自动重打；
          想恢复原样就在上面关掉开关（注入会被逐字节剥离）。
          {/*
            「App 管理」是 macOS 专有授权：写官方客户端的产物要它放行，而本应用是
            **固定自签证书**签的 —— 系统只拦截、**永远不弹授权框**（只往 tccd 记一条
            拒绝）。这一步绕不过去，那就别让用户再去搜「在哪」。
            非 macOS 不显示：那张面板只存在于 macOS。
          */}
          {/Mac/.test(navigator.userAgent) && (
            <>
              {" "}
              <b>macOS 首次开启要手动授权一次</b>
              ：「App 管理」只拦截、<b>不会弹授权框</b>（本应用用固定自签证书签名），
              去「系统设置 → 隐私与安全性 → App 管理」把本应用打开，再重启本应用即可。
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
            </>
          )}
        </span>
      </p>

      {/* ── 紧凑控制条：开关 + 状态 + 扣费账号 / 限流切换（弹窗入口）+ 端口 ── */}
      <div className={`card tk-bar ${live ? "live" : ""}`}>
        <label
          className="switch"
          title="开启后 Qoder 的对话请求将由本地代理分流扣费"
        >
          <input
            type="checkbox"
            checked={proxyOn}
            disabled={busy}
            onChange={(e) => void doToggle(e.target.checked)}
          />
          <span className="track">
            <span className="thumb" />
          </span>
        </label>
        <div className="tk-bar-text" title={stealth?.note}>
          <strong>{proxyOn ? "接管已开启" : "接管已关闭"}</strong>
          <span className={`tk-bar-sub ${live ? "ok" : ""}`}>{stateText}</span>
        </div>
        <span className="spacer" />
        <button
          className="tk-accts"
          title={`只有${labelOf(region)}的账号允许被扣费（跨区域的 token 在对方网关上无效）；点击细选`}
          onClick={() => {
            setDraft(billing);
            setQuery("");
            setPickerOpen(true);
          }}
        >
          <span className="ta-label">扣费账号</span>
          <span className="ta-value">{billingSummary}</span>
          <span className="ta-edit">选择</span>
        </button>
        <button
          className="tk-accts"
          title="0 积分模型默认享受 429 无感换号；付费模型在此勾选后同样生效"
          onClick={openModelPicker}
        >
          <span className="ta-label">限流切换</span>
          <span className="ta-value">{modelSummary}</span>
          <span className="ta-edit">选择</span>
        </button>
        <label
          className="tk-field"
          title={proxyOn ? "接管开启期间不允许修改端口；请先关闭接管" : "代理监听端口"}
        >
          端口
          <input
            type="number"
            value={proxyPort}
            min={1024}
            max={65535}
            disabled={proxyOn || busy}
            onChange={(e) => setPortDraft(e.target.value)}
            onBlur={() => void onPortBlur()}
          />
        </label>
      </div>

      {err && <p className="form-err">{err}</p>}

      {/* ── 接管区域上还没有账号：说清这一页为什么没得选，并给一键换到有账号的那边 ── */}
      {regionNudge && (
        <p className="tk-nudge">
          <IconInfo size={14} />
          <span>
            <b>{labelOf(region)}</b> 下还没有账号：扣费池是空的，模型清单也只能靠本机痕迹。
          </span>
          <span className="spacer" />
          <button
            className="btn small"
            disabled={proxyOn}
            title={proxyOn ? "接管开启中不能切换区域：请先关闭接管" : undefined}
            onClick={() => onSwitchRegion(regionNudge.key)}
          >
            切到{regionNudge.label}（{regionNudge.n} 个账号）
          </button>
        </p>
      )}

      {/* ── 接管动态：铺满剩余空间，列表内部滚动（滚动条隐藏） ── */}
      <div className="card tk-feed">
        <div className="card-head">
          <h3>接管动态</h3>
          <span className="card-head-sub">
            只说你需要知道的：开关动作、每次对话用的哪个账号、异常。请求级细节在调试日志里
          </span>
          <span className="spacer" />
          <button
            className="btn small ghost"
            title="在文件管理器里定位调试日志（每个请求的路径、鉴权、上游状态都记在里面）"
            onClick={() => void doRevealLog()}
          >
            查看日志
          </button>
          <button
            className="btn small ghost"
            disabled={feedBusy}
            title="重新拉取接管状态与动态"
            onClick={() => void doRefreshFeed()}
          >
            {feedBusy ? "刷新中…" : "刷新"}
          </button>
          <button
            className="btn small ghost"
            disabled={events.length === 0}
            title="清空界面上的通知（调试日志不受影响）"
            onClick={() => void doClearEvents()}
          >
            清空
          </button>
        </div>
        {events.length === 0 ? (
          <p className="hint">
            暂无通知。开启接管并对话后，这里会出现「本次对话由账号 X 提供」这类记录；
            要查某个请求具体怎么被处理的，点「查看日志」。
          </p>
        ) : (
          <ul className="evt-list">
            {groupedEvents.map(({ e, count }, i) => {
              const k = eventKind(e);
              return (
                <li
                  key={`${e.at_ms}-${i}`}
                  className={`evt evt-${k.cls}`}
                  title={count > 1 ? `相同事件连续出现 ${count} 次` : undefined}
                >
                  <span className="e-at">{e.at}</span>
                  <span className={`e-tag tag-${k.cls}`}>{k.label}</span>
                  {count > 1 && <span className="e-count">×{count}</span>}
                  <span className="e-detail">{e.detail}</span>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      {/* ── 扣费账号选择弹框（草稿制：点「保存」才生效） ── */}
      {pickerOpen && (
        <Dialog
          size="md"
          icon={<IconUser size={16} />}
          title="选择扣费账号"
          label="选择扣费账号"
          onClose={() => {
            setDraft(null);
            setPickerOpen(false);
          }}
          tools={
            <button
              className="btn small"
              onClick={() => setDraft(regionIds)}
              disabled={regionAccounts.length === 0}
            >
              全部勾选
            </button>
          }
          footer={
            <>
              <button
                className="btn"
                onClick={() => {
                  setDraft(null);
                  setPickerOpen(false);
                }}
              >
                取消
              </button>
              <button
                className="btn primary"
                disabled={busy || draft == null}
                onClick={() => void doSaveBilling()}
              >
                {busy ? "保存中…" : "保存"}
              </button>
            </>
          }
        >
          <p className="note">
            <IconInfo size={14} />
            <span>
              勾选的账号才允许被扣费（会话粘滞 + 积分最早过期优先轮换），未勾选的账号会被排除；
              默认全部勾选（智能轮换）。点「保存」立即生效，无需重启。
              这里<b>只列「{labelOf(region)}」的账号</b>：反代只在该区域里选号扣费，
              另一个区域的账号召上来也收不到请求。
            </span>
          </p>
          {/* 一个普通的文本输入框：`.modal input` 已经给了外观，
              上下间距由 `.modal-body > * + *` 统一负责，不再需要专属 class */}
          <input
            type="text"
            placeholder="按用户名或手机号过滤…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          {regionAccounts.length === 0 ? (
            <p className="empty">
              还没有{labelOf(region)}的账号。先到「账号签到」页登录或导入，
              或在左下角把区域切成另一个版本。
            </p>
          ) : filteredAccounts.length === 0 ? (
            <p className="empty">没有匹配「{query}」的账号。</p>
          ) : (
            <ul className="pick-list">
              {filteredAccounts.map((a) => {
                const picked = draftEffective.includes(a.id);
                return (
                  <li
                    key={a.id}
                    className={"pick-item" + (picked ? " picked" : "")}
                    onClick={() => toggleDraft(a.id)}
                  >
                    <input
                      type="checkbox"
                      checked={picked}
                      onChange={() => toggleDraft(a.id)}
                      onClick={(e) => e.stopPropagation()}
                    />
                    {/* 用全应用统一的账号单元格（头像 + 名称 + 手机号）——
                        此前这里是手拼的两段文字，与表格里的账号列长得不一样 */}
                    <AccountCell name={a.name} phone={a.phone} />
                    <span className="pick-tail">
                      <span className="pick-state">
                        {picked ? "可扣费" : "已排除"}
                      </span>
                    </span>
                  </li>
                );
              })}
            </ul>
          )}
        </Dialog>
      )}

      {/* ── 限流切换模型弹框（草稿制：点「保存」才生效） ── */}
      {mdlOpen && (
        <Dialog
          size="md"
          icon={<IconBolt size={16} />}
          title="限流切换"
          label="限流切换"
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
          <p className="note">
            <IconInfo size={14} />
            <span>
              选中的模型触发限流（429）时，代理会将该账号冷却 10 分钟、自动换备用账号重发同一请求，
              对话完全无感；换号按「积分最早过期」优先（先消耗快过期的额度）。
              0 积分（免费）模型默认全部生效、不可取消；付费模型勾选后同样生效。
              清单的来源依次是：Qoder 模型目录（联网，缓存 1 小时）→ 落盘快照 →
              本机 Qoder 的记录。该接口实测「对常规客户端不开放」（国际版 404、国内版 503），
              所以多数时候给的是后两层 —— 那不是这台机器的网络问题，点「刷新」也一样。
              清单<b>跟着左下角选的「{labelOf(region)}」区域走</b> ——
              两个区域的模型目录不在同一个域上，缓存也是分开存的。
            </span>
          </p>
          <Row
            title="限流时在同一会话内换号"
            desc="关掉后 429 原样透传给客户端，不冷却、不换号——同一会话自始至终只用一个账号。风控视角下「一个会话中途换凭证」是极高异常值，代价是这种情况要等上游自己解除限流。"
            ctrl={
              <Toggle
                checked={mdlFailover ?? rlFailover}
                onChange={setMdlFailover}
                title="会话内不换号 = 调用凭证稳定"
              />
            }
          />
          {fm && <p className="modal-meta">来源：{SOURCE_LABEL[fm.source]}</p>}
          {/* 第 1 层为什么没结果，与「来源」分开成一行：前者说的是「为什么不是刚拉到的」，
              后者说的是「这份清单来自哪一层」，塞进同一行两个都读不清 */}
          {fm?.note && (
            <p className="tk-why">
              <IconInfo size={13} />
              <span>{fm.note}</span>
            </p>
          )}
          {fm == null ? (
            <p className="empty">加载中…</p>
          ) : fm.models.length === 0 ? (
            <p className="empty">
              {regionAccounts.length === 0
                ? `「${labelOf(region)}」下还没有账号，清单只能靠本机痕迹，而这台机器也没留下记录。` +
                  "先在「账号签到」页登录一个该区域的账号，或在左下角把区域切成另一个版本。"
                : "暂未发现模型：Qoder 模型目录、落盘快照、本机记录三层都没拿到。" +
                  "确认 Qoder 已登录、且本机跑过一次对话，再点右上角「刷新」。"}
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
                      {/* 免费模型：一个胶囊说清「免费 + 默认生效」——
                          旧实现是「免费」胶囊 + 「默认」两个元素，说的是同一件事 */}
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
