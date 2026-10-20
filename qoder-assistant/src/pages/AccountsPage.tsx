import { useState } from "react";
import type { Account, BrokerStatus } from "../types";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  accountIdent,
  copyText,
  formatCredits,
  signState,
  tokenCountdown,
  needsRelogin,
  type SignState,
} from "../common";
import { creditsOf, totalCredits, useCredits } from "../credits";
import {
  IconBolt,
  IconCircleCheck,
  IconClock,
  IconCloud,
  IconFile,
  IconLink,
  IconTrash,
  IconUnlink,
  IconUpload,
  IconUser,
  IconRefresh,
  IconSwap,
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
 * 页头「添加账号」弹窗（本机登录文件导入或 OAuth 登录），避免手工粘贴 token 出错。
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
  onBrokerSync,
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
  /** 手动双向同步云端那池：本机新票/新账号推上去，云端新票/新账号拉下来 */
  onBrokerSync: () => void;
}) {
  // 订阅那个全局积分对象：后台一次采集（整点采样 / 刷新 / 签到）就会换掉它的引用，
  // 本页随之重渲染并显示新读数。hook 必须在任何提前 return 之前调用，所以挂在最上面。
  const book = useCredits();

  // 顶部四张卡全部取自本页已有的事实，**不打任何额外接口**：
  // 总数来自账号列表、已签/未签来自每行的 `signState`、剩余额度来自那个全局积分对象。
  //
  // 这里曾经挂过两份额外数据：一份「额度面」（套餐 / 连续活跃天数 / 近一年消耗 / 热力图）、
  // 一份「活动面」（今天的活动权益状态）。2026-09-18 按产品决定改版后两者都没有消费方了，
  // 连同后端的 `account_overview` / `account_heatmap` / `account_campaigns` 三个命令一起删除。
  // 少两个首屏请求、少两块「可能失败因而可能空着」的区域，这页现在没有任何网络副作用。

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
      onSync={onBrokerSync}
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
          hint="点右上角「添加账号」导入或扫码登录。"
        />
      </section>
    );

  const total = accounts.length;
  const credits = totalCredits(book, accounts);
  // 已签 / 未签用**行内那一套** `signState` 逐账号数，所以卡片数字与下面每行的状态点
  // 永远一致 —— 汇总和明细各算一套，正是这类数字最容易分叉的地方。
  // 正在签的（`signing`）算进「未签到」：它确实还没签完，签完那一行会自己翻过去。
  const signedCount = accounts.filter((a) => signState(a, busyIds.has(a.id)) === "done").length;
  const unsignedCount = total - signedCount;

  return (
    <section className="panel-page">
      {poolBar}

      {/* 四张卡：两张「今天的事」（已签 / 未签）+ 一张规模 + 一张额度。
          已签 / 未签是**这个页面唯一的行动指引** —— 看到「未签到 2」就知道还要点两下。
          刻意不显示「今天能不能领」这类活动细节：那是签到按钮自己的事，
          在这里再描述一遍只会多一份可能过期的状态。 */}
      <div className="summary">
        <div className="sum-card card">
          <span className="sum-icon">
            <IconUser size={17} />
          </span>
          <div className="sum-label">账号总数</div>
          <div className="sum-num">{total}</div>
        </div>
        <div className="sum-card card tone-ok">
          <span className="sum-icon">
            <IconCircleCheck size={17} />
          </span>
          <div className="sum-label">今日已签到</div>
          <div className="sum-num ok">{signedCount}</div>
        </div>
        <div className="sum-card card tone-warn">
          <span className="sum-icon">
            <IconClock size={17} />
          </span>
          <div className="sum-label">未签到</div>
          <div className="sum-num warn">{unsignedCount}</div>
        </div>
        {/* 唯一一张着色卡：整页的视觉锚点，也是用户最关心的那个数 */}
        <div className="sum-card card feature">
          <span className="sum-icon">
            <IconBolt size={17} />
          </span>
          <div className="sum-label">剩余额度</div>
          <div className="sum-num">{formatCredits(credits)}</div>
        </div>
      </div>

      <div className="table-wrap card">
        <table className="data-table">
          <thead>
            <tr>
              <th>账号</th>
              <th className="num">剩余积分</th>
              {/* 次要列：窄窗口下收起（见 responsive.css）。
                  滚动条已全局隐藏，横向溢出就是静默截断，所以窄窗必须真的不溢出；
                  Token 过期在日志管理 / 弹窗里都有，是这一行里最该让位的一列。 */}
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
              const tok = tokenCountdown(a.expires_at);
              return (
                <tr key={a.id}>
                  <td>
                    <AccountCell
                      name={a.name}
                      ident={accountIdent(a.phone, a.email)}
                    />
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
                  <td className="ac-cell-expiry col-secondary">
                    {needsRelogin(a.rt_expires_at) ? (
                      // 链死了：显示时间只会让人以为「等它自动续上就好」，而它已经续不动了
                      <span
                        className="ac-expiry expired"
                        title="refresh token 已过有效期，自动续签换不出新凭证，只能重新登录这个账号"
                      >
                        需重新登录
                      </span>
                    ) : tok.text === "—" ? (
                      <span className="muted">—</span>
                    ) : (
                      <span
                        className={"ac-expiry" + (tok.expired ? " expired" : "")}
                        title={tok.expired ? `已于 ${tok.at} 过期` : `${tok.at} 过期`}
                      >
                        {tok.text}
                      </span>
                    )}
                  </td>
                  <td>
                    <StatusDot tone={st} label={STATUS_LABEL[st]} />
                  </td>
                  <td className="col-actions">
                    <div className="ac-cell-actions">
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
                        title="查看日志"
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
                    </div>
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

/**
 * 这里曾经有 `BenefitBar`：「今日权益」那一栏（今天是哪场活动、能领多少、领没领、
 * 还有多久刷新），数据来自后端的 `account_campaigns`。2026-09-18 按产品决定撤掉那一栏，
 * 组件、命令、以及它专用的 `dailyCampaign` / `campaignCountdown` 一并删除。
 *
 * 签到本身没受影响：动作仍是表格行里那个「签到」按钮，状态仍是 `signState`。
 */

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
 * 账号页的「云端凭证池」一栏：本机的账号有没有托管到云端、要不要接上别的机器。
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
  onSync,
}: {
  status: BrokerStatus | null;
  accountCount: number;
  busy: boolean;
  onUpload: () => void;
  onLink: () => void;
  onUnbind: () => void;
  onSync: () => void;
}) {
  const uuid = status?.uuid ?? null;
  const bound = Boolean(status?.bound && uuid);

  const note = (() => {
    // 错误优先：出过错的绑定状态比「已绑定」更该被看见
    if (status?.error) return status.error;
    if (bound) return `与云端共用 ${accountCount} 个账号${syncNote(status)}`;
    if (accountCount === 0) return "别的机器已有账号池？绑定后即可接过来。";
    return "上传后，其他机器可用同一串 uuid 共用。";
  })();

  return (
    <div className="info-bar card">
      <span className="info-bar-title">
        <IconCloud size={15} />
        云端凭证池
      </span>
      <span className={`info-bar-note${status?.error ? " err" : ""}`} title={note}>
        {note}
      </span>
      <span className="info-bar-actions">
        {bound && uuid ? (
          <>
            <CopyChip uuid={uuid} />
            <button
              className="btn small"
              disabled={busy}
              onClick={onSync}
              title="立即与云端双向同步"
            >
              <IconSwap size={13} />
              同步
            </button>
            <button
              className="btn small danger"
              disabled={busy}
              onClick={onUnbind}
              title="摘掉本机绑定；云端池与其他机器不受影响"
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
                accountCount === 0 ? "还没有账号可上传" : "在云端新建一池"
              }
            >
              <IconUpload size={13} />
              {accountCount === 0 ? "上传本机账号" : `上传 ${accountCount} 个账号`}
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
