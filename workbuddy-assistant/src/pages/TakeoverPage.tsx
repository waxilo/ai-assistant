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
import { AccountCell } from "../common";
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
        `${action}需要重启 WorkBuddy 与长驻 CLI host，才能安全清除旧端点。` +
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
      onToast({ kind: "ok", text: `已${action}，WorkBuddy 已安全重启` });
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
      onToast({ kind: "ok", text: "端口已保存" });
    } catch (e) {
      onToast({ kind: "err", text: "端口保存失败：" + String(e) });
    }
  };

  /** 弹框「保存」：草稿落库立即生效（纯账号池调整，不需要重启） */
  const doSaveBilling = async () => {
    if (draft == null) return;
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

  /** 扣费账号弹框草稿里勾/去勾（基准：草稿为空视为「当前全选」） */
  const toggleDraft = (id: string) =>
    setDraft((list) => {
      const base = list == null ? effective : list.length === 0 ? allIds : list;
      return base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
    });

  const live = stealth?.installed && stealth.alive;

  /** 扣费账号摘要（控制条上的那颗胶囊按钮） */
  const billingSummary =
    accounts.length === 0
      ? "暂无账号"
      : billing.length === 0
      ? `全部 ${accounts.length} 个（默认）`
      : `已选 ${billing.length} · 未选 ${accounts.length - billing.length}`;

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

  /** 弹框内按用户名 / 手机号过滤 */
  const filteredAccounts = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return accounts;
    return accounts.filter(
      (a) =>
        a.name.toLowerCase().includes(q) || (a.phone ?? "").includes(q)
    );
  }, [accounts, query]);

  const draftEffective =
    draft == null ? effective : draft.length === 0 ? allIds : draft;

  return (
    <section className="panel-page tk-page">
      <p className="set-intro">
        <IconInfo size={14} />
        <span>
          开启后 WorkBuddy 的对话请求由本地代理转发，按「积分最早过期优先」在账号间分配扣费；
          下方记录每一次开关、路由与异常。
        </span>
      </p>

      {/* ── 紧凑控制条：开关 + 状态 + 扣费账号 / 限流切换（弹窗入口）+ 端口 ── */}
      <div className={`card tk-bar ${live ? "live" : ""}`}>
        <label
          className="switch"
          title="开启后 WorkBuddy 的对话请求将由本地代理分流扣费"
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
          title="勾选的账号才允许被扣费，未勾选的会被排除；点击细选"
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
          className="tk-port"
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
              onClick={() => setDraft(allIds)}
              disabled={accounts.length === 0}
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
          {accounts.length === 0 ? (
            <p className="empty">还没有账号。先到「账号签到」页登录或导入账号。</p>
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
              列表从网关动态拉取（缓存 1 小时），腾讯增删模型后点「刷新」即可同步。
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
          {fm && (
            <p className="modal-meta">
              {fm.source === "fetched"
                ? "来源：刚从网关拉取"
                : fm.source === "cache"
                ? "来源：缓存（1 小时内有效）"
                : "来源：内置兜底列表（网关拉取失败，可点「刷新」重试）"}
            </p>
          )}
          {fm == null ? (
            <p className="empty">加载中…（从网关拉取模型列表）</p>
          ) : fm.models.length === 0 ? (
            <p className="empty">暂未发现模型。</p>
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
        </Dialog>
      )}
    </section>
  );
}
