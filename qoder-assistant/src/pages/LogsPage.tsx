import { useCallback, useEffect, useState } from "react";
import type { Account, CheckinLog } from "../types";
import { clearCheckinLogs, getCheckinLogs } from "../api";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  accountIdent,
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
  region,
  initialAccountId,
  askConfirm,
  onToast,
}: {
  accounts: Account[];
  /** 当前区域（左下角选择器）；null = 还没读到 settings（首屏那一瞬），此时展示全部 */
  region: string | null;
  initialAccountId?: string;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onToast: (t: Toast) => void;
}) {
  const [logs, setLogs] = useState<CheckinLog[]>([]);
  const [loading, setLoading] = useState(true);
  // 空串 = 全部账号；从账号条目进入时初始为该账号
  const [accountId, setAccountId] = useState(initialAccountId ?? "");

  // 日志按当前区域过滤：后端的 get_checkin_logs 只有「按账号」一个筛选维度，
  // 没有区域维度，所以这里在前端按区域账号的 id 集合过一遍。列表本身限最近
  // 300 条 —— 过滤是在这 300 条里挑本区域的，不会因此漏掉更早的本区域记录
  // 之外的任何东西（想看更早的本来就没有）。
  const regionIds = new Set(
    (region ? accounts.filter((a) => a.region === region) : accounts).map((a) => a.id)
  );
  const regionAccounts = accounts.filter((a) => regionIds.has(a.id));
  const shownLogs = logs.filter((l) => regionIds.has(l.account_id));

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

  /** 清空当前筛选范围内的日志（不可恢复）。「全部账号」也只清**本区域**的：
      清空是危险动作，范围必须与这页展示的口径一致 —— 界面上看不到另一区域的
      日志，一键却把它们删了，等于让人替别人按了删除键。后端只支持按单账号清，
      所以这里逐个账号调一遍。 */
  const doClear = async () => {
    const acc = accounts.find((a) => a.id === accountId);
    const ok = await askConfirm({
      title: "清空日志",
      body: acc
        ? `确认清空「${accountLabel(acc.name, accountIdent(acc.region, acc.phone, acc.email))}」的全部签到日志？`
        : "确认清空当前区域全部账号的签到日志？",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      if (accountId) {
        await clearCheckinLogs(accountId);
      } else {
        for (const a of regionAccounts) await clearCheckinLogs(a.id);
      }
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
            {regionAccounts.map((a) => (
              <option key={a.id} value={a.id}>
                {accountLabel(a.name, accountIdent(a.region, a.phone, a.email))}
              </option>
            ))}
          </select>
        </label>
        <span className="spacer" />
        <span className="count">{loading ? "加载中…" : `共 ${shownLogs.length} 条`}</span>
        <button
          className="btn small danger"
          disabled={shownLogs.length === 0}
          onClick={() => void doClear()}
        >
          <IconTrash size={15} />
          清空日志
        </button>
      </div>

      {loading ? (
        <p className="empty">加载中…</p>
      ) : shownLogs.length === 0 ? (
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
              {shownLogs.map((l) => {
                const st = logStatus(l);
                return (
                  <tr key={l.id}>
                    <td>
                      {/* 区域取自当前筛选（这页按区域过滤过，可见行的账号必属该区域）；
                          没读到 settings 时 region 为 null ⇒ 退回手机号优先的展示 */}
                      <AccountCell
                        name={l.account_name}
                        ident={accountIdent(region, l.account_phone, l.account_email)}
                      />
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
