import { useCallback, useEffect, useRef, useState } from "react";
import type {
  Account,
  BrokerStatus,
  CloudAccount,
  CloudSnapshot,
  CreditPackage,
} from "../types";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  accountKey,
  accountLabel,
  copyText,
  formatCredits,
  signState,
  expiryCountdown,
  tokenCountdown,
  needsRelogin,
  type SignState,
} from "../common";
import { packagesOf, creditsOf, expiryOf, totalCredits, soonestExpiry, soonestExpiringTotal, useCredits, seedCredits } from "../credits";
import {
  refreshAccountCredits,
  brokerCloudSnapshot,
  brokerRestoreAccount,
  brokerSyncNow,
} from "../api";
import { Dialog } from "../components/Dialog";
import {
  IconBolt,
  IconCircleCheck,
  IconCloud,
  IconFile,
  IconLink,
  IconList,
  IconTrash,
  IconUnlink,
  IconUpload,
  IconUser,
  IconUsers,
  IconRefresh,
  IconSwap,
  IconClock,
  IconPower,
  IconShield,
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
 * 账号的**凭证**没有编辑入口：一律来自页头「添加账号」（本机登录文件或 OAuth），
 * 避免手工粘贴 token 出错。
 *
 * 主表放三类账号：本机登录 / 云端副本 / 手动导入；主表里的删除只动本机这一份
 * （删云端副本不会动云端池）。云端池本身在池栏的「云端账号」弹窗里看：
 * 逻辑删除与恢复只有**续签设备**能做，其他设备只能查看。
 */
export function AccountsPage({
  accounts,
  loading,
  busyIds,
  onCheckinOne,
  onRemove,
  onRemoveCloud,
  onOpenLogs,
  brokerStatus,
  brokerBusy,
  onBrokerUpload,
  onBrokerLink,
  onBrokerUnbind,
  onBrokerSync,
  onBrokerClaim,
  onBrokerRelease,
  onRefreshBroker,
  reload,
}: {
  accounts: Account[];
  loading: boolean;
  busyIds: Set<string>;
  onCheckinOne: (id: string) => void;
  onRemove: (a: Account) => void;
  /** 云端池删除（按 key）：确认 + 调后端，返回更新后的云端读数；null = 取消/失败 */
  onRemoveCloud: (key: string, label: string) => Promise<CloudSnapshot | null>;
  onOpenLogs: (accountId: string) => void;
  /** 凭证池状态；null = 还没读到（首屏那一瞬） */
  brokerStatus: BrokerStatus | null;
  /** 上传 / 绑定 / 解绑进行中：三个动作都会打网络，按钮要一起禁用 */
  brokerBusy: boolean;
  onBrokerUpload: () => void;
  onBrokerLink: () => void;
  onBrokerUnbind: () => void;
  /** 手动双向同步：本机新票/新账号推上去，云端新票/新账号拉下来 */
  onBrokerSync: () => void;
  /** 把本机设为这一池的续签设备 */
  onBrokerClaim: () => void;
  /** 取消本机续签身份（10 秒内没有新设备接手会自动恢复） */
  onBrokerRelease: () => void;
  /** 只刷新凭证池状态（续签宽限倒计时用） */
  onRefreshBroker: () => void;
  /** 重新拉账号列表 + 凭证池状态（改资料 / 恢复之后） */
  reload: () => Promise<void>;
}) {
  // 订阅那个全局积分对象：后台一次采集（整点采样 / 刷新 / 签到）就会换掉它的引用，
  // 本页随之重渲染并显示新读数。hook 必须在任何提前 return 之前调用，所以挂在最上面。
  const book = useCredits();
  // 当前被点开「资源包列表」的账号（null = 没有弹窗打开）
  const [pkgAccount, setPkgAccount] = useState<Account | null>(null);
  // 「云端账号」弹窗是否打开（云端账号 / 已删除都在它里面）
  const [showCloud, setShowCloud] = useState(false);
  // 云端池的读数：**直接来自云端**（缓存 60 秒），不是主表「云端副本」的过滤结果
  const [cloudSnap, setCloudSnap] = useState<CloudSnapshot | null>(null);
  const [cloudErr, setCloudErr] = useState("");
  const [cloudBusy, setCloudBusy] = useState(false);

  const refreshCloud = useCallback(
    async (force: boolean) => {
      if (!brokerStatus?.bound) {
        setCloudSnap(null);
        setCloudErr("");
        return;
      }
      setCloudBusy(true);
      try {
        const snap = await brokerCloudSnapshot(force);
        setCloudSnap(snap);
        setCloudErr(
          snap.stale && snap.error
            ? `云端暂时没连上，显示的是 ${sinceMs(snap.fetched_at)}的缓存：${snap.error}`
            : ""
        );
      } catch (e) {
        // 读不到就保留上一次成功的列表，只把原因说出来
        setCloudErr(String(e));
      } finally {
        setCloudBusy(false);
      }
    },
    [brokerStatus?.bound]
  );

  // 绑定状态 / 同步版本变化时重读：版本变了说明刚同步过，强制绕过缓存
  const cloudVer = useRef<number | null | undefined>(undefined);
  useEffect(() => {
    const v = brokerStatus?.version ?? null;
    const changed = cloudVer.current !== undefined && cloudVer.current !== v;
    cloudVer.current = v;
    void refreshCloud(changed);
  }, [brokerStatus?.bound, brokerStatus?.version, refreshCloud]);

  const openCloud = () => {
    setShowCloud(true);
    void refreshCloud(true);
  };

  const deleteCloud = async (a: CloudAccount) => {
    const snap = await onRemoveCloud(a.key, accountLabel(a.name, a.phone));
    if (snap) {
      setCloudSnap(snap);
      setCloudErr("");
    }
  };

  const restoreCloud = async (key: string) => {
    const snap = await brokerRestoreAccount(key);
    if (snap) setCloudSnap(snap);
    await reload();
    // 提交复活 + 刷新：后台跑，成功后会带回新的池版本，上面的 effect 再强制读一次云端
    void brokerSyncNow()
      .then(() => reload())
      .catch(() => {});
  };

  // 凭证池那一栏在**两种空态下都要在**：一台全新的机器正是「先绑定、再拿账号」的
  // 典型场景 —— 把它藏在「先有账号」之后，等于逼用户在一台空机器上没法接上已有的池。
  const poolBar = (
    <PoolBar
      status={brokerStatus}
      accountCount={accounts.length}
      cloudCount={cloudSnap?.accounts.length ?? null}
      busy={brokerBusy}
      onUpload={onBrokerUpload}
      onLink={onBrokerLink}
      onUnbind={onBrokerUnbind}
      onSync={onBrokerSync}
      onClaim={onBrokerClaim}
      onRelease={onBrokerRelease}
      onRefreshBroker={onRefreshBroker}
      onOpenCloud={openCloud}
    />
  );

  // 云端弹窗在空态与正常态都要能打开：池栏入口指向它，全部账号删光之后
  // 也还能从中恢复墓碑
  const cloudDialog = showCloud ? (
    <CloudAccountsDialog
      status={brokerStatus}
      snapshot={cloudSnap}
      error={cloudErr}
      busy={cloudBusy}
      localAccounts={accounts}
      onClose={() => setShowCloud(false)}
      onRefresh={() => void refreshCloud(true)}
      onDelete={deleteCloud}
      onRestore={restoreCloud}
    />
  ) : null;

  if (loading)
    return (
      <section className="panel-page">
        <div className="card">
          <p className="empty">加载中…</p>
        </div>
      </section>
    );
  if (accounts.length === 0)
    return (
      <>
        <section className="panel-page">
          {poolBar}
          <div className="card">
            <EmptyState
              icon={<IconUser size={26} />}
              title="还没有账号"
              hint="从页头「添加账号」接入，也可以绑定云端凭证池。"
            />
          </div>
        </section>
        {cloudDialog}
      </>
    );

  const total = accounts.length;
  const done = accounts.filter((a) => signState(a, false) === "done").length;
  const credits = totalCredits(book, accounts);
  // 「即将过期」= 最近一个到期日到期的所有资源包剩余积分之和（与行内天数同一判据）
  const expiringSoon = soonestExpiringTotal(book, accounts);

  return (
    <>
    <section className="panel-page accounts-page">
      {poolBar}

      <div className="summary">
        <div className="card sum-card">
          <span className="sum-icon">
            <IconUsers size={17} />
          </span>
          <div className="sum-label">账号总数</div>
          <div className="sum-num">{total}</div>
        </div>
        <div className="card sum-card tone-ok">
          <span className="sum-icon">
            <IconCircleCheck size={17} />
          </span>
          <div className="sum-label">今日已签到</div>
          <div className="sum-num ok">{done}</div>
        </div>
        <div
          className="card sum-card tone-warn"
          title={
            expiringSoon === null
              ? "没有尚未到期的资源包"
              : `最近一日到期的资源包剩余积分合计（${
                  expiringSoon.daysUntil === 0
                    ? "今天"
                    : `${expiringSoon.daysUntil} 天后`
                }）`
          }
        >
          <span className="sum-icon">
            <IconClock size={17} />
          </span>
          <div className="sum-label">即将过期</div>
          <div className="sum-num warn">
            {formatCredits(expiringSoon?.total ?? null)}
          </div>
        </div>
        <div className="card sum-card feature">
          <span className="sum-icon">
            <IconBolt size={17} />
          </span>
          <div className="sum-label">总积分</div>
          <div className="sum-num">{formatCredits(credits)}</div>
        </div>
      </div>

      <div className="table-wrap card">
        <table className="data-table">
          <thead>
            <tr>
              <th>账号</th>
              <th className="num">积分</th>
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
              const e = expiryCountdown(expiryOf(book, a.id));
              // 快过期汇总：最早到期的**未过期**包还有几天、挂着多少积分（已过期的旧包跳过）
              const soon = soonestExpiry(book, a.id);
              const tok = tokenCountdown(a.expires_at);
              return (
                <tr key={a.id}>
                  <td>
                    <AccountCell
                      name={a.name}
                      phone={a.phone}
                      badge={
                        a.origin === "cloud" ? (
                          <span
                            className="badge badge-idle"
                            title="云端副本：这里的删除只移除本机这一份，云端池在「云端账号」里管理"
                          >
                            云端
                          </span>
                        ) : undefined
                      }
                    />
                  </td>
                  <td className="num">
                    {bal != null ? (
                      <span
                        className={"ac-credits clickable" + (low ? " low" : "")}
                        title="查看资源包"
                        onClick={() => setPkgAccount(a)}
                      >
                        {formatCredits(bal)}
                      </span>
                    ) : (
                      <span className="muted">—</span>
                    )}
                    {/* 积分与过期合在一列：到期时间就是「智能接管先扣谁」的第一排序键。
                        整行可点，打开逐资源包列表。 */}
                    {soon ? (
                      <span
                        className="sub clickable"
                        title="查看资源包"
                        onClick={() => setPkgAccount(a)}
                      >
                        {`${soon.daysUntil} 天后过期 ${formatCredits(soon.remaining)}`}
                      </span>
                    ) : e.text === "—" ? (
                      <span
                        className="sub clickable"
                        title="查看资源包"
                        onClick={() => setPkgAccount(a)}
                      >
                        查看资源包
                      </span>
                    ) : (
                      <span
                        className={"sub clickable" + (e.expired ? " bad" : "")}
                        title="查看资源包"
                        onClick={() => setPkgAccount(a)}
                      >
                        {e.text}
                      </span>
                    )}
                  </td>
                  <td className="col-secondary">
                    {needsRelogin(a.rt_expires_at) ? (
                      // 链死了：显示时间只会让人以为「等它自动续上就好」，而它已经续不动了
                      <span
                        className="sub bad"
                        title="登录已失效，需重新登录"
                      >
                        需重新登录
                      </span>
                    ) : tok.text === "—" ? (
                      <span className="muted">—</span>
                    ) : (
                      <span
                        className={"sub" + (tok.expired ? " bad" : "")}
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
                        title="查看签到日志"
                        onClick={() => onOpenLogs(a.id)}
                      >
                        <IconFile size={16} />
                      </button>
                      <button
                        className="icon-btn icon-danger"
                        title={
                          a.origin === "cloud"
                            ? "删除本机副本（云端池不受影响）"
                            : "删除账号"
                        }
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
      {cloudDialog}
    </>
  );
}

/**
 * 「云端账号」弹窗：**直接读云端池**的独立视图，不是主表「云端副本」的过滤结果。
 *
 * - 数据源 `GET /v1/pool/{uuid}`（后端缓存 60 秒，打开弹窗 / 同步后强制刷新）；
 *   云端正发生的增删不必等本机下一轮同步就能看见。
 * - 在线：查看；**续签设备**还能从云端池逻辑删除。
 * - 已删除：云端墓碑，**续签设备**可以恢复。
 *
 * 写操作（删除 / 恢复）只对续签设备开放：其他设备进来看，按钮一概没有。
 * 「积分」一列是本地台账的旁注（按 key 对照到本机副本才有），列表本身不依赖本机账号。
 */
function CloudAccountsDialog({
  status,
  snapshot,
  error,
  busy,
  localAccounts,
  onClose,
  onRefresh,
  onDelete,
  onRestore,
}: {
  status: BrokerStatus | null;
  snapshot: CloudSnapshot | null;
  error: string;
  busy: boolean;
  /** 只用于「积分」对照：按 key 找到本机副本才显示读数 */
  localAccounts: Account[];
  onClose: () => void;
  onRefresh: () => void;
  onDelete: (a: CloudAccount) => Promise<void>;
  onRestore: (key: string) => Promise<void>;
}) {
  const book = useCredits();
  // 云端池的写操作（删除 / 恢复）只给续签设备；其他设备在这份列表里只能查看
  const canManage = status?.is_renewer ?? false;
  const [tab, setTab] = useState<"live" | "deleted">("live");
  const [err, setErr] = useState("");
  const [busyKey, setBusyKey] = useState<string | null>(null);

  const live = snapshot?.accounts ?? [];
  const deleted = snapshot?.deleted ?? [];

  const restore = async (key: string) => {
    setBusyKey(key);
    setErr("");
    try {
      await onRestore(key);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusyKey(null);
    }
  };

  const note = err || error;

  return (
    <Dialog
      size="lg"
      icon={<IconCloud size={16} />}
      title="云端账号"
      subtitle="直接读取云端凭证池（缓存 60 秒），与主表的本机副本无关。"
      onClose={onClose}
      footer={
        <button className="btn" onClick={onClose}>
          关闭
        </button>
      }
    >
      <div className="cloud-dialog-bar">
        <span className="pool-bar-actions">
          <button
            className={"btn small" + (tab === "live" ? " primary" : " ghost")}
            onClick={() => setTab("live")}
          >
            账号 {snapshot ? live.length : "…"}
          </button>
          <button
            className={"btn small" + (tab === "deleted" ? " primary" : " ghost")}
            onClick={() => setTab("deleted")}
          >
            已删除 {snapshot ? deleted.length : "…"}
          </button>
          <button
            className="btn small ghost"
            disabled={busy}
            onClick={onRefresh}
            title="重新读取云端池（跳过缓存）"
          >
            {busy ? "读取中…" : "刷新"}
          </button>
        </span>
      </div>

      {note && <p className="note danger">{note}</p>}

      {snapshot === null ? (
        <p className="note">加载中…</p>
      ) : tab === "live" ? (
        live.length === 0 ? (
          <p className="note">云端池里还没有账号。</p>
        ) : (
          <div className="table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th>账号</th>
                  <th className="num">积分</th>
                  <th>Token 过期</th>
                  {canManage && <th className="col-actions">操作</th>}
                </tr>
              </thead>
              <tbody>
                {live.map((a) => {
                  // 只做展示对照：云端条目按 key 对到本机副本才有台账读数
                  const local = localAccounts.find((x) => accountKey(x) === a.key);
                  const bal = local ? creditsOf(book, local.id) : null;
                  const tok = tokenCountdown(a.expires_at);
                  return (
                    <tr key={a.key}>
                      <td>
                        <AccountCell name={a.name} phone={a.phone} />
                      </td>
                      <td className="num">
                        {bal != null ? (
                          formatCredits(bal)
                        ) : (
                          <span className="muted">—</span>
                        )}
                      </td>
                      <td>
                        {needsRelogin(a.rt_expires_at) ? (
                          <span className="sub bad" title="登录已失效，需重新登录">
                            需重新登录
                          </span>
                        ) : tok.text === "—" ? (
                          <span className="muted">—</span>
                        ) : (
                          <span
                            className={"sub" + (tok.expired ? " bad" : "")}
                            title={tok.expired ? `已于 ${tok.at} 过期` : `${tok.at} 过期`}
                          >
                            {tok.text}
                          </span>
                        )}
                      </td>
                      {canManage && (
                        <td className="col-actions">
                          <div className="ac-cell-actions">
                            <button
                              className="icon-btn icon-danger"
                              title="从云端池删除（可在「已删除」里恢复）"
                              onClick={() => void onDelete(a)}
                            >
                              <IconTrash size={16} />
                            </button>
                          </div>
                        </td>
                      )}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )
      ) : deleted.length === 0 ? (
        <p className="note">没有已删除的账号。</p>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th>账号</th>
                <th>手机号</th>
                <th>删除时间</th>
                {canManage && <th className="col-actions">操作</th>}
              </tr>
            </thead>
            <tbody>
              {deleted.map((it) => (
                <tr key={it.key}>
                  <td>{it.name || it.key}</td>
                  <td>{it.phone || <span className="muted">—</span>}</td>
                  <td className="num-muted">
                    {it.deleted_at ? sinceMs(it.deleted_at) : "未知"}
                  </td>
                  {canManage && (
                    <td className="col-actions">
                      <button
                        className="btn small"
                        disabled={busyKey !== null}
                        onClick={() => void restore(it.key)}
                      >
                        {busyKey === it.key ? "恢复中…" : "恢复"}
                      </button>
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Dialog>
  );
}

/**
 * 资源包列表弹窗内容：一个账号的逐额度包（名称 / 剩余积分 / 到期）。
 * `packages` 从全局积分对象里取，缺失时给一条空态提示。
 *
 * **已过期的包不显示**：接口在包过期后仍会挂着正余量（服务端还没清），
 * 原样列出只会让人误以为那些分还能用 —— 与 [`soonestExpiry`] 是同一条口径。
 * `expiry_ms` 未知的包保留（没法判断它过没过期，宁可多显示一行也不悄悄藏掉）。
 *
 * 「刷新」就地重拉当前账号的最新读数：后端拉一次进台账，返回的投影
 * `seedCredits` 并回全局对象，弹窗随订阅自动更新 —— 不需要本地再存一份 state。
 */
function PkgListAccount({ book, account }: { book: ReturnType<typeof useCredits>; account: Account }) {
  const [refreshing, setRefreshing] = useState(false);
  const [err, setErr] = useState("");
  const now = Date.now();
  const all = packagesOf(book, account.id) as CreditPackage[];
  const packs = all.filter((p) => p.expiry_ms == null || p.expiry_ms > now);
  const hidden = all.length - packs.length;

  const refresh = async () => {
    setRefreshing(true);
    setErr("");
    try {
      seedCredits([await refreshAccountCredits(account.id)]);
    } catch (e) {
      setErr(String(e));
    } finally {
      setRefreshing(false);
    }
  };

  const note = err
    ? `刷新失败：${err}`
    : hidden > 0
    ? `已隐藏 ${hidden} 个已过期的资源包`
    : "";

  return (
    <div className="pkg-list-body">
      <div className="pkg-dialog-bar">
        {note && (
          <span className={`note pkg-note${err ? " danger" : ""}`}>
            <span>{note}</span>
          </span>
        )}
        <button
          className="btn small"
          disabled={refreshing}
          onClick={() => void refresh()}
          title="拉取当前账号最新的资源包"
        >
          <IconRefresh size={13} className={refreshing ? "spin" : ""} />
          {refreshing ? "刷新中…" : "刷新"}
        </button>
      </div>
      {packs.length === 0 ? (
        <p className="note pkg-empty">暂无可用的资源包数据。</p>
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
                .sort((a, b) => (a.expiry_ms ?? Infinity) - (b.expiry_ms ?? Infinity))
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
      )}
    </div>
  );
}

/** 毫秒 → `YYYY-MM-DD`，资源包到期的完整日期展示 */
function mmddyyyy(ms: number | null): string {
  if (ms == null) return "未知";
  const d = new Date(ms);
  if (isNaN(d.getTime())) return "未知";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
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
      className={`pool-chip${state === "ok" ? " ok" : ""}`}
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
  cloudCount,
  busy,
  onUpload,
  onLink,
  onUnbind,
  onSync,
  onClaim,
  onRelease,
  onRefreshBroker,
  onOpenCloud,
}: {
  status: BrokerStatus | null;
  /** 本机**本地**账号数（上传 / 简介都按这个数说） */
  accountCount: number;
  /** 云端池在线条数（直接读云端；null = 还没读到） */
  cloudCount: number | null;
  busy: boolean;
  onUpload: () => void;
  onLink: () => void;
  onUnbind: () => void;
  onSync: () => void;
  /** 把本机设为这一池的续签设备（云端写入本机组合 id） */
  onClaim: () => void;
  /** 取消本机续签身份；10 秒内没有新设备接手会自动写回 */
  onRelease: () => void;
  /** 只刷新凭证池状态（宽限倒计时轮询用） */
  onRefreshBroker: () => void;
  /** 打开「云端账号」弹窗（在线 / 已删除都在里面） */
  onOpenCloud: () => void;
}) {
  const uuid = status?.uuid ?? null;
  const bound = Boolean(status?.bound && uuid);
  const renewer = renewerInfo(status, bound);
  const graceLeft = useGraceCountdown(status?.grace_until ?? null, onRefreshBroker);

  const note = (() => {
    // 错误优先：出过错的绑定状态比「已绑定」更该被看见
    if (status?.error) return status.error;
    if (bound) {
      return `本机 ${accountCount} 个账号与云端共用${syncNote(status)}`;
    }
    if (accountCount === 0) {
      return "绑定别的机器上的池即可接入账号。";
    }
    return `上传本机 ${accountCount} 个账号，供其他机器绑定共用`;
  })();

  return (
    <div className="pool-bar card">
      <span className="pool-bar-title">
        <span className="pool-bar-icon">
          <IconCloud size={16} />
        </span>
        云端凭证池
      </span>
      <span className={`pool-bar-note${status?.error ? " err" : ""}`} title={note}>
        {note}
      </span>
      {renewer && (
        <span
          className={`pool-renewer ${renewer.tone}`}
          title={
            graceLeft != null
              ? `${renewer.title}；已取消续签，${graceLeft} 秒内没有新设备接手会自动恢复`
              : renewer.title
          }
        >
          {renewer.label}
          {graceLeft != null && (
            <em className="pool-renewer-grace">{graceLeft}s 后自动恢复</em>
          )}
        </span>
      )}
      <span className="pool-bar-actions">
        {bound && uuid ? (
          <>
            <CopyChip uuid={uuid} />
            <button
              className="btn small"
              disabled={busy}
              onClick={onSync}
              title="与云端双向同步"
            >
              <IconSwap size={13} />
              同步
            </button>
            {/* 云端列表的独立入口：在线与已删除都在弹窗里，和主表互不影响 */}
            <button
              className="btn small ghost"
              disabled={busy}
              onClick={onOpenCloud}
              title="查看 / 管理云端账号"
            >
              <IconCloud size={13} />
              云端账号{cloudCount == null ? "" : ` ${cloudCount}`}
            </button>
            {renewer?.tone === "self" ? (
              <button
                className="btn small ghost"
                disabled={busy}
                onClick={onRelease}
                title="云端清空续签设备；10 秒内没有新设备接手，本机自动恢复"
              >
                <IconPower size={13} />
                取消本机续签
              </button>
            ) : renewer ? (
              <button
                className="btn small ghost"
                disabled={busy}
                onClick={onClaim}
                title={
                  renewer.tone === "other"
                    ? "这一池的续签设备是另一台机器；要换过来，先在那台机器上取消续签"
                    : "把本机设为这一池的续签设备：云端同步账号将由本机续签"
                }
              >
                <IconShield size={13} />
                设为本机续签
              </button>
            ) : null}
            <button
              className="btn small danger"
              disabled={busy}
              onClick={onUnbind}
              title="摘掉本机绑定，云端那一池保留"
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
                  : "新建一池并上传本机账号"
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

/**
 * 续签设备的短标签：完整组合 id 在 `title` 里，界面上只给「本机 / 其他机器 / 未指定」。
 *
 * 一个池只认一个组合 id（客户端类型 + 机器标识），所以这一栏说的是**整池**的事：
 * 云端同步账号由谁续签。本机登录的账号不受它影响（自己登的自己续）。
 */
function renewerInfo(
  status: BrokerStatus | null,
  bound: boolean
): { label: string; title: string; tone: "self" | "other" | "none" | "unknown" } | null {
  if (!status || !bound) return null;
  if (!status.renewer_known) {
    return {
      label: "续签设备：同步后可知",
      title: "本机还没跟云端同步过，暂时不知道这一池谁在续签",
      tone: "unknown",
    };
  }
  if (status.is_renewer) {
    return {
      label: "续签设备：本机",
      title: `本机组合 id：${status.self_id}`,
      tone: "self",
    };
  }
  if (status.renewer) {
    return {
      label: "续签设备：其他机器",
      title: `池上的续签设备是 ${status.renewer}；云端同步账号由它续签`,
      tone: "other",
    };
  }
  return {
    label: "续签设备：未指定",
    title:
      "没有设备认领这一池：云端同步账号暂不续签（本机登录的账号不受影响）",
    tone: "none",
  };
}

/**
 * 取消续签后的 10 秒倒计时：返回剩余秒数（没有宽限期时 `null`），并在到期前后轮询状态。
 *
 * 为什么要在前端轮询：宽限结束会发生两种事（本机自动写回 / 新设备接走后 409），
 * 都由后端那支定时任务更新内存；界面不主动问就还停在「未指定」。
 */
function useGraceCountdown(
  graceUntil: number | null,
  onRefresh: () => void
): number | null {
  const [, tick] = useState(0);
  useEffect(() => {
    if (graceUntil == null) return;
    const timer = window.setInterval(() => tick((n) => n + 1), 500);
    return () => window.clearInterval(timer);
  }, [graceUntil]);
  useEffect(() => {
    if (graceUntil == null) return;
    let alive = true;
    const poll = async () => {
      // 宽限结束后再问两三跳：写回是 10 秒整才发出的，内存更新还要一个来回
      while (alive && Date.now() < graceUntil + 5000) {
        await new Promise((r) => window.setTimeout(r, 1500));
        if (!alive) return;
        onRefresh();
      }
    };
    void poll();
    return () => {
      alive = false;
    };
  }, [graceUntil, onRefresh]);
  if (graceUntil == null) return null;
  return Math.max(0, Math.ceil((graceUntil - Date.now()) / 1000));
}

/** 「上次同步」那句人话。同步是热路径上两分钟一跳的东西，说清时刻比说「同步中」有用 */
function syncNote(status: BrokerStatus | null): string {
  if (!status) return "";
  if (status.syncing) return "；正在同步…";
  if (!status.last_ok_ms) return "；还没同步过";
  return `；上次同步 ${sinceMs(status.last_ok_ms)}`;
}
