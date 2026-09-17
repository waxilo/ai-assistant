import { useState } from "react";
import type { Account, BrokerStatus } from "../types";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  copyText,
  formatCredits,
  signState,
  expiryInfo,
  type SignState,
} from "../common";
import { creditsOf, expiryOf, totalCredits, useCredits } from "../credits";
import {
  IconCloud,
  IconFile,
  IconLink,
  IconTrash,
  IconUnlink,
  IconUpload,
  IconUser,
  IconRefresh,
} from "../components/Icons";

/** 账号签到状态 → 状态圆点文案（圆点本身的视觉由 common 的 StatusDot 统一提供） */
const STATUS_LABEL: Record<SignState, string> = {
  signing: "签到中",
  done: "今日已签到",
  pending: "待签到",
  fail: "签到失败",
  inactive: "活动未开",
};

/**
 * 首页：账号列表 + 签到操作。
 *
 * 账号**没有**「添加 / 编辑」入口：条目只是给用户看的，凭证一律来自
 * 页头「登录新账号」（OAuth）或「导入本机账号」（本机登录文件），避免手工粘贴 token 出错。
 */
export function AccountsPage({
  accounts,
  loading,
  busyIds,
  onCheckinOne,
  onRemove,
  onOpenLogs,
  brokerStatus,
  brokerBusy,
  onBrokerUpload,
  onBrokerLink,
  onBrokerUnbind,
}: {
  accounts: Account[];
  loading: boolean;
  busyIds: Set<string>;
  onCheckinOne: (id: string) => void;
  onRemove: (a: Account) => void;
  onOpenLogs: (accountId: string) => void;
  /** 凭证池状态；null = 还没读到（首屏那一瞬） */
  brokerStatus: BrokerStatus | null;
  /** 上传 / 绑定 / 解绑进行中：三个动作都会打网络，按钮要一起禁用 */
  brokerBusy: boolean;
  onBrokerUpload: () => void;
  onBrokerLink: () => void;
  onBrokerUnbind: () => void;
}) {
  // 订阅那个全局积分对象：后台一次采集（整点采样 / 刷新 / 签到）就会换掉它的引用，
  // 本页随之重渲染并显示新读数。hook 必须在任何提前 return 之前调用，所以挂在最上面。
  const book = useCredits();

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

  if (loading)
    return (
      <section className="panel-page">
        <p className="empty">加载中…</p>
      </section>
    );
  if (accounts.length === 0)
    return (
      <section className="panel-page">
        {poolBar}
        <EmptyState
          icon={<IconUser size={26} />}
          title="还没有账号"
          hint="点页头「登录新账号」用系统浏览器扫码登录，或点「导入本机账号」直接读取 WorkBuddy 写在本机的登录信息（自动带上昵称与手机号）。如果别的机器上已经有一池，也可以直接从上面「绑定云端凭证池」。"
        />
      </section>
    );

  const total = accounts.length;
  const done = accounts.filter((a) => signState(a, false) === "done").length;
  const pending = total - done;
  const credits = totalCredits(book, accounts);

  return (
    <section className="panel-page">
      {poolBar}

      <div className="summary">
        <div className="sum-card card">
          <span className="sum-label">账号总数</span>
          <span className="sum-num">{total}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">今日已签到</span>
          <span className="sum-num ok">{done}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">待签到</span>
          <span className="sum-num">{pending}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">总积分</span>
          <span className="sum-num">{formatCredits(credits)}</span>
        </div>
      </div>

      <div className="table-wrap card">
        <table className="data-table">
          <thead>
            <tr>
              <th>账号</th>
              <th>剩余积分</th>
              <th>积分到期</th>
              {/* 次要列：窄窗口下收起（见 responsive.css）。
                  滚动条已全局隐藏，横向溢出就是静默截断，所以窄窗必须真的不溢出；
                  Token 过期在签到日志 / 弹窗里都有，是这一行里最该让位的一列。 */}
              <th className="col-secondary">Token 过期</th>
              <th>状态</th>
              <th className="col-actions">操作</th>
            </tr>
          </thead>
          <tbody>
            {accounts.map((a) => {
              const busy = busyIds.has(a.id);
              const st = signState(a, busy);
              const bal = creditsOf(book, a.id);
              const low = bal != null && bal < 100;
              const e = expiryInfo(expiryOf(book, a.id));
              const tok = expiryInfo(a.expires_at);
              return (
                <tr key={a.id}>
                  <td>
                    <AccountCell name={a.name} phone={a.phone} />
                  </td>
                  <td className="num">
                    {bal != null ? (
                      <span className={"ac-balance" + (low ? " low" : "")}>
                        {formatCredits(bal)}
                      </span>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="ac-cell-expiry">
                    {e.text === "—" ? (
                      <span className="muted">—</span>
                    ) : (
                      <span className={"ac-expiry" + (e.expired ? " expired" : "")}>
                        {e.text}
                      </span>
                    )}
                  </td>
                  <td className="ac-cell-expiry col-secondary">
                    {tok.text === "—" ? (
                      <span className="muted">—</span>
                    ) : (
                      <span className={"ac-expiry" + (tok.expired ? " expired" : "")}>
                        {tok.text}
                      </span>
                    )}
                  </td>
                  <td>
                    <StatusDot tone={st} label={STATUS_LABEL[st]} />
                  </td>
                  <td className="ac-cell-actions">
                    <button
                      className="btn small primary"
                      disabled={busy}
                      onClick={() => onCheckinOne(a.id)}
                    >
                      {busy ? (
                        <>
                          <IconRefresh size={14} className="spin" /> 签到中
                        </>
                      ) : (
                        "签到"
                      )}
                    </button>
                    <button
                      className="icon-btn"
                      title="查看签到日志"
                      onClick={() => onOpenLogs(a.id)}
                    >
                      <IconFile size={16} />
                    </button>
                    <button
                      className="icon-btn icon-danger"
                      title="删除账号"
                      onClick={() => onRemove(a)}
                    >
                      <IconTrash size={16} />
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

/** 毫秒时间戳 → 人话的「多久之前」。与 `common.relativeTime` 同一套档位，只是吃毫秒 */
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
      className={`chip mono${state === "ok" ? " chip-ok" : ""}`}
      title={`点一下复制完整 uuid：${uuid}`}
      onClick={async () => setState((await copyText(uuid)) ? "ok" : "err")}
    >
      {state === "ok" ? "已复制" : state === "err" ? "复制失败" : `${uuid.slice(0, 8)}…`}
    </button>
  );
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
              title="摘掉本机绑定（云端那一池保留，其他机器不受影响）；本机与云端重复的凭证会从本机移除"
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

/** 「上次同步」那句人话。同步是热路径上两分钟一跳的东西，说清时刻比说「同步中」有用 */
function syncNote(status: BrokerStatus | null): string {
  if (!status) return "";
  if (status.syncing) return "；正在同步…";
  if (!status.last_ok_ms) return "；还没同步过";
  return `；上次同步 ${sinceMs(status.last_ok_ms)}`;
}
