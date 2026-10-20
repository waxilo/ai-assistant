import { memo, useCallback, useEffect, useMemo, useState } from "react";
import type { Account, LogCategory, LogEntry } from "../types";
import { clearLogs, getLogs } from "../api";
import {
  EmptyState,
  StatusDot,
  accountIdent,
  accountLabel,
  expiryInfo,
  formatCredits,
  logStatus,
} from "../common";
import type { ConfirmReq, Toast } from "../common";
import { IconList, IconSearch, IconTrash } from "../components/Icons";

/**
 * 「日志管理」页：把**全部**日志并成一条按时间倒序的流水。
 *
 * 数据源有三路（后端 `logs::unified` 已并好）：签到日志、调度器日志（续签/通知/系统）、
 * 接管动态。「报错」不是一个类别，而是**横跨全部的成败筛选**：排查时关心的是
 * 「哪里出错了」，不是「哪个模块出错了」，所以来源与成败是两个独立的筛选轴。
 *
 * 每 5 秒轮询而不是只读一次：日志多半由后台线程写（定时签到、12 小时续签扫描、
 * 每小时的简报采样），打开页面时它们已经发生；轮询才能让它像「正在看」而不是
 * 一张快照。窗口隐藏时跳过（与接管页同一个口径）。
 */
const POLL_MS = 5000;

type Filter = "all" | "error" | LogCategory;

const FILTERS: { key: Filter; label: string }[] = [
  { key: "all", label: "全部" },
  { key: "error", label: "报错" },
  { key: "checkin", label: "签到" },
  { key: "renew", label: "续签" },
  { key: "notify", label: "通知" },
  { key: "takeover", label: "接管" },
  { key: "system", label: "系统" },
];

const CATEGORY_LABEL: Record<LogCategory, string> = {
  checkin: "签到",
  renew: "续签",
  notify: "通知",
  takeover: "接管",
  system: "系统",
};

/** 时间列只保留必要精度：今天给时分秒，跨天带上月日；完整时间在 title 里。 */
function shortTime(l: LogEntry): string {
  const hms = l.at.length >= 19 ? l.at.slice(11, 19) : l.at;
  const isToday = new Date(l.at_ms).toDateString() === new Date().toDateString();
  return isToday ? hms : `${l.at.slice(5, 10)} ${hms.slice(0, 5)}`;
}

export function LogsPage({
  accounts,
  initialAccountId,
  askConfirm,
  onToast,
}: {
  accounts: Account[];
  initialAccountId?: string;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onToast: (t: Toast) => void;
}) {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  // 空串 = 全部账号；从账号条目进入时初始为该账号。统一流只带账号名，
  // 筛选也按名对（流里的 `account` 就是它）。
  const [accountName, setAccountName] = useState(() => {
    const acc = accounts.find((a) => a.id === initialAccountId);
    return acc ? acc.name : "";
  });

  const refresh = useCallback(() => {
    getLogs()
      .then((rows) => {
        setLogs(rows);
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    refresh();
    const t = window.setInterval(() => {
      if (document.hidden) return;
      refresh();
    }, POLL_MS);
    return () => window.clearInterval(t);
  }, [refresh]);

  /** 每个筛选项的条数（报错是 `!success` 的横切计数）。 */
  const counts = useMemo(() => {
    const c: Record<Filter, number> = {
      all: 0,
      error: 0,
      checkin: 0,
      renew: 0,
      notify: 0,
      takeover: 0,
      system: 0,
    };
    for (const l of logs) {
      if (accountName && l.account !== accountName) continue;
      c.all += 1;
      if (!l.success) c.error += 1;
      c[l.category] += 1;
    }
    return c;
  }, [logs, accountName]);

  const shown = useMemo(() => {
    const k = q.trim().toLowerCase();
    return logs.filter((l) => {
      if (accountName && l.account !== accountName) return false;
      if (filter === "error" && l.success) return false;
      if (filter !== "all" && filter !== "error" && l.category !== filter) return false;
      if (!k) return true;
      return (
        l.account.toLowerCase().includes(k) ||
        l.message.toLowerCase().includes(k) ||
        CATEGORY_LABEL[l.category].includes(k)
      );
    });
  }, [logs, q, filter, accountName]);

  /** 清空日志（不可恢复）：签到、调度、接管三路一起清，界面上不留半张流水。 */
  const doClear = async () => {
    const ok = await askConfirm({
      title: "清空全部日志",
      body: `确认清空全部日志（当前 ${logs.length} 条，含接管动态）？清空后无法恢复。`,
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearLogs();
      setLogs([]);
      onToast({ kind: "ok", text: "日志已清空" });
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
    }
  };

  return (
    <section className="panel-page">
      {/* 工具条：上一行是账号 / 搜索与动作，下一行是来源筛选 —— 七个类别挤一行会互相挤压 */}
      <div className="card logs-toolbar">
        <div className="logs-toolbar-row">
          <label className="filter">
            账号
            <select value={accountName} onChange={(e) => setAccountName(e.target.value)}>
              <option value="">全部账号</option>
              {accounts.map((a) => (
                <option key={a.id} value={a.name}>
                  {accountLabel(a.name, accountIdent(a.phone, a.email))}
                </option>
              ))}
            </select>
          </label>
          <label className="search-box">
            <IconSearch size={16} />
            <input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="搜索类别或内容"
            />
          </label>
          <span className="spacer" />
          <span className="count">
            {loading
              ? "加载中…"
              : q || filter !== "all" || accountName
                ? `匹配 ${shown.length} / ${logs.length} 条`
                : `共 ${logs.length} 条`}
          </span>
          <button
            className="btn small danger"
            disabled={logs.length === 0}
            onClick={() => void doClear()}
          >
            <IconTrash size={15} />
            清空日志
          </button>
        </div>
        <div className="log-filters">
          {FILTERS.map((f) => (
            <button
              key={f.key}
              className={"chip" + (filter === f.key ? " on" : "")}
              title={`只看${f.label}（${counts[f.key]} 条）`}
              onClick={() => setFilter(f.key)}
            >
              {f.label}
              <span className="n">{counts[f.key]}</span>
            </button>
          ))}
        </div>
      </div>

      {loading ? (
        <div className="card">
          <p className="empty">加载中…</p>
        </div>
      ) : shown.length === 0 ? (
        <div className="card">
          <EmptyState
            icon={<IconList size={26} />}
            title={
              error ? "日志读取失败" : logs.length === 0 ? "暂无日志" : "没有匹配的记录"
            }
            hint={
              error ??
              (logs.length === 0
                ? "签到、续签、通知与接管动作都会记录在这里"
                : "换一个关键字、类别或账号试试")
            }
          />
        </div>
      ) : (
        <div className="table-wrap card logs-scroll">
          <table className="data-table">
            <thead>
              <tr>
                <th>时间</th>
                <th>类型</th>
                <th>账号</th>
                <th>结果</th>
                <th>内容</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((l, i) => {
                const st = logStatus(l);
                return (
                  <tr key={`${l.at_ms}-${i}`}>
                    <td className="log-cell-time" title={l.at}>
                      {shortTime(l)}
                    </td>
                    <td>
                      <span className={`log-tag ${l.category}`}>
                        {CATEGORY_LABEL[l.category]}
                      </span>
                    </td>
                    <td className="log-cell-account">{l.account || "—"}</td>
                    <td>
                      <StatusDot tone={st.tone} label={st.label} />
                    </td>
                    <td className="log-cell-msg">
                      {l.message || "—"}
                      {/* 签到行的富字段原样保留（来自旧日志页）：本次额度、这笔积分
                          的到期、当天的活动键。挂在内容列下面而不新开列 —— 统一流
                          已经五列，再加列窄窗必然溢出，而溢出在这里是静默截断。 */}
                      {l.category === "checkin" && (l.credit != null || l.campaign_key) && (
                        <span className="log-key">
                          {l.credit != null && (
                            <span className="log-credit">
                              本次 +{formatCredits(l.credit)}
                            </span>
                          )}
                          {l.expires_at != null && (
                            <span title="这笔积分的到期时间">
                              {" "}
                              至 {expiryInfo(l.expires_at).text}
                            </span>
                          )}
                          {l.balance != null && (
                            <span title="签到那一刻读到的剩余积分">
                              {" "}
                              余额 {formatCredits(l.balance)}
                            </span>
                          )}
                          {l.campaign_key && (
                            <span className="mono" title="当天的活动键">
                              {" "}
                              {l.campaign_key}
                            </span>
                          )}
                        </span>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

export default memo(LogsPage);
