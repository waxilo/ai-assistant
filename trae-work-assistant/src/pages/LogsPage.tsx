import { memo, useEffect, useMemo, useState } from "react";
import { getLogs, clearLogs } from "../api";
import type { LogEntry } from "../types";
import { EmptyState, StatusDot, logStatus, type ConfirmReq, type Toast } from "../common";
import { IconList, IconTrash } from "../components/Icons";

/**
 * 签到日志页：按时间倒序列出全部记录，可按账号 / 内容搜索。
 *
 * 展示层与「账号与签到」页共用同一套语言：结果列走 common 的 `StatusDot`
 * （圆点 + 文字），表格骨架走通用的 `.data-table`。
 *
 * ⚠️ 搜索是**纯前端过滤**：后端的 `get_logs` 只返回一个扁平列表（每条带
 * `account` 名称字符串，没有账号 id），加一个后端筛选参数需要动 IPC 契约，
 * 而日志上限只有几百条 —— 本地过滤既够快又不动后端。
 */
interface Props {
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onToast: (t: Toast) => void;
}

function LogsPage({ askConfirm, onToast }: Props) {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [q, setQ] = useState("");

  const refresh = () => {
    setLoading(true);
    getLogs()
      .then(setLogs)
      .catch(() => setLogs([]))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    refresh();
  }, []);

  const shown = useMemo(() => {
    const k = q.trim().toLowerCase();
    if (!k) return logs;
    return logs.filter(
      (l) =>
        l.account.toLowerCase().includes(k) || l.message.toLowerCase().includes(k)
    );
  }, [logs, q]);

  /** 清空日志（不可恢复） */
  const doClear = async () => {
    const ok = await askConfirm({
      title: "清空日志",
      body: `确认清空全部签到日志（当前 ${logs.length} 条）？清空后无法恢复。`,
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearLogs();
      onToast({ kind: "ok", text: "日志已清空" });
      refresh();
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + e });
    }
  };

  return (
    <>
      {/* 筛选与清空同属这一页的工具条，用与下方表格相同的卡片语言承载 */}
      <div className="card logs-toolbar">
        <label className="filter">
          搜索
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="账号或消息关键字"
          />
        </label>
        <span className="spacer" />
        <span className="count">
          {loading ? "加载中…" : q ? `匹配 ${shown.length} / ${logs.length} 条` : `共 ${logs.length} 条`}
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

      {loading ? (
        <div className="card">
          <p className="empty">加载中…</p>
        </div>
      ) : shown.length === 0 ? (
        <div className="card">
          <EmptyState
            icon={<IconList size={26} />}
            title={logs.length === 0 ? "暂无签到记录" : "没有匹配的记录"}
            hint={
              logs.length === 0
                ? "完成签到后，记录会按时间倒序显示在这里"
                : "换一个关键字试试，或清空搜索框"
            }
          />
        </div>
      ) : (
        <div className="table-wrap card logs-scroll">
          <table className="data-table">
            <thead>
              <tr>
                <th>账号</th>
                <th>结果</th>
                <th>时间</th>
                <th>消息</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((l, i) => {
                const st = logStatus(l);
                return (
                  <tr key={`${l.at}-${i}`}>
                    <td>{l.account}</td>
                    <td>
                      <StatusDot tone={st.tone} label={st.label} />
                    </td>
                    <td className="log-cell-time">{l.at}</td>
                    <td className="log-cell-msg">{l.message || "—"}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

export default memo(LogsPage);
