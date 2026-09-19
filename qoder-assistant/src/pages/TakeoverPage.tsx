import { useCallback, useEffect, useMemo, useState } from "react";
import type { Account, ModelReport, JournalEvent, Settings, StealthStatus } from "../types";
import {
  applySettings,
  takeoverEvents,
  clearTakeoverEvents,
  saveSettings,
  stealthStatus,
  freeModels,
} from "../api";
import { AccountCell } from "../common";
import { regionHint, regionLabel, useRegions } from "../regions";
import type { ConfirmReq, Toast } from "../common";
import { IconBolt, IconInfo, IconUser } from "../components/Icons";
import { Row, Toggle } from "../components/SettingsControls";
import { Dialog } from "../components/Dialog";

/** 事件类型 → 界面标签与配色 */
function eventKind(e: JournalEvent): {
  label: string;
  cls: "on" | "off" | "route" | "failover" | "restart" | "err";
} {
  switch (e.event) {
    case "install":
      return { label: "开启接管", cls: "on" };
    case "uninstall":
      return { label: "关闭接管", cls: "off" };
    case "route_start":
      return { label: "开始使用账号", cls: "route" };
    case "failover":
      return { label: "限流切换", cls: "failover" };
    case "restart_qoder":
      return { label: "重启 Qoder", cls: "restart" };
    case "proxy_upstream_error":
    case "proxy_stream_error":
    case "proxy_conn_setup_failed":
    case "proxy_bad_request":
      return { label: "代理错误", cls: "err" };
    // 客户端连上后迟迟不发请求头（此前会被误判成 400，现改为 408 + 本事件）
    case "proxy_head_stalled":
      return { label: "请求卡住", cls: "err" };
    default:
      return { label: "事件", cls: "restart" };
  }
}

/**
 * 全选归一：勾选集覆盖**整池**时存空（= 默认全选，之后新增的账号自动可扣费）。
 *
 * 池 = 接管目标区域的账号；换区域时会被清空（见 `onChangeRegion`）。
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
  onToast,
}: {
  settings: Settings;
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 保存后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  onToast: (t: Toast) => void;
}) {
  const [proxyOn, setProxyOn] = useState(settings.proxy_enabled);
  const [proxyPort, setProxyPort] = useState(String(settings.proxy_port || 8789));
  // 接管目标区域：这一页的「作用对象」—— 端点写进哪套客户端的配置、模型清单从哪个域拉、
  // 扣费账号在哪个池里选，全由它决定。改它属于拓扑变更，不是普通保存（见 onChangeRegion）。
  const [region, setRegion] = useState(settings.takeover_region);
  const regionOpts = useRegions();
  // 扣费备选池：空 = 默认全部勾选（智能轮换）；非空 = 只有勾选的账号允许扣费
  const [billing, setBilling] = useState<string[]>(settings.billing_account_ids);
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
  // 限流切换模型勾选：用户额外启用的付费模型（免费模型恒生效，不进这里）
  const [rlModels, setRlModels] = useState<string[]>(settings.rate_limit_models);
  // 「限流时在同一会话内换号」：与模型清单同一个弹窗、同样草稿制（点「保存」才落库）
  const [rlFailover, setRlFailover] = useState(settings.failover_on_rate_limit);
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
      body: "将删除全部接管事件记录（开启/关闭/账号启用/错误），此操作不可恢复。继续？",
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

  /** 组装一份以当前界面状态为准的设置 */
  const snapshot = (over?: Partial<Settings>): Settings => ({    ...settings,
    proxy_enabled: proxyOn,
    proxy_port: Number(proxyPort) || 8789,
    billing_account_ids: billing,
    ...over,
  });

  /** 开关即拨即用：确认后立即应用（开启/关闭都会安全重启 Qoder） */
  const doToggle = async (next: boolean) => {
    const action = next ? "开启接管" : "关闭接管";
    const ok = await askConfirm({
      title: `${action}并重启 Qoder`,
      body:
        `${action}需要重启 Qoder 与长驻 CLI host，才能安全清除旧端点。` +
        "代理会在整个切换过程中保持可用，不会留下死端口。现在继续吗？（请先保存未提交的输入）",
      okText: action,
    });
    if (!ok) return; // 取消：开关状态不动
    setBusy(true);
    setErr("");
    try {
      const saved = await applySettings(snapshot({ proxy_enabled: next }));
      onSettings(saved);
      setProxyOn(next);
      onToast({ kind: "ok", text: `已${action}，Qoder 已安全重启` });
      await refreshStealth();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  /**
   * 换区域：**属于拓扑变更**，不是普通保存。
   *
   * 换区域 = 换一套官方客户端来接管 —— 端点写进的配置文件（`~/.qoder` ↔ `~/.qoder-cn`）、
   * 反代的上游网关、以及扣费账号所在的那一池，全都跟着换。所以接管开着的时候必须走
   * 安全切换流程（摘掉旧区域的端点 → 重启受影响的客户端 → 把端点装进新区域）；
   * 关着的时候只是把设置存下来，不必惊动任何进程。
   *
   * 同时把扣费池清回「全选」：原来选的是另一个区域的账号 id，那些 id 在新区域里
   * 一个都不存在，留着会让代理选不出任何账号。
   */
  const onChangeRegion = async (next: string) => {
    if (next === region) return;
    if (proxyOn) {
      const ok = await askConfirm({
        title: `把接管切换到${labelOf(next)}`,
        body:
          `接管正开着，换区域需要先摘掉旧区域的端点并重启受影响的客户端，再把端点装进新区域的配置。` +
          `代理在整个过程中保持可用，不会留下死端口。` +
          `另外扣费账号会重置为「全部」—— 两个区域的账号互不通用。现在继续吗？（请先保存未提交的输入）`,
        okText: "切换区域",
      });
      if (!ok) return;
    }
    setBusy(true);
    setErr("");
    try {
      // 关着的时候走 saveSettings：此时没有任何端点装着，applySettings 会去动进程，
      // 而它无事可做（也没有要重启的理由）
      const patch = {
        takeover_region: next,
        billing_account_ids: [] as string[],
      };
      const saved = proxyOn
        ? await applySettings(snapshot(patch))
        : await saveSettings(snapshot(patch));
      onSettings(saved);
      setRegion(next);
      setBilling([]);
      if (proxyOn) await refreshStealth();
      onToast({
        kind: "ok",
        text: proxyOn
          ? `接管已切换到${labelOf(next)}，客户端已安全重启`
          : `接管区域已设为${labelOf(next)}，开启接管时生效`,
      });
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** 端口只在接管关闭时可改；失焦时若变了就立即落盘（纯配置，无需重启） */
  const onPortBlur = async () => {
    const port = Number(proxyPort) || 8789;
    if (proxyOn || port === settings.proxy_port) return;
    try {
      const saved = await saveSettings(snapshot({ proxy_port: port }));
      onSettings(saved);
      onToast({ kind: "ok", text: "端口已保存" });
    } catch (e) {
      onToast({ kind: "err", text: "端口保存失败：" + String(e) });
    }
  };

  /** 弹框「保存」：草稿落库立即生效（纯账号池调整，不需要重启） */
  const doSaveBilling = async () => {
    if (draft == null) return;
    const next = normalizeBilling(draft, regionIds);
    setBusy(true);
    setErr("");
    try {
      const saved = await saveSettings(
        snapshot({ billing_account_ids: next })
      );
      onSettings(saved);
      setBilling(next);
      setDraft(null);
      setPickerOpen(false);
      onToast({ kind: "ok", text: "扣费账号已生效" });
    } catch (e) {
      onToast({ kind: "err", text: "保存失败：" + String(e) });
    } finally {
      setBusy(false);
    }
  };

  /** 限流切换弹框「保存」：草稿落库立即生效（纯模型白名单 + 换号开关，不需要重启） */
  const doSaveModels = async () => {
    if (mdlDraft == null) return;
    const failover = mdlFailover ?? rlFailover;
    setBusy(true);
    setErr("");
    try {
      const saved = await saveSettings(
        snapshot({ rate_limit_models: mdlDraft, failover_on_rate_limit: failover })
      );
      onSettings(saved);
      setRlModels(mdlDraft);
      setRlFailover(failover);
      setMdlDraft(null);
      setMdlFailover(null);
      setMdlOpen(false);
      onToast({ kind: "ok", text: "限流切换设置已生效" });
    } catch (e) {
      onToast({ kind: "err", text: "保存限流设置失败：" + String(e) });
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

  /**
   * 某个区域下的账号数。
   *
   * 直接标在区域下拉的选项里：两个区域各登了几个账号是**决定这一页有没有得选**的
   * 前提（扣费池、模型清单的口径都跟着区域走），不该等用户点开下拉再猜。
   */
  const countIn = (key: string) => accounts.filter((a) => a.region === key).length;

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

  /** 状态副文案 */
  const stateText = live
    ? "接管生效中，对话正按备选账号扣费"
    : stealth?.installed
    ? "状态异常：关闭开关即可恢复直连"
    : proxyOn
    ? "应用中：会安全重启 Qoder"
    : "开启后对话自动按备选账号分流扣费";

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
          <b>接管只作用于上面选的区域</b>：端点写进那一套客户端的配置，
          扣费也只在该区域的账号里选（跨区域的 token 在对方网关上无效）。
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
          title={
            regionHint(regionOpts, region) ??
            "接管哪一套部署的客户端（换区域会重启受影响的客户端）"
          }
        >
          区域
          <select
            value={region}
            disabled={busy || regionOpts.length === 0}
            onChange={(e) => void onChangeRegion(e.target.value)}
          >
            {/* 只在**没有账号**的那个区域上打标记：那才是「选了它就没得选」的情况，
                而给所有选项都缀上账号数会把下拉撑宽、把整条控制条挤到换行。
                正常态保持原样，异常态自己冒出来。 */}
            {regionOpts.map((r) => (
              <option key={r.key} value={r.key}>
                {countIn(r.key) > 0 ? r.label : `${r.label}（无账号）`}
              </option>
            ))}
          </select>
        </label>
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
            onChange={(e) => setProxyPort(e.target.value)}
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
            onClick={() => void onChangeRegion(regionNudge.key)}
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
            开启 / 关闭 / 每个会话用哪个账号 / 异常。开启接管时自动重置，本轮记录不会丢
          </span>
          <span className="spacer" />
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
            title="删除全部接管事件记录，不可恢复"
            onClick={() => void doClearEvents()}
          >
            清空
          </button>
        </div>
        {events.length === 0 ? (
          <p className="hint">
            暂无事件。开启接管并产生对话后，这里会记录每一次账号启用与开关动作。
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
              或把上面的「区域」换成另一个版本。
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
              清单<b>跟着上面选的「{labelOf(region)}」区域走</b> ——
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
                  "先在「账号签到」页登录一个该区域的账号，或把上面的「区域」换成另一个版本。"
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
