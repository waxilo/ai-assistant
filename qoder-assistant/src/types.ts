/**
 * 单次签到（= 领取当天活动权益）的结果。
 *
 * 字段对应 Qoder 的「活动权益」语义（见 `basedata/20260918_Qoder活动权益接口逆向.md`）。
 * 旧版的 `streak` / `host` / `code` 已删：`streak` 是 CodeBuddy 的连续签到天数
 * （Qoder 的活动里没有这个概念，连续活跃天数在 `seatActivity` 里，属于额度面）、
 * `host` 是 CodeBuddy 多域时代的候选域、`code` 是旧接口的业务返回码。
 * 三者本来就没有任何显示出口。
 */
export interface CheckinRecord {
  success: boolean;
  /** 今天已经领过（幂等成功态） */
  already: boolean;
  /** 当前没有可领取的活动（还没到刷新点 / 活动已结束） */
  inactive: boolean;
  message: string;
  /** 本次领取到的额度（`benefit.amount`）；已领或失败时为 null */
  credit: number | null;
  /** 领取那一刻读到的剩余积分（不是「当前余额」—— 那是台账的投影，见 Account.credits） */
  balance: number | null;
  /** 领的是哪一场活动（`campaignKey`，形如 act-20260918-899） */
  campaign_key: string | null;
  /**
   * 这笔签到的积分**自己的到期时刻**（毫秒）；取不到为 `null`。
   *
   * 来自领取接口发放凭据里的 `expiresAt` —— 已领取的账号靠**幂等回放**同样能拿到，
   * 所以「今日已领」不等于「拿不到到期时间」。它是「积分过期」列对免费号唯一的日期来源：
   * `/sash/api/v2/me/usage` 对免费号给的是「无期限」哨兵（见 `ledger::note_grant_expiry`）。
   */
  expires_at: number | null;
  at: string;
}

/**
 * 一个可选区域（国际版 / 国内版）。
 *
 * 清单来自后端的 `regions` 命令（`Region::ALL` 的投影），而不是前端自己写一份：
 * 中文名、以及「OpenAPI 在哪个域」这类事实只该有一处定义 ——
 * 两边各写一份的下场是「界面说的」与「请求实际打的」各走各的。
 */
export interface RegionOption {
  /** 落盘 / IPC 的稳定标识（`global` / `cn`），原样回传给后端 */
  key: string;
  /** 中文名（「国际版」/「国内版」） */
  label: string;
  /** 一句话说明差异（域在哪） */
  hint: string;
}

export interface Account {
  /**
   * 这个账号属于**哪一套部署**（`global` / `cn`）—— 是账号的**身份**，不是可覆盖的选项。
   *
   * 同一台机器上可以同时有国际版与国内版的账号，同一个手机号也完全可能在两边都出现，
   * 所以「哪个区域里的哪个号」才是唯一标识（后端合并键就是 `(region, phone)`）。
   * 它还决定每一次请求打哪个域：跨区域的 token 在对方的网关上无效。
   */
  region: string;
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
  // 这里曾经有 `base_url: string | null`：多域时代「这个账号打哪个域」的**逐账号覆盖**。
  // 它只写不读，写进去的还恒是错的（登录流程塞的是授权域，不是模型网关），
  // 2026-09-18 前后端一并删除。它想表达的东西现在由上面那个 `region` 承担 ——
  // 区别在于 `region` 是账号**被谁签发的**，不是可以随手改的覆盖项。
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
  /**
   * **接管目标区域**（`global` / `cn`）：智能接管作用于哪一套官方客户端。
   *
   * 它决定三件事：端点写进哪个 CLI 配置目录（`~/.qoder` / `~/.qoder-cn`）、
   * 反代把对话请求转发到哪个模型网关、以及扣费账号只在**该区域**里选。
   * 「两个官方客户端同时装着」是常态，而「现在该接管哪一个」是用户的意图，
   * 不是能从文件系统推断出来的事实，所以是显式选择。
   *
   * 它取代了旧的 `default_base_url`：那个字段是模板残留的 CodeBuddy 域名，
   * 而上游其实由区域唯一决定 —— 留一个可写字段只会多一处「改错了不报错」。
   */
  takeover_region: string;
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
   * 智能接管总开关（Qoder 专用反代）：开启 = 监听 127.0.0.1 并把 Qoder
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
 * 直接读本机 Qoder **凭据文件**（`auth.v1.dat`，Chromium safeStorage 加密）得到的账号。
 * 一次就能拿到 token + 昵称 + 手机号，是首选通道。
 */
export interface LocalAccount {
  /**
   * 这条凭据属于**哪一套部署** —— 由它来自哪个 profile 目录决定，不是猜出来的。
   * 两套部署的登录文件分别在 `com.qoder.app.stable` 与 `com.qodercn.app.stable` 下，
   * 同一台机器可以各有一个「当前账号」。导入时必须把它一起带上（见 [`ImportItem`]）。
   */
  region: string;
  token: string;
  /** 续签用的 refresh token */
  refresh_token: string | null;
  source: string;
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

/**
 * **单个区域**的读取结果（`LocalScan.probes` 里的一条）。
 *
 * 存在的理由：`LocalScan.accounts` 只能表达「有没有」，而「没有」至少有三种**互不相同**
 * 的原因 —— 那个版本从未登录过 / 钥匙串里没条目（或用户拒绝授权）/ 解密失败。
 * 上一版把它们都渲染成「请先在 Qoder 桌面端登录一次」，于是**已经登录**的用户
 * 被告知去重新登录一次。
 */
export interface LocalProbe {
  /** 区域标识（`global` / `cn`） */
  region: string;
  /** 该区域是否读到了账号 */
  found: boolean;
  /** 一句话说明：读到了什么，或者为什么没读到 */
  detail: string;
}

/**
 * 一次「导入本机账号」扫描的完整结果。
 *
 * `probes` 每个区域一条、顺序与后端 `Region::ALL` 一致：界面直接按它渲染，
 * 不要再自己按区域分组（分组规则会在前端多出一份定义）。
 */
export interface LocalScan {
  accounts: LocalAccount[];
  probes: LocalProbe[];
}

/** 一次导入请求：token 必填，其余为可直接预填的元信息 */
export interface ImportItem {
  /**
   * 该账号所属区域。**缺省 = 国际版**（后端 `#[serde(default)]`）：
   * 老版本攒下的条目里没有这个字段，而它们全部来自国际版域。
   */
  region?: string;
  token: string;
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
  expires_in: number;
  /**
   * 这一轮登录打的是**哪套部署** —— 授权链接本身不含区域信息，只有发起方能告诉我们。
   * 轮询结果（[`OAuthPoll`]）里**没有**这个字段，所以前端要自己记住它、并在导入时带上。
   */
  region: string;
}

/**
 * 「无感登录」轮询结果：done=false 表示还在等用户授权（不是错误）。
 *
 * 没有 `host`：Qoder 只有一套 Global 域，那个字段恒等于同一个常量，已随「选域」一起删掉。
 */
export interface OAuthPoll {
  done: boolean;
  token: string | null;
  /** 续签用的 refresh token（授权接口一并返回） */
  refresh_token: string | null;
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

/**
 * 一个资源包的展示快照：名字 + 剩余积分 + 到期。
 *
 * 「到期」是**三态**，靠两个字段联用表示（不是冗余字段）：
 * - `expiry_ms != null` → 有明确到期日
 * - `never_expires` → 服务端明说不会过期（响应里 `expiresAt` 给的是 `9999-12-31` 哨兵）
 * - 两者皆无 → 响应里根本没给到期信息（未知）
 *
 * 后端在**解析响应时**就已经把哨兵归一掉了（`ledger::normalize_expiry`），
 * 所以这里永远收不到 9999 年的假日期；界面只需要把三态分别说清楚。
 */
export interface CreditPackage {
  name: string;
  /** 本包剩余积分 */
  remaining: number;
  /** 本包到期时间（毫秒）；null = 没有真实到期日（是「不过期」还是「未知」看下一项） */
  expiry_ms: number | null;
  /** 服务端明说这个包不会过期 */
  never_expires: boolean;
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

/**
 * 一条签到日志 = [`CheckinRecord`] + 账号标识 + `id`。
 *
 * 旧版的 `code` / `host` 已随签到线重写删除（见 [`CheckinRecord`]）。
 */
export interface CheckinLog {
  id: string;
  account_id: string;
  account_name: string;
  account_phone: string | null;
  at: string;
  success: boolean;
  already: boolean;
  inactive: boolean;
  message: string;
  /** 本次领取到的额度 */
  credit: number | null;
  balance: number | null;
  /** 领的是哪一场活动（`campaignKey`） */
  campaign_key: string | null;
  /** 这笔积分的到期时刻（毫秒）；见 [`CheckinRecord.expires_at`] */
  expires_at: number | null;
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
  /** 端点是否真的写进 Qoder 配置了 */
  installed: boolean;
  /** 租约是否新鲜（心跳还在跳） */
  alive: boolean;
  /** 这份状态说的是**哪个区域**的接管（即接管目标区域） */
  region: string;
  port: number;
  url: string;
  /** 人话说明当前状态与下一步该做什么 */
  note: string;
}

/** 接管事件流的一条记录（takeover-journal.jsonl） */
export interface JournalEvent {
  at_ms: number;
  at: string;
  /** install / uninstall / route_start / restart_qoder / proxy_upstream_error / … */
  event: string;
  detail: string;
}

/** 「限流切换」模型列表中的单个模型 */
export interface ModelInfo {
  /** 给网关用的模型 id（Qoder 形如 qmodel_38max） */
  id: string;
  /** 给人看的显示名（形如 Qwen3.8-Max）；取不到时为空串，界面退回显示 id */
  name: string;
  /** 是否 0 积分免费模型（恒生效、UI 锁定勾选不可取消） */
  free: boolean;
  /** 积分倍率原始串（如 "x0.00" / "x0.05"），仅展示用；空串 = 倍率未知 */
  multiplier: string;
}

/**
 * 「限流切换」模型清单（全模型，三层来源）。
 *
 * source 与后端一一对应：
 * `fetched` = 刚从 Qoder 模型目录拉取；`cache` = 落盘快照；
 * `local` = 本机 Qoder 痕迹；`empty` = 三层都没拿到（界面显示空态）。
 * 这里**没有**「内置兜底」这一档 —— 兜底写死模型名正是旧实现认错模型的根源。
 *
 * ⚠️ 这份清单**不要求所在区域有账号**：三层里两层是纯本地的，「没账号」只是让
 * 第 1 层（联网）缺席 —— 理由见后端 `crate::models` 模块头。
 */
export interface ModelReport {
  /** 免费排前、其余按 id 排序 */
  models: ModelInfo[];
  source: "fetched" | "cache" | "local" | "empty";
  /**
   * 第 1 层为什么没结果，直接显示给用户（`source === "fetched"` 时恒为 null）。
   *
   * 与 `source` 是两件事：那个说「清单来自哪一层」，这个说「为什么不是刚拉到的」。
   * 后端给的是成句的中文，界面不要自己拼（「没有账号」与「接口不开放」两种原因
   * 的下一步动作完全不同）。
   */
  note: string | null;
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

// ── 已删除：Qoder 原生额度面 / 活动面的一组类型 ───────────────────────────
//
// 这里曾经有 9 个接口：`CampaignBenefit` / `Campaign` / `CampaignView`（活动面）与
// `PlanView` / `SeatActivity` / `CreditsSummary` / `HeatDay` / `Heatmap` /
// `AccountOverview`（额度面）。它们各对应账号页上的一块：一个是「今日权益」那一栏，
// 其余是顶部指标卡与热力图。
//
// 2026-09-18 按产品决定改版：顶部只留「账号总数 / 已签到 / 未签到 / 剩余额度」，
// 热力图不做，「今日权益」那一栏撤掉 —— 9 个类型全部失去消费方，
// 连同后端 `account_overview` / `account_heatmap` / `account_campaigns` 三个命令一并删除。
//
// 响应形态与实测证据仍在 `basedata/20260918_Qoder缺失接口逆向.md` 与
// `basedata/20260918_Qoder活动权益接口逆向.md`，将来要接照那两份接。
