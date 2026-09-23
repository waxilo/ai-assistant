import { memo, useCallback, useEffect, useMemo, useState } from "react";
import {
  getTakeoverStatus,
  enableTakeover,
  disableTakeover,
  setTakeoverApps,
  takeoverEvents,
  clearTakeoverEvents,
} from "../api";
import type { Account, AppStatus, JournalEvent, TakeoverStatus, Settings } from "../types";
import { mmdd, type Toast } from "../common";
import Switch from "../components/Switch";
import { IconTrash } from "../components/Icons";

/**
 * 智能接管页。
 *
 * 设计取向：**一个开关 + 一张应用单选 + 一张账号勾选表**，其余控件都只在「它此刻真的挡着路」时出现。
 *
 * - 改道方式**只有一种：端点改写**（把应用安装目录里 `product.json` 的 `bootConfig`
 *   指向 `http://127.0.0.1:PORT`），端点恒为**明文回环、不需要任何证书**。
 * - **接管哪些应用**：本机可能同时装了多个 Trae shell（如 `TRAE SOLO CN` + `Trae CN`），
 *   「接管对象」是**单选**：点击即选中该应用，同一时刻只有被选中的那个被改道、被重启。
 *   没配置过时后端名单为空（语义 = 全部），界面上把空名单渲染成「全部选中」；
 *   点任意一个即收窄为单选。单选没有「取消」——点已选中的那个不做任何事。
 *   ⚠️ 单选**只在真的多于一个应用时**才有交互（只有一个时那枚 chip 是只读的）。
 *   ⚠️ **接管开着时 chip 是灰的**（改名单要重启那些应用，跟改端口是同一类事：
 *   先关接管再改）。界面只是**显示**这条规则，规则本身在后端 `set_apps` 里 ——
 *   界面过期也不会让「动别人应用」这件事悄悄发生。
 *   注意这与「参与扣费」**不同**：那个不碰任何应用，所以开着也能改、下一请求即生效。
 * - **补丁的生命周期完全跟着选择走**：选中时后端自动打、被换下时后端自动还原。
 *   留一个「还原补丁」按钮只会多出一个「忘了点」的状态。唯一仍要在这里说的是
 *   **打不成的时候** —— 那正是开关灰着的理由。
 * - 开关是唯一的「开着吗」，接管动态是唯一的「刚才发生了什么」，所以「已生效 / 未生效 /
 *   直连官方 / 处理中」这类复述型标签全部不进页面（页头那颗胶囊已经答了）。
 * - **接管动态是持久的**：打开页面就把磁盘上的全部历史读出来显示，切页面 / 重开窗口都不丢。
 *   要清只能点「清空」。
 *   ⚠️ 曾经是「打开即 `clearTakeoverEvents()`」——**每次打开页面都在删对账证据**
 *   （2026-09-16 用户报「用账号一却扣了账号二」时，唯一能回答「换给了谁」的 `route_start`
 *   就是这么丢的）；中间还试过「显示层按打开时刻过滤」，但那让用户以为日志又被清空了 ——
 *   「看不见旧记录」和「没有旧记录」在他那里是同一件事，**所以干脆都显示**。
 * - 选号规则、上游地址这类实现细节不进界面 —— 排障看「接管动态」与 `proxy-rules.json`。
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
 * 加载效果，接管应用列表与接管动态也白白重拉。这里把它们提到模块级：首次加载后写回缓存，
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
    case "takeover_apps_changed":
      // 「接管应用」名单被改过（只在接管关着时能改）。中性色同上：
      // 它是「下次开接管为什么会重启某个应用」的唯一解释。
      return { label: "接管名单已改", cls: "restart" };
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
 * chip 上的短标签。账号名是「用户0044120650」这种，直接铺开会把一行撑爆，
 * 所以优先用**人认得的尾部数字**：手机号打码串最后一个星号后可见的数字
 * （如 `190******75` ⇒ `75`）> user_id 后 4 位 > 名称本身。
 *
 * ⚠️ 必须与后端 `accounts.rs::tail` 算**同一个数**（真实尾号在星号之后，
 * 不要把所有数字拼起来取后 4 位，那会把开头的 `190` 也算进去）。
 */
function shortLabel(a: Account): string {
  const phone = a.phone ?? "";
  let digits = "";
  const star = phone.lastIndexOf("*");
  if (star !== -1) digits = phone.slice(star + 1).replace(/\D/g, "");
  if (!digits) digits = phone.replace(/\D/g, "").slice(-4);
  if (digits) return `尾号 ${digits}`;
  const uid = a.user_id ?? "";
  if (uid.length >= 4) return `ID ${uid.slice(-4)}`;
  return a.name.length > 6 ? a.name.slice(0, 6) : a.name;
}

/**
 * 单个应用的健康灯。只在**接管开着**时才渲染（关着时它必然全灰，是噪声）：
 * - `ok`   = 端点已由本助手改道到本机，且它的补丁在位；
 * - `warn` = 这个应用有事（后端给了 `message`），或改道丢了 / 不是本助手改的。
 */
function appHealth(a: AppStatus, enabled: boolean): "ok" | "warn" | "idle" {
  if (!a.selected || !enabled) return "idle";
  if (a.message) return "warn";
  return a.installed && a.ours && a.patch.patched ? "ok" : "warn";
}

/** chip 的悬停提示：在改谁、改到哪、有没有问题 —— 排障时要能一眼看到这几样。 */
function appTitle(a: AppStatus, enabled: boolean, multi: boolean): string {
  const parts: string[] = [a.bundle];
  if (a.app_dir && a.app_dir !== a.bundle) parts.push(`目录 ${a.app_dir}`);
  if (a.message) {
    parts.push(a.message);
  } else if (!a.selected) {
    parts.push("未接管");
  } else if (!enabled) {
    parts.push("接管未开启，选择只决定下次开启时改谁");
  } else {
    parts.push(a.installed && a.ours ? "端点已改道本机" : "尚未改道");
  }
  if (a.upstream_http) parts.push(`上游 ${a.upstream_http}`);
  if (a.running) parts.push("正在运行");
  // 悬停提示要说清「为什么点不动」——灰掉的控件不解释原因，就等于一个坏掉的控件。
  if (multi) parts.push(enabled ? "开启接管时不可改（先关闭接管）" : "点击接管该应用");
  return parts.join(" · ");
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

  /** 本机发现到的**全部**应用（没勾的也在里面 —— 多选的前提就是能看见它们）。 */
  const apps = st?.apps ?? [];
  const appIds = useMemo(() => new Set(apps.map((a) => a.id)), [apps]);
  const multi = apps.length > 1;

  /**
   * 接管名单（界面上是**单选**，存储仍是后端那份列表）。
   *
   * **空列表 = 全部**（与后端 `target::select` 一致，也是「没配置过」的默认状态），
   * 所以界面上把空列表渲染成「全部选中」；用户一点就收窄成只含那一个应用的显式列表。
   * 本机已不存在的 id 在后端会被丢掉，这里也过滤一遍，免得设置里留一条永远选不中的名字。
   */
  const picked = useMemo(
    () => (settings?.takeover_apps ?? []).filter((id) => appIds.has(id)),
    [settings, appIds]
  );
  const appOn = useCallback((id: string) => picked.length === 0 || picked.includes(id), [picked]);
  /** 真正在名单里的应用 —— 判据与展示都用它，别在两处各算一遍（必然分叉）。 */
  const active = useMemo(() => apps.filter((a) => appOn(a.id)), [apps, appOn]);

  /**
   * 「生效中」= 开关开着、反代真在监听、且**至少一个**在名单里的应用端点还被改着。
   * 三者缺一都不算生效（应用升级会悄悄把 product.json 换回去）。
   */
  const live = enabled && !!st?.proxy_active && active.some((a) => a.installed);

  /**
   * 开接管会给名单里的应用**逐个**打补丁，任何一个打不成整次开启都会失败（后端回滚已打的）
   * ⇒ 开关能不能点的判据是「名单里**每个**应用都打得成」，不是「至少一个」。
   */
  const blocked = useMemo(
    () => active.filter((a) => !(a.patch.recognized && a.patch.writable)),
    [active]
  );
  const patchReady = active.length > 0 && blocked.length === 0;

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
        : { text: "没有找到可接管的 Trae 应用，本机无法开启接管。", items: [] };
    }

    const items: string[] = [];
    if (st.missing_apps.length > 0) {
      items.push(`接管名单里有本机找不到的应用，已跳过：${st.missing_apps.join("、")}`);
    }
    // 名单里每个应用各一句（后端只在有事时说，正常是空串）。只有一条明细时前缀是废话。
    for (const a of active) {
      if (a.message) items.push(multi ? `${a.label}：${a.message}` : a.message);
    }

    let text: string | null = null;
    if (error) {
      text = error;
    } else if (!enabled && blocked.length > 0) {
      // 开关此刻灰着 ⇒ 要说「怎么才能开」。原因已在明细里逐条写清了，这里只给下一步。
      // 单选下出路只有一条：改选一个打得成补丁的应用（接管对象只能有一个）。
      text = "在上方改选一个可以打补丁的应用，就可以开启接管。";
    } else if (enabled && !st.proxy_active) {
      text = st.proxy_error ? `本地反代没有在监听：${st.proxy_error}` : "本地反代没有在监听。";
    } else if (enabled && st.rules?.observe_only) {
      text =
        "当前是观察模式：流量已全部经过本机，但一个凭据都还没换 —— 把 proxy-rules.json 的 observe_only 改成 false 才会真正走账号池。";
    }
    if (!text && items.length === 0) return null;
    return { text, items };
  }, [error, st, apps, enabled, blocked, active, multi]);

  /** 参与扣费的账号。语义与「接管应用」同一套：**空 = 全部**，且禁止全不选。 */
  const accountIds = useMemo(() => new Set(accounts.map((a) => a.id)), [accounts]);
  const billingIds = useMemo(
    () => (settings?.billing_account_ids ?? []).filter((id) => accountIds.has(id)),
    [settings, accountIds]
  );
  const billingOn = useCallback(
    (id: string) => billingIds.length === 0 || billingIds.includes(id),
    [billingIds]
  );

  const toggleBilling = (id: string) => {
    const current = billingIds.length === 0 ? accounts.map((a) => a.id) : billingIds;
    const next = current.includes(id) ? current.filter((x) => x !== id) : [...current, id];
    if (next.length === 0) {
      notify({ kind: "info", text: "至少保留一个账号" });
      return;
    }
    // 全部选回 = 写空列表（后端语义就是「全部」），避免设置里留一份和默认等价的显式名单
    update({ billing_account_ids: next.length === accounts.length ? [] : next });
  };

  /**
   * 改「接管哪个应用」（单选）。
   *
   * 点击即选中该应用、取消其他；单选没有「取消」——已经是唯一选中项时点击不做任何事。
   *
   * 必须走后端命令（而不是像账号那样直接写设置）：设置要对本机做一次解析
   * （丢掉本机不存在的 id、排序让文件稳定），而且这条规则**只在后端生效** ——
   * 开着接管时会被后端拒绝（见 `commands::set_apps`）。界面在那种情况下根本点不动，
   * 所以这里不再有确认框：**「改名单 = 重启那些应用」这件事，改由「必须先关接管」来表达**，
   * 比一个能被点掉的确认弹窗硬得多。
   *
   * 关着接管时它只是记一笔设置，所以也不该弹框打扰。
   */
  const selectApp = async (id: string) => {
    // 已经是唯一选中项 ⇒ 无事发生（单选不可取消）
    if (picked.length === 1 && picked[0] === id) return;
    const ids = [id];
    setBusy(true);
    setError(null);
    update({ takeover_apps: ids });
    try {
      const r = await setTakeoverApps(ids);
      setStCached(r);
      notify({ kind: "ok", text: "已记录，开启接管时生效" });
    } catch (e) {
      // 选不上必须留在页面上：否则「点了却没接管」会变成一个看不见的状态
      setError(String(e));
    } finally {
      setBusy(false);
      void load();
    }
  };

  /**
   * chip 的悬停提示：给「核对选号依据」用 —— 账号是谁、还剩多少积分、哪天到期。
   * 到期时间是选号的第一排序键（越早到期越先用），所以必须能在这里看到。
   */
  const chipTitle = (a: Account) => {
    const parts = [a.name];
    if (a.phone) parts.push(a.phone);
    const snap = a.credit_snapshot;
    if (snap?.unlimited) {
      parts.push("积分不限量");
    } else if (snap && (snap.credits !== null || snap.earliest_expiry_ms)) {
      const c = snap.credits === null ? "未知" : String(snap.credits);
      parts.push(
        snap.earliest_expiry_ms
          ? `积分 ${c}，${mmdd(snap.earliest_expiry_ms)} 到期`
          : `积分 ${c}，到期未知`
      );
    } else {
      parts.push("积分未知");
    }
    return parts.join(" · ");
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
      {/* ── 控制区：一个开关 + 接管哪些应用 + 一个端口 + 哪些账号参与扣费。没有别的控件，也没有复述状态的文字 ── */}
      <section className={`card hero${live ? " live" : ""}`}>
        <div className="hero-row">
          <h2>智能接管</h2>
          <Switch
            size="lg"
            checked={enabled}
            disabled={
              busy ||
              apps.length === 0 ||
              // 名单里有应用打不成补丁 ⇒ 开接管必然失败（它第一步就是逐个打补丁），
              // 先在「能开」之前灰掉（原因与下一步见下面那枚应用单选和 issue）。
              (!enabled && !patchReady)
            }
            title={
              enabled
                ? "关闭接管：恢复官方直连，并还原给这些应用打的免证书补丁（正在运行的会被强制重启，未保存的输入可能丢失；没开着的下次启动自然生效）"
                : "开启接管：给选中的应用打免证书补丁、把端点改到本机反代（正在运行的会被强制重启，未保存的输入可能丢失；没开着的下次启动自然生效）"
            }
            onChange={(v) => void onToggle(v)}
          />
        </div>

        {apps.length > 0 && (
          <div className="hero-row">
            <span
              className="field-label"
              title={
                !multi
                  ? "本机只发现这一个 Trae 应用，没有选择余地"
                  : enabled
                    ? "开启接管时不可改（改名单要重启这些应用）；先关闭接管再改"
                    : "单选：只接管点击选中的应用"
              }
            >
              接管应用
            </span>
            <div className="chips">
              {apps.map((a) => {
                const on = appOn(a.id);
                return (
                  <button
                    key={a.id}
                    className={"chip" + (on ? " on" : "") + (multi ? "" : " static")}
                    // 只有一个应用时不给点：选了也表达不出别的意思。
                    // ⚠️ 接管开着时也不给点：改名单意味着**重启这些应用**，跟改端口是同一类事 ——
                    // 先关接管再改。这条规则后端也拦（见 `commands::set_apps`），界面只是显示它。
                    disabled={busy || !multi || enabled}
                    title={appTitle(a, enabled, multi)}
                    onClick={() => void selectApp(a.id)}
                  >
                    {on && <span className="tick">✓</span>}
                    {a.label}
                    {on && enabled && <span className={"app-dot " + appHealth(a, enabled)} />}
                  </button>
                );
              })}
            </div>
          </div>
        )}

        <div className="hero-row">
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
          <div className="hero-row">
            <span
              className="field-label"
              title="没勾选的账号一律不参与扣费；在勾选的账号里，积分到期最早的最先被使用"
            >
              参与扣费
            </span>
            <div className="chips">
              {accounts.map((a) => {
                const on = billingOn(a.id);
                return (
                  <button
                    key={a.id}
                    className={"chip" + (on ? " on" : "")}
                    disabled={busy}
                    title={chipTitle(a)}
                    onClick={() => toggleBilling(a.id)}
                  >
                    {on && <span className="tick">✓</span>}
                    {shortLabel(a)}
                  </button>
                );
              })}
            </div>
          </div>
        )}

      </section>

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
