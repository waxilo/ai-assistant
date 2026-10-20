import { memo, useCallback, useEffect, useMemo, useState } from "react";
import {
  getTakeoverStatus,
  enableTakeover,
  disableTakeover,
  takeoverEvents,
  clearTakeoverEvents,
} from "../api";
import type { Account, JournalEvent, TakeoverStatus, Settings } from "../types";
import { AccountCell, formatCredits, mmdd, type Toast } from "../common";
import Switch from "../components/Switch";
import { Dialog } from "../components/Dialog";
import {
  IconAlertCircle,
  IconChevron,
  IconShield,
  IconTrash,
  IconUsers,
} from "../components/Icons";

/**
 * 智能接管页。
 *
 * 设计取向：**一个开关 + 一行设置（端口 / 参与扣费）**，其余控件都只在「它此刻真的挡着路」时出现。
 *
 * - 改道方式**只有一种：端点改写**（把应用安装目录里 `product.json` 的 `bootConfig`
 *   指向 `http://127.0.0.1:PORT`），端点恒为**明文回环、不需要任何证书**。
 * - **接管对象只有 Trae CN，并且不再单独展示**：本机发现到哪个就接哪个，没有可勾选的名单
 *   （后端 `target::discover` 已收窄，见 `target::is_supported_id`），所以页面上没有那一行
 *   「接管对象」—— 它没有可操作余地，留着只是复述。它出的事仍要说：走 `issue`（按应用各说各的）
 *   与「接管动态」。
 *   注意这与「参与扣费」**不同**：那个还能勾，因为它不碰任何应用，开着也能改、下一请求即生效。
 * - **补丁的生命周期完全跟着开关走**：开启时后端自动打、关闭时自动还原。
 *   留一个「还原补丁」按钮只会多出一个「忘了点」的状态。唯一仍要在这里说的是
 *   **打不成的时候** —— 那正是开关灰着的理由。
 * - 开关是唯一的「开着吗」，接管动态是唯一的「刚才发生了什么」，所以「已生效 / 未生效 /
 *   直连官方 / 处理中」这类复述型标签全部不进页面。
 * - **接管动态是持久的**：打开页面就把磁盘上的全部历史读出来显示，切页面 / 重开窗口都不丢。
 *   要清只能点「清空」。
 *   ⚠️ 曾经是「打开即 `clearTakeoverEvents()`」——**每次打开页面都在删对账证据**
 *   （2026-09-16 用户报「用账号一却扣了账号二」时，唯一能回答「换给了谁」的 `route_start`
 *   就是这么丢的）；中间还试过「显示层按打开时刻过滤」，但那让用户以为日志又被清空了 ——
 *   「看不见旧记录」和「没有旧记录」在他那里是同一件事，**所以干脆都显示**。
 * - 选号规则收进标题旁的感叹号悬浮说明（用户要核对「为什么先扣这个号」时才展开），
 *   不占正文；上游地址这类实现细节仍不进界面 —— 排障看「接管动态」与 `proxy-rules.json`。
 * - 文字只在**有东西挡住你**时出现（见 `issue`，现在排进下方的接管动态第一位）：
 *   不正常才是需要解释的时刻，而且是**按应用**各说各的 —— 合成一句话必然要说谎。
 */
interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
  notify: (t: Toast) => void;
  /** 账号池：决定哪些账号可以被接管扣费（未勾选的一律不参与） */
  accounts: Account[];
}

/** 接管动态轮询间隔。页面只在被打开时挂载，所以这个定时器不会在后台空转。 */
const POLL_MS = 5000;

/**
 * 接管状态 + 接管动态的**全局内存缓存**。
 *
 * 页面全靠 `App` 的 `key={page}` 切页重挂载（见 `App.tsx`），若 `st` / `events` 只是
 * 页面内部 state，每次进入「智能接管」都会重置为空再触发一次 `load()` —— 每次都闪一把
 * 加载效果，本应用状态与接管动态也白白重拉。这里把它们提到模块级：首次加载后写回缓存，
 * 再次进入直接从缓存取值渲染（无加载态），数据新鲜度交给挂载期的 5 秒轮询，切页零成本。
 */
const takeoverCache: { st: TakeoverStatus | null; events: JournalEvent[] } = {
  st: null,
  events: [],
};

type Kind = "on" | "off" | "route" | "failover" | "restart" | "warn" | "err";

/**
 * 事件类型 → 圆点配色与短标签。
 *
 * 只覆盖**本版本仍会写出**的事件。历史上那几条（`cert_install` / `mode_switch` /
 * `proxy_route_*` / `tunnel_*`）随对应功能一并删掉了：旧的日志文件里可能还留着它们，
 * 落到 `default` 分支显示成通用的「事件」即可，不值得为一个再也产生不了的类型留映射。
 */
function eventKind(e: JournalEvent): { label: string; cls: Kind } {
  switch (e.event) {
    case "install":
      return { label: "开启接管", cls: "on" };
    case "uninstall":
      return { label: "关闭接管", cls: "off" };
    case "sweep":
      return { label: "自动恢复", cls: "off" };
    // 旧版本的「经系统代理接管」在应用的 `User/settings.json` 里留过回环代理痕迹。
    // 那条路已整体移除（本端点对 CONNECT 一律 405），痕迹留着 = 整应用不可用，
    // 所以每次开机与关接管都会确认清一遍 —— 清干净了也要在这里留个痕，证明它被处理过。
    case "legacy_proxy_clear":
      return { label: "清理旧版痕迹", cls: "off" };
    case "restart_trae":
      // ⚠️ 「强制」这两个字必须留在标签上：结束是 `taskkill /f`（2026-09-23 起），
      // 也就是说这一行背后**可能丢掉未保存的输入**。详情里已经把代价写全了
      // （后端 `FORCED_RESTART_CAVEAT`），标签只负责让人一眼看出它不是「请求它关一下」。
      return { label: "强制重启", cls: "restart" };
    case "restart_skipped":
      // 「本该重启、却没有」：被接管的应用当时没在运行，而 `target::with_restart` **只碰在跑的**
      // （也不替你打开）。以前这件事一个字都不写，于是「没重启」和「重启这步没跑到」
      // 长得一模一样 —— 用户来问的正是「为什么我开启接管不会重启 Trae」。
      // 中性色：这是正常结果，不是故障。
      return { label: "无需重启", cls: "restart" };
    case "restart_fail":
      // 应用**已被我们强制结束**、却没拉起来 —— 用户手上少了一个窗口，而且它不会自己回来。
      // 这是接管能做出来的最坏结果之一，必须最显眼。
      return { label: "重启失败", cls: "err" };
    // 开关拨了、但这一趟没生效（已回滚成关闭）。名字取通用形态是因为它有两条来路：
    // 闸门在写盘那一刻拦住，或**应用退不掉**（`with_restart` 在动手之前就放弃）。
    // 具体哪一条写在详情里 —— 关键是它**必须留在页面上**：那行红字切页就没了。
    case "takeover_fail":
      return { label: "接管未生效", cls: "err" };
    // 补丁由开关/选择自动打与还原，但这两条记录**必须留着**：它是「应用被改过没有」的唯一实证。
    case "patch_apply":
      return { label: "打补丁", cls: "on" };
    case "patch_revert":
      return { label: "还原补丁", cls: "off" };
    // 补丁**失败**有自己的事件类型，不再和成功挤在 `patch_apply` / `patch_revert` 里。
    // 以前两者共用事件名（只在 detail 里写一句「失败」），于是「打补丁失败」会渲染成一枚
    // **绿色的「打补丁」标签** —— 页面上的红框去掉、把补丁失败交给接管动态之后，这个坑就致命了。
    // （同一个理由曾经把 `proxy_upstream_status` 从 `proxy_error` 里拆出来。）
    case "patch_fail":
      return { label: "补丁失败", cls: "err" };
    case "patch_revert_fail":
      return { label: "补丁还原失败", cls: "err" };
    // 闸门拦下的改写：这是**保护性**拦截（写下去应用会崩），不是接管坏了，但必须让人看见。
    case "install_blocked":
      return { label: "已阻止改道", cls: "err" };
    case "rules_save":
    case "rules_write":
      return { label: "接管规则", cls: "restart" };
    case "route_start":
      return { label: "开始使用账号", cls: "route" };
    // ⚠️ 2026-09-16 起**技术过程不再进这张表**。事件按受众分两条通道落盘（判据在
    // `journal.rs` 的 `TRACE_EVENTS`），本页只显示**用户日志**：
    //   · 走诊断日志（`takeover-trace.jsonl`，不上界面）的是：`proxy_path`（每个新路径一条，
    //     量最大）、`unbound_session` / `unbound_session_hint`（会话识别取证）、
    //     `proxy_error` / `proxy_upstream_status` / `proxy_bad_request` / `proxy_stream_error`
    //     （连接层与上游状态）、`ws_handshake` / `ws_swap` / `ws_open`（WebSocket 细节）。
    //   · 留在这里的是**用户能对账或能行动**的事实：谁被换了号、名单什么时候被改的、
    //     哪个应用被重启了、哪一步失败了。
    // 所以下面不再为那些事件保留分支 —— 但 `default` 永远兜底：万一将来漏了分类，
    // 它会显示成「事件」，而**不会凭空消失**。
    case "billing_list_stale":
      // 「参与扣费」勾的账号一个都没对上 ⇒ 后端按**全部账号**兜底了（fail-open）。
      // 必须看起来像告警：用户以为自己收窄了扣费范围，其实全体都在名单里。
      // （2026-09-16 那起「我明明用账号一，怎么扣了账号二」就属这一类的可能性之一。）
      return { label: "扣费名单失效", cls: "warn" };
    case "billing_list_changed":
      // 「参与扣费」被改过。中性色：这是用户自己的配置动作，不是故障 ——
      // 但它**必须留在页面上**，因为它是「每笔扣费换给谁」的**前提**：
      // 没有它，「为什么今天扣的是 9152」在日志里只有结果、查不到起因。
      return { label: "扣费名单已改", cls: "restart" };
    case "token_swap_rejected":
      // 拿池化账号的 token 去顶 `x-ide-token` 被上游拒了 ⇒ 该域从此退回原凭据。
      // 琥珀而不是红：**用户的登录态没受影响**（已立刻用原凭据原样重发），
      // 而且「换身份这条路对这段域不成立」是个结论，不是故障。
      return { label: "换身份被拒", cls: "warn" };
    case "failover":
      return { label: "限流切换", cls: "failover" };
    case "takeover_blocked":
      // 接管开着、但**没有账号可以扣费**（账号池为空，或白名单里一个能用的都没有）⇒ 请求直接吃 503。
      // 这条原先混在 `proxy_error` 里，而那是技术诊断通道 —— 于是「接管开着却什么都做不了、
      // 为什么」在界面上永远查不到。可它恰恰是**只有用户能解决**的那类问题（添账号 / 改白名单），
      // 所以必须留在页面上，而且必须用异常色。
      return { label: "无可用账号", cls: "err" };
    default:
      return { label: "事件", cls: "restart" };
  }
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

/**
 * 账号的积分摘要（弹窗每行右侧）：积分 + 最早到期日。
 * 到期时间是选号的第一排序键（越早到期越先用），所以必须能在这里看到。
 */
function creditMeta(a: Account): string {
  const snap = a.credit_snapshot;
  if (snap?.unlimited) return "积分不限量";
  if (!snap) return "积分未知";
  const credits =
    snap.credits === null
      ? "积分未知"
      : `积分 ${formatCredits(snap.credits)}`;
  return snap.earliest_expiry_ms
    ? `${credits} · ${mmdd(snap.earliest_expiry_ms)} 到期`
    : credits;
}

function TakeoverPage({ settings, update, notify, accounts }: Props) {
  const port = settings?.takeover_port ?? 8788;
  // 从全局缓存取初值：首次进入是空（走一次加载），之后再进入直接用缓存，不闪加载态
  const [st, setSt] = useState<TakeoverStatus | null>(takeoverCache.st);
  const [events, setEvents] = useState<JournalEvent[]>(takeoverCache.events);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 端口输入的草稿：直接改设置会让「删一位数字」被 parseInt 兜成默认值，输入过程很跳。 */
  const [portDraft, setPortDraft] = useState(String(port));
  /** 参与扣费弹窗是否打开 */
  const [billingOpen, setBillingOpen] = useState(false);
  /** 弹窗里的勾选草稿：点「保存」才写设置，取消则原样丢弃。 */
  const [draft, setDraft] = useState<Set<string>>(new Set());

  useEffect(() => setPortDraft(String(port)), [port]);

  /** 写状态的同时同步到全局缓存（页面一切数据出口都走它，保证缓存与界面一致）。 */
  const setStCached = useCallback((v: TakeoverStatus | null) => {
    takeoverCache.st = v;
    setSt(v);
  }, []);
  const setEventsCached = useCallback((v: JournalEvent[]) => {
    takeoverCache.events = v;
    setEvents(v);
  }, []);

  /** 一次拉齐状态 + 接管动态，结果写回全局缓存（下次进入页面直接复用）。 */
  const load = useCallback(async () => {
    try {
      const [status, ev] = await Promise.all([getTakeoverStatus(), takeoverEvents()]);
      setStCached(status);
      setEventsCached(ev);
    } catch {
      // 状态读取失败不打扰用户：下一次轮询会自愈
    }
  }, [setStCached, setEventsCached]);

  /**
   * 拉齐状态 + 接管动态。
   *
   * ⚠️ 这里**不再**调 `clearTakeoverEvents()`。原来「一打开就清空」的写法把
   * 「本次打开之后」这个语义实现成了「删掉历史」，于是每次打开页面都在销毁排障证据
   * （2026-09-16 就因此无法回答「这笔扣费换给了谁」）。现在改成显示层按 `openedAt` 过滤，
   * 后端历史一直留着；要真清，用户得点「清空」。
   */
  useEffect(() => {
    let alive = true;
    void (async () => {
      if (alive) await load();
    })();
    // 窗口被隐藏（最小化 / 收到托盘）时不再轮询：看不见的界面不需要每 5 秒问一次状态，
    // 而每一次查询都要扫应用目录 + 取进程表。
    const t = window.setInterval(() => {
      if (document.hidden) return;
      void load();
    }, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, [load]);

  const enabled = !!settings?.takeover_enabled;

  /** 本机发现到的 Trae CN（没装时为空）。接管对象就是它，没有可勾选的名单。 */
  const apps = st?.apps ?? [];

  /**
   * 「生效中」= 开关开着、反代真在监听、且**至少一个**发现到的应用端点还被改着。
   * 三者缺一都不算生效（应用升级会悄悄把 product.json 换回去）。
   */
  const live = enabled && !!st?.proxy_active && apps.some((a) => a.installed);

  /**
   * 开接管会给发现到的应用**逐个**打补丁，任何一个打不成整次开启都会失败（后端回滚已打的）
   * ⇒ 开关能不能点的判据是「**每个**应用都打得成」，不是「至少一个」。
   */
  const blocked = useMemo(
    () => apps.filter((a) => !(a.patch.recognized && a.patch.writable)),
    [apps]
  );
  const patchReady = apps.length > 0 && blocked.length === 0;

  /**
   * 页面上唯一的文字出口。只在「有东西挡住你 / 坏了」时给内容，且**按应用**分条。
   *
   * 为什么必须按应用分：本机可能装了两个 Trae shell，一个能接管、另一个版本不认识 ——
   * 合成一句话必然要说谎，用户也无从知道该点掉哪一个。
   */
  const issue = useMemo<{ text: string | null; items: string[] } | null>(() => {
    if (!st) return error ? { text: error, items: [] } : null;
    if (apps.length === 0) {
      return error
        ? { text: error, items: [] }
        : { text: "本机没有找到 Trae CN，无法开启接管。", items: [] };
    }

    const items: string[] = [];
    // 每个应用各一句（后端只在有事时说，正常是空串）。只有一条明细时前缀是废话。
    for (const a of apps) {
      if (a.message) items.push(apps.length > 1 ? `${a.label}：${a.message}` : a.message);
    }

    let text: string | null = null;
    if (error) {
      text = error;
    } else if (!enabled && blocked.length > 0) {
      // 开关此刻灰着 ⇒ 要说「怎么才能开」。原因已在明细里逐条写清了，这里只给下一步。
      // 只支持一个应用，没有「改接别的」这条出路 —— 能做的只有修好它（权限 / 版本）。
      text = "Trae CN 补丁未能应用，暂时无法开启接管。按下方提示处理后即可开启。";
    } else if (enabled && !st.proxy_active) {
      text = st.proxy_error ? `本地代理未在运行：${st.proxy_error}` : "本地代理未在运行。";
    } else if (enabled && st.rules?.observe_only) {
      text = "当前为观察模式：流量只记录、不切换账号。";
    }
    if (!text && items.length === 0) return null;
    return { text, items };
  }, [error, st, apps, enabled, blocked]);

  /** 参与扣费的账号：**空 = 全部**，且禁止全不选。 */
  const accountIds = useMemo(() => new Set(accounts.map((a) => a.id)), [accounts]);
  const billingIds = useMemo(
    () => (settings?.billing_account_ids ?? []).filter((id) => accountIds.has(id)),
    [settings, accountIds]
  );
  const openBilling = () => {
    setDraft(new Set(billingIds.length === 0 ? accounts.map((a) => a.id) : billingIds));
    setBillingOpen(true);
  };

  const toggleDraft = (id: string) =>
    setDraft((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  /**
   * 保存勾选。全选写空列表（后端语义就是「全部」），避免设置里留一份和默认等价的
   * 显式名单 —— 同时也让以后新增的账号自动参与。
   */
  const saveBilling = () => {
    if (draft.size === 0) {
      notify({ kind: "info", text: "至少保留一个账号" });
      return;
    }
    update({
      billing_account_ids:
        draft.size === accounts.length
          ? []
          : accounts.map((a) => a.id).filter((id) => draft.has(id)),
    });
    setBillingOpen(false);
  };

  /**
   * 开关。**它是唯一会动别人应用的东西**，两个方向都是「打包动作」：
   * - 开：给名单里的应用逐个打补丁 → 预检 → 起反代 → 改端点 → 重启它们；
   * - 关：还原端点 → 还原补丁 → 重启它们 → 停反代。
   * 所以文案要说清「它会动别人的应用」，别让用户以为只是本机一个开关。
   */
  const onToggle = async (checked: boolean) => {
    setBusy(true);
    setError(null);
    try {
      if (checked) {
        const r = await enableTakeover();
        setStCached(r);
        update({ takeover_enabled: true });
        notify({ kind: "ok", text: r.message });
      } else {
        const r = await disableTakeover();
        setStCached(r);
        update({ takeover_enabled: false });
        notify({ kind: "ok", text: "已恢复官方直连，补丁也已还原。" });
      }
    } catch (e) {
      // 失败必须留在页面上（toast 会消失），否则开关弹回去却没说为什么
      setError(String(e));
    } finally {
      setBusy(false);
      void load();
    }
  };

  /** 端口只在停用时能改；失焦/回车提交，非法值静默回退（输入框里不留半截数字）。 */
  const commitPort = () => {
    const n = parseInt(portDraft, 10);
    if (!Number.isFinite(n) || n < 1024 || n > 65535 || n === port) {
      setPortDraft(String(port));
      return;
    }
    update({ takeover_port: n });
  };

  const doClearEvents = async () => {
    try {
      await clearTakeoverEvents();
      setEventsCached([]);
    } catch (e) {
      notify({ kind: "err", text: "清空失败：" + e });
    }
  };

  /**
   * 连续相同（类型 + 内容都一样）的事件聚合成一条并附次数。
   * 事件流是「新的在前」，相邻即时间连续 —— 重启风暴、心跳重复这类刷屏只会占一行。
   */
  const grouped = useMemo(() => {
    const out: { e: JournalEvent; count: number }[] = [];
    for (const e of events) {
      const last = out[out.length - 1];
      if (last && last.e.event === e.event && last.e.detail === e.detail) {
        last.count += 1;
      } else {
        out.push({ e, count: 1 });
      }
    }
    return out.slice(0, 80);
  }, [events]);

  /**
   * 异常 / 通知**统一进下面的动态流**，不再在顶部单独占一块红色横幅。
   * 已在动态里出现过的原文（例如 `takeover_fail` 已由后端落盘）不再重复插一条，
   * 否则会跟「接管未生效」那条红动态撞车。只补那些后端**不落盘**的持续状态提示。
   */
  const feedIssues = useMemo(() => {
    const out: { cls: Kind; label: string; detail: string }[] = [];
    if (!issue) return out;
    const seen = new Set(grouped.map((g) => g.e.detail));
    const push = (cls: Kind, label: string, detail: string) => {
      if (detail && !seen.has(detail)) out.push({ cls, label, detail });
    };
    // text 的语义由来源定：接管失败 / 反代失联是真故障（红），其余是「怎么才能开」等提示（琥珀）。
    const hardError = !!error || (enabled && !!st && !st.proxy_active);
    push(hardError ? "err" : "warn", hardError ? "接管异常" : "提示", issue.text ?? "");
    for (const it of issue.items) push("warn", "应用提示", it);
    return out;
  }, [issue, grouped, error, enabled, st]);

  return (
    <>
      {/* ── 控制区：一个开关 + 一个端口 + 哪些账号参与扣费。没有别的控件，也没有复述状态的文字 ── */}
      <section className={`card hero${live ? " live" : ""}`}>
        <div className="hero-top">
          <div className="hero-copy">
            <span className="hero-icon">
              <IconShield size={19} />
            </span>
            <div className="hero-text">
              <div className="hero-title">
                <h2>接管 Trae CN 请求</h2>
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
            checked={enabled}
            disabled={
              busy ||
              apps.length === 0 ||
              // 有应用打不成补丁 ⇒ 开接管必然失败（它第一步就是逐个打补丁），
              // 先在「能开」之前灰掉（原因与下一步全在下面的 issue 明细里）。
              (!enabled && !patchReady)
            }
            title={
              enabled
                ? "关闭接管会恢复官方直连，并重启正在运行的 Trae CN（未保存的输入可能丢失）"
                : "开启接管会重启正在运行的 Trae CN（未保存的输入可能丢失）"
            }
            onChange={(v) => void onToggle(v)}
          />
        </div>

        <div className="hero-settings">
          <div className="hero-field">
            <span className="field-label">端口</span>
            <input
              className="port-input"
              type="number"
              inputMode="numeric"
              value={portDraft}
              disabled={enabled || busy}
              title={enabled ? "开启接管时端口不可改，先关闭接管" : "本地反代监听端口"}
              onChange={(e) => setPortDraft(e.target.value)}
              onBlur={commitPort}
              onKeyDown={(e) => {
                if (e.key === "Enter") e.currentTarget.blur();
              }}
            />
          </div>

          {accounts.length > 0 && (
            <div className="hero-field">
              <span
                className="field-label"
                title="未勾选的账号不参与扣费；优先使用到期最早的积分"
              >
                参与扣费
              </span>
              <button
                className="billing-trigger"
                disabled={busy}
                title="选择参与扣费的账号"
                onClick={openBilling}
              >
                <IconUsers size={14} />
                {billingIds.length === 0
                  ? `全部账号（${accounts.length}）`
                  : `已选 ${billingIds.length} / ${accounts.length}`}
                <IconChevron size={13} className="billing-chevron" />
              </button>
            </div>
          )}
        </div>

      </section>

      {billingOpen && (
        <Dialog
          icon={<IconUsers size={16} />}
          title="参与扣费"
          subtitle="未勾选的账号不参与扣费；全选时以后新增的账号也会自动参与。"
          label="选择参与扣费的账号"
          onClose={() => setBillingOpen(false)}
          tools={
            <div className="billing-tools">
              <label className="billing-all">
                <input
                  type="checkbox"
                  checked={draft.size === accounts.length}
                  ref={(el) => {
                    if (el) el.indeterminate = draft.size > 0 && draft.size < accounts.length;
                  }}
                  onChange={() =>
                    setDraft(
                      draft.size === accounts.length
                        ? new Set()
                        : new Set(accounts.map((a) => a.id))
                    )
                  }
                />
                全选
              </label>
              <span className="billing-count">
                已选 {draft.size} / {accounts.length}
              </span>
            </div>
          }
          footer={
            <>
              <button className="btn" onClick={() => setBillingOpen(false)}>
                取消
              </button>
              <button className="btn primary" onClick={saveBilling}>
                保存
              </button>
            </>
          }
        >
          <div className="billing-list">
            {accounts.map((a) => (
              <label
                key={a.id}
                className={"billing-row" + (draft.has(a.id) ? " picked" : "")}
              >
                <input
                  type="checkbox"
                  checked={draft.has(a.id)}
                  onChange={() => toggleDraft(a.id)}
                />
                <AccountCell name={a.name} phone={a.phone} />
                <span className="billing-meta">{creditMeta(a)}</span>
              </label>
            ))}
          </div>
        </Dialog>
      )}

      {/* ── 接管动态：谁在什么时候用了哪个账号 / 有没有被限流换号 / 代理有没有报错。
              ⚠️ 每次打开本页都会先清空，所以这里给的是「这次打开之后」的事。 ── */}
      <section className="card">
        <div className="card-head">
          <h3>接管动态</h3>
          <span className="spacer" />
          {events.length > 0 && (
            <button className="icon-btn icon-danger" title="清空动态" onClick={() => void doClearEvents()}>
              <IconTrash size={15} />
            </button>
          )}
        </div>

        {grouped.length === 0 && feedIssues.length === 0 ? (
          <div className="feed-empty">暂无动态</div>
        ) : (
          <ul className="feed">
            {/* 不在动态里落盘的持续状态（/失败回滚后没成功的那一行）先排在最前，和真实事件同一条时间轴 */}
            {feedIssues.map((it, i) => (
              <li key={`issue-${i}`} className={`feed-item k-${it.cls}`}>
                <span className="feed-dot" />
                <span className="feed-time">—</span>
                <span className="feed-label">{it.label}</span>
                <span className="feed-detail">{it.detail}</span>
              </li>
            ))}
            {grouped.map(({ e, count }, i) => {
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
    </>
  );
}

export default memo(TakeoverPage);
