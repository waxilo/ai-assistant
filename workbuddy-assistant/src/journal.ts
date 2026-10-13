/**
 * 接管动态的展示层聚合 —— 纯函数，与页面解耦（便于直接用真实日志回放验证）。
 *
 * 输入是后端 `takeover_events` 的返回值：**已经滤掉 `proxy_request`、且新的在前**。
 *
 * 聚合**不改变一行的长相**：时间、标签、正文一律取代表事件（这一组里最新的那条），
 * 只在标签旁多一个 `×N` —— 与「逐字相同的事件连续出现」共用同一个徽标、同一套长相。
 */

import type { JournalEvent } from "./types";

/** 正文里的「服务会话 <id>」片段（老版后端会写，新版已去掉），展示时一律抹掉 */
const CONV_IN_DETAIL = /\s*服务会话\s*[0-9A-Za-z]+/;

/** 「开始使用账号」事件的开头，只认前缀 */
const ROUTE_START = /^开始使用账号「([^」]*)」/;

/** 「开始使用账号」事件拆出来的账号名 */
export interface RouteStart {
  /** 被扣费的账号名（手机号或用户名） */
  account: string;
}

/**
 * 拆「开始使用账号「X」（当前扣费备选 N 个）」，只认前缀。
 *
 * 不锚定行尾：后端在这句后面追加信息（例如备选数变化）不该让聚合失效。
 * 前缀对不上就返回 null —— 展示层据此退化成「原样一行、不聚合」：
 * 宁可少聚合一次，也不能因为文案改了就把事件显示成空白。
 */
export function parseRouteStart(detail: string): RouteStart | null {
  const m = ROUTE_START.exec(detail.trim());
  return m ? { account: m[1] } : null;
}

/**
 * 展示文案：抹掉「服务会话 <id>」。
 *
 * 会话号是给日志比对用的内部标识，界面上没人看得懂；更要紧的是它**每条都不同**，
 * 留着会让同一个账号的多次「开始使用」看起来像 N 件不同的事。
 * 新版后端已不再写入，这里顺手兜住历史记录（旧日志文件里还有）。
 */
export function displayDetail(e: JournalEvent): string {
  return e.detail.replace(CONV_IN_DETAIL, "");
}

/** 接管动态的一行（可能是多条事件的聚合） */
export interface JournalRow {
  /** 代表事件：这一组里最新的一条（列表新的在前） */
  e: JournalEvent;
  /** 这一行代表的事件条数，>1 时界面在标签旁显示 ×N */
  count: number;
  /** 「开始使用账号」聚合行对应的账号名（悬停提示用） */
  account?: string;
}

/**
 * 接管动态的展示层聚合。事件流**新的在前**：
 *
 * 1. **同一账号的「开始使用账号」（不要求相邻）** → 累加次数：
 *    一个账号本次接管服务了多少个会话，本该读成一行「×N」，
 *    而不是每开一个新会话就刷一条；
 * 2. **相邻且逐字相同**（类型 + detail 都一样）→ 累加次数：
 *    重启风暴、重复心跳这类刷屏只占一行。
 *
 * 两条规则的宽严是刻意的：`route_start` 是唯一会成规模重复的事件，按账号在整个会话内合并，
 * 才看得出「用了哪个号、各用了几次」；其余事件（报错、限流切换、开关）隔着上下文才有意义，
 * 所以只合并**相邻**的逐字相同项，绝不打乱它们的先后。
 */
export function groupJournal(events: JournalEvent[]): JournalRow[] {
  const out: JournalRow[] = [];
  const byAccount = new Map<string, JournalRow>();
  for (const e of events) {
    const rs = e.event === "route_start" ? parseRouteStart(e.detail) : null;
    // ① 同一账号 → 归并到已有那行（代表行仍是最新的那条，位置不动）
    if (rs) {
      const hit = byAccount.get(rs.account);
      if (hit) {
        hit.count += 1;
        continue;
      }
    }
    // ② 相邻的逐字相同 → 累加次数
    const last = out[out.length - 1];
    if (!rs && last && last.e.event === e.event && last.e.detail === e.detail) {
      last.count += 1;
      continue;
    }
    const row: JournalRow = { e, count: 1, account: rs?.account };
    if (rs) byAccount.set(rs.account, row);
    out.push(row);
  }
  return out;
}
