export interface CheckinRecord {
  success: boolean;
  already: boolean;
  inactive: boolean;
  message: string;
  credit: number | null;
  balance: number | null;
  streak: number | null;
  host: string | null;
  at: string;
  code: number | null;
}

export interface Account {
  id: string;
  name: string;
  phone: string | null;
  token: string;
  /** 续签用的 refresh token（有它才能自动续期） */
  refresh_token: string | null;
  /** access token 过期时间（毫秒）；null 表示未知 */
  expires_at: number | null;
  /** refresh token 过期时间（毫秒）；null 表示未知。它才是「还能不能换出新 token」的判据 */
  rt_expires_at: number | null;
  base_url: string | null;
  created_at: string;
  last: CheckinRecord | null;
  /**
   * 当前积分事实（**投影，不落盘**）：后端从唯一的积分台账里投影出来，
   * 每一次拉取（刷新 / 简报采样 / 签到 / 接管路由补拉）都会更新那份台账。
   * 账户管理与积分简报读的是同一个数；null = 还没读到过。
   *
   * 注意它**不是账号自己的字段**：后端把「账号」与「账号 + 积分读数」拆成了两个类型
   * （`Account` / `AccountView`），这里收到的一律是后者。前端只有一个模型（不落盘），
   * 所以合并在一起写；但别据此以为它能被存回后端。
   */
  credits: CreditFact | null;
  /** 服务端「今日是否已签到」的真实状态（持久化）；null = 未查询/失败 */
  checked_today: boolean | null;
}

export interface Settings {
  default_base_url: string;
  auto_checkin_on_start: boolean;
  /** 每天定时自动签到（应用需保持运行） */
  schedule_enabled: boolean;
  /** `HH:MM`，24 小时制 */
  schedule_time: string;
  /** 定时签到的随机时间窗（分钟）：当天实际触发 = `schedule_time` + `[0, 窗口]` 内随机；0 = 关闭随机 */
  schedule_window_minutes: number;
  /** 通知总开关 */
  notify_enabled: boolean;
  /** 通知 webhook，形如 https://…/hook/<key> */
  notify_webhook: string;
  /** 定时签到后推送 */
  notify_on_schedule: boolean;
  /** 手动「全部签到」后推送 */
  notify_on_manual: boolean;
  /**
   * 智能接管总开关（WorkBuddy 专用反代）：开启 = 监听 127.0.0.1 并把 WorkBuddy
   * 端点指向它，按积分过期时间优先路由；关闭 = 停止监听并摘掉端点。无鉴权 Key。
   */
  proxy_enabled: boolean;
  /** 反代监听端口 */
  proxy_port: number;
  /** 扣费备选账号 id 列表（多选）：反代只在这批账号里选号扣费；空 = 全部可用 */
  billing_account_ids: string[];
  /** 限流无感切换生效的模型 id 列表（多选）：这些模型触发 429 时自动换备用账号重发；
   *  0 积分免费模型恒生效无需勾选，这里只存用户额外勾选的付费模型；空 = 仅免费模型 */
  rate_limit_models: string[];
  /** 限流（429）时是否在**同一会话内**换备用账号。
   *  true = 无感续跑，但同一会话会出现中途换凭证；false = 防御优先，429 原样透传 */
  failover_on_rate_limit: boolean;
  /** 多账号风控预防：批量签到时在账号之间加入随机间隔 */
  stagger_checkin: boolean;
  /** 随机间隔上限（秒），实际在 2..=max 之间取值 */
  stagger_max_seconds: number;
  /** 批量签到时随机打乱账号顺序（只影响请求次序，列表顺序不变） */
  shuffle_checkin_order: boolean;
  /** 手动「全部签到」也加账号间隔（避免手动路径成为唯一的瞬时连发入口） */
  manual_stagger: boolean;
  /** 手动「全部签到」的间隔上限（秒），实际在 2..=max 之间取值 */
  manual_stagger_max_seconds: number;
  /**
   * 是否开启积分简报（后台每小时结算一次时条目）。**默认关闭**，由用户主动开启。
   *
   * 这个开关的入口在**积分简报页**（那一页才是它的主场），不在设置页。
   * 开启时会清掉已有的简报与台账里的小时桶，并用此刻读数**只对齐基线**
   * （见 `enableCreditBriefing`），因此列表不会出现「历史与新基线混在一起」。
   */
  briefing_enabled: boolean;
  /**
   * 简报是否推送到 webhook（复用通知总开关与地址）。
   *
   * 粒度是**天**：时条目每小时就在结算，但「今天花了多少」要等当天结束才有定论。
   * 在设置页与两条签到通知开关并排展示 —— 「哪些东西会推送」集中一处才好核对。
   */
  notify_on_briefing: boolean;
}

/**
 * 直接读本机 WorkBuddy 登录文件（auth/*.info）得到的账号。
 * 一次就能拿到 token + 昵称 + 手机号，是首选通道。
 */
export interface LocalAccount {
  token: string;
  /** 续签用的 refresh token */
  refresh_token: string | null;
  source: string;
  host: string | null;
  /** 官方文件里的 account.uid */
  uid: string | null;
  nickname: string | null;
  phone: string | null;
  /** access token 过期时间（毫秒时间戳） */
  expires_at: number | null;
  /** refresh token 过期时间（毫秒时间戳）；登录文件里没写就是 null */
  rt_expires_at: number | null;
  /** 来自官方固定文件或带 lastLogin 标记，即当前实际登录的账号 */
  is_current: boolean;
  file: string;
}

/** 一次导入请求：token 必填，其余为可直接预填的元信息 */
export interface ImportItem {
  token: string;
  host?: string | null;
  name?: string | null;
  phone?: string | null;
  refresh_token?: string | null;
  expires_at?: number | null;
  rt_expires_at?: number | null;
}

/** 批量导入结果：已存在的账号会被合并补全而不是跳过 */
export interface ImportReport {
  added: number;
  updated: number;
}

/**
 * 凭证池的绑定状态 —— **只读**。
 *
 * 池的粒度是**整台机器**：一池一个 uuid、池内一把闸。所以配置面只有三个动作
 * （上传 / 绑定 / 解绑），没有「保存配置」这类接口，也不需要开关 ——
 * `bound` 就是开关本身。
 */
export interface BrokerStatus {
  /** 本机绑定到某一池了吗（有 uuid 就是绑了） */
  bound: boolean;
  /** 池 uuid：**另一台机器靠它接上同一池**，所以要能复制出去 */
  uuid: string | null;
  /** 云端那一池的版本号，每提交一次 +1（仅展示） */
  version: number | null;
  /** 上次「问过管家」的时刻（毫秒）；null = 本次进程内还没问过 */
  last_sync_ms: number | null;
  /** 上次**成功**同步的时刻（毫秒）；null = 从未成功 */
  last_ok_ms: number | null;
  /** 上次失败原因，成功后清空 */
  error: string | null;
  /** 正在同步：长请求期间界面不至于显示成「从未同步」 */
  syncing: boolean;
}

/** 上传 / 绑定的结果：uuid 是唯一要展示给用户复制的东西 */
export interface PoolOp {
  uuid: string;
  /** 云端这一池有多少条账号 */
  account_count: number;
  /** 本次并进本地的条数（新增 + 更新） */
  merged: number;
  /** 人话说明（直接拿去 toast） */
  message: string;
}

/** 「无感登录」第一步的返回：授权链接与本次会话 id */
export interface OAuthStart {
  login_id: string;
  verification_uri: string;
  host: string;
  expires_in: number;
}

/** 「无感登录」轮询结果：done=false 表示还在等用户授权（不是错误） */
export interface OAuthPoll {
  done: boolean;
  token: string | null;
  /** 续签用的 refresh token（授权接口一并返回） */
  refresh_token: string | null;
  host: string | null;
  uid: string | null;
  nickname: string | null;
  phone: string | null;
  /** access token 过期时间（毫秒时间戳） */
  expires_at: number | null;
  /** refresh token 过期时间（毫秒时间戳）；授权响应没给就是 null */
  rt_expires_at: number | null;
  error: string | null;
}

/**
 * 积分事实 —— 一个账号的「资源包快照」：还剩多少、什么时候读到的、包什么时候过期。
 *
 * 这三样是同**一次**资源接口拉取的产物，所以合成一个类型：拆成几个变量就会各自
 * 演化，变成「积分是新的、过期时间是旧的」这种更难查的不一致。
 *
 * 前后端各只有一个出口 —— 后端 `ledger::Store`（进程内唯一台账）、前端
 * `src/credits.ts`（模块级唯一对象）。智能接管的路由排序与账户展示的
 * 「剩余 / 过期时间」读的都是它，因此天然同源。
 */
export interface CreditFact {
  /** 剩余积分（get-user-resource 汇总，取不到为 null） */
  credits: number | null;
  /** 读数时刻（本地时间串）；接管路由用它判断是否需要重拉 */
  at: string;
  /** 还有余量的资源包里最早的重置/过期时间（毫秒时间戳）；null = 未知 */
  earliest_expiry_ms: number | null;
  /** 逐资源包明细（只含还有余量的包）；前端「资源包列表」弹窗展示用 */
  packages: CreditPackage[];
}

/** 一个资源包的展示快照：名字 + 剩余积分 + 到期时间 */
export interface CreditPackage {
  name: string;
  /** 本包剩余积分 */
  remaining: number;
  /** 本包到期时间（毫秒）；null = 未知 */
  expiry_ms: number | null;
}

/**
 * 后端广播的一条积分事实（`credits-updated` 事件的一条）。
 *
 * 就是在 [`CreditFact`] 上加一个账号 id —— 事件是一整批推过来的，
 * 前端要按 id 并进自己那本内存对象（见 `src/credits.ts`）。
 * 字段不再抄一遍：抄一遍就等于多埋一处会漂移的定义。
 */
export interface CreditRow extends CreditFact {
  id: string;
}

export interface CheckinLog {
  id: string;
  account_id: string;
  account_name: string;
  account_phone: string | null;
  at: string;
  success: boolean;
  already: boolean;
  inactive: boolean;
  code: number | null;
  message: string;
  host: string | null;
  credit: number | null;
  balance: number | null;
}

/** 「网络急救」里的一个可疑点 */
export interface NetIssue {
  id: string;
  /** 分类：配置文件 / launchd 全局环境 / shell 启动脚本 / 本应用反代 */
  scope: string;
  /** 具体位置：文件路径、变量名或设置项 */
  target: string;
  value: string;
  /**
   * block = 会让网络不通；warn = 残留但当前不影响连通性；
   * ok = 正常状态（例如本应用智能接管正在工作），不算问题
   */
  level: "block" | "warn" | "ok";
  note: string;
  /** 是否属于「一键恢复」能自动处理的范畴 */
  fixable: boolean;
}

export interface NetReport {
  /** 没有 block 级问题时为 true */
  healthy: boolean;
  issues: NetIssue[];
  /** 已扫描的位置 */
  scanned: string[];
}

export interface NetStep {
  action: string;
  ok: boolean;
  detail: string;
}

export interface NetRestoreReport {
  steps: NetStep[];
  /** 已备份的文件路径 */
  backups: string[];
  /** 本地反代是否被本次恢复关掉 */
  proxy_disabled: boolean;
  report: NetReport;
}

/** 智能接管的当前状态 */
export interface StealthStatus {
  /** 设置里是否开启 */
  enabled: boolean;
  /** 端点是否真的写进 WorkBuddy 配置了 */
  installed: boolean;
  /** 租约是否新鲜（心跳还在跳） */
  alive: boolean;
  port: number;
  url: string;
  /** 人话说明当前状态与下一步该做什么 */
  note: string;
}

/** 接管事件流的一条记录（takeover-journal.jsonl） */
export interface JournalEvent {
  at_ms: number;
  at: string;
  /** install / uninstall / route_start / restart_workbuddy / proxy_upstream_error / … */
  event: string;
  detail: string;
}

/** 「限流切换」模型列表中的单个模型 */
export interface ModelInfo {
  /** 模型 id，如 hy3 / hy3-x / deepseek-v3 … */
  id: string;
  /** 是否 0 积分免费模型（恒生效、UI 锁定勾选不可取消） */
  free: boolean;
  /** 积分倍率原始串（如 "x0.00" / "x0.05"），仅展示用 */
  multiplier: string;
}

/** 「限流切换」支持的模型列表（全模型，从网关动态拉取） */
export interface FreeModelsReport {
  /** 免费排前、其余按 id 排序 */
  models: ModelInfo[];
  /** fetched = 刚从网关拉取；cache = 1 小时缓存内；fallback = 拉取失败用内置兜底 */
  source: "fetched" | "cache" | "fallback";
}

/**
 * 简报里的一行：某个账号在一段时间（一条时条目 / 一天）里的动静。
 *
 * 「消耗」与「新增」都取自接口里资源包的**累计**字段（`CapacityUsed` / `CapacitySize`）
 * 的增量 —— 累计量只增不减，所以多个客户端同时消耗也都能算进来，
 * 不需要按请求归因，并发也不会算错。
 */
export interface BriefAccount {
  account_id: string;
  name: string;
  phone: string | null;
  consumed: number;
  gained: number;
  /** 读数时刻的剩余积分（取不到为 null —— 不谎报 0） */
  balance: number | null;
}

/**
 * 一条**时条目**：某一天某一个小时的消耗与新增，带**逐账号明细**。
 *
 * 这是简报唯一的落盘数据，由后台每小时结算一次：
 * 整点前采一次样（让这一小时的增量落进即将结束的那个小时），整点后固化。
 * 界面上没有任何「手动生成一条」的入口。
 */
export interface HourEntry {
  /** 归属日期 YYYY-MM-DD */
  date: string;
  /** 归属小时（0–23） */
  hour: number;
  /** 固化时刻（本地时间串） */
  generated_at: string;
  consumed: number;
  gained: number;
  /** 该小时结束时全部账号的剩余积分合计；都取不到时为 null */
  balance: number | null;
  /** 这一小时里**有动静**的账号（按消耗降序），没动静的不进列表 */
  accounts: BriefAccount[];
  /**
   * 「开启简报」那一刻落下的**起点条目**：消耗/新增都是 0，只有余额读数。
   * 它不参与后端去重，这一小时走完后的真实固化会把它覆盖掉。
   */
  baseline: boolean;
}

/**
 * 一条**日条目**：当天所有时条目之和，**读的时候现算、不落盘**。
 *
 * 这样「日 = 时之和」是结构上的事实，不可能出现「日条目与它下面的时条目对不上」。
 */
export interface DayEntry {
  /** 归属日期 YYYY-MM-DD（列表按它倒序） */
  date: string;
  /** 这个自然日是否已经走完（今天为 false ⇒ 界面显示「进行中」） */
  sealed: boolean;
  consumed: number;
  gained: number;
  /** 当天最后一个有时点读数的小时的余额合计 —— 「这天结束时还剩多少」 */
  balance: number | null;
  /** 当天全部时条目（按小时升序） */
  hours: HourEntry[];
  /** 当天各账号的合计（由时条目相加而来，按消耗降序） */
  accounts: BriefAccount[];
}

