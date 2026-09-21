import { useState } from "react";
import type { Account, BrokerStatus, CreditGrant, CreditPackage } from "../types";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  copyText,
  formatCredits,
  packageExpiry,
  signState,
  expiryInfo,
  expiryCountdown,
  type SignState,
} from "../common";
import { packagesOf, creditsOf, expiryOf, totalCredits, soonestExpiry, useCredits } from "../credits";
import { Dialog } from "../components/Dialog";
import {
  IconCloud,
  IconFile,
  IconLink,
  IconList,
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
  region,
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
  /** 当前区域（左下角选择器）；null = 还没读到 settings（首屏那一瞬），此时展示全部 */
  region: string | null;
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
  // 当前被点开「资源包列表」的账号（null = 没有弹窗打开）
  const [pkgAccount, setPkgAccount] = useState<Account | null>(null);

  // 只展示当前区域的账号：签到 / 批量动作在后端就只作用于当前区域，
  // 列表若混着另一区域，卡片数字（已签 / 未签）会对不上实际能签的集合。
  // 全量 `accounts` 只留给凭证池那一栏 —— 那是**整台机器**的池子，不分区域。
  const shown = region ? accounts.filter((a) => a.region === region) : accounts;

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
      accountCount={shown.length}
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
          hint="点页头「登录新账号」用系统浏览器扫码登录，或点「导入本机账号」直接读取 Qoder 写在本机的登录信息（自动带上昵称与手机号）。如果别的机器上已经有一池，也可以直接从上面「绑定云端凭证池」。"
        />
      </section>
    );
  // 机器上有账号、但当前区域一个都没有：登录入口是全局的，账号落在哪个区域
  // 由它登录的部署决定 —— 该提示切区域，而不是让人在这页找不到账号以为丢了。
  if (shown.length === 0)
    return (
      <section className="panel-page">
        {poolBar}
        <EmptyState
          icon={<IconUser size={26} />}
          title="当前区域还没有账号"
          hint="本机的账号都属于另一个区域。用左下角的区域选择器切过去即可看到；新登录的账号会归到它登录的那个部署。"
        />
      </section>
    );

  const total = shown.length;
  const credits = totalCredits(book, shown);
  // 已签 / 未签用**行内那一套** `signState` 逐账号数，所以卡片数字与下面每行的状态点
  // 永远一致 —— 汇总和明细各算一套，正是这类数字最容易分叉的地方。
  // 正在签的（`signing`）算进「未签到」：它确实还没签完，签完那一行会自己翻过去。
  const signedCount = shown.filter((a) => signState(a, busyIds.has(a.id)) === "done").length;
  const unsignedCount = total - signedCount;

  return (
    <>
    <section className="panel-page">
      {poolBar}

      {/* 四张卡：两张「今天的事」（已签 / 未签）+ 一张规模 + 一张额度。
          已签 / 未签是**这个页面唯一的行动指引** —— 看到「未签到 2」就知道还要点两下。
          刻意不显示「今天能不能领」这类活动细节：那是签到按钮自己的事，
          在这里再描述一遍只会多一份可能过期的状态。 */}
      <div className="summary">
        <div className="sum-card card">
          <span className="sum-label">账号总数</span>
          <span className="sum-num">{total}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">已签到</span>
          <span className="sum-num">{signedCount}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">未签到</span>
          <span className="sum-num">{unsignedCount}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">剩余额度</span>
          <span className="sum-num">{formatCredits(credits)}</span>
        </div>
      </div>

      <div className="table-wrap card">
        <table className="data-table">
          <thead>
            <tr>
              <th>账号</th>
              <th className="num">剩余积分</th>
              <th className="num">积分过期</th>
              {/* 次要列：窄窗口下收起（见 responsive.css）。
                  滚动条已全局隐藏，横向溢出就是静默截断，所以窄窗必须真的不溢出；
                  Token 过期在签到日志 / 弹窗里都有，是这一行里最该让位的一列。 */}
              <th className="col-secondary">Token 过期</th>
              <th>状态</th>
              <th className="col-actions">操作</th>
            </tr>
          </thead>
          <tbody>
            {shown.map((a) => {
              const busy = busyIds.has(a.id);
              const st = signState(a, busy);
              const bal = creditsOf(book, a.id);
              const low = bal != null && bal < 100;
              const e = expiryCountdown(expiryOf(book, a.id));
              // 快过期汇总：最早到期（有余量）的资源包还有几天、挂着多少积分
              const soon = soonestExpiry(book, a.id);
              const tok = expiryInfo(a.expires_at);
              return (
                <tr key={a.id}>
                  <td>
                    <AccountCell
                      name={a.name}
                      phone={a.phone}
                      region={a.region}
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
                  <td className="ac-cell-expiry num">
                    {soon ? (
                      <span
                        className={
                          "ac-expiry clk" +
                          (soon.kind === "dated" && soon.daysUntil < 0 ? " expired" : "")
                        }
                        title="点开看逐资源包列表；智能接管优先使用到期最早的积分"
                        onClick={() => setPkgAccount(a)}
                      >
                        {soon.kind === "never"
                          ? `不过期 ${formatCredits(soon.remaining)}`
                          : soon.daysUntil < 0
                          ? "已过期"
                          : `${soon.daysUntil} 天后过期 ${formatCredits(soon.remaining)}`}
                      </span>
                    ) : e.text === "—" ? (
                      <span
                        className="muted clk"
                        title="没有到期的额度信息，可点击查看资源包列表"
                        onClick={() => setPkgAccount(a)}
                      >
                        查看资源包
                      </span>
                    ) : (
                      <span
                        className={"ac-expiry clk" + (e.expired ? " expired" : "")}
                        title="点开看逐资源包列表；智能接管优先使用到期最早的积分"
                        onClick={() => setPkgAccount(a)}
                      >
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

      {/* 逐资源包列表：从「积分过期」那一列点开，看每个额度包的名称/剩余/到期 */}
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
          <PkgListAccount book={book} account={pkgAccount} />
        </Dialog>
      )}
    </>
  );
}

/**
 * 这里曾经有 `BenefitBar`：「今日权益」那一栏（今天是哪场活动、能领多少、领没领、
 * 还有多久刷新），数据来自后端的 `account_campaigns`。2026-09-18 按产品决定撤掉那一栏，
 * 组件、命令、以及它专用的 `dailyCampaign` / `campaignCountdown` 一并删除。
 *
 * 签到本身没受影响：动作仍是表格行里那个「签到」按钮，状态仍是 `signState`。
 */

/**
 * 资源包列表弹窗内容：一个账号的逐额度包（名称 / 剩余积分 / 到期）。
 * `packages` 从全局积分对象里取，缺失时给一条空态提示。
 *
 * **不在这里排序**：顺序由后端定好（有到期日 → 永不过期 → 未知，
 * 见 `ledger::project_packages`）。前端再排一次就是同一条规则的第二份实现，
 * 而且「未来日期」与「未来永远不会过期」谁在前，两边一定会给出不同答案。
 *
 * 「到期」列走 [`packageExpiry`]：它把「不过期」「未知」与真日期分开说 ——
 * 上一版这里是对 `expiry_ms` 直接 `new Date()`，于是把服务端表示不过期的
 * `9999-12-31` 哨兵原样印了出来。
 */
function PkgListAccount({ book, account }: { book: ReturnType<typeof useCredits>; account: Account }) {
  const packs = packagesOf(book, account.id) as CreditPackage[];
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
          {packs.map((p, i) => (
            <PkgRows key={i} p={p} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

/**
 * 一个资源包的行（+ 可选的一行**逐笔明细**）。
 *
 * 聚合包（「附加额度」把每天领的 100 分合成一格）在后端只有一个到期日，
 * 而它按笔到期：主行的日期是**最早一笔**（快作废的那批），
 * 下面那行列清楚每一笔各是哪天 —— 只报一个日期的话，
 * 用户没法判断「那天是只用掉一笔，还是全都没了」。
 * 只有一笔发放（或压根没有逐笔数据）时就只渲染主行。
 */
function PkgRows({ p }: { p: CreditPackage }) {
  return (
    <>
      <tr>
        <td>{p.name || "未命名额度包"}</td>
        <td className="num">{formatCredits(p.remaining)}</td>
        <td className="num num-muted">
          {packageExpiry(p).text}
          {/* 多笔时把口径说清楚：这个日期是**最早一笔**的，不是「全部用完」的那天 */}
          {p.grants.length > 1 ? "（最早一笔）" : ""}
        </td>
      </tr>
      {p.grants.length > 1 && (
        <tr>
          <td colSpan={3} style={{ paddingTop: 0 }}>
            <span className="num-muted" style={{ fontSize: "var(--fs-cap)" }}>
              {`共 ${p.grants.length} 笔：${p.grants.map(grantLabel).join("、")}`}
            </span>
          </td>
        </tr>
      )}
    </>
  );
}

/** 一笔发放的说明文字：「100（2026-10-18 到期）」；凭据没带回量时只报日期 */
function grantLabel(g: CreditGrant): string {
  const date = packageExpiry({ expiry_ms: g.expires_ms, never_expires: false }).text;
  return g.credits != null ? `${formatCredits(g.credits)}（${date} 到期）` : `${date} 到期`;
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
 * 账号页的「云端凭证池」一栏：**当前区域**的账号有没有托管到云端、要不要接上别的机器。
 *
 * 为什么它是**一整栏**而不是表格里的一列：一池一个 uuid、池内一把闸，绑定只有
 * 「全都绑了」和「全都没绑」两种状态。画到每一行上，会让人以为能逐个账号决定 ——
 * 而按账号分粒度恰恰是错的（两台机器可以各自拿着不同账号的闸、同时提交同一批账号，
 * 于是出现「一半新一半旧」这种谁也没签错的错状态）。
 * 绑定按区域各是一池：这里的数字与状态都只说当前区域的账号（`accountCount` 由外层
 * 传本区域行数），另一区域的池在切换区域后由同一栏展示。
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
      return `本区域这 ${accountCount} 个账号与云端共用，续签仍在本机执行${syncNote(status)}`;
    }
    if (accountCount === 0) {
      return "本区域还没有账号。别的机器上已有的话，直接绑定它那串 uuid 即可把账号接过来。";
    }
    return `把本区域这 ${accountCount} 个账号放到云端（每个区域各绑一池），其他机器绑定同一串 uuid 就能共用（续签始终在本机执行）`;
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
                  ? "本区域还没有账号可上传"
                  : "在云端新建一池并把本区域的账号放进去，之后会给你一串 uuid（每个区域各绑一池）"
              }
            >
              <IconUpload size={13} />
              {accountCount === 0 ? "上传本区域账号" : `上传本区域 ${accountCount} 个账号`}
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
