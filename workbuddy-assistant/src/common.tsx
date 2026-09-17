import type { ReactNode } from "react";
import type { Account, CheckinLog } from "./types";
import { IconUser } from "./components/Icons";

/**
 * 跨页面共享的 UI 基础件：类型、展示助手与状态原子。
 *
 * 这里的组件是「同一个概念在多个页面必须长得一样」的地方——状态圆点、账号单元格。
 * 页面各自复制一份必然会漂移（历史上账号页用圆点、日志页用实心胶囊，就是那样来的）。
 */

export type Toast = { kind: "ok" | "err" | "info"; text: string } | null;

/** 自研确认框的请求描述（resolve 由 App 统一收口） */
export type ConfirmReq = {
  title: string;
  body?: string;
  okText?: string;
  danger?: boolean;
  resolve: (ok: boolean) => void;
};

export function maskToken(t: string): string {
  if (t.length <= 12) return "•".repeat(t.length);
  return t.slice(0, 6) + "…" + t.slice(-4);
}

/** 路径取文件名（兼容 Windows 反斜杠） */
export function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

/**
 * 复制文本到剪贴板。返回是否成功。
 *
 * 两级兜底：`navigator.clipboard` 在 WKWebView 里常常直接不可用（非 https 上下文时
 * 整个对象都不存在），而它一失败就静默什么都没复制 —— 用户点一下「已复制」却发现
 * 粘贴出来是空的，比不复制更糟。所以退回 `execCommand` 那条老路（WebKit 至今支持），
 * 两条都失败就返回 false，由调用方把原文显示出来让用户手动选。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 落到 execCommand 那条路
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "-1000px";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}


/** 剩余积分展示：最多两位小数且不留尾随 0（接口给的是 805.14000097 这种精度） */
export function formatCredits(v?: number | null): string {
  if (v == null) return "—";
  return String(Math.round(v * 100) / 100);
}

// 积分取值函数（`creditsOf` / `totalCredits` …）**不在这里**：它们属于那个
// 「全局积分内存对象」，见 `src/credits.ts`。留在公共 UI 件里会诱使调用方
// 从 `Account` 上就地读 `credits / last.balance` —— 那正是两个页面数字对不上的老路。

/** 手机号脱敏（纯展示）：11 位纯数字按 138****1234 处理，其它字符串原样返回 */
export function maskPhone(s: string): string {
  return /^\d{11}$/.test(s) ? s.slice(0, 3) + "****" + s.slice(7) : s;
}

/** 字节数展示：B / KB / MB（更新下载进度用）。非法输入返回「—」 */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${Math.round(n)} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/// 账号在日志/筛选中的展示名：名称 + 手机号（手机号缺失时省略）。
/// 名称本身是手机号时同样脱敏；过滤/匹配请直接用原始字段，不要经过这里。
export function accountLabel(name: string, phone?: string | null): string {
  return phone ? `${maskPhone(name)}（${maskPhone(phone)}）` : maskPhone(name);
}

/** 解析「YYYY-MM-DD HH:MM:SS」为 Date；格式不符返回 null */
function parseAt(at?: string | null): Date | null {
  if (!at) return null;
  const d = new Date(at.replace(" ", "T"));
  return isNaN(d.getTime()) ? null : d;
}

/** 相对时间：刚刚 / N 分钟前 / N 小时前 / 昨天 / N 天前 / 日期（用于「最近签到」主信息） */
export function relativeTime(at?: string | null): string {
  const d = parseAt(at);
  if (!d) return "";
  const diff = Date.now() - d.getTime();
  const min = Math.floor(diff / 60000);
  if (min < 1) return "刚刚";
  if (min < 60) return `${min} 分钟前`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr} 小时前`;
  const day = Math.floor(hr / 24);
  if (day === 1) return "昨天";
  if (day < 7) return `${day} 天前`;
  return at!.slice(0, 10);
}

/** 是否为今天（本地时区） */
export function isToday(at?: string | null): boolean {
  const d = parseAt(at);
  if (!d) return false;
  const now = new Date();
  return (
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate()
  );
}

export type SignState = "signing" | "done" | "pending" | "fail" | "inactive";

/**
 * 账号签到状态：用于状态列徽标 + 顶部统计卡片（busy 优先于一切）。
 *
 * 判定优先级（解决「状态不真实 / 本地报错 / 跨天不复位」三类问题）：
 * 1. 今天真正点过签到（`last.at` 是今天）→ 以那次真实结果为准（最权威），
 *    因为官方 `checkin-status` 的 `today_checked_in` 偶发不可靠，必须让真实尝试压过它。
 * 2. 否则以持久化的服务端「今日是否已签到」真实状态（`a.checked_today`）为准
 *    （由刷新命令写入 accounts.json，绝不依赖本地试签的陈旧缓存）。
 * 3. 都没有今天的真实依据 → 「待签到」。`checked_today===false` 或查询失败(null) 都算未知，
 *    绝不再把昨天的 `already` 当「已签」、也不再拿陈旧本地报错当「失败」。
 */
export function signState(a: Account, busy: boolean): SignState {
  if (busy) return "signing";
  const r = a.last;
  // 今天有过一次真实签到尝试：以它的结果为准
  if (r && isToday(r.at)) {
    if (r.already || r.success) return "done";
    if (r.inactive) return "inactive";
    return "fail"; // 今天真的签失败了（网络 / 鉴权 / 非活动未开以外的真错误）
  }
  // 没有今天的真实尝试：以持久化的服务端真实返回为准
  if (a.checked_today === true) return "done";
  // false / 查询失败(null) / 未查询 → 今天状态未知，显示「待签到」
  return "pending";
}

/** 一批签到结果的互斥计数（成功 / 已签 / 失败），避免「已签」被重复算成「成功」 */
export function tally(
  items: { success: boolean; already: boolean; inactive: boolean }[]
) {
  return {
    ok: items.filter((l) => l.success && !l.already).length,
    already: items.filter((l) => l.already).length,
    fail: items.filter((l) => !l.success && !l.already && !l.inactive).length,
  };
}

/**
 * 账号最近一次签到结果的**状态圆点**文案。
 *
 * 注意顺序：服务端对「今天已签到」返回 HTTP 400 + `code=10001`，
 * 此时 `success` 也是 true（幂等成功），所以必须**先判 already**，
 * 否则「今日已签」永远显示成「成功」。
 */
export type DotTone = "signing" | "done" | "pending" | "fail" | "inactive";

/** 状态圆点：全应用唯一的「状态」表达。表格里的状态列一律用它，不要再用实心胶囊。 */
export function StatusDot({ tone, label }: { tone: DotTone; label: string }) {
  return (
    <span className={`status-dot ${tone}`}>
      <i className="dot" />
      <span>{label}</span>
    </span>
  );
}

/** 签到日志的一条结果 → 状态圆点（与账号页「状态」列同一套视觉，只是文案不同） */
export function logStatus(log: CheckinLog): { tone: DotTone; label: string } {
  // 同 signState：`already` 必须优先于 `success`（已签时二者都为 true）
  if (log.already) return { tone: "done", label: "今日已签" };
  if (log.success) return { tone: "done", label: "成功" };
  if (log.inactive) return { tone: "inactive", label: "活动未开" };
  return { tone: "fail", label: "失败" };
}

/**
 * 账号单元格：头像 + 名称 + 手机号。
 *
 * 账号页与签到日志页共用，「同一个人在两处长得一样」由它保证；日志页此前只用
 * 一行文字拼接，名称本身就是手机号时会显示成 `191****2883（191****2883）`。
 */
export function AccountCell({
  name,
  phone,
}: {
  name: string;
  phone?: string | null;
}) {
  const initial = /^\d/.test(name) ? null : name.slice(0, 1);
  const shown = maskPhone(name);
  const alt = phone ? maskPhone(phone) : "";
  return (
    <div className="ac-cell-name">
      <span className="ac-avatar">{initial ?? <IconUser size={16} />}</span>
      <div className="ac-id">
        <span className="ac-name">{shown}</span>
        {/* 手机号与名称相同时不再重复一行 */}
        {alt && alt !== shown && <span className="ac-phone">{alt}</span>}
      </div>
    </div>
  );
}

/** 空态：图标 + 主文案 +（可选）补充说明。账号页与签到日志页共用。 */
export function EmptyState({
  icon,
  title,
  hint,
}: {
  icon?: ReactNode;
  title: string;
  hint?: string;
}) {
  return (
    <div className="empty">
      {icon}
      <span>{title}</span>
      {hint && <span className="empty-sub">{hint}</span>}
    </div>
  );
}

/// 积分过期时间展示：毫秒时间戳 → 「MM-DD HH:mm」；已过期的标 expired。
/// 返回 { text, expired }，UI 用 expired 加样式。null/非法返回「—」。
export function expiryInfo(
  ms?: number | null
): { text: string; expired: boolean } {
  if (ms == null) return { text: "—", expired: false };
  const d = new Date(ms);
  if (isNaN(d.getTime())) return { text: "—", expired: false };
  const expired = ms < Date.now();
  const p = (n: number) => String(n).padStart(2, "0");
  const text = `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  return { text, expired };
}
