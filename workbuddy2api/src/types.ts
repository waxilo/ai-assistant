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
   * 对外 API 网关总开关。**默认关闭**：它会把账号池的积分对外提供服务，
   * 属于用户明确知道自己在干什么才该开的功能。
   */
  gateway_enabled: boolean;
  /** 网关监听端口（默认 8790；避开接管的 8787 与 qoder 的 8789） */
  gateway_port: number;
  /** 网关 API Key（`sk-wb-<32hex>`）。空 = 尚未生成，首次开启时自动签发 */
  gateway_api_key: string;
  /** 扣费备选账号 id 列表（多选）：网关只在这批账号里选号扣费；空 = 全部可用 */
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
  /**
   * 静默自动更新：每天到 `auto_update_time` 后自动检查、下载并安装，装完应用自行重启。
   * **默认开启**；关掉只停掉后台这条路径，设置页的「检查更新」按钮照常可用。
   */
  auto_update_enabled: boolean;
  /**
   * 静默更新的每日时刻 `HH:MM`（默认 `05:00`）。
   * 语义是「**不早于**该时刻、每天最多一次」：夜里关机错过之后，当天首次轮询补做。
   */
  auto_update_time: string;
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

/**
 * 一轮双向整池同步的结论（点「同步」按钮返回的就是这个）。
 *
 * `message` 已经是给人看的话，直接 toast 即可；数字字段留着，
 * 将来要按「有没有真的改动」分支处理时不用再加一次后端调用。
 */
export interface SyncReport {
  changed: boolean;
  /** 被跳过（未绑定 / 两分钟内已同步过 / 闸在别的机器手里） */
  deferred: boolean;
  /** 云端 → 本地：并入本地的条数（新增账号 + 采纳的新票） */
  merged: number;
  /** 本轮续出来的凭证数 */
  refreshed: number;
  /** 本轮真失败数（网络 / 服务端临时拒），会把整池拖进冷静期 */
  failed: number;
  /** 本机续不动的账号数（票已失效、要重新登录），不拖停整池 */
  blocked: number;
  version: number | null;
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

/** 网关运行状态（后端 `gateway_status`） */
export type GatewayState = {
  /** 端口真的在监听（不是「开关打开」） */
  listening: boolean;
  port: number;
  /** 最近一次绑定失败的原因；成功后为 null */
  error: string | null;
};

/**
 * Codex 接入状态（后端 `codex::state`，**只读**，不改任何文件）。
 *
 * `catalogModels` 是回读模型目录文件得到的 slug 列表 —— 页面按它回填勾选，
 * 而不是按「上次勾了什么」：用户手改过目录、或换台机器，后者会显示一份并不存在的状态。
 */
export interface CodexState {
  /** `~/.codex` 目录是否存在（没装 Codex 就没得接） */
  found: boolean;
  configPath: string;
  /** `model_provider` 是否已指向本网关 */
  connected: boolean;
  /** provider 段里声明的 base_url（用来判断「接的是不是同一个端口」） */
  baseUrl: string;
  /** 当前 `model` 顶层键 */
  model: string;
  /** 已写进模型目录的 slug（空 = 还没接过 / 目录被删了） */
  catalogModels: string[];
}

/** 重启 Codex 应用的结果（后端 `codex_restart`） */
export interface CodexRestartOutcome {
  /** 重启前应用在不在跑 */
  running: boolean;
  /** 是否真的完成了「结束 + 拉起」 */
  restarted: boolean;
  /** 识别到的应用进程名；没识别到为 null */
  app: string | null;
}

export type LogCategory = "checkin" | "renew" | "notify" | "api" | "takeover" | "system";

/**
 * 「日志管理」里的一条。来源是 `logs.rs` 的持久化日志（后端已按时间倒序）。
 */
export interface LogEntry {
  at: string;
  /** 毫秒时间戳：跨来源排序用 */
  at_ms: number;
  category: LogCategory;
  /** 账号名；系统类为空串 */
  account: string;
  message: string;
  success: boolean;
}

/**
 * 一个可供客户端使用的对话模型（后端 `chat_models`，已滤掉非对话模型）。
 *
 * 两处用：接入 Codex 时作为候选集（勾选后写进它的模型目录），
 * 以及手动填别的客户端时照着抄。
 */
export interface ChatModel {
  slug: string;
  name: string;
  /** 上游倍率原文（`x0.00` / `x0.29 credits`）；空 = 上游没给 */
  credits: string;
  /** 是否免费（倍率解析不出来时一律 false —— 不能猜成免费） */
  free: boolean;
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

