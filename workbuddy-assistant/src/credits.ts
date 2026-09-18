import { useSyncExternalStore } from "react";
import { listen } from "@tauri-apps/api/event";
import type { Account, CreditFact, CreditRow } from "./types";
import { CREDITS_UPDATED_EVENT } from "./api";

/**
 * 积分内存对象 —— 「账号还剩多少积分」在前端**唯一的一份数据**。
 *
 * 账号页的「剩余积分 / 总积分」、简报页的「当前剩余」读的都是它；任何路径拿到的读数
 * （启动加载 / 手动刷新 / 签到 / 后台每小时采样 / 开启简报 / 接管路由补拉）都往它里面并。
 *
 * # 为什么必须是全局单例，而不是某个组件的 state
 *
 * 采集发生在后端，而且**后台线程也会采**（每小时一次，应用最小化时照样跑），
 * 结果由 `credits-updated` 事件推过来。事件是全局面，把它落进组件 state 就会出现
 * 三种漂移：谁先挂载谁拿到、页面切走再回来各自再取一次、以及「简报页采了、账号页不知道」。
 * 放在模块级 + 订阅之后，采集一次、所有读它的地方同时更新。
 *
 * 这与后端那本 `ledger::store()`（进程内唯一台账）是**同一条链的两端**：
 * 后端一份内存台账 → 每次写入广播 → 前端一份内存对象。中间的落盘文件只是持久化，
 * 两边都不把它当数据源去各读一份。
 */

/** 「账号 id → 积分事实」。只在写入时整体换引用（`useSyncExternalStore` 靠它判断变更） */
export type CreditBook = Record<string, CreditFact>;

type Listener = () => void;

let book: CreditBook = {};
const listeners = new Set<Listener>();

function publish(next: CreditBook) {
  book = next;
  for (const l of listeners) l();
}

/** 订阅变更（React 走 [`useCredits`]；非 React 的地方也可以直接用） */
export function subscribeCredits(l: Listener): () => void {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

/** 当前快照（未变更时引用不变） */
export function creditBook(): CreditBook {
  return book;
}

/** React 绑定：组件读到的就是全局那一份 */
export function useCredits(): CreditBook {
  return useSyncExternalStore(subscribeCredits, creditBook);
}

/**
 * 取值一律**显式传入那份对象**（由 [`useCredits`] 或 [`creditBook`] 得到），
 * 而不是让函数自己去读模块变量：这样「读的是哪一份」在调用点一目了然，
 * 也不会再出现「某个页面顺手从 `Account` 上取一个旧值」的旁路。
 */

/** 某个账号当前的剩余积分（未知 = `null`，界面显示「—」，绝不编一个 0） */
export function creditsOf(book: CreditBook, id: string): number | null {
  return book[id]?.credits ?? null;
}

/** 读数时刻（空串 = 这个账号还没读到过） */
export function creditsAt(book: CreditBook, id: string): string {
  return book[id]?.at ?? "";
}

/** 最早重置／过期时刻（毫秒）；未知 = `null`（界面显示「—」） */
export function expiryOf(book: CreditBook, id: string): number | null {
  return book[id]?.earliest_expiry_ms ?? null;
}

/** 某账号的逐资源包明细（空数组 = 没有或还没读到） */
export function packagesOf(book: CreditBook, id: string) {
  return book[id]?.packages ?? [];
}

/** 快过期提示：最早到期（非 null）的资源包，`{daysUntil, remaining}`；没有就返回 `null` */
export function soonestExpiry(book: CreditBook, id: string) {
  const pkgs = packagesOf(book, id).filter((p) => p.expiry_ms != null);
  if (pkgs.length === 0) return null;
  const p = pkgs[0];
  return { daysUntil: Math.ceil((p.expiry_ms! - Date.now()) / 86_400_000), remaining: p.remaining };
}

/**
 * 最近一次读数时刻（全部账号里最新那条；一条都没有 → 空串）。
 *
 * 用来回答「这个数是什么时候的」—— 尤其简报页展示「当前剩余」时必须带上它，
 * 否则用户没法判断它是刚采的还是半小时前的。
 */
export function latestAt(book: CreditBook): string {
  let at = "";
  for (const id in book) {
    const t = book[id]?.at ?? "";
    // `YYYY-MM-DD HH:MM:SS` 的字典序就是时间序，直接比字符串即可
    if (t > at) at = t;
  }
  return at;
}

/**
 * 一批账号的积分合计（账号页的「总积分」）。
 *
 * 按每个账号**展示用的两位小数**累加，保证它恰好等于列表里各行「剩余积分」之和；
 * 没读到的账号不计入（0 与「未知」含义不同）；一个都没读到 → `null`，调用方显示「—」。
 */
export function totalCredits(book: CreditBook, accounts: Account[]): number | null {
  let sum = 0;
  let known = false;
  for (const a of accounts) {
    const v = creditsOf(book, a.id);
    if (v == null) continue;
    sum += Math.round(v * 100) / 100;
    known = true;
  }
  return known ? Math.round(sum * 100) / 100 : null;
}

/**
 * 一条读数是否比手上这条更新。
 *
 * 空 `at`（连「什么时候读到的」都不知道的读数）永远不算更新 —— 它不足以覆盖
 * 一条有时刻的读数。`YYYY-MM-DD HH:MM:SS` 的字典序就是时间序（与 [`latestAt`] 同一套约定）。
 */
function fresher(incoming: CreditFact, current: CreditFact | undefined): boolean {
  if (!current) return true;
  if (incoming.at === "") return false;
  return incoming.at >= current.at;
}

/**
 * 从后端返回的账号记录里取出积分事实。
 *
 * `a.credits` 是后端台账的投影（`AccountView`）；缺失时退回「最近一次**签到**读到的
 * 余额」—— 那是刚升级上来、台账还没采过一次的窗口。**兜底只在这一处**：
 * 页面读到的一律是这份对象里的值，所以不会再有「这个页面兜底、那个页面不兜底」的偏差。
 */
function factOf(a: Account): CreditFact | null {
  if (a.credits) return a.credits;
  if (a.last?.balance == null) return null;
  return { credits: a.last.balance, at: a.last.at, earliest_expiry_ms: null, packages: [] };
}

/**
 * 把一份账号列表里的积分事实并进来（**不在列表里的账号保持原样**）。
 *
 * 用于启动加载与签到／刷新这类「后端顺手把读数投影在返回值里」的路径：
 * 单个签到传一个、批量传一批，语义都一样。
 *
 * **只往新的方向走**：命令返回值与 `credits-updated` 事件是两条并行到达的通道，
 * 谁先谁后由 IPC 调度决定。少了这道守卫，先到的新读数会被后到的旧快照盖回去 ——
 * 症状就是「点刷新积分反而退回上一次签到的数」。
 */
export function seedCredits(accounts: Account[]) {
  if (accounts.length === 0) return;
  const next: CreditBook = { ...book };
  let changed = false;
  for (const a of accounts) {
    const incoming = factOf(a);
    if (!incoming || !fresher(incoming, next[a.id])) continue;
    next[a.id] = incoming;
    changed = true;
  }
  if (changed) publish(next);
}

/** 并入后端广播的读数（只动推送里出现的账号，其余保持原样；同样只许往新的方向走） */
export function mergeCredits(rows: CreditRow[]) {
  if (rows.length === 0) return;
  const next: CreditBook = { ...book };
  let changed = false;
  for (const r of rows) {
    const incoming: CreditFact = {
      credits: r.credits,
      at: r.at,
      earliest_expiry_ms: r.earliest_expiry_ms,
      packages: r.packages ?? [],
    };
    if (!fresher(incoming, next[r.id])) continue;
    next[r.id] = incoming;
    changed = true;
  }
  if (changed) publish(next);
}

let bound = false;

/**
 * 接上后端广播（幂等，由根组件调用一次）。
 *
 * 事件是全局的，注册点也该是全局的：接在某个页面里会随页面挂载/卸载反复注册，
 * 而采集恰恰发生在页面没打开的时候（后台整点采样）。
 */
export function bindCredits() {
  if (bound) return;
  bound = true;
  void listen<CreditRow[]>(CREDITS_UPDATED_EVENT, (e) => mergeCredits(e.payload));
}
