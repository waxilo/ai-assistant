export interface Account {
  /** 本地记录 id，**由后端分配**（导入时不要自己造） */
  id: string;
  name: string;
  phone?: string | null;
  region?: string | null;
  user_id?: string | null;
  token: string;
  refresh_token?: string | null;
  host?: string | null;
  expires_at?: number | null;
  refresh_expires_at?: number | null;
  device_id?: string | null;
  machine_id?: string | null;
  created_at: string;
  /** 积分快照（供「智能接管」选号：先扣谁的额度） */
  credit_snapshot?: CreditSnapshot | null;
}

/**
 * 「待导入」的账号：只有前端真正知道的部分。
 *
 * `id` 与 `created_at` 属于**服务端职责**（由 `import_accounts` 统一补齐），前端不编：
 * 早期这里硬编码 `id: ""`，结果 `checkin_one(a.id)` / `remove_account(a.id)` /
 * `statuses[a.id]` 全靠 id 匹配，两个空 id 就会永远命中同一个账号。
 */
export type NewAccount = Omit<Account, "id" | "created_at">;

/**
 * 账号**已有积分**快照（来自 TraeWork 的 `ide_user_ent_usage` 额度用量接口，
 * 即「剩余可用额度」）。⚠️ 不是签到奖励积分。
 */
export interface CreditSnapshot {
  /** 剩余可用积分；未知为 null */
  credits: number | null;
  /** 不限量（账号存在 credits_limit = -1 的额度包） */
  unlimited: boolean;
  /** 「还有余量的额度包」里最早的到期时间（毫秒）；未知为 null */
  earliest_expiry_ms: number | null;
  fetched_at: string;
}

export interface Settings {
  checkin_enabled: boolean;
  checkin_time: string;
  /**
   * 智能接管：端点改写（`product.json` 指向本机明文反代）+ 免证书（不装 CA、不讲 TLS）一体开关。
   *
   * ⚠️ 免证书是**唯一**形态：端点恒为 `http://127.0.0.1:PORT`，前提是目标应用已打过
   * 「免证书补丁」；未打补丁时写明文端点会让它启动即崩。
   */
  takeover_enabled: boolean;
  /** 本地反代监听端口 */
  takeover_port: number;
  /**
   * 接管哪些应用（按应用 id，即 `.app` 名，如 `TRAE SOLO CN` / `Trae CN`）。
   *
   * ⚠️ **空列表 = 全部**（与 `billing_account_ids` 同一套语义，也是「从没配置过」的默认态）
   * —— 所以界面上把空列表渲染成「全部勾选」，且「全不选」被禁止（那会写回空列表、退回全部）。
   * 本机已经不存在的 id 会被后端丢掉，不会留在设置里。
   */
  takeover_apps: string[];
  billing_account_ids: string[];
  webhook_url: string;
  /**
   * 积分简报开关：开启后后台每小时采样一次（`ide_user_ent_usage`），
   * 并把已经走完的小时固化成「时条目」。默认关：开启那一刻才对齐基线。
   */
  briefing_enabled: boolean;
}

export interface CheckinResult {
  success: boolean;
  already: boolean;
  inactive: boolean;
  /** 服务端限流（9074）等瞬时失败，稍后会自动补签 */
  transient: boolean;
  /** token 失效，需要重新登录 */
  auth_failed: boolean;
  message: string;
  credit?: number | null;
  host?: string | null;
  at: string;
}

export interface LogEntry {
  at: string;
  account: string;
  message: string;
  success: boolean;
}

/**
 * 一条「接管动态」= **用户日志**里的一条（见 `journal.rs` 的 `read`）。
 *
 * ⚠️ 2026-09-16 起事件按**受众**分两条通道落盘，本类型只承载**用户日志**那一侧
 * （判据在 `journal.rs` 的 `TRACE_EVENTS`）：技术过程（`proxy_path` / `unbound_session*` /
 * `proxy_error` / `proxy_upstream_status` / `ws_*` 等）走 `takeover-trace.jsonl`，
 * **不会出现在界面上**，因此这里也不必再列它们。
 *
 * 这里会出现的是用户能**对账**或能**行动**的事实：
 * `install` / `uninstall` / `sweep` / `legacy_proxy_clear` / `restart_trae` / `restart_skipped` /
 * `restart_fail` / `patch_apply` / `patch_revert` / `patch_fail` / `patch_revert_fail` /
 * `install_blocked` / `rules_save` / `rules_write` / `route_start` / `failover` /
 * `takeover_fail`（开关拨了却没生效，已回滚成关闭 —— 原因在 detail 里）/
 * `takeover_blocked`（接管开着但没有账号能扣费，请求会 503 —— **只有用户能解决**）/
 * `token_swap_rejected`（换 `x-ide-token` 被上游拒，该域退回原凭据）/
 * `billing_list_changed`（参与扣费名单改动 —— 它是「这笔换给了谁」的前提）/
 * `billing_list_stale`（名单勾的账号全对不上，已 fail-open 成全部账号）/
 * `takeover_apps_changed`（接管应用名单改动 —— 只在接管关着时可能发生）
 */
export interface JournalEvent {
  at: string;
  at_ms: number;
  event: string;
  detail: string;
}

export interface AcctStatus {
  id: string;
  checked_in: boolean;
  /** 账号**已有积分**（entitlement 剩余额度）；未知为 null */
  credits: number | null;
  /** 不限量 */
  unlimited: boolean;
  /**
   * 「还有余量的额度包」里最早的到期时间（毫秒）；未知为 null。
   * 「智能接管」选号的第一排序键就是它（先扣快到期的额度）。
   */
  earliest_expiry_ms?: number | null;
  message: string;
}

// `RenewReport` / `RenewSource` 已随「手动续签」一起删除（2026-09-15）：
// 后端不再有 `renew_accounts` 命令，续签结论只从两条路体现 ——
// 账号列表里的到期时间（被推远 = 续上了）与签到日志里的失败记录。

export type Page = "accounts" | "takeover" | "briefing" | "logs" | "settings";

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
  /** 最近一次解绑从本机删除的「与云端一致」账号数（平时为 0，仅解绑结果反馈） */
  removed_local?: number;
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

export interface OAuthStart {
  login_id: string;
  verification_uri: string;
  host: string;
  expires_in: number;
}

export interface OAuthPoll {
  done: boolean;
  token?: string | null;
  refresh_token?: string | null;
  host?: string | null;
  region?: string | null;
  uid?: string | null;
  nickname?: string | null;
  phone?: string | null;
  expires_at?: number | null;
  device_id?: string | null;
  machine_id?: string | null;
  error?: string | null;
}

/**
 * 接管规则（`proxy-rules.json`，运行时热加载，改完立即生效）。
 *
 * 「哪些请求该换成账号池凭据」是靠实测收敛的，所以这几个旋钮必须能在**不重编译**的前提下调 ——
 * 真机对账时一轮「发消息 → 看扣了谁」只要几十秒。
 */
export interface TakeoverRules {
  /** 只观察、不换凭据（诊断用，也是唯一「绝对弄不坏应用」的形态） */
  observe_only: boolean;
  /** 覆盖内置扣费前缀表（空 = 用内置表；非空 = **完全取代**） */
  swap_http_prefixes: string[];
  /** 是否连 WebSocket 握手里的 `Authorization` 一起换 */
  swap_ws: boolean;
  /** **仅诊断**：强制判为「透传」的前缀，即使内置表命中 */
  never_swap_prefixes: string[];
}

/**
 * 某个应用的主进程「闸门补丁」状态。
 *
 * 补丁把它的 URL pattern 闸门从「只认 https」改成「认任何 scheme」，
 * 于是本地端点可以走**明文回环** —— 免证书模式的唯一前置条件。
 * ⚠️ 打得成与否由 `writable` 决定：macOS「App 管理」(TCC) 会拦住对已签名应用包的修改。
 * ⚠️ 现在**每个应用各有一份**（本机可能装了多个 Trae shell），互不影响。
 */
export interface PatchStatus {
  /** 找到该应用的 `out/main.js` 了吗（false = 这个应用不支持免证书模式） */
  supported: boolean;
  /** 被改的目标文件 */
  target?: string | null;
  /** 目标文件**真能写**吗（TCC / 只读卷只有真写一次才知道） */
  writable: boolean;
  /** 当前是否已打过补丁 */
  patched: boolean;
  /** 版本是否被识别（`false` 时助手**拒绝**打补丁 —— 宁可不禁用证书，也不能把应用弄坏） */
  recognized: boolean;
  /** 扫到的闸门处数（诊断用，正常是 2） */
  gates: number;
  /** 第 3 处补丁点（身份头规则的 pattern 数组）是否在位 */
  identity_patterns: boolean;
  /** 后端给的那句话：能打 / 已打 / 为什么打不了。界面直接显示，不重写。 */
  message: string;
}

/**
 * 本机发现到的**一个** Trae 应用（含未被勾选的）。
 *
 * `id` = `.app` 名（macOS）/ 安装目录名（Windows），同时是设置里的选择键、界面标签与
 * 进程控制句柄 —— 三者同源，不给它加一层会漂的映射。
 */
export interface AppStatus {
  /** 稳定 id（macOS 下 = `.app` 名），也是设置里记录选择用的键 */
  id: string;
  /** 显示名（当前与 id 相同） */
  label: string;
  /** 应用包（macOS）/ 安装目录（Windows）—— 排障时要能一眼看到在改谁 */
  bundle: string;
  app_dir: string;
  /** 是否在接管名单里（名单为空 = 全部 ⇒ 这里恒 `true`） */
  selected: boolean;
  /** 当前是否在运行 */
  running: boolean;
  /** `product.json` 的端点是否已指向本机反代 */
  installed: boolean;
  /** 上述改写是不是本助手写的（只有带标记才敢还原） */
  ours: boolean;
  /** 它的安装目录是否**真能写** —— 不能写就没有任何一步能成 */
  writable: boolean;
  /** 它自己的闸门补丁状态 */
  patch: PatchStatus;
  upstream_http: string | null;
  upstream_ws: string | null;
  /**
   * **只在有事要说时非空**（版本不认识 / 不可写 / 被别人改过 / 端点丢了）。
   * 一切正常时是空串 —— 界面上一行文字都不该出现。
   */
  message: string;
}

/** 智能接管状态（本地反代 + 应用改道）。 */
export interface TakeoverStatus {
  /** 用户是否已开启接管 */
  enabled: boolean;
  /** 本地反代监听端口 */
  port: number;
  /** 本地反代是否正在监听 */
  proxy_active: boolean;
  proxy_error: string | null;
  /** 本机反代端点基址（恒为明文 `http://127.0.0.1:PORT`，只作展示） */
  endpoint_base: string;
  /** 当前生效的接管规则 */
  rules: TakeoverRules;
  /** 端点覆盖租约是否新鲜（反代的心跳） */
  lease_fresh: boolean;
  /** 本机发现到的**全部** Trae 应用（含未勾选的）—— 界面据此渲染「接管应用」多选 */
  apps: AppStatus[];
  /** 接管名单里、但本机已经不存在的 id（应用卸载了 / 改名了） */
  missing_apps: string[];
  /** 总状态那句话（成功时也可以是陈述句；界面只在有东西挡路时才显示） */
  message: string;
}

/**
 * 简报里的一行：某个账号在一段时间（一条时条目 / 一天）里的动静。
 *
 * 「消耗」与「新增」都取自接口里资源包的**累计**字段（`credits_amount` / `credits_limit`）
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
  /** 这一小时各账号的情况（按消耗降序，含没动静但采到了余额的） */
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
