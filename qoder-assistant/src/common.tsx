import type { ReactNode } from "react";
import type { Account, CheckinLog } from "./types";
import { IconUser } from "./components/Icons";
import { regionLabel, useRegions } from "./regions";

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

/** 手机号脱敏（纯展示）：11 位纯数字按 138****1234 处理，其它字符串原样返回
    （邮箱等标识没有要打码的部分，本来就该完整显示）。 */
export function maskPhone(s: string): string {
  return /^\d{11}$/.test(s) ? s.slice(0, 3) + "****" + s.slice(7) : s;
}

/**
 * 账号的**展示标识**：国内版看手机号、国际版看邮箱（另一项作为回退）。
 *
 * 区域决定哪一项才是「这个账号是谁」—— 与后端 `Account::identity` 是同一套规则。
 * 两处各写一份的下场是「界面显示的那个」与「去重 / 补全认的那个」对不上。
 * 区域未知时（历史日志没有这个字段、settings 还没读到）按手机号优先，
 * 与加邮箱之前的展示保持一致。
 */
export function accountIdent(
  region: string | null | undefined,
  phone?: string | null,
  email?: string | null
): string | null {
  const [first, second] = region === "global" ? [email, phone] : [phone, email];
  return first || second || null;
}

/** 字节数展示：B / KB / MB（更新下载进度用）。非法输入返回「—」 */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${Math.round(n)} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/// 账号在日志/筛选中的展示名：名称 + 展示标识（[`accountIdent`] 按区域选好的那个，
/// 缺失时省略）。名称本身是手机号时同样脱敏；过滤/匹配请直接用原始字段，不要经过这里。
export function accountLabel(name: string, ident?: string | null): string {
  return ident ? `${maskPhone(name)}（${maskPhone(ident)}）` : maskPhone(name);
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
 *    因为活动接口的状态也可能刚好处在刷新点上。
 * 2. 否则以持久化的服务端真实状态（`a.checked_today`）为准 ——
 *    它由刷新命令从活动接口的 `claimStatus` 写入，绝不依赖本地试签的陈旧缓存。
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

// `dailyCampaign(view)` 与 `campaignCountdown(endAt)` 曾经在这里：它们服务于账号页那一条
// 「今日权益」（挑出当天 `CLAIM_BENEFIT` 活动、算活动窗口倒计时）。2026-09-18 撤掉那一栏后
// 两者都没有第二个调用方，随 `account_campaigns` 命令一起删除 —— 判定「今天能不能领」的
// 逻辑现在只在**后端** `checkin::plan` / `checked_from_status` 里有一份，这也正是它该在的位置。

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
 * 账号单元格：头像 + 名称 + 展示标识（手机号 / 邮箱，由 [`accountIdent`] 按区域选好）。
 *
 * 账号页与签到日志页共用，「同一个人在两处长得一样」由它保证；日志页此前只用
 * 一行文字拼接，名称本身就是手机号时会显示成 `191****2883（191****2883）`。
 *
 * `ident` 由调用方选好再传进来（而不是这里收 region + phone + email 自己挑）：
 * 区域标签（`region`）是**可选**的，收成同一个字段会让「要标识但不要标签」的
 * 地方（日志 / 简报）被迫带上一个它本不该有的区域胶囊。
 */
export function AccountCell({
  name,
  ident,
  region,
}: {
  name: string;
  /** 展示标识：国内版手机号 / 国际版邮箱，用 [`accountIdent`] 选好 */
  ident?: string | null;
  /**
   * 账号所属区域（`global` / `cn`）。**不传就不显示区域标签** ——
   * 签到日志与简报里没有这个字段，那里也就不该凭空补一个出来。
   */
  region?: string | null;
}) {
  const initial = /^\d/.test(name) ? null : name.slice(0, 1);
  const shown = maskPhone(name);
  const alt = ident ? maskPhone(ident) : "";
  // 区域名取自后端清单；清单还没到货时为 null ⇒ 不显示标签（而不是显示一个空胶囊）
  const badge = regionLabel(useRegions(), region);
  // 标识与名称相同时不再重复
  const sub = alt && alt !== shown ? alt : "";
  return (
    <div className="ac-cell-name">
      <span className="ac-avatar">{initial ?? <IconUser size={16} />}</span>
      <div className="ac-id">
        <span className="ac-name">{shown}</span>
        {/* 次级行：展示标识 + 区域并排。两者都是「这个账号是谁」的补充信息，
            所以同一行；一个都没有就整行不渲染，不留空行。 */}
        {(sub || badge) && (
          <span className="ac-sub">
            {sub && <span className="ac-ident">{sub}</span>}
            {badge && <span className="ac-region">{badge}</span>}
          </span>
        )}
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

/// 积分过期倒计时：毫秒时间戳 → 「还有 N 天后过期」，看起来更直观。
/// 已过期的返回「已过期」并标 expired。null/非法返回「—」。
export function expiryCountdown(
  ms?: number | null
): { text: string; expired: boolean } {
  if (ms == null) return { text: "—", expired: false };
  const d = new Date(ms);
  if (isNaN(d.getTime())) return { text: "—", expired: false };
  const days = Math.ceil((ms - Date.now()) / 86_400_000);
  if (days <= 0) return { text: "已过期", expired: true };
  return { text: `还有 ${days} 天后过期`, expired: false };
}

/** 资源包到期的三种状态（界面文案由 [`packageExpiry`] 统一给） */
export type ExpiryTone = "dated" | "never" | "unknown";

/**
 * 资源包到期展示 —— 三态**唯一**的文案出口。
 *
 * `YYYY-MM-DD` / 「不过期」/ 「未知」都从这儿出，页面不再各自 `new Date(ms)`：
 * 上一版就是在页面里直接渲染时间戳，把服务端表示「永不过期」的
 * `9999-12-31` 哨兵原样印了出来。哨兵现在在后端就被归一了
 *（`ledger::normalize_expiry`），这里再把剩下两种「没有日期」的情形说人话。
 */
export function packageExpiry(p: {
  expiry_ms: number | null;
  never_expires: boolean;
}): { text: string; tone: ExpiryTone } {
  if (p.never_expires) return { text: "不过期", tone: "never" };
  if (p.expiry_ms == null) return { text: "未知", tone: "unknown" };
  const d = new Date(p.expiry_ms);
  if (isNaN(d.getTime())) return { text: "未知", tone: "unknown" };
  const q = (n: number) => String(n).padStart(2, "0");
  return {
    text: `${d.getFullYear()}-${q(d.getMonth() + 1)}-${q(d.getDate())}`,
    tone: "dated",
  };
}
