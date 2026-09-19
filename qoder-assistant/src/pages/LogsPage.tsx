import { useCallback, useEffect, useState } from "react";
import type { Account, CheckinLog } from "../types";
import { clearCheckinLogs, getCheckinLogs } from "../api";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  accountLabel,
  expiryInfo,
  formatCredits,
  logStatus,
} from "../common";
import type { ConfirmReq, Toast } from "../common";
import { IconTrash, IconList } from "../components/Icons";

/**
 * 「签到日志」页：按时间倒序列出全部记录，可按账号筛选。
 *
 * 从账号条目跳进来时默认锁定该账号；作为整页后也可以随时切换查看范围。
 *
 * 展示层刻意与「账号签到」页共用同一套语言：账号列走 common 的 AccountCell
 * （头像 + 名称 + 手机号），结果列走 StatusDot（圆点 + 文字），表格骨架走
 * 通用的 .data-table。这些此前都是本页自己的一套（纯文字拼接 + 实心胶囊 +
 * 灰底紧凑表头），同一个概念在两页长得不一样。
 */
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
  const [logs, setLogs] = useState<CheckinLog[]>([]);
  const [loading, setLoading] = useState(true);
  // 空串 = 全部账号；从账号条目进入时初始为该账号
  const [accountId, setAccountId] = useState(initialAccountId ?? "");

  const refresh = useCallback(() => {
    setLoading(true);
    getCheckinLogs(300, accountId || undefined)
      .then(setLogs)
      .catch(() => setLogs([]))
      .finally(() => setLoading(false));
  }, [accountId]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  /** 清空当前筛选范围内的日志（不可恢复） */
  const doClear = async () => {
    const acc = accounts.find((a) => a.id === accountId);
    const ok = await askConfirm({
      title: "清空日志",
      body: acc
        ? `确认清空「${accountLabel(acc.name, acc.phone)}」的全部签到日志？`
        : "确认清空全部签到日志？",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearCheckinLogs(accountId || undefined);
      onToast({ kind: "ok", text: "日志已清空" });
      refresh();
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
    }
  };

  return (
    <section className="panel-page">
      {/* 筛选与清空同属这一页的工具条，用与下方表格相同的卡片语言承载 */}
      <div className="card logs-toolbar">
        <label className="filter">
          账号
          <select value={accountId} onChange={(e) => setAccountId(e.target.value)}>
            <option value="">全部账号</option>
            {accounts.map((a) => (
              <option key={a.id} value={a.id}>
                {accountLabel(a.name, a.phone)}
              </option>
            ))}
          </select>
        </label>
        <span className="spacer" />
        <span className="count">{loading ? "加载中…" : `共 ${logs.length} 条`}</span>
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
        <p className="empty">加载中…</p>
      ) : logs.length === 0 ? (
        <EmptyState
          icon={<IconList size={26} />}
          title="暂无签到记录"
          hint="完成签到后，记录会按时间倒序显示在这里"
        />
      ) : (
        <div className="table-wrap card">
          <table className="data-table">
            <thead>
              <tr>
                <th>账号</th>
                <th>状态</th>
                <th>时间</th>
                <th className="num">本次额度</th>
                {/* 次要列：窄窗口下收起（见 responsive.css）。日志六列在 664px 里
                    必然横向溢出，而滚动条已全局隐藏 —— 溢出即静默截断，所以窄窗
                    必须真的减列。「剩余余额」每次签到都会重新读到，最该让位。 */}
                <th className="num col-secondary">剩余余额</th>
                <th>消息</th>
              </tr>
            </thead>
            <tbody>
              {logs.map((l) => {
                const st = logStatus(l);
                return (
                  <tr key={l.id}>
                    <td>
                      <AccountCell name={l.account_name} phone={l.account_phone} />
                    </td>
                    <td>
                      <StatusDot tone={st.tone} label={st.label} />
                    </td>
                    <td className="log-cell-time">
                      {l.at}
                      {/* 当天的活动键（`act-20260918-899`）：每天换一个，所以它是
                          「这条日志领的是哪一场活动」的唯一凭据 —— 排查「明明点了却没到账」
                          时要靠它去活动页对账。挂在时间下面而不是新开一列：日志已经六列，
                          列数一多窄窗必然溢出，而溢出在这里是静默截断。 */}
                      {l.campaign_key && (
                        <span className="log-key mono" title="当天的活动键">
                          {l.campaign_key}
                        </span>
                      )}
                    </td>
                    <td className="num log-credit">
                      {l.credit != null ? "+" + formatCredits(l.credit) : "—"}
                      {/* 这笔积分**自己的**到期时刻（来自发放凭据）。免费号的「积分过期」
                          只有这一条来源（额度接口对它给的是「无期限」哨兵），所以放在这里
                          而不是只留在资源包弹窗里。挂在额度下面、不新开一列：理由同上面
                          那个活动键 —— 日志已经六列，再加就是静默截断。 */}
                      {l.expires_at != null && (
                        <span className="log-key mono" title="这笔积分的到期时间">
                          至 {expiryInfo(l.expires_at).text}
                        </span>
                      )}
                    </td>
                    <td className="num log-balance col-secondary">
                      {l.balance != null ? formatCredits(l.balance) : "—"}
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
