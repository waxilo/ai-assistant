import { memo, useMemo, useState } from "react";
import type { Account, AcctStatus, BrokerStatus, CreditPackage } from "../types";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  copyText,
  daysUntil,
  formatCredits,
  maskToken,
  stamp,
} from "../common";
import { Dialog } from "../components/Dialog";
import {
  IconUsers,
  IconTrash,
  IconCloud,
  IconUpload,
  IconLink,
  IconUnlink,
  IconList,
} from "../components/Icons";

/**
 * 账号与签到页。
 *
 * **数据全部由外层（`App`）持有**：这里只做展示与触发，不做「挂载即拉取」。
 * 原因见 `App.tsx` 顶部说明 —— 页面每次切 tab 都会重新挂载，若把列表状态放在页面内部，
 * 每切一次回来就要重打一遍 `checkin_status`（每个账号两次网络请求），切 tab 会明显发顿。
 *
 * ⚠️ **续签没有任何按钮，也永远不会有**：token 续签由后端后台线程全自动完成
 * （启动即巡、之后每 30 分钟一轮），每次签到之前还会顺手续一次。所以这一页只负责
 * **把续签的结果显示出来** —— 那一列到期时间被推远了，就是它干的活。
 */
interface Props {
  accounts: Account[];
  statuses: Record<string, AcctStatus>;
  statusText: string;
  /** 正在签到的账号 id */
  busyIds: Set<string>;
  onCheckinOne: (id: string) => void;
  onRemove: (a: Account) => void;
  /** 凭证池状态；null = 还没读到（首屏那一瞬） */
  brokerStatus: BrokerStatus | null;
  /** 上传 / 绑定 / 解绑进行中：三个动作都会打网络，按钮要一起禁用 */
  brokerBusy: boolean;
  onBrokerUpload: () => void;
  onBrokerLink: () => void;
  onBrokerUnbind: () => void;
}

/** 毫秒时间戳 → 人话的「多久之前」。 */
function sinceMs(ms: number): string {
  const min = Math.floor((Date.now() - ms) / 60000);
  if (min < 1) return "刚刚";
  if (min < 60) return `${min} 分钟前`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr} 小时前`;
  return `${Math.floor(hr / 24)} 天前`;
}

/**
 * 可点的 uuid 胶囊：点一下把**完整**那串复制走，标签就地变「已复制」。
 *
 * 标签只显示前 8 位（完整串在 `title` 里），但复制的是完整值 ——
 * 搬去另一台机器靠的就是这串，截断显示只是为了让这一栏不至于被 36 个字符撑爆。
 * 复制失败时标签会说「复制失败」而不是静默什么都不做（那比不复制更糟）。
 */
function CopyChip({ uuid }: { uuid: string }) {
  const [state, setState] = useState<"idle" | "ok" | "err">("idle");
  return (
    <button
      type="button"
      className={`pool-chip${state === "ok" ? " ok" : ""}`}
      title={`点一下复制完整 uuid：${uuid}`}
      onClick={async () => setState((await copyText(uuid)) ? "ok" : "err")}
    >
      {state === "ok" ? "已复制" : state === "err" ? "复制失败" : `${uuid.slice(0, 8)}…`}
    </button>
  );
}

/** 「上次同步」那句人话。同步是热路径上两分钟一跳的东西，说清时刻比说「同步中」有用 */
function syncNote(status: BrokerStatus | null): string {
  if (!status) return "";
  if (status.syncing) return "；正在同步…";
  if (!status.last_ok_ms) return "；还没同步过";
  return `；上次同步 ${sinceMs(status.last_ok_ms)}`;
}

/**
 * 账号页的「云端凭证池」一栏：本机这批账号有没有托管到云端、要不要接上别的机器。
 *
 * 为什么它是**一整栏**而不是表格里的一列：一池一个 uuid、池内一把闸，绑定只有
 * 「全都绑了」和「全都没绑」两种状态。画到每一行上，会让人以为能逐个账号决定 ——
 * 而按账号分粒度恰恰是错的（两台机器可以各自拿着不同账号的闸、同时提交同一批账号，
 * 于是出现「一半新一半旧」这种谁也没签错的错状态）。
 */
function PoolBar({
  status,
  accountCount,
  busy,
  onUpload,
  onLink,
  onUnbind,
}: {
  status: BrokerStatus | null;
  accountCount: number;
  busy: boolean;
  onUpload: () => void;
  onLink: () => void;
  onUnbind: () => void;
}) {
  const uuid = status?.uuid ?? null;
  const bound = Boolean(status?.bound && uuid);

  const note = (() => {
    // 错误优先：出过错的绑定状态比「已绑定」更该被看见
    if (status?.error) return status.error;
    if (bound) {
      return `本机这 ${accountCount} 个账号与云端共用，续签仍在本机执行${syncNote(status)}`;
    }
    if (accountCount === 0) {
      return "本机还没有账号。别的机器上已有的话，直接绑定它那串 uuid 即可把账号接过来。";
    }
    return `把本机这 ${accountCount} 个账号放到云端，其他机器绑定同一串 uuid 就能共用（续签始终在本机执行）`;
  })();

  return (
    <div className="pool-bar card">
      <span className="pool-bar-title">
        <IconCloud size={15} />
        云端凭证池
      </span>
      <span className={`pool-bar-note${status?.error ? " err" : ""}`} title={note}>
        {note}
      </span>
      <span className="pool-bar-actions">
        {bound && uuid ? (
          <>
            <CopyChip uuid={uuid} />
            <button
              className="btn small danger"
              disabled={busy}
              onClick={onUnbind}
              title="摘掉本机绑定，云端那一池保留（其他机器不受影响）；本机与云端一致的账号会从本机删除"
            >
              <IconUnlink size={13} />
              解绑
            </button>
          </>
        ) : (
          <>
            <button
              className="btn small"
              disabled={busy || accountCount === 0}
              onClick={onUpload}
              title={
                accountCount === 0
                  ? "本机还没有账号可上传"
                  : "在云端新建一池并把本机账号放进去，之后会给你一串 uuid"
              }
            >
              <IconUpload size={13} />
              {accountCount === 0 ? "上传本机账号" : `上传本机 ${accountCount} 个账号`}
            </button>
            <button className="btn small ghost" disabled={busy} onClick={onLink}>
              <IconLink size={13} />
              绑定云端凭证池
            </button>
          </>
        )}
      </span>
    </div>
  );
}

/** 读 JWT 载荷里的 `exp`（秒）。不是 JWT / 解不出来 / 载荷里没有 → `null`。 */
function jwtExp(token: string): number | null {
  const seg = token.split(".")[1];
  if (!seg) return null;
  try {
    // base64url → base64（换字符表 + 补 padding），再按 UTF-8 解出 JSON
    const b64 = seg.replace(/-/g, "+").replace(/_/g, "/");
    const pad = "=".repeat((4 - (b64.length % 4)) % 4);
    const json = JSON.parse(atob(b64 + pad)) as { exp?: unknown };
    return typeof json.exp === "number" ? json.exp : null;
  } catch {
    return null;
  }
}

/**
 * token 到期时间（毫秒）。**必须与后端 `renew::expiry_ms` 完全同源**：
 * 先读 JWT 载荷里的 `exp`（秒），再退回账号上的 `expires_at`
 * （浏览器登录给毫秒、本机登录态给秒，统一按「小于 1e12 视为秒」归一化）。
 *
 * 两边不同源就会出现「界面说还有 20 小时、后台认为已经进窗口」这种错位；
 * 而这一列现在是「自动续签在不在干活」的**唯一反馈**，所以不容许存在两套算法。
 */
function tokenExpiry(a: Account): number | null {
  const raw = jwtExp(a.token) ?? a.expires_at;
  if (typeof raw !== "number" || raw <= 0) return null;
  return raw < 1_000_000_000_000 ? raw * 1000 : raw;
}

/** 自动续签在「到期前 24 小时」内动手，所以剩余不足 24 小时就算没续上，要提出来。 */
const RENEW_WINDOW_MS = 24 * 3600 * 1000;

type Credits = {
  value: number | null;
  unlimited: boolean;
  /** 实时值没取到，显示的是账号上存的上次已知值 */
  stale: boolean;
  at: string;
  /** 最早到期时间（毫秒）；未知为 null */
  expiry: number | null;
  /** 逐额度包明细（名称 / 剩余 / 到期），供「资源包列表」弹窗 */
  packages: CreditPackage[] | null;
};

/**
 * 账号**已有积分**（当前剩余可用额度）：实时状态优先，取不到时回落到账号上存的快照。
 *
 * 两个值同源（后端的 `ide_user_ent_usage` 额度用量接口）：后端每拉一轮状态、每走一次接管
 * 都会写回 `account.credit_snapshot`，所以限流 9074 或掉线时这里仍能算上这个账号，
 * 而不是把它当成 0 悄悄少算。
 *
 * ⚠️ 与「今日签到」无关 —— 签到接口返回的 `credits` 是**签到奖励**，不是已有积分。
 */
function creditsOf(a: Account, statuses: Record<string, AcctStatus>): Credits {
  const live = statuses[a.id];
  if (live && (typeof live.credits === "number" || live.unlimited)) {
    return {
      value: live.credits ?? null,
      unlimited: !!live.unlimited,
      stale: false,
      at: "",
      expiry: live.earliest_expiry_ms ?? a.credit_snapshot?.earliest_expiry_ms ?? null,
      packages: live.packages ?? a.credit_snapshot?.packages ?? null,
    };
  }
  const snap = a.credit_snapshot;
  if (snap && (typeof snap.credits === "number" || snap.unlimited)) {
    return {
      value: snap.credits ?? null,
      unlimited: !!snap.unlimited,
      stale: true,
      at: snap.fetched_at || "",
      expiry: snap.earliest_expiry_ms ?? null,
      packages: snap.packages ?? null,
    };
  }
  return {
    value: null,
    unlimited: false,
    stale: false,
    at: "",
    expiry: null,
    packages: null,
  };
}

/**
 * 最早到期的那个资源包还剩多少积分 —— 「N 天后过期」旁边的数量（需求：快到期提示展示
 * 「2 天后过期 322 积分」）。取 `packages` 里 `expiry_ms` 最小的项的 `remaining`；未知或缺数据 → null。
 */
function earliestPkgRemaining(packages: CreditPackage[] | null): number | null {
  if (!packages || packages.length === 0) return null;
  let best: CreditPackage | null = null;
  for (const p of packages) {
    if (best === null || p.expiry_ms < best.expiry_ms) best = p;
  }
  return best ? best.remaining : null;
}

function AccountsPage({
  accounts,
  statuses,
  statusText,
  busyIds,
  onCheckinOne,
  onRemove,
  brokerStatus,
  brokerBusy,
  onBrokerUpload,
  onBrokerLink,
  onBrokerUnbind,
}: Props) {
  // 资源包列表弹窗：记录当前打开的是哪个账号；null = 未打开
  const [pkgAccount, setPkgAccount] = useState<Account | null>(null);
  const stats = useMemo(() => {
    const total = accounts.length;
    const checked = accounts.filter((a) => statuses[a.id]?.checked_in).length;
    // 按**展示用的两位小数**累加，保证合计恰好等于列表里各行「积分」之和。
    let credits = 0;
    let known = 0;
    let unlimited = 0;
    for (const a of accounts) {
      const c = creditsOf(a, statuses);
      if (c.unlimited) unlimited += 1;
      else if (c.value !== null) {
        credits += Math.round(c.value * 100) / 100;
        known += 1;
      }
    }
    const unknown = total - known - unlimited;
    const hint = [
      `${total} 个账号：${known} 个已取到积分`,
      unlimited ? `${unlimited} 个不限量（未计入合计）` : "",
      unknown > 0 ? `${unknown} 个未取到` : "",
    ]
      .filter(Boolean)
      .join("，");
    return { total, checked, credits: Math.round(credits * 100) / 100, unlimited, hint };
  }, [accounts, statuses]);

  // 凭证池那一栏在**两种空态下都要在**：一台全新的机器正是「先绑定、再拿账号」的
  // 典型场景 —— 把它藏在「先有账号」之后，等于逼用户在一台空机器上没法接上已有的池。
  const poolBar = (
    <PoolBar
      status={brokerStatus}
      accountCount={accounts.length}
      busy={brokerBusy}
      onUpload={onBrokerUpload}
      onLink={onBrokerLink}
      onUnbind={onBrokerUnbind}
    />
  );

  return (
    <>
      {poolBar}
      {/* ── 概览：4 张汇总卡。总积分口径与表格里的「积分」列**共用同一个 creditsOf**，
             所以两处永远对得上（历史上各算一份，结果是合计与逐行之和对不上）。 ── */}
      <div className="summary">
        <div className="card sum-card">
          <div className="sum-label">账号总数</div>
          <div className="sum-num">{stats.total}</div>
        </div>
        <div className="card sum-card">
          <div className="sum-label">今日已签到</div>
          <div className="sum-num ok">{stats.checked}</div>
        </div>
        <div className="card sum-card">
          <div className="sum-label">待签到</div>
          <div className="sum-num warn">{stats.total - stats.checked}</div>
        </div>
        <div className="card sum-card" title={stats.hint}>
          <div className="sum-label">总积分</div>
          <div className="sum-num">
            {formatCredits(stats.credits)}
            {stats.unlimited > 0 && <span className="sum-plus">+</span>}
          </div>
        </div>
      </div>

      {accounts.length === 0 ? (
        <div className="card">
          <EmptyState
            icon={<IconUsers size={26} />}
            title="还没有任何账号"
            hint="点右上角「添加账号」扫描本机已登录的 TraeWork 桌面端，或走浏览器授权登录。如果别的机器上已经有一池，也可以直接在上面的「云端凭证池」里绑定它那串 uuid。"
          />
        </div>
      ) : (
        <div className="table-wrap card">
          <table className="data-table">
            <thead>
              <tr>
                <th>账号</th>
                <th>区域</th>
                <th className="num" title="该账号在 TraeWork 里的剩余可用积分（额度用量，非签到奖励）">
                  积分
                </th>
                <th>今日</th>
                <th title="token 全自动续签：到期前 24 小时后台自己动手，界面上没有手动按钮">
                  Token
                </th>
                <th className="col-actions">操作</th>
              </tr>
            </thead>
            <tbody>
              {accounts.map((a) => {
                const st = statuses[a.id];
                const cr = creditsOf(a, statuses);
                const days = !cr.unlimited && cr.expiry ? daysUntil(cr.expiry) : null;
                const texp = tokenExpiry(a);
                // 剩余不足 24 小时 = 自动续签没能续上；**已过期**只能重新登录（红），
                // 还没过期但已进窗口是黄 —— 两者要能一眼分开
                const texpExpired = texp !== null && texp <= Date.now();
                const texpSoon = texp !== null && texp - Date.now() < RENEW_WINDOW_MS;
                const credCls = days === null ? "" : days < 0 ? " bad" : days <= 3 ? " warn" : "";
                const texpCls = texpExpired ? " bad" : texpSoon ? " warn" : "";
                const cellTitle = cr.unlimited
                  ? "不限量"
                  : cr.stale
                  ? `本次未取到，显示上次已知值${cr.at ? `（${cr.at}）` : ""}`
                  : undefined;
                const busyOne = busyIds.has(a.id);
                return (
                  <tr key={a.id}>
                    <td>
                      <AccountCell name={a.name} phone={a.phone} />
                    </td>
                    <td className="muted">{a.region || "—"}</td>
                    <td className="num" title={cellTitle}>
                      <span className="ac-credits">
                        {cr.unlimited ? "不限" : formatCredits(cr.value)}
                      </span>
                      {/* 积分与过期合在一列：过期时间就是「智能接管先扣谁」的第一排序键，所以直接显示。
                          有 `packages` 时整行可点击，点开逐资源包列表 */}
                    {!cr.unlimited && cr.expiry ? (
                      <span
                        className={`sub ${credCls}`}
                        title="智能接管优先使用到期最早的积分"
                        onClick={() => setPkgAccount(a)}
                      >
                        {days !== null && days < 0
                          ? "已过期"
                          : `${days} 天后过期 ${
                              earliestPkgRemaining(cr.packages) ?? ""
                            }`.trim()}
                      </span>
                    ) : (
                      <span
                        className="sub"
                        title="没有到期的额度信息，可点击查看资源包列表"
                        onClick={() => setPkgAccount(a)}
                      >
                        查看资源包
                      </span>
                    )}
                    </td>
                    <td>
                      {st ? (
                        <StatusDot
                          tone={st.checked_in ? "done" : "pending"}
                          label={st.checked_in ? "已签" : "待签"}
                        />
                      ) : (
                        <span className="muted">—</span>
                      )}
                    </td>
                    <td>
                      <span className="ac-token">{maskToken(a.token)}</span>
                      {/* 自动续签的可见证据：到期时间被推远了就是续上了。没有按钮之后，
                          这一列就是唯一的反馈 —— 所以「未知」必须显式说出来，否则会有一个
                          「永远不续、界面上又看不出来」的沉默账号。 */}
                      {texp !== null ? (
                        <span
                          className={`sub${texpCls}`}
                          title={
                            texpExpired
                              ? "token 已过期且自动续签没成功，需要重新登录这个账号"
                              : texpSoon
                              ? "已进入续签窗口，后台会在 30 分钟内续一轮"
                              : "自动续签会持续推远这个时间"
                          }
                        >
                          {texpExpired ? "已过期" : `${stamp(texp)} 到期`}
                        </span>
                      ) : (
                        <span
                          className="sub warn"
                          title="这个 token 不是 JWT、账号上也没有到期时间 —— 后台无从判断何时该续签，只能跳过它（总不能每 30 分钟盲换一次票）。重新登录一次即可恢复自动续签。"
                        >
                          到期未知
                        </span>
                      )}
                    </td>
                    <td className="col-actions">
                      <div className="ac-cell-actions">
                        <button
                          className="btn small"
                          disabled={busyOne || busyIds.size > 0}
                          onClick={() => onCheckinOne(a.id)}
                        >
                          {busyOne ? "签到中" : "签到"}
                        </button>
                        <button
                          className="btn small danger"
                          title="从本地账号池里移除"
                          onClick={() => onRemove(a)}
                        >
                          <IconTrash size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* 刷新状态的结果（「已刷新 N 个账号状态」/ 查询失败原因） */}
      {statusText && <p className="hint">{statusText}</p>}

      {/* 逐资源包列表：从「积分/过期」那一列点开，看每个额度包的名称/剩余/到期 */}
      {pkgAccount && (
        <Dialog
          size="lg"
          icon={<IconList size={16} />}
          title={`${pkgAccount.name} 的资源包`}
          onClose={() => setPkgAccount(null)}
          footer={
            <button className="btn" onClick={() => setPkgAccount(null)}>
              关闭
            </button>
          }
        >
          <PkgListAccount account={pkgAccount} statuses={statuses} />
        </Dialog>
      )}
    </>
  );
}

/**
 * 资源包列表弹窗内容：一个账号的逐额度包（名称 / 剩余积分 / 到期）。
 * `packages` 从实时状态（或快照兜底）取，缺失时给一条空态提示。
 */
function PkgListAccount({
  account,
  statuses,
}: {
  account: Account;
  statuses: Record<string, AcctStatus>;
}) {
  const cr = creditsOf(account, statuses);
  const packs = cr.packages ?? [];
  return packs.length === 0 ? (
    <p className="note" style={{ margin: 0 }}>
      这个账号暂时没有带到期时间的额度包数据（可能从未拉到，或余额已被用尽）。
    </p>
  ) : (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <tr>
            <th>资源包</th>
            <th className="num">剩余</th>
            <th className="num">到期</th>
          </tr>
        </thead>
        <tbody>
          {[...packs]
            .sort((a, b) => a.expiry_ms - b.expiry_ms)
            .map((p, i) => (
              <tr key={i}>
                <td>{p.name || "未命名额度包"}</td>
                <td className="num">{formatCredits(p.remaining)}</td>
                <td className="num num-muted">{mmddyyyy(p.expiry_ms)}</td>
              </tr>
            ))}
        </tbody>
      </table>
    </div>
  );
}

/** 毫秒 → `YYYY-MM-DD`，资源包到期的完整日期展示 */
function mmddyyyy(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

export default memo(AccountsPage);
