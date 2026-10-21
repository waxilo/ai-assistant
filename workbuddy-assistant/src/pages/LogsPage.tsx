import { useEffect, useMemo, useState } from "react";
import { getLogs, clearLogs } from "../api";
import type { LogCategory, LogEntry } from "../types";
import { EmptyState, StatusDot, logStatus } from "../common";
import type { ConfirmReq, Toast } from "../common";
import { IconList, IconSearch, IconTrash } from "../components/Icons";

/**
 * 日志管理页：把**全部**日志并成一条按时间倒序的流水。
 *
 * 数据源有两路（后端 `logs::unified` 已并好）：
 * - 应用日志：签到 / 续签 / 通知 / 系统，落盘、跨重启；
 * - 接管动态：谁用了哪个账号、限流切没切（`takeover-journal.jsonl`，持久化）。
 *
 * 「报错」不是一个类别，而是**横跨全部的成败筛选**：排查时关心的是「哪里出错了」，
 * 不是「哪个模块出错了」，所以来源与成败是两个独立的筛选轴。
 *
 * 每 5 秒轮询而不是只读一次：日志多半由后台线程写（定时签到 / 自动续签 / 反代），
 * 打开页面时它们已经发生；轮询才能让它像「正在看」而不是一张快照。
 * 窗口隐藏时跳过（与接管页同一个口径）。
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

/**
 * 从账号条目跳进来时，用账号名预填搜索框锁定该账号的记录（应用日志按账号名记）。
 */
export function LogsPage({
  initialAccount,
  askConfirm,
  onToast,
}: {
  initialAccount?: string;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onToast: (t: Toast) => void;
}) {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState(initialAccount ?? "");
  const [filter, setFilter] = useState<Filter>("all");

  useEffect(() => {
    let alive = true;
    const load = () => {
      getLogs()
        .then((rows) => {
          if (!alive) return;
          setLogs(rows);
          setError(null);
        })
        .catch((e) => {
          if (alive) setError(String(e));
        })
        .finally(() => {
          if (alive) setLoading(false);
        });
    };
    load();
    const t = window.setInterval(() => {
      if (document.hidden) return;
      load();
    }, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, []);

  /** 每个筛选项的条数（报错是 `!success` 的横切计数）。 */
  const counts = useMemo(() => {
    const c: Record<Filter, number> = {
      all: logs.length,
      error: 0,
      checkin: 0,
      renew: 0,
      notify: 0,
      takeover: 0,
      system: 0,
    };
    for (const l of logs) {
      if (!l.success) c.error += 1;
      c[l.category] += 1;
    }
    return c;
  }, [logs]);

  const shown = useMemo(() => {
    const k = q.trim().toLowerCase();
    return logs.filter((l) => {
      if (filter === "error" && l.success) return false;
      if (filter !== "all" && filter !== "error" && l.category !== filter) return false;
      if (!k) return true;
      return (
        l.account.toLowerCase().includes(k) ||
        l.message.toLowerCase().includes(k) ||
        CATEGORY_LABEL[l.category].includes(k)
      );
    });
  }, [logs, q, filter]);

  /** 清空日志（不可恢复）：应用日志与接管动态一起清，界面上不留半张流水。 */
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
      onToast({ kind: "err", text: "清空失败：" + e });
    }
  };

  return (
    <section className="panel-page">
      {/* 工具条：上一行是搜索与动作，下一行是来源筛选 —— 七个类别挤一行会互相挤压 */}
      <div className="card logs-toolbar">
        <div className="logs-toolbar-row">
          <label className="search-box">
            <IconSearch size={16} />
            <input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="搜索账号、类别或内容"
            />
          </label>
          <span className="spacer" />
          <span className="count">
            {loading
              ? "加载中…"
              : q || filter !== "all"
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
                : "换一个关键字或类别试试")
            }
          />
        </div>
      ) : (
        <div className="table-wrap card logs-scroll">
          <table className="data-table logs-table">
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
                    <td className="log-cell-msg">{l.message || "—"}</td>
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
