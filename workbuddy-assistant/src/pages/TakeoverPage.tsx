import { useCallback, useEffect, useMemo, useState } from "react";
import type { Account, FreeModelsReport, JournalEvent, Settings, StealthStatus } from "../types";
import {
  applySettings,
  takeoverEvents,
  clearTakeoverEvents,
  saveSettings,
  stealthStatus,
  freeModels,
} from "../api";
import { AccountCell, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";
import {
  IconAlertCircle,
  IconAlertTriangle,
  IconBolt,
  IconChevron,
  IconRefresh,
  IconShield,
  IconTrash,
  IconUsers,
} from "../components/Icons";
import { Row } from "../components/SettingsControls";
import Switch from "../components/Switch";
import { Dialog } from "../components/Dialog";
import { displayDetail, groupJournal } from "../journal";

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
    case "restart_workbuddy":
      return { label: "重启 WorkBuddy", cls: "restart" };
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

/** 全选归一：列表覆盖全部账号时存空（= 默认全选，新增账号自动可扣费） */
function normalizeBilling(list: string[], allIds: string[]): string[] {
  return allIds.length > 0 && list.length === allIds.length ? [] : list;
}

/**
 * 时间列只保留**必要的精度**：今天的事件给 `HH:MM:SS`，跨天的才带上 `MM-DD`。
 * （后端 `at` 已是本地时间串，`at_ms` 用来判断是不是今天。）
 */
function shortTime(e: JournalEvent): string {
  const hms = e.at.length >= 19 ? e.at.slice(11, 19) : e.at;
  const isToday = new Date(e.at_ms).toDateString() === new Date().toDateString();
  return isToday ? hms : `${e.at.slice(5, 10)} ${hms.slice(0, 5)}`;
}

/** 账号的积分摘要（扣费账号弹窗每行右侧）：余额 + 最早到期天数 */
function creditMeta(a: Account): string {
  const c = a.credits;
  if (!c || c.credits == null) return "积分未知";
  const now = Date.now();
  const soon = c.packages.find((p) => p.expiry_ms != null && p.expiry_ms > now);
  const days = soon ? Math.ceil((soon.expiry_ms! - now) / 86_400_000) : null;
  return `${formatCredits(c.credits)} 积分${days != null ? ` · ${days} 天后过期` : ""}`;
}

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
  const [proxyPort, setProxyPort] = useState(String(settings.proxy_port || 8787));
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
  // 模型清单：从网关动态拉取，弹窗列表与控制条摘要共用
  const [fm, setFm] = useState<FreeModelsReport | null>(null);
  const [fmBusy, setFmBusy] = useState(false);
  // 限流切换模型勾选：用户额外启用的付费模型（免费模型恒生效，不进这里）
  const [rlModels, setRlModels] = useState<string[]>(settings.rate_limit_models);
  // 「限流时在同一会话内换号」：与模型清单同一个弹窗、同样草稿制（点「保存」才落库）
  const [rlFailover, setRlFailover] = useState(settings.failover_on_rate_limit);
  const [mdlFailover, setMdlFailover] = useState<boolean | null>(null);

  const allIds = useMemo(() => accounts.map((a) => a.id), [accounts]);
  /** 实际生效的勾选集：未指定时视为全选 */
  const effective = billing.length === 0 ? allIds : billing;

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

  /** 接管页打开即拉取模型列表（控制条摘要 + 限流切换弹窗共用） */
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
      body: "将删除全部接管事件记录（开启/关闭/账号启用/错误），此操作不可恢复。继续？",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearTakeoverEvents();
      setEvents([]);
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
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

  /** 组装一份以当前界面状态为准的设置 */
  const snapshot = (over?: Partial<Settings>): Settings => ({    ...settings,
    proxy_enabled: proxyOn,
    proxy_port: Number(proxyPort) || 8787,
    billing_account_ids: billing,
    ...over,
  });

  /** 开关即拨即用：确认后立即应用（开启/关闭都会安全重启 WorkBuddy） */
  const doToggle = async (next: boolean) => {
    const action = next ? "开启接管" : "关闭接管";
    const ok = await askConfirm({
      title: `${action}并重启 WorkBuddy`,
      body:
        `${action}需要重启 WorkBuddy，才能安全清除旧端点。` +
        "重启期间代理保持可用，不会留下死端口。请先保存未提交的输入，现在继续吗？",
      okText: action,
    });
    if (!ok) return; // 取消：开关状态不动
    setBusy(true);
    setErr("");
    try {
      const saved = await applySettings(snapshot({ proxy_enabled: next }));
      onSettings(saved);
      setProxyOn(next);
      await refreshStealth();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** 端口只在接管关闭时可改；失焦时若变了就立即落盘（纯配置，无需重启） */
  const onPortBlur = async () => {
    const port = Number(proxyPort) || 8787;
    if (proxyOn || port === settings.proxy_port) return;
    try {
      const saved = await saveSettings(snapshot({ proxy_port: port }));
      onSettings(saved);
    } catch (e) {
      onToast({ kind: "err", text: "端口保存失败：" + String(e) });
    }
  };

  /** 弹框「保存」：草稿落库立即生效（纯账号池调整，不需要重启） */
  const doSaveBilling = async () => {
    if (draft == null) return;
    if (draft.length === 0) {
      onToast({ kind: "info", text: "至少保留一个账号" });
      return;
    }
    const next = normalizeBilling(draft, allIds);
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
    } catch (e) {
      onToast({ kind: "err", text: "保存限流设置失败：" + String(e) });
    } finally {
      setBusy(false);
    }
  };

  /** 扣费账号弹框草稿里勾/去勾（打开时已把「空 = 全部」展开成显式全选） */
  const toggleDraft = (id: string) =>
    setDraft((list) => {
      const base = list ?? effective;
      return base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
    });

  const live = stealth?.installed && stealth.alive;

  /**
   * 限流切换摘要（控制条上的胶囊按钮）：免费模型恒生效，付费模型按勾选数。
   * 关掉「会话内换号」时补一个后缀——那是一个会改变 429 行为的关键状态，
   * 不该只藏在弹窗里。
   */
  const freeCount = fm?.models.filter((m) => m.free).length ?? 0;
  const modelSummary =
    fm == null
      ? "加载中…"
      : (rlModels.length === 0
          ? `${freeCount} 个免费（默认）`
          : `${freeCount} 免费 · 付费 ${rlModels.length}`) +
        (rlFailover ? "" : " · 不换号");

  /** 状态副文案 */
  const stateText = live
    ? "接管生效中，对话正按备选账号扣费"
    : stealth?.installed
    ? "状态异常：关闭开关即可恢复直连"
    : proxyOn
    ? "应用中：会安全重启 WorkBuddy"
    : "开启后对话自动按备选账号分流扣费";

  /**
   * 事件流聚合（逐字相同的刷屏、以及同一账号本次接管服务过的会话数），见 [`groupJournal`]。
   * 不截断末尾：后端已把日志限定在「一次接管会话」内，整段历史都值得看。
   */
  const rows = useMemo(() => groupJournal(events), [events]);

  /** 弹框内按用户名 / 手机号过滤 */
  const filteredAccounts = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return accounts;
    return accounts.filter(
      (a) =>
        a.name.toLowerCase().includes(q) || (a.phone ?? "").includes(q)
    );
  }, [accounts, query]);

  const draftEffective = draft ?? effective;

  return (
    <section className="panel-page takeover-page">
      {/* ── 控制区：一个开关 + 一个端口 + 哪些账号参与扣费 / 哪些模型触发换号 ── */}
      <section className={`card hero${live ? " live" : ""}`}>
        <div className="hero-top">
          <div className="hero-copy">
            <span className="hero-icon">
              <IconShield size={19} />
            </span>
            <div className="hero-text">
              <div className="hero-title">
                <h2>接管 WorkBuddy 请求</h2>
                {/* 常驻一句规则说明会把标题区撑成一段文字；收进悬浮层，只有要核对时展开 */}
                <span
                  className="rule-hint"
                  role="note"
                  tabIndex={0}
                  aria-label="选号规则：优先使用最早到期的积分；同一天到期时待过期积分少者优先；关闭接管即恢复官方直连"
                >
                  <IconAlertCircle size={16} />
                  <span className="rule-hint-pop" role="tooltip">
                    <strong>选号规则</strong>
                    <span>优先使用最早到期的积分账号；同一天到期时，待过期积分少的优先。</span>
                    <span>剩余积分为 0 的账号不参与。</span>
                    <span>关闭接管即恢复官方直连。</span>
                  </span>
                </span>
              </div>
            </div>
          </div>
          <Switch
            size="lg"
            checked={proxyOn}
            disabled={busy}
            title={
              proxyOn
                ? "关闭接管会恢复官方直连，并重启 WorkBuddy"
                : "开启接管会重启 WorkBuddy"
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
              min={1024}
              max={65535}
              inputMode="numeric"
              value={proxyPort}
              disabled={proxyOn || busy}
              title={proxyOn ? "开启接管时端口不可改，先关闭接管" : "本地反代监听端口"}
              onChange={(e) => setProxyPort(e.target.value)}
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
                title="未勾选的账号不参与扣费"
              >
                参与扣费
              </span>
              <button
                className="billing-trigger"
                disabled={busy}
                title="选择参与扣费的账号"
                onClick={() => {
                  setDraft(effective);
                  setQuery("");
                  setPickerOpen(true);
                }}
              >
                <IconUsers size={14} />
                {billing.length === 0
                  ? `全部账号（${accounts.length}）`
                  : `已选 ${billing.length} / ${accounts.length}`}
                <IconChevron size={13} className="billing-chevron" />
              </button>
            </div>
          )}

          <div className="hero-field">
            <span
              className="field-label"
              title="免费模型默认生效，付费模型可勾选"
            >
              限流切换
            </span>
            <button
              className="billing-trigger"
              disabled={busy}
              title="选择触发限流时参与换号的模型"
              onClick={openModelPicker}
            >
              <IconBolt size={14} />
              {modelSummary}
              <IconChevron size={13} className="billing-chevron" />
            </button>
          </div>
        </div>
      </section>

      {err ? (
        <p className="note danger">
          <IconAlertTriangle size={14} />
          <span>{err}</span>
        </p>
      ) : stealth?.installed && !live ? (
        <p className="note warn" title={stealth.note}>
          <IconAlertTriangle size={14} />
          <span>
            {stateText}
            {stealth.note ? `：${stealth.note}` : ""}
          </span>
        </p>
      ) : null}

      {/* ── 接管动态：谁在什么时候用了哪个账号 / 有没有被限流换号 / 代理有没有报错 ── */}
      <section className="card feed-card">
        <div className="card-head">
          <h3>接管动态</h3>
          <span className="spacer" />
          <button
            className="icon-btn"
            disabled={feedBusy}
            title="刷新接管动态"
            onClick={() => void doRefreshFeed()}
          >
            <IconRefresh size={15} className={feedBusy ? "spin" : ""} />
          </button>
          {events.length > 0 && (
            <button
              className="icon-btn icon-danger"
              title="清空接管动态"
              onClick={() => void doClearEvents()}
            >
              <IconTrash size={15} />
            </button>
          )}
        </div>

        {events.length === 0 ? (
          <div className="feed-empty">暂无动态</div>
        ) : (
          <ul className="feed">
            {rows.map(({ e, count, account }, i) => {
              const k = eventKind(e);
              // 正文取最新那条的原文（只抹掉看不懂的会话号）：一眼看得出用的是哪个号、用了几次
              return (
                <li
                  key={`${e.at_ms}-${i}`}
                  className={`feed-item k-${k.cls}`}
                  title={
                    account && count > 1
                      ? `账号「${account}」本次接管共服务 ${count} 个会话`
                      : count > 1
                      ? `相同事件连续出现 ${count} 次`
                      : undefined
                  }
                >
                  <span className="feed-dot" />
                  <span className="feed-time">{shortTime(e)}</span>
                  <span className="feed-label">{k.label}</span>
                  {count > 1 && <span className="feed-count">×{count}</span>}
                  <span className="feed-detail">{displayDetail(e)}</span>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      {/* ── 扣费账号选择弹框（草稿制：点「保存」才生效） ── */}
      {pickerOpen && (
        <Dialog
          size="md"
          icon={<IconUsers size={16} />}
          title="参与扣费"
          subtitle="未勾选的账号不参与扣费；全选时以后新增的账号也会自动参与。"
          label="选择参与扣费的账号"
          fixedBodyHeight={390}
          onClose={() => {
            setDraft(null);
            setPickerOpen(false);
          }}
          tools={
            <div className="billing-tools">
              <label className="billing-all">
                <input
                  type="checkbox"
                  checked={
                    accounts.length > 0 &&
                    draftEffective.length === accounts.length
                  }
                  ref={(el) => {
                    if (el)
                      el.indeterminate =
                        draftEffective.length > 0 &&
                        draftEffective.length < accounts.length;
                  }}
                  onChange={() =>
                    setDraft(
                      draftEffective.length === accounts.length ? [] : allIds
                    )
                  }
                />
                全选
              </label>
              <span className="billing-count">
                已选 {draftEffective.length} / {accounts.length}
              </span>
            </div>
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
          <div className="modal-fill">
            <input
              type="text"
              placeholder="按用户名或手机号过滤…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            {accounts.length === 0 ? (
              <p className="empty">还没有账号。先到「账号签到」页登录或导入账号。</p>
            ) : filteredAccounts.length === 0 ? (
              <p className="empty">没有匹配「{query}」的账号。</p>
            ) : (
              <div className="billing-list">
                {filteredAccounts.map((a) => {
                  const picked = draftEffective.includes(a.id);
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
                      <AccountCell name={a.name} phone={a.phone} />
                      <span className="billing-meta">{creditMeta(a)}</span>
                    </label>
                  );
                })}
              </div>
            )}
          </div>
        </Dialog>
      )}

      {/* ── 限流切换模型弹框（草稿制：点「保存」才生效） ── */}
      {mdlOpen && (
        <Dialog
          size="md"
          icon={<IconBolt size={16} />}
          title="限流切换"
          label="限流切换"
          fixedBodyHeight={460}
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
          <div className="modal-fill">
            <Row
              title={
                <span className="failover-row-title">
                  限流时在同一会话内换号
                  <span
                    className="rule-hint"
                    role="note"
                    tabIndex={0}
                    aria-label="触发限流（429）时自动换备用账号重发同一请求，对话无感；免费模型默认生效。关掉后 429 原样透传，不冷却、不换号。"
                  >
                    <IconAlertCircle size={15} />
                    <span className="rule-hint-pop pop-left" role="tooltip">
                      <strong>限流切换</strong>
                      <span>
                        触发限流（429）时自动换备用账号重发同一请求，对话无感；免费模型默认生效。
                      </span>
                      <span>关掉后 429 原样透传，不冷却、不换号。</span>
                    </span>
                  </span>
                </span>
              }
              ctrl={
                <Switch
                  checked={mdlFailover ?? rlFailover}
                  onChange={setMdlFailover}
                  title="会话内不换号 = 调用凭证稳定"
                />
              }
            />
            {fm && (
              <p className="modal-meta">
                {fm.source === "fetched"
                  ? "来源：网关"
                  : fm.source === "cache"
                  ? "来源：缓存"
                  : "来源：内置列表"}
              </p>
            )}
            {fm == null ? (
              <p className="empty">加载中…</p>
            ) : fm.models.length === 0 ? (
              <p className="empty">暂未发现模型。</p>
            ) : (
              <ul className="pick-list">
                {fm.models.map((m) => {
                  const checked =
                    m.free || (mdlDraft ?? rlModels).includes(m.id);
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
                          : "勾选后同样自动换号"
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
                        <span className="pick-name mono">{m.id}</span>
                      </span>
                      <span className="pick-tail">
                        {/* 免费模型：一个胶囊说清「免费 + 默认生效」——
                            旧实现是「免费」胶囊 + 「默认」两个元素，说的是同一件事 */}
                        <span
                          className={"pick-state " + (m.free ? "ok" : "warn")}
                        >
                          {m.free ? "免费 · 默认" : m.multiplier || "付费"}
                        </span>
                      </span>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>
        </Dialog>
      )}
    </section>
  );
}
