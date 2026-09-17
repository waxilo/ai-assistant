import type { ReactNode } from "react";
import type { LogEntry } from "./types";
import { IconUser } from "./components/Icons";

/**
 * 跨页面共享的 UI 基础件：类型、展示助手与状态原子。
 *
 * 这里的组件是「同一个概念在多个页面必须长得一样」的地方 —— 状态圆点、账号单元格、
 * 空态、时间格式。页面各自复制一份必然会漂移（旧版账号页用 `<span className="tag">`
 * 实心胶囊表示签到状态、日志页则纯文字拼接并自算一套时间格式，就是那样来的）。
 *
 * 旧版 src/format.ts 也被收进这里：那些函数（mmdd / stamp / daysUntil）和展示助手
 * 是同一类东西，分散在两个文件里只会让「改格式要改哪」变成一个需要搜索的问题。
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

// ---------------------------------------------------------------------------
// 展示助手
// ---------------------------------------------------------------------------

/** 手机号脱敏（纯展示）：11 位纯数字按 138****1234 处理，其它字符串原样返回 */
export function maskPhone(s: string): string {
  return /^\d{11}$/.test(s) ? s.slice(0, 3) + "****" + s.slice(7) : s;
}

/** token 预览：只留头尾，中间省略（够核对凭证，又不把整串铺在表格里） */
export function maskToken(t: string): string {
  if (!t) return "—";
  return t.length > 14 ? `${t.slice(0, 6)}…(${t.length})` : t;
}

/// 账号在表格/日志中的展示名：名称 + 手机号（手机号缺失或与名称相同时省略）。
export function accountLabel(name: string, phone?: string | null): string {
  const n = maskPhone(name);
  const p = phone ? maskPhone(phone) : "";
  return p && p !== n ? `${n}（${p}）` : n;
}

/**
 * 积分展示：最多两位小数、不留尾随 0。
 *
 * 接口给的是 `805.14000097` 这种精度，直接铺进表格既挤又会把列宽撑爆；
 * **合计与逐行必须用同一个函数**，否则「总积分」与列表里各行之和会对不上。
 */
export function formatCredits(v?: number | null): string {
  if (v == null) return "—";
  return String(Math.round(v * 100) / 100);
}

/** 毫秒 → `MM-DD`（积分包到期只按自然日比大小，日粒度足够） */
export function mmdd(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/**
 * 毫秒 → `MM-DD HH:MM`。
 * token 有效期是「小时」量级（自动续签在到期前 24 小时内动手），只给 `MM-DD`
 * 会看不出「还剩几小时」，所以这里保留到分钟。
 */
export function stamp(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/**
 * 距今天还有几个自然日：今天到期 = 0、已过期 < 0。
 *
 * 先各自归零到当天 0 点再相减 —— 直接减毫秒会因「现在几点」把 23 小时算成 0 天。
 */
export function daysUntil(ms: number): number {
  const target = new Date(ms);
  target.setHours(0, 0, 0, 0);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  return Math.round((target.getTime() - today.getTime()) / 86_400_000);
}

/** 毫秒时间戳 → 人话的「多久之前」。凭证池「上次同步」那句用 */
export function sinceMs(ms: number): string {
  const min = Math.floor((Date.now() - ms) / 60000);
  if (min < 1) return "刚刚";
  if (min < 60) return `${min} 分钟前`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr} 小时前`;
  return `${Math.floor(hr / 24)} 天前`;
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

// ---------------------------------------------------------------------------
// 状态原子
// ---------------------------------------------------------------------------

export type DotTone = "signing" | "done" | "pending" | "fail" | "inactive";

/** 状态圆点：全应用唯一的「状态」表达。表格里的状态列一律用它，不要再写实心胶囊。 */
export function StatusDot({ tone, label }: { tone: DotTone; label: string }) {
  return (
    <span className={`status-dot ${tone}`}>
      <i className="dot" />
      <span>{label}</span>
    </span>
  );
}

/**
 * 签到日志的一条结果 → 状态圆点。
 * 与账号页的状态列同一套视觉，只是文案不同（那里是「已签 / 待签」，这里是「成功 / 失败」）。
 */
export function logStatus(l: LogEntry): { tone: DotTone; label: string } {
  return l.success ? { tone: "done", label: "成功" } : { tone: "fail", label: "失败" };
}

/**
 * 账号单元格：头像 + 名称 + 手机号。
 *
 * 账号页与签到日志页共用，「同一个人在两处长得一样」由它保证。
 * 名称来自服务端 `ScreenName`，形如「用户0044120650」—— 光看名字认不出是哪个号，
 * 手机号才是人认得的标识，所以直接显示而不是藏进 title。
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
        {alt && alt !== shown && <span className="ac-phone">{alt}</span>}
      </div>
    </div>
  );
}

/** 空态：图标 + 主文案 +（可选）补充说明。各页共用。 */
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
