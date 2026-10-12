import { useCallback, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type { Account, DayEntry, HourEntry, Settings } from "../types";
import {
  BRIEFING_SEALED_EVENT,
  clearCreditBriefing,
  creditBriefing,
  enableCreditBriefing,
  saveSettings,
} from "../api";
import { AccountCell, EmptyState, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";
import { Dialog } from "../components/Dialog";
import Switch from "../components/Switch";
import { IconActivity, IconClock, IconInfo, IconTrash } from "../components/Icons";

/** 小时写成 `09:00`：补零之后整列数字才对得齐 */
const hh = (h: number) => `${String(h).padStart(2, "0")}:00`;

/**
 * 「积分简报」页：**列表里只有日条目**，每一条展开后是当天的小时条目，
 * 点小时条目弹窗看**每个账号的扣费**。
 *
 * 三层之间的关系是「同一份数据的三种切法」，不是三份数据：
 *
 * - 时条目 = 后台每小时结算一次的落盘记录（唯一的事实来源）；
 * - 日条目 = 当天时条目之和，后端现算、不落盘 ⇒ 不可能出现「日与它下面的时对不上」；
 * - 弹窗里的账号明细 = 时条目自带的逐账号扣费。
 *
 * 这一页**没有任何「手动生成/触发」的入口**：条目只由后台的每小时结算产生，
 * 所以页面上的按钮只有「开关」和「清空」两个（都是对整份历史的操作，不是生成条目）。
 *
 * 简报的**开关**放在这一页（而不是设置页）：设置页管的是「全局开关与推送」，
 * 而「记不记账」是这一页的主场 —— 开关就在列表上方，关了立刻能看出列表会停更。
 */
export default function BriefingPage({
  accounts,
  settings,
  askConfirm,
  onSettings,
  onToast,
}: {
  /** 账号列表：算「当前剩余」的合计（积分值来自各账号的 `credit_snapshot`） */
  accounts: Account[];
  settings: Settings;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 开关落库后把最新 settings 同步回外层（后端可能代为改写字段） */
  onSettings: (s: Settings) => void;
  onToast: (t: Toast) => void;
}) {
  const [days, setDays] = useState<DayEntry[]>([]);
  const [loading, setLoading] = useState(true);
  /** 开启/关闭简报的落库状态 */
  const [toggleBusy, setToggleBusy] = useState(false);
  const [on, setOn] = useState(settings.briefing_enabled);
  /** 展开的日期；一次只展开一天，避免长列表被撑得找不到北 */
  const [openDate, setOpenDate] = useState<string | null>(null);
  /** 弹出明细的小时条目（非 null 时弹窗） */
  const [hour, setHour] = useState<HourEntry | null>(null);

  // 外部改了 settings 时要同步本地开关，否则页面上显示的还是旧状态，
  // 一点就会把对面的改动顶回去。
  useEffect(() => {
    setOn(settings.briefing_enabled);
  }, [settings.briefing_enabled]);

  const refresh = useCallback(
    () =>
      creditBriefing()
        .then(setDays)
        .catch(() => setDays([]))
        .finally(() => setLoading(false)),
    []
  );

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 后台每小时固化出新的时条目时刷新列表。
  // 不弹提示：每小时弹一次就是噪音，而这一页本来就是「回来看历史」的地方。
  useEffect(() => {
    const un = listen(BRIEFING_SEALED_EVENT, () => void refresh());
    return () => {
      void un.then((f) => f());
    };
  }, [refresh]);

  /**
   * 当前剩余：各账号 `credit_snapshot` 的合计（与账号页读的是**同一份**数据）。
   *
   * 后台每小时采样会把它回写（`commands::fetch_samples` 顺手落盘），
   * 所以这个数会跟着采样走；未知不计入（0 与「未知」含义不同），
   * 一个都没读到 → `null`，界面显示「—」。
   */
  const nowTotal = useMemo(() => {
    let sum = 0;
    let known = false;
    for (const a of accounts) {
      const v = a.credit_snapshot?.credits ?? null;
      if (v == null) continue;
      sum += Math.round(v * 100) / 100;
      known = true;
    }
    return known ? Math.round(sum * 100) / 100 : null;
  }, [accounts]);

  /** 最近一次读数时刻（全部账号里最新的那条；一条都没有 → 空串），
   *  用来告诉用户这个数是「什么时候的」 */
  const readAt = useMemo(() => {
    let at = "";
    for (const a of accounts) {
      const t = a.credit_snapshot?.fetched_at ?? "";
      // `YYYY-MM-DD HH:MM:SS` 的字典序就是时间序，直接比字符串即可
      if (t > at) at = t;
    }
    return at;
  }, [accounts]);

  const latest = days[0] ?? null;
  // 「累计」= 已保留的全部日条目之和。日条目本身是当天时条目之和，
  // 而且都是完整时段 ⇒ 直接相加就是总量。
  const lifetime = days.reduce(
    (acc, d) => ({
      consumed: acc.consumed + d.consumed,
      gained: acc.gained + d.gained,
    }),
    { consumed: 0, gained: 0 }
  );

  /**
   * 开启 / 关闭积分简报。
   *
   * **开启要经过确认**：它会清掉已有的时条目**以及台账里的小时明细**，并只对齐
   * 一次基线（不记这一段增量）。只清条目的活下一次结算会把它们原样重建出来 ——
   * 所以「清空」必须连明细一起清，用户点开列表发现历史没了，得先知道这是预期的。
   * 关闭则只是改个开关，不动任何数据（已有条目照常留在列表里）。
   */
  const doToggle = async (next: boolean) => {
    if (next) {
      const ok = await askConfirm({
        title: "开启积分简报",
        body:
          "开启后会清空已有的简报历史（时条目与台账里的小时明细），从此刻重新开始累积。\n\n" +
          "清掉的只是「明细」：账号的累计积分不受影响，之后每小时会结算一条时条目，" +
          "日条目由当天时条目相加得出。",
        okText: "开启并重新开始",
      });
      if (!ok) return;
    }
    setToggleBusy(true);
    try {
      if (next) {
        await enableCreditBriefing();
        setDays([]);
        setOpenDate(null);
      }
      const saved = await saveSettings({ ...settings, briefing_enabled: next });
      onSettings(saved);
      setOn(next);
      void refresh();
      onToast({
        kind: "ok",
        text: next ? "简报已开启，从现在起按小时结算" : "简报已关闭，不再生成新条目",
      });
    } catch (e) {
      setOn(!next);
      onToast({ kind: "err", text: "操作失败：" + String(e) });
    } finally {
      setToggleBusy(false);
    }
  };

  const doClear = async () => {
    const ok = await askConfirm({
      title: "清空积分简报",
      body:
        "确认清空全部简报历史？时条目会与台账里的小时明细一起清掉 —— " +
        "只清条目的活，下一次结算会把它们原样重建出来。账号的累计积分不受影响。",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearCreditBriefing();
      setDays([]);
      setOpenDate(null);
      onToast({ kind: "ok", text: "简报已清空" });
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
    }
  };

  return (
    <section className="panel-page">
      <p className="set-intro">
        <IconInfo size={14} />
        <span>
          后台<b>每小时</b>结算一条时条目：整点前采一次样，整点一过就把这一个小时
          固化成一条记录。<b>日条目是当天时条目之和</b>，所以「日 = 时」永远对得上。
          消耗与新增都按资源包的<b>累计量</b>取差值，多个客户端同时消耗也都算得进来。
          应用没运行的时段不会采样，那几格是空的；恢复运行后的第一次采样会把这段
          攒下的量整块记进恢复后的那个小时。
        </span>
      </p>

      <div className="card logs-toolbar">
        <label className="bp-toggle" title="开启后每小时结算一条时条目；关闭则停止生成">
          <Switch
            checked={on}
            disabled={toggleBusy}
            onChange={(v) => void doToggle(v)}
          />
          <span className="bp-toggle-text">{on ? "已开启" : "已关闭"}</span>
        </label>
        <span className="count">
          {loading ? "加载中…" : `共 ${days.length} 天`}
        </span>
        <span className="spacer" />
        {/* 与账号页同源的「当前剩余」：后台每采一次就更新，不是某条时条目里冻结的快照。
            下面列表里的「当日末剩余」才是快照 —— 两个数各归其位，不会被拿来互相对照。 */}
        <span
          className="count"
          title={
            readAt
              ? `最近一次采集 ${readAt}；与「账号签到」页读的是同一份数据`
              : "还没有采集到余额"
          }
        >
          当前剩余 <b className="bp-now">{formatCredits(nowTotal)}</b>
          {readAt ? ` · ${readAt.slice(11, 16)}` : ""}
        </span>
        <button
          className="btn small danger"
          disabled={days.length === 0}
          onClick={() => void doClear()}
        >
          <IconTrash size={15} />
          清空简报
        </button>
      </div>

      {!on && (
        <p className="set-intro">
          <IconInfo size={14} />
          <span>
            简报已关闭，不会再结算新的条目；已有的记录仍保留，你随时可以重新开启。
            重新开启时会清掉旧历史并<b>从此刻重新开始</b>，避免把关闭这段时间的量
            算进开启后的第一个小时。
          </span>
        </p>
      )}

      {loading ? (
        <p className="empty">加载中…</p>
      ) : days.length === 0 ? (
        <EmptyState
          icon={<IconActivity size={26} />}
          title={on ? "还没有简报" : "简报未开启"}
          hint={
            on
              ? "已经开启，正在按小时累积。整点过后就会出现第一条时条目 —— 应用保持运行才会采样，跑满一小时即可看到。"
              : "用上方的开关开启：开启后从此刻重新开始累积，每小时结算一条时条目。"
          }
        />
      ) : (
        <>
          <div className="summary">
            <div className="sum-card card hoverable">
              <span className="sum-label">
                最近一天消耗{latest ? `（${latest.date}）` : ""}
              </span>
              <span className="sum-num bp-consumed">
                {formatCredits(latest?.consumed ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">
                最近一天新增{latest ? `（${latest.date}）` : ""}
              </span>
              <span className="sum-num ok">
                {formatCredits(latest?.gained ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">累计消耗（{days.length} 天）</span>
              <span className="sum-num bp-consumed">
                {formatCredits(lifetime.consumed)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">累计新增（{days.length} 天）</span>
              <span className="sum-num ok">{formatCredits(lifetime.gained)}</span>
            </div>
          </div>

          <div className="brief-list">
            {days.map((d) => {
              const open = openDate === d.date;
              return (
                <article className="card brief" key={d.date}>
                  <button
                    className="brief-head"
                    aria-expanded={open}
                    onClick={() => setOpenDate(open ? null : d.date)}
                  >
                    <span className="brief-date">{d.date}</span>
                    <span className="brief-window">
                      {d.sealed ? "全天" : "进行中"} · {d.hours.length} 个小时条目
                    </span>
                    <span className="spacer" />
                    <span className="brief-metric">
                      <span className="bm-label">消耗</span>
                      <span className="bm-num bp-consumed">
                        {formatCredits(d.consumed)}
                      </span>
                    </span>
                    <span className="brief-metric">
                      <span className="bm-label">新增</span>
                      <span className="bm-num ok">{formatCredits(d.gained)}</span>
                    </span>
                    <span
                      className="brief-metric"
                      title="这一天的最后一条读数，是历史快照；要看此刻的余额见工具栏的「当前剩余」"
                    >
                      <span className="bm-label">当日末剩余</span>
                      <span className="bm-num">
                        {d.balance == null ? "—" : formatCredits(d.balance)}
                      </span>
                    </span>
                    <span
                      className={"brief-caret" + (open ? " open" : "")}
                      aria-hidden="true"
                    />
                  </button>

                  {open && <DayHours day={d} onPick={setHour} />}
                </article>
              );
            })}
          </div>
        </>
      )}

      {hour && <HourDialog entry={hour} onClose={() => setHour(null)} />}
    </section>
  );
}

/**
 * 一天的时条目列表（按小时升序）。
 *
 * 每行是一颗按钮 —— 点开才是「每个账号的扣费」。列表里只放合计，
 * 因为「这一小时总共花了多少」是扫描时唯一要看的；想看是谁花的再点进去，
 * 免得一屏塞满账号名之后反而看不清哪个小时最贵。
 */
function DayHours({
  day,
  onPick,
}: {
  day: DayEntry;
  onPick: (h: HourEntry) => void;
}) {
  // 迷你条按当天峰值归一：只为了看出「哪个小时最贵」的形状，不是精确读数
  const peak = day.hours.reduce((m, h) => Math.max(m, h.consumed, h.gained), 0);
  // 0 值必须真的是 0：给一个最小宽度会让人以为那个小时花了钱
  const pct = (v: number) =>
    peak > 0 && v > 0 ? Math.max(3, (v / peak) * 100) : 0;

  return (
    <div className="brief-hours">
      {day.hours.map((h) => {
        // 列表里含「没动静但采到了余额」的账号（为了让剩余覆盖全部账号），
        // 所以账号数要说清分子分母：有几个真在花 / 这一小时一共采到几个账号
        const moved = h.accounts.filter((a) => a.consumed > 0 || a.gained > 0).length;
        return (
          <button className="bh-row" key={h.hour} onClick={() => onPick(h)}>
            <span className="bh-hour">{hh(h.hour)}</span>
            <span className="bh-track" aria-hidden="true">
              <i
                className="bh-fill consumed"
                style={{ width: `${pct(h.consumed)}%` }}
              />
              <i
                className="bh-fill gained"
                style={{ width: `${pct(h.gained)}%` }}
              />
            </span>
            <span className="bh-num bp-consumed">
              {h.consumed > 0 ? formatCredits(h.consumed) : "—"}
            </span>
            <span className="bh-num bp-gained">
              {h.gained > 0 ? "+" + formatCredits(h.gained) : "—"}
            </span>
            <span className="bh-accts">
              {h.baseline ? "起点基准" : `${moved} / ${h.accounts.length} 个账号`}
            </span>
          </button>
        );
      })}
      <p className="brief-hint">
        只统计应用运行期间；没有采样的时段不会出现在这里。整行都可以点开，看这一小时
        <b>每个账号</b>的扣费与余额。带「起点基准」的那条是<b>开启简报那一刻</b>
        的余额读数（消耗与新增都从那一刻开始算），下一小时那条就能和它对着看。
      </p>
    </div>
  );
}

/**
 * 时条目明细弹窗：**这一小时每个账号的扣费**。
 *
 * 只列这一小时**有动静**的账号（后端就只固化这些）—— 一个全是横杠的账号行
 * 会把真正要看的那几行淹掉，而「这条时条目里出现了谁」本身就是有用的信息。
 * 唯一的例外是开启简报时那条**起点条目**（见 `entry.baseline`）：它没有动静，
 * 但它的用途就是记下那一刻的余额给后面几小时当对照。
 */
function HourDialog({
  entry,
  onClose,
}: {
  entry: HourEntry;
  onClose: () => void;
}) {
  // 覆盖到下一个整点：`23` 的后一小时写成 `00:00`，别显示成 `24:00`
  const next = hh((entry.hour + 1) % 24);
  // 列表里也含「没动静但采到了余额」的账号（剩余要覆盖全部账号），这里把两者分开数
  const moved = entry.accounts.filter((a) => a.consumed > 0 || a.gained > 0).length;
  return (
    <Dialog
      size="lg"
      icon={<IconClock size={16} />}
      // 起点条目不覆盖任何时间区间（统计从那一刻才开始），标题里别有「– 17:00」
      title={
        entry.baseline
          ? `${entry.date} ${hh(entry.hour)} 起点基准`
          : `${entry.date} ${hh(entry.hour)} – ${next}`
      }
      label={`${entry.date} ${hh(entry.hour)} 扣费明细`}
      onClose={onClose}
      footer={
        <button className="btn" onClick={onClose}>
          关闭
        </button>
      }
    >
      <p className="note">
        <IconInfo size={14} />
        {entry.baseline ? (
          <span>
            这条是<b>开启简报那一刻</b>的起点基准：消耗与新增都从这一刻开始统计，
            所以都是 0；下面列的是当时各账号读到的余额。下一小时那条可以和它对着看。
          </span>
        ) : (
          <span>
            消耗与新增都取自资源包的<b>累计量</b>差值（消耗量 / 授予量各自独立），
            所以这段时间里哪怕换了客户端、或者先花后得，也都不会被抹平。
            有变化的按扣费降序排在前面；这一小时采到余额的其它账号也列在下面
            （扣费与新增显示 —），这样「剩余」覆盖的是<b>全部账号</b>。
          </span>
        )}
      </p>

      <div className="stat-row">
        <div className="stat">
          <span className="stat-label">消耗</span>
          <span className="stat-num bp-consumed">
            {formatCredits(entry.consumed)}
          </span>
        </div>
        <div className="stat">
          <span className="stat-label">新增</span>
          <span className="stat-num ok">{formatCredits(entry.gained)}</span>
        </div>
        <div className="stat">
          <span className="stat-label">剩余</span>
          <span className="stat-num">
            {entry.balance == null ? "—" : formatCredits(entry.balance)}
          </span>
        </div>
        <div className="stat">
          <span className="stat-label">账号</span>
          <span className="stat-num">
            {moved} / {entry.accounts.length}
          </span>
        </div>
      </div>

      <p className="modal-meta">结算时刻 {entry.generated_at}</p>

      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              <th>账号</th>
              <th className="num">扣费</th>
              <th className="num">新增</th>
              <th className="num">剩余</th>
            </tr>
          </thead>
          <tbody>
            {entry.accounts.map((a) => (
              <tr key={a.account_id}>
                <td>
                  <AccountCell name={a.name} phone={a.phone} />
                </td>
                <td className="num">
                  {a.consumed > 0 ? (
                    <span className="bp-consumed">
                      {formatCredits(a.consumed)}
                    </span>
                  ) : (
                    <span className="muted">—</span>
                  )}
                </td>
                <td className="num">
                  {a.gained > 0 ? (
                    <span className="bp-gained">
                      +{formatCredits(a.gained)}
                    </span>
                  ) : (
                    <span className="muted">—</span>
                  )}
                </td>
                <td className="num num-muted">
                  {a.balance == null ? "—" : formatCredits(a.balance)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Dialog>
  );
}
