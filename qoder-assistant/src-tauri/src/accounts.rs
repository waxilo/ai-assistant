use crate::ledger;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 单次签到（= 领取当天活动权益）的结果（用于前端展示与落库）。
///
/// 字段全部对应 Qoder 的「活动权益」语义（见 `basedata/20260918_Qoder活动权益接口逆向.md`）。
/// 旧版的 `streak` / `host` / `code` 三个字段已经删掉：`streak` 是 CodeBuddy 的连续签到天数
/// （Qoder 的活动里没有这个概念，连续活跃天数在 `seat-activity` 里，属于额度面）、
/// `host` 是 CodeBuddy 多域时代的候选域（Qoder 只有一个域）、
/// `code` 是旧接口的业务返回码（活动接口用 HTTP 状态 + `status` 字段表达结果）。
/// 三个字段都**没有任何显示出口**，留着只会让人以为签到还在走旧接口。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CheckinRecord {
    pub success: bool,
    /// 今天已经领过（幂等成功态）
    pub already: bool,
    /// 当前没有可领取的活动（还没到刷新点 / 活动已结束）
    pub inactive: bool,
    pub message: String,
    /// 本次领取到的额度（`benefit.amount`）；已领或失败时为 None
    #[serde(default)]
    pub credit: Option<f64>,
    /// **领取那一刻**读到的账号剩余积分（可能是小数；取不到为 None）。
    ///
    /// ⚠️ 它**不是**「当前余额」—— 当前余额只存在于积分台账里（见 [`Account::credits`]），
    /// 因为只有台账同时知道「读到了多少」和「什么时候读到的」。
    ///
    /// 两个出口：签到时间线（`logs::log_from_record` 把同一份值落进 `logs.json`）；
    /// 账户列表在台账还没有该账号读数时兜底（刚升级上来、首次采样还没跑）。
    /// **写点只有签到本身** —— 刷新不再回填它，否则会给这条记录的时刻配上不属于它的读数。
    #[serde(default)]
    pub balance: Option<f64>,
    /// 领的是哪一场活动（`campaignKey`，形如 `act-20260918-899`）。
    /// 每天换一个，所以它同时是「这是哪一天的签到」的可核对凭据。
    #[serde(default)]
    pub campaign_key: Option<String>,
    /// **这笔积分的真实到期时刻**（毫秒）。
    ///
    /// 来源是领取接口发放凭据里的 `expiresAt`（见 `qoder_api::ClaimReceipt`）——
    /// 它比 `/sash/api/v2/me/usage` 的 `expiresAt` 更细：后者说的是**整个额度概览**的到期
    /// （计划周期终点；免费号还是「无期限」哨兵），而这里是签到领到的**这一笔**。
    ///
    /// ⚠️ 已领的账号靠**幂等回放**同样能拿到（响应 `replayed: true`，见
    /// `qoder_api::claim_campaign`），所以「今日已领」不再等于「拿不到到期时间」。
    /// 真的取不到（老响应、网络失败）才是 `None` —— 界面据此显示「—」。
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub at: String,
}

/// 一个签到账号
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    pub id: String,
    pub name: String,
    /// 手机号（展示用标识，手动录入，可空）
    #[serde(default)]
    pub phone: Option<String>,
    /// 邮箱（展示用标识，可空）。
    ///
    /// 与 [`Account::phone`] 是一对：界面的次级标识按「手机号优先、邮箱兜底」展示
    /// （规则与前端 `accountIdent` 同源）。**两个都要存**：只留手机号，手机号为空的
    /// 账号就只剩一个昵称可认（昵称会改，也常为空）。
    ///
    /// 来源与手机号同一批：本机登录文件的 `user.email`、登录时的 `/api/v1/userinfo`、
    /// 以及给老账号惰性补一次的那个请求（见 `commands::fill_identity_if_missing`）。
    #[serde(default)]
    pub email: Option<String>,
    pub token: String,
    /// 续签用的 refresh token（导入本机账号 / 无感登录时一并带上；老账号为 None）
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// access token 过期时间（毫秒时间戳）；None 表示未知
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// **refresh token** 过期时间（毫秒时间戳）；None 表示未知。
    ///
    /// 与 [`Account::expires_at`] 分开存，因为两者是**两条独立的命**：access token 常见 60 天，
    /// refresh token 常见 30 天，而只有后者能换来前者。合成一个字段，
    /// 「refresh token 已经死了、但 access token 看着还很新」这种状态就看不出来了 ——
    /// 而那恰好是「签到突然全部失败」最容易被误判成网络问题的成因。
    ///
    /// 来源按优先级：续签响应的 `refreshExpiresIn` / 授权响应的同名字段 /
    /// 本机登录文件的 `auth.refreshExpiresAt`。三条路都没有就是 None（不编）。
    /// 它的下游用途是**上传给管家**：云端据此回答「这一池的续签链还能撑多久」。
    #[serde(default)]
    pub rt_expires_at: Option<i64>,
    // 这里曾经有 `pub base_url: Option<String>`：CodeBuddy 多域时代「这个账号打哪个域」的
    // 逐账号覆盖。它有两处硬伤，2026-09-18 删除：
    //   ① **只写不读** —— 转发实际读的是 `settings.default_base_url`（见 `proxy.rs`），
    //      这个字段除了导入时被写一次之外没有任何消费方；
    //   ② **写进去的值还是错的** —— 登录新账号那条路把它写成授权域（`qoder.com`，登录页），
    //      而转发目标是模型网关（国内版 `gateway.qoder.com.cn`）。
    //      一个既没人读、内容又不对的字段，
    //      留着只会让人以为「账号可以各自配端点」。
    // Qoder 只有一套域，唯一可改的那一处是 `Settings::default_base_url`（带迁移）。
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last: Option<CheckinRecord>,
    /// 服务端「今日是否已签到」的真实状态（只读查询，持久化）。
    /// None = 尚未查询/查询失败（前端按「未知 / 待签到」处理，
    /// 绝不再把昨天的本地缓存或一次本地报错当成确定状态）。
    #[serde(default)]
    pub checked_today: Option<bool>,
    /// **COSY 凭据里的 uid**（Qoder 侧账号 id，形如 `019eb647-a8b6-7664-…`）。
    ///
    /// 与 [`Account::id`] 是**两码事**：后者是本应用内部的 UUID（`8aa97773-…`，v4），
    /// 前者是 Qoder 侧的账号标识（v7 形态）。混用时服务端回 `105 Login expired` ——
    /// 看起来像 token 过期，其实与 token 毫无关系（2026-09-19 实测两条账号各验一遍）。
    ///
    /// 它只在「反代要重签 COSY 凭据」时被消费（见 [`crate::cosy`]）。首次需要时
    /// 用 token 调 `/api/v3/user/status` 取回并落盘（[`set_cosy_uid`]），
    /// 所以老账号第一次接管会多一次查询、之后走缓存。
    #[serde(default)]
    pub cosy_uid: Option<String>,
    /// **活性判定**：usage 接口对这张 token 返回 401 的时刻（毫秒，见
    /// `checkin::apply_liveness`）。
    ///
    /// 这是唯一可信的「票已被吊销」证据 —— `expires_at` 只是本地记的账，服务端可以
    /// 提前作废一张到期时间还在未来的票。`broker::adopt` 的失效抢救规则用它：
    /// 本机已判死 + 池里那份与本地不同 → 无条件换血，不再比 `expires_at`。
    /// 续签/采纳成功后清除。
    #[serde(default)]
    pub invalidated_at: Option<i64>,
    /// 续签冷却的截止时刻（毫秒）：死链（`RefreshError::dead`）后冷 6 小时。
    ///
    /// **持久化**的失败退避 —— 不落盘的话应用一重启就失忆，死链每轮都去打一个
    /// 必然被拒的接口。续签成功 / 换到新票时清除。
    #[serde(default)]
    pub renew_blocked_until: Option<i64>,
}

impl Account {
    /// 这个账号认人的那一样：手机号优先，邮箱兜底。
    ///
    /// 它有两个消费方，都是「缺了它就认不出这是谁」的地方：
    /// - 界面上的次级标识（手机号为主、邮箱兜底）；
    /// - [`completeness`] 的「有身份标识」判分 —— 只数手机号会让手机号为空的账号
    ///   在去重里永远比不出高低：真实账号（有邮箱）与池里收养的幽灵条目（没有邮箱）
    ///   打平后按「谁在前」决胜，幽灵就可能留下，而它的 id 没有台账、日志挂账也跟着断。
    pub fn identity(&self) -> Option<&str> {
        fn pick(v: &Option<String>) -> Option<&str> {
            v.as_deref().map(str::trim).filter(|s| !s.is_empty())
        }
        pick(&self.phone).or_else(|| pick(&self.email))
    }
}

/// 发给前端的账号视图 = **账号本身** + 从唯一台账投影出来的积分事实。
///
/// 为什么积分不直接挂在 [`Account`] 上：`Account` 是**落盘模型**（`accounts.json` 里
/// 就是这些字段），而积分是积分台账（`credit_ledger.json`）的只读投影。把两者塞进
/// 同一个类型，「不落盘」和「要发给前端」就会互相打架 —— 上一版用 `#[serde(skip)]`
/// 想让前者生效，可 serde 的 `skip` 连**序列化**一起跳过，而 Tauri 的 IPC 正是走 serde：
/// 投影永远到不了界面，账号页读不到读数只好回退成「上次签到时读到的余额」，
/// 到期时间则一律为空。拆成两个类型后各归各的，谁也不会再误伤谁。
///
/// `flatten` 保证发出去的 JSON 仍是**扁平**的（「账号字段 + credits」），
/// 与从前完全一致 —— 前端不必为一个实现细节改结构。
#[derive(Serialize, Clone, Debug)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    /// 当前积分事实（剩余积分 + 读数时刻 + 最早过期时间）。
    ///
    /// 来源是积分台账（见 [`crate::ledger::fact`]）：每一次拉取（手动刷新 / 简报采样 /
    /// 签到 / 接管路由决策）都写进那份台账，账户管理与积分简报都从它读。
    /// 台账里还没有该账号的读数时是 `None`（界面显示「—」，不回退到别的字段编一个数）。
    pub credits: Option<ledger::CreditFact>,
}

impl AccountView {
    /// 给一个账号配上它当前的积分读数。
    pub fn of(account: Account, dir: &Path) -> Self {
        let credits = ledger::store(dir).fact_of(&account.id);
        AccountView { account, credits }
    }
}

/// 给一批账号配上积分读数 —— **「账号 + 积分」的唯一出口**。
///
/// 每个需要两者的地方都走它，所以看到的永远是同一份数（同一本内存台账、同一个时刻）。
/// 调用方拿不到「自己 load 一份账号列表、再手动拼一份积分」的机会，
/// 也就不会再有第二个各自漂移的口径。
pub fn view_accounts(accounts: Vec<Account>, dir: &Path) -> Vec<AccountView> {
    accounts.into_iter().map(|a| AccountView::of(a, dir)).collect()
}

/// 读账号 + 配上积分读数。所有「要给界面/路由看」的调用点都该用它，而不是裸 load。
pub fn load_account_views(dir: &Path) -> Vec<AccountView> {
    view_accounts(load_accounts(dir), dir)
}

/// 应用设置 —— `settings.json` 的**扁平落盘模型**，界面与所有消费方（反代、调度、
/// 命令、界面 IPC）读到的都是这一张表。
///
/// 新增字段一律带 `#[serde(default)]`，这样老版本写下的 settings.json
/// 仍能反序列化（不会整个文件被判为非法而回退成默认值）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default)]
    pub auto_checkin_on_start: bool,
    /// 是否开启「每天定时自动签到」
    #[serde(default)]
    pub schedule_enabled: bool,
    /// 定时签到时刻，24 小时制 `HH:MM`（应用需保持运行才会触发）
    #[serde(default = "default_schedule_time")]
    pub schedule_time: String,
    /// 定时签到的随机时间窗（分钟）：当天实际触发时刻 = `schedule_time` + `[0, 窗口]` 内随机。
    ///
    /// 「每天同一分钟触发」是脚本最好认的特征，给一个窗口就能让每天都不一样；
    /// 当天挑定的时刻会写进 `schedule_state.json` 复用，重启不会重新摇。0 = 关闭随机。
    #[serde(default = "default_schedule_window")]
    pub schedule_window_minutes: u32,
    // ── 以下几项 `notify_*` 与 `briefing` 的推送开关是**整机行为**（一个 webhook 频道），
    //    不随任何配置分片：签到结果、简报都往同一个频道推。
    /// 通知总开关（关掉后定时与手动都不推送，**续签失败告警也一起静音**）
    #[serde(default)]
    pub notify_enabled: bool,
    /// 通知 webhook 地址，形如 `https://…/hook/<key>`
    #[serde(default)]
    pub notify_webhook: String,
    /// 定时签到结束后推送
    #[serde(default = "default_true")]
    pub notify_on_schedule: bool,
    /// 手动「全部签到」结束后推送（默认关，避免连点造成刷屏）
    #[serde(default)]
    pub notify_on_manual: bool,
    /// 智能接管总开关（127.0.0.1 反代 + 把 Qoder 端点指向它，按积分过期时间优先路由）。
    /// 这是 Qoder 专用通道：只监听本机、无鉴权 Key、无独立「仅反代」模式。
    #[serde(default)]
    pub proxy_enabled: bool,
    /// 反代监听端口（默认 8789，避开 workbuddy-assistant 占用的 8787 与 Clash 的 7897）
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    /// 扣费备选账号（多选）：反代只在这批账号里选号扣费，**未选中的账号不允许扣费**。
    /// 空 = 全部账号都可作为备选（智能轮换）。
    #[serde(default)]
    pub billing_account_ids: Vec<String>,
    /// 限流无感切换生效的模型（多选）：这些模型触发 429 时自动换备用账号重发同一请求。
    /// 0 积分（免费）模型恒生效、无需勾选；这里只存用户额外勾选的付费模型。
    /// 空 = 仅 0 积分免费模型享受限流切换。
    #[serde(default)]
    pub rate_limit_models: Vec<String>,
    /// 多账号风控预防：批量签到时在账号之间加入随机间隔，避免同一 IP 瞬时连发多账号请求。
    #[serde(default = "default_true")]
    pub stagger_checkin: bool,
    /// 随机间隔上限（秒）；实际间隔在 2..=max 之间取值
    #[serde(default = "default_stagger_max")]
    pub stagger_max_seconds: u32,
    /// 批量签到时随机打乱账号顺序。
    ///
    /// 顺序本身也是特征：每次都按账号列表的固定先后连发，等于把「同一批账号」
    /// 直接写进请求序列里。打乱只影响**发请求的次序**，列表展示与落盘顺序不变。
    #[serde(default = "default_true")]
    pub shuffle_checkin_order: bool,
    /// 手动「全部签到」也加账号间隔（默认开）。
    ///
    /// 手动路径曾为了「点完就想看结果」跳过等待，于是它成了全程唯一的瞬时连发入口——
    /// 风控看到的恰好就是那一串几秒内完成的签到。
    #[serde(default = "default_true")]
    pub manual_stagger: bool,
    /// 手动「全部签到」的间隔上限（秒）；实际在 2..=max 之间取值。
    /// 比自动签到小得多：手动场景还得让人等得下去。
    #[serde(default = "default_manual_stagger_max")]
    pub manual_stagger_max_seconds: u32,
    /// 限流（429）时是否在同一会话内换备用账号。
    ///
    /// true = 无感续跑，但同一个会话会出现「中途换凭证」——正常用户的一个会话
    /// 自始至终只有一个账号，这在风控眼里是极高异常值。false = 防御优先：
    /// 429 原样透传、不记冷却，该会话不会被切到别的账号上。
    #[serde(default = "default_true")]
    pub failover_on_rate_limit: bool,
    /// 积分简报：后台每小时结算一次时条目，日条目是当天时条目之和。
    ///
    /// 开关的入口在**积分简报页**（那一页才是它的主场），不在设置页。
    /// 开启时会清掉已有的简报与台账里的小时桶，并用此刻读数**只对齐基线**
    /// （见 `ledger::rebaseline`）—— 断档期攒下的增量既归不到具体的小时、
    /// 又不该算进开启后的第一个小时，宁可不记。
    ///
    /// **默认关闭**（[`Settings::default`] 同样是 false）：它是「每小时打一批接口 + 改写台账 +
    /// 开启时清历史」的常驻统计，用户没主动开就不该在后台自己跑起来。所以用裸 `serde(default)`，
    /// 不是 `default_true`。
    ///
    /// `alias` 是为了让升级上来的旧配置（键名还是 `report_enabled`）继续生效：
    /// 默认值翻成关闭之后，只认新键就会把**上次打开过**的开关在升级后悄悄关掉 ——
    /// 这种「设置被重置」只会被当成「版本越更越不对劲」，很难联想到是键名换了。
    #[serde(default, alias = "report_enabled")]
    pub briefing_enabled: bool,
    /// 简报是否推送到 webhook（复用通知总开关 `notify_enabled` 与 `notify_webhook`）。
    ///
    /// 推的粒度是**天**：时条目每小时就在结算，但「今天花了多少」要等当天结束才有定论，
    /// 每小时推一条只会把通知刷成流水账。同样带 `alias` 兼容旧键名。
    ///
    /// 与上面几项 `notify_*` 一样是**整机行为**。
    #[serde(default = "default_true", alias = "notify_on_report")]
    pub notify_on_briefing: bool,
    /// 静默自动更新：每天到设定时刻后自动检查、下载并安装新版本，装完应用自行重启。
    ///
    /// **默认开启**（[`default_true`]）：这是个「装完就没人管」的常驻工具，更新是纯收益
    /// 动作。关掉只停掉后台这条路径，设置页的「检查更新」按钮与它无关，照常可用。
    #[serde(default = "default_true")]
    pub auto_update_enabled: bool,
    /// 静默更新的每日时刻，24 小时制 `HH:MM`。
    ///
    /// 默认 05:00 —— 这个点机器通常空闲，自动重启不会打断正在用的接管与签到。
    /// 语义是「**不早于**该时刻、每天最多一次」：夜里关机/休眠错过之后，
    /// 当天首次轮询就补做（否则「默认凌晨 5 点」对从不留夜机的用户等于永不生效）。
    #[serde(default = "default_auto_update_time")]
    pub auto_update_time: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_checkin_on_start: false,
            schedule_enabled: false,
            schedule_time: default_schedule_time(),
            schedule_window_minutes: default_schedule_window(),
            notify_enabled: false,
            notify_webhook: String::new(),
            notify_on_schedule: true,
            notify_on_manual: false,
            proxy_enabled: false,
            proxy_port: default_proxy_port(),
            billing_account_ids: Vec::new(),
            rate_limit_models: Vec::new(),
            stagger_checkin: true,
            stagger_max_seconds: default_stagger_max(),
            shuffle_checkin_order: true,
            manual_stagger: true,
            manual_stagger_max_seconds: default_manual_stagger_max(),
            failover_on_rate_limit: true,
            // 积分简报默认关闭：它要按小时打接口、改写台账，用户主动开启才跑
            briefing_enabled: false,
            notify_on_briefing: true,
            // 静默更新默认开：常驻工具的更新不该等人想起来点按钮
            auto_update_enabled: true,
            auto_update_time: default_auto_update_time(),
        }
    }
}

/// 风控随机间隔默认上限：45 秒足够打散节奏，又不至于让「全部签到」等太久
fn default_stagger_max() -> u32 {
    45
}

/// 手动「全部签到」的间隔上限：8 秒。用户是主动点的，等待要明显短于自动签到
fn default_manual_stagger_max() -> u32 {
    8
}

/// 定时签到的随机窗口：90 分钟。既能消掉「每天同一分钟」，又不会把签到推到太晚
fn default_schedule_window() -> u32 {
    90
}

/// 规范化时刻字符串：接受 `9:7` / `09:07` / 首尾空格，统一输出 `HH:MM`。
/// 非法输入返回 None（调用方保留原值，避免把用户输入悄悄改掉）。
pub fn normalize_time(input: &str) -> Option<String> {
    let t = input.trim();
    let (h, m) = t.split_once(':')?;
    // 只允许纯数字，排除 `9:7:0`、`09am` 之类
    if h.is_empty() || m.is_empty() || !h.chars().all(|c| c.is_ascii_digit()) || !m.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(format!("{h:02}:{m:02}"))
}

// 这里曾有 `fn default_base_url()` —— 把「模型网关上游」做成了一个可配置字段，
// 还配了一条把 CodeBuddy 域名迁过来的迁移。上游是编译期常量（国内版网关
// `gateway.qoder.com.cn`，见 `proxy`），设置里不再有这一项；
// 老 `settings.json` 里的 `default_base_url` 键会被 serde 直接忽略。

/// 默认定时签到时刻：**10:00**，对齐服务端的活动刷新点。
///
/// 每日活动在 10:00（UTC+8）生成、窗口到次日 09:59 关闭，所以定时任务必须落在窗口内。
/// 上一版这里的默认值是 CodeBuddy 时代抄来的 `09:07`，**恰好落在刷新前的夹缝里**：
/// 那一刻昨天那条已经领过（`CLAIMED`）、今天那条还没生成，于是每天跑一次、
/// 每天只能报「当前没有可领取的活动」—— 不报错，但一次也领不到。
fn default_schedule_time() -> String {
    "10:00".to_string()
}

/// 上一版遗留的定时签到默认值（见 [`default_schedule_time`]）。只迁移这一个确切值。
const LEGACY_SCHEDULE_TIME: &str = "09:07";

fn default_true() -> bool {
    true
}

/// 静默更新的默认时刻：凌晨 5 点。
///
/// 挑深夜不是为了「隐藏」，而是因为这一刻**最不容易撞上正在使用的界面/接管**：
/// 自动更新装完会重启整个应用，落在白天就是一次无预警的中断。
fn default_auto_update_time() -> String {
    "05:00".to_string()
}

/// 反代默认端口：**8789**。
///
/// 为什么不是 8787：那个端口被**同机的 workbuddy-assistant** 的常驻反代占着
/// （实测 `netstat` 里 8787 是 `workbuddy-assistant.exe` 在 LISTENING）。
/// 三个助手各自的反代要能同时开着，端口就不能撞。8789 同时避开了 Clash 的 7897 等常用端口。
fn default_proxy_port() -> u16 {
    8789
}

pub fn accounts_file(dir: &Path) -> PathBuf {
    dir.join("accounts.json")
}

pub fn settings_file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

pub fn load_accounts(dir: &Path) -> Vec<Account> {
    let f = accounts_file(dir);
    if !f.exists() {
        return Vec::new();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    // 先读成 Value 逐条过滤：`region:"global"` 的条目是国际版账号 —— 国际版支持
    // 已整体移除（2026-10-16），它们不再是本应用管理的账号，读入时丢弃。
    // `region` 字段本身也已从落盘模型里删掉：清洗后保存的条目没有该字段，照常读入。
    let accounts: Vec<Account> = serde_json::from_str::<Vec<serde_json::Value>>(&s)
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v.get("region").and_then(|r| r.as_str()) != Some("global"))
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    // 读出口就收敛重复条目：**这里是所有「给界面/路由看」的账号的唯一来源**，
    // 在别处兜底都只能治一条路（见 `dedupe_by_credential` 的说明）。
    dedupe_by_credential(accounts)
}

/// 同一份凭证只该有一条记录 —— 把重复条目收敛掉。
///
/// 为什么要在**读**的路径上做：重复记录是跨机凭证池认错身份锚点时留下的
/// （池里同一份凭证因「手机号后补」挂了两个 key，见 `broker::claims`），
/// 锚点修好之后盘上那条幽灵记录**不会自己消失** —— 它看起来完全正常
/// （有昵称、有 token），用户只能在界面上看着一个重复账号发愣。
///
/// 判据只有一条不变量：**access / refresh token 相等 = 同一个账号**。
/// 两套部署的 token 由各自后端签发，不会撞；token 为空的条目不参与合并
/// （否则所有「还没导入凭证」的东西会被合成一条）。
///
/// 保留哪条：信息最全的那条。权重最高的是「有签到结果」——
/// **一次成功的签到就是「这份凭证真的能用」的实证**，而幽灵记录恰恰是靠身份锚点漂移
/// （手机号后补、昵称被改）才活下来的。被并掉那条的独有字段（手机号、昵称、续签信息）
/// 会补进保留的那条，一个字段都不丢。**幂等**：对已收敛的列表再跑一次结果不变。
pub fn dedupe_by_credential(accounts: Vec<Account>) -> Vec<Account> {
    let mut kept: Vec<Account> = Vec::with_capacity(accounts.len());
    for a in accounts {
        let Some(i) = kept.iter().position(|k| same_credential(k, &a)) else {
            kept.push(a);
            continue;
        };
        if completeness(&a) > completeness(&kept[i]) {
            let mut winner = a;
            merge_gaps(&mut winner, &kept[i]);
            kept[i] = winner;
        } else {
            merge_gaps(&mut kept[i], &a);
        }
    }
    kept
}

/// 两份凭证是不是同一份？（判据与 `broker::same_credential` 同源，那一侧是「池条目 vs 账号」）
fn same_credential(a: &Account, b: &Account) -> bool {
    let (at, bt) = (a.token.trim(), b.token.trim());
    if !at.is_empty() && at == bt {
        return true;
    }
    match (
        a.refresh_token.as_deref().map(str::trim),
        b.refresh_token.as_deref().map(str::trim),
    ) {
        (Some(x), Some(y)) => !x.is_empty() && x == y,
        _ => false,
    }
}

/// 信息完整度（越高越该保留这条）。三项的含义：
/// 有签到结果 = 这份凭证真的在后端上签成功过；
/// 有身份标识 = 认得出这是谁（见 [`Account::identity`]）；
/// 问过服务端状态 = 至少确认过一次真实状态。
fn completeness(a: &Account) -> u8 {
    u8::from(a.last.is_some()) * 4
        + u8::from(a.identity().is_some()) * 2
        + u8::from(a.checked_today.is_some())
}

/// 把 `from` 有、`into` 没有的字段补进去（**只补空，绝不覆盖**）。
/// id、昵称都不是「空字段」问题，所以一律不动 —— 保留方是谁，身份就是谁。
fn merge_gaps(into: &mut Account, from: &Account) {
    if into.token.trim().is_empty() {
        into.token = from.token.clone();
    }
    if into.refresh_token.as_deref().map(str::trim).unwrap_or("").is_empty() {
        into.refresh_token = from.refresh_token.clone().filter(|s| !s.trim().is_empty());
    }
    if into.phone.as_deref().map(str::trim).unwrap_or("").is_empty() {
        into.phone = from.phone.clone().filter(|s| !s.trim().is_empty());
    }
    if into.email.as_deref().map(str::trim).unwrap_or("").is_empty() {
        into.email = from.email.clone().filter(|s| !s.trim().is_empty());
    }
    if into.name.trim().is_empty() {
        into.name = from.name.clone();
    }
    if into.expires_at.is_none() {
        into.expires_at = from.expires_at;
    }
    if into.rt_expires_at.is_none() {
        into.rt_expires_at = from.rt_expires_at;
    }
    if into.last.is_none() {
        into.last = from.last.clone();
    }
    if into.checked_today.is_none() {
        into.checked_today = from.checked_today;
    }
    if into.created_at.trim().is_empty() {
        into.created_at = from.created_at.clone();
    }
}

/// **原子写**：临时文件 + `sync_all` + `rename`，并且**把错误原样还回去**。
///
/// 为什么单独一条路：这条路上落盘的是凭证 —— `accounts.json` 里的 refresh token
/// 是**一次性**的，换过票之后旧票在服务端就作废了。此时半截落盘等于
/// 「旧票已死、新票没留下」，这条链只能重新登录；而 `let _ = fs::write(..)`
/// 会把这件事变成几周后才被发现的沉默事故。
///
/// 两点不能反过来：
/// 1. 先 `sync_all` 再 `rename` —— 顺序反了只是「名义上原子」，崩溃时仍可能
///    留下一个名字已经换过去、内容却还空着的文件。
/// 2. 权限在**创建临时文件时**就收到 0600，而不是 rename 之后再补 —— 后者中间
///    有个窗口：新凭证已经落盘、却还是同机其它用户可读的。
pub(crate) fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::other(format!("路径没有文件名：{}", path.display()))
    })?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let opened = || -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
        }
        #[cfg(not(unix))]
        fs::File::create(&tmp)
    };
    let result = opened().and_then(|mut f| {
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    });
    // 失败时把半截临时文件扫掉：它带着新凭证的内容，留在那儿既脏又是一份多余副本。
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    write_atomic(&accounts_file(dir), &serde_json::to_string_pretty(accounts)?)
}

/// 把某个账号的 [`Account::cosy_uid`] 落盘（幂等：值没变就不写）。
///
/// 为什么是「运行期补齐」而不是「导入时填好」：uid 要联网问一次
/// `/api/v3/user/status` 才拿得到，而反代是**收到第一个 COSY 请求时**才第一次需要它。
/// 在那里顺手取一次、落盘，之后所有转发都命中缓存 —— 不给导入流程加网络依赖。
///
/// 返回是否真的改了盘（只用于日志；写失败不影响本次转发，uid 内存里还能用一次）。
/// 反代是多线程的，所以写前加锁：两个请求同时补齐同一账号时，只写一次、不互相覆盖。
pub fn set_cosy_uid(dir: &Path, account_id: &str, uid: &str) -> bool {
    static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = match WRITE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut accounts = load_accounts(dir);
    let mut changed = false;
    for a in accounts.iter_mut() {
        if a.id == account_id && a.cosy_uid.as_deref() != Some(uid) {
            a.cosy_uid = Some(uid.to_string());
            changed = true;
        }
    }
    changed && save_accounts(dir, &accounts).is_ok()
}

/// 账号 → COSY 签名身份（缺 `uid` 时联网补一次并落盘，与 [`set_cosy_uid`] 同一套惰性策略）。
///
/// 存在的理由是「**谁来签一个 COSY 请求，需要的三样东西都只在账号上**」：
/// 接管热路径（[`crate::proxy`] 转发时换号）和我们自己发起的查询
/// （[`crate::models`] 拉模型目录）都得先拿到 uid。这两处各写一份「有就用、没有就问一次
/// 再落盘」，就会在补齐条件、失败处理上慢慢漂 —— 而漂掉的后果是「一边能签、另一边永远签不出」。
///
/// `client` / `gateway` 由调用方给：取 uid 打的是**与业务请求同域的网关**
/// （避免跨域拿到两套账号 id），而两个调用方手上的客户端本来就不同（反代用透传客户端、
/// 目录用直连客户端）。
///
/// 返回 `None` = 拿不到 uid（没登录、token 过期、网络不通）。调用方**不能**在半签状态下
/// 发请求 —— 宁可原样透传或退到本地缓存。
pub async fn cosy_identity(
    dir: &Path,
    account: &Account,
    client: &reqwest::Client,
    gateway: &str,
) -> Option<crate::cosy::Identity> {
    let uid = match account.cosy_uid.clone().filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => crate::cosy::fetch_uid(client, gateway, &account.token).await?,
    };
    if account.cosy_uid.as_deref() != Some(uid.as_str()) {
        set_cosy_uid(dir, &account.id, &uid);
    }
    Some(crate::cosy::Identity {
        uid,
        name: account.name.clone(),
        email: String::new(),
        token: account.token.clone(),
    })
}

/// 读设置。
///
/// 兼容两种历史落盘格式，都收敛成扁平的 [`Settings`]：
/// - **「global + regions」嵌套表**（区域化年代的格式）：取国内版切片，
///   通知项以顶层 `global` 为准（通知是整机行为，`global` 是唯一可信来源）；
///   顶层 `takeover_region` 等已退役的键由 serde 直接忽略。
/// - **更早的扁平格式**：整体就是一份 [`Settings`]，直接读。
/// - 嵌套格式保存时会被**整体替换**成扁平格式（[`save_settings`] 只写新格式），
///   即迁移在第一次保存时自然完成。
pub fn load_settings(dir: &Path) -> Settings {
    let f = settings_file(dir);
    if !f.exists() {
        return Settings::default();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    // 先读成 Value 再分格式：判据只看结构（有没有 `regions` 键），不看内容 ——
    // 内容判据（比如「有没有 notify 键」）会随字段增删悄悄失效。
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) else {
        return Settings::default();
    };
    let mut out = if value.get("regions").is_some() {
        let mut slice = value
            .get("regions")
            .and_then(|r| r.get("cn"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if let (Some(obj), Some(g)) = (
            slice.as_object_mut(),
            value.get("global").and_then(|g| g.as_object()),
        ) {
            for (k, v) in g {
                obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(slice).unwrap_or_default()
    } else {
        serde_json::from_value(value).unwrap_or_default()
    };
    migrate_settings(&mut out);
    out
}

/// 把历史遗留的配置值挪到当前正确的取值。
///
/// **为什么必须做**：`#[serde(default)]` 只在字段**缺失**时才生效，而本机
/// （以及任何从模板改名过来的机器）的 `settings.json` 里已经写死了过时的值。
/// 不迁移的话，那台机器会一直按老规则跑，而新装的机器却是对的 ——
/// 同一份代码两种行为，且没有任何一处报错。
fn migrate_settings(s: &mut Settings) {
    // 1) 原先这里有一条「把 CodeBuddy 的上游域名改成 Qoder 的」迁移 —— 随
    //    `default_base_url` 字段本身一起删掉了（字段没了，迁移就无从谈起）。

    // 2) 定时签到时刻：只迁那一个确切的历史默认值。`09:07` 在 Qoder 上落在
    //    活动刷新（10:00 UTC+8）之前的夹缝里，每天都领不到（理由见
    //    `default_schedule_time`）。**用户自己改过的时刻一律不动** ——
    //    拿「不在窗口内」当条件去猜，会把凌晨故意错峰的用户也一起改掉。
    if s.schedule_time.trim() == LEGACY_SCHEDULE_TIME {
        s.schedule_time = default_schedule_time();
    }
}

/// 写设置（扁平格式整体落盘）。
pub fn save_settings(dir: &Path, settings: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    write_atomic(&settings_file(dir), &serde_json::to_string_pretty(settings)?)
}

/// 把账号/设置文件权限设为 0600（仅当前用户可读写）。
/// Windows 下忽略（ACL 模型不同，文件仍在用户专属 AppData 内）。
pub(crate) fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger;

    /// 造一条「账号 + 积分读数」的视图，账号字段取全量（便于断言扁平化没漏项）。
    fn a_view_with_credits() -> AccountView {
        AccountView {
            account: Account {
                id: "a1".into(),
                name: "主号".into(),
                phone: Some("190****9775".into()),
                email: Some("zh@example.com".into()),
                token: "t".into(),
                refresh_token: None,
                expires_at: None,
                rt_expires_at: None,
                created_at: "2026-09-16 09:00:00".into(),
                last: None,
                checked_today: Some(true),
                cosy_uid: None,
                invalidated_at: None,
                renew_blocked_until: None,
            },
            credits: Some(ledger::CreditFact {
                credits: Some(2957.83),
                at: "2026-09-16 21:20:58".into(),
                earliest_expiry_ms: Some(1_790_783_999_000),
                packages: vec![],
            }),
        }
    }

    /// 发往界面的视图**必须**带积分读数 —— 这条断言是那道门闩，钉住一个真实的坑：
    ///
    /// 上一版把 `credits` 直接挂在 [`Account`] 上并标了 `#[serde(skip)]`，本意是
    /// 「它不是账号的字段、不许落盘」；可 serde 的 `skip` 连**序列化**一起跳过，
    /// 而 Tauri 的 IPC 正是走 serde —— 于是投影永远到不了界面：账户页读不到读数，
    /// 只好回退成「上次签到时读到的余额」，到期时间一律为空。
    #[test]
    fn account_view_sends_credits_to_the_ui() {
        let json = serde_json::to_value(a_view_with_credits()).unwrap();
        // 三栏缺一不可：界面的「剩余积分 / 上次读到 / 过期时间」都指着它
        assert_eq!(json["credits"]["credits"], 2957.83, "{json}");
        assert_eq!(json["credits"]["at"], "2026-09-16 21:20:58", "{json}");
        assert_eq!(
            json["credits"]["earliest_expiry_ms"], 1_790_783_999_000i64,
            "{json}"
        );
        // 扁平：账号字段与 credits 同级，前端不必为一个实现细节改结构
        assert_eq!(json["id"], "a1", "{json}");
        assert_eq!(json["name"], "主号", "{json}");
        assert_eq!(json["email"], "zh@example.com", "{json}");
        assert!(json.get("account").is_none(), "不该多出一层包装：{json}");
    }

    /// 同一个硬币的另一面：落盘的账号里**不许**出现 credits（它是台账的投影）。
    #[test]
    fn account_itself_never_stores_credits() {
        let json = serde_json::to_value(a_view_with_credits().account).unwrap();
        assert!(json.get("credits").is_none(), "账号落盘不该带积分：{json}");
        assert_eq!(json["id"], "a1", "{json}");
    }

    #[test]
    fn normalizes_loose_time_input_to_hh_mm() {
        assert_eq!(normalize_time("9:7").as_deref(), Some("09:07"));
        assert_eq!(normalize_time(" 09:07 ").as_deref(), Some("09:07"));
        assert_eq!(normalize_time("23:59").as_deref(), Some("23:59"));
        assert_eq!(normalize_time("00:00").as_deref(), Some("00:00"));
    }

    #[test]
    fn rejects_invalid_time_input() {
        for bad in ["", "9", "9:", ":7", "24:00", "09:60", "09:07:00", "09am", "aa:bb", "-1:5"] {
            assert_eq!(normalize_time(bad), None, "{bad:?} 不应通过");
        }
    }

    /// 升级路径：老 `settings.json` 里那个 `default_base_url` 键**必须还能读出来**
    /// —— 字段已经删了，读到时直接忽略。
    ///
    /// 这条断言钉的是「删字段不会让整份配置报废」：serde 默认容忍未知键，
    /// 但哪天给 `Settings` 加上 `deny_unknown_fields`，老用户的配置就会**整份回退成默认值**
    /// （定时时刻、通知开关、风控开关全丢），而那看起来只会像「升级后设置全没了」。
    #[test]
    fn settings_with_the_removed_base_url_key_still_load() {
        let old = r#"{"default_base_url":"https://copilot.tencent.com","auto_checkin_on_start":true,"schedule_time":"08:30"}"#;
        let s: Settings = serde_json::from_str(old).expect("多出未知键不该让整份配置报废");
        assert!(s.auto_checkin_on_start);
        assert_eq!(s.schedule_time, "08:30", "同一份配置里的其它值必须保留");
    }

    /// 迁移：`09:07` 是 CodeBuddy 时代的默认定时签到时刻，在 Qoder 上**落在活动刷新
    /// （10:00 UTC+8）之前的夹缝里** —— 那一刻昨天那条已领、今天那条还没生成，
    /// 于是每天跑一次、每天报「没有可领的活动」，一次也领不到。
    ///
    /// 只迁这一个确切的历史默认值：用户自己改过的时刻一律不动
    /// （拿「不在窗口内」当条件猜，会把故意错峰的用户也一起改掉）。
    #[test]
    fn migrate_only_rewrites_the_legacy_schedule_time() {
        let mut s = Settings {
            schedule_time: LEGACY_SCHEDULE_TIME.into(),
            ..Settings::default()
        };
        migrate_settings(&mut s);
        assert_eq!(s.schedule_time, "10:00");

        for keep in ["09:30", "11:00", "00:05", "23:59"] {
            let mut s = Settings {
                schedule_time: keep.into(),
                ..Settings::default()
            };
            migrate_settings(&mut s);
            assert_eq!(s.schedule_time, keep, "用户自己设的时刻不该被改");
        }
    }

    #[test]
    fn old_settings_file_still_deserializes_with_new_fields_defaulted() {
        // 老版本只写了这两个字段，新增的定时/通知字段必须走默认值而不是让整份配置报废
        let old = r#"{"default_base_url":"https://x","auto_checkin_on_start":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert!(s.auto_checkin_on_start);
        assert!(!s.schedule_enabled);
        // 默认时刻对齐活动刷新点（10:00 UTC+8），不是 CodeBuddy 时代的 09:07
        assert_eq!(s.schedule_time, "10:00");
        assert!(!s.notify_enabled);
        assert!(s.notify_webhook.is_empty());
        // 定时推送默认开、手动推送默认关
        assert!(s.notify_on_schedule);
        assert!(!s.notify_on_manual);
        // 优先扣费账号：老配置没有 → 空（= 全部账号都可作为备选）
        assert!(s.billing_account_ids.is_empty());
        // 风控相关字段同样必须落到默认值，否则升级后老用户会突然丢掉整套打散策略
        assert_eq!(s.schedule_window_minutes, 90);
        assert!(s.stagger_checkin);
        assert_eq!(s.stagger_max_seconds, 45);
        assert!(s.shuffle_checkin_order);
        assert!(s.manual_stagger);
        assert_eq!(s.manual_stagger_max_seconds, 8);
        // 限流换号默认保持开启 = 与升级前的行为一致（关掉才是主动选的防御姿态）
        assert!(s.failover_on_rate_limit);
        // 积分简报：**默认关闭**（老配置里没有这个键 → 不能替用户把它打开）。
        // 它自己的推送开关仍是默认开，只在简报真的开着时才有意义
        assert!(!s.briefing_enabled);
        assert!(s.notify_on_briefing);
    }

    /// 升级路径：旧配置里的键名是 `report_enabled` / `notify_on_report`，
    /// 必须**继续生效** —— 两个方向都要，因为默认值是关闭：
    ///
    /// - `report_enabled: true`（上次开着）漏认 → 升级后简报被悄悄关掉，用户以为功能坏了；
    /// - `report_enabled: false`（上次主动关掉）漏认 → 被 default 之外的规则重新打开。
    #[test]
    fn legacy_report_field_names_still_take_effect() {
        // 上次开着：必须仍然开着，不能被新默认值（false）压掉
        let s: Settings = serde_json::from_str(r#"{"report_enabled":true}"#).unwrap();
        assert!(s.briefing_enabled, "旧键打开过的简报不该在升级后被关掉");

        let s: Settings =
            serde_json::from_str(r#"{"report_enabled":false,"notify_on_report":false}"#).unwrap();
        assert!(!s.briefing_enabled);
        assert!(!s.notify_on_briefing);

        // 写回去时用新键名，旧键不再出现（alias 只作用于读）
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("briefing_enabled"));
        assert!(!json.contains("\"report_enabled\""));
    }

    #[test]
    fn legacy_preferred_account_field_is_ignored() {
        // 旧版单选字段不再使用：读到也不影响新逻辑（多选列表仍为空 = 全部可用）
        let s: Settings = serde_json::from_str(r#"{"preferred_account_id":"abc"}"#).unwrap();
        assert!(s.billing_account_ids.is_empty());
    }

    // ── settings.json 的升级路径（嵌套表 → 扁平）────────────────────────────

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qoder-settings-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_raw(dir: &Path) -> serde_json::Value {
        let s = fs::read_to_string(settings_file(dir)).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    /// 升级路径：区域化年代落盘的嵌套表（`global` + `regions`）必须被读成国内版
    /// 的扁平设置 —— 切片值原样、通知项以顶层 `global` 为准（通知是整机行为）。
    #[test]
    fn nested_legacy_file_reads_the_cn_slice_with_global_notify() {
        let dir = temp_dir("nested");
        fs::write(
            settings_file(&dir),
            r#"{"global":{"notify_enabled":true,"notify_webhook":"https://hook"},"takeover_region":"cn","regions":{"cn":{"schedule_time":"08:30","proxy_enabled":true}}}"#,
        )
        .unwrap();

        let s = load_settings(&dir);
        assert_eq!(s.schedule_time, "08:30", "国内版切片的值原样");
        assert!(s.proxy_enabled);
        assert!(s.notify_enabled, "通知以顶层 global 为准");
        assert_eq!(s.notify_webhook, "https://hook");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 保存只写**扁平**格式：第一次保存就把嵌套老文件整体替换掉，
    /// 退役键（`takeover_region` / `regions` / 顶层 `global`）不再出现。
    #[test]
    fn save_writes_the_flat_format_and_retires_the_nested_one() {
        let dir = temp_dir("flat");
        fs::write(
            settings_file(&dir),
            r#"{"global":{"notify_enabled":true},"takeover_region":"cn","regions":{"cn":{"schedule_time":"08:30"}}}"#,
        )
        .unwrap();

        let s = load_settings(&dir);
        save_settings(&dir, &s).unwrap();

        let raw = read_raw(&dir);
        assert_eq!(raw["schedule_time"], "08:30", "{raw}");
        assert!(raw["notify_enabled"].as_bool().unwrap(), "{raw}");
        assert!(raw.get("regions").is_none(), "嵌套表必须整体退役：{raw}");
        assert!(raw.get("takeover_region").is_none(), "{raw}");
        assert!(raw.get("global").is_none(), "{raw}");
        // 读回一致
        let s2 = load_settings(&dir);
        assert_eq!(s2.schedule_time, "08:30");
        assert!(s2.notify_enabled);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 嵌套表里没有 `cn` 切片（老数据 + 从没配过国内版）不能炸：
    /// 切片按默认值兜底，但顶层 `global` 的通知项仍要生效。
    #[test]
    fn missing_cn_slice_falls_back_to_defaults_but_keeps_globals() {
        let dir = temp_dir("missing");
        fs::write(
            settings_file(&dir),
            r#"{"global":{"notify_webhook":"https://g"},"takeover_region":"cn","regions":{"global":{"schedule_time":"08:30"}}}"#,
        )
        .unwrap();
        let s = load_settings(&dir);
        assert_eq!(s.schedule_time, "10:00", "缺切片 = 国内版从没配过，用默认值");
        assert_eq!(s.notify_webhook, "https://g");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 遗留值迁移（`09:07` → `10:00`）对嵌套老文件同样要跑 —— 迁移发生在
    /// 格式收敛之后，两条升级路径（嵌套 / 扁平）最后过的是同一道闸。
    #[test]
    fn legacy_schedule_time_is_migrated_on_the_nested_file() {
        let dir = temp_dir("migrate");
        fs::write(
            settings_file(&dir),
            r#"{"takeover_region":"cn","regions":{"cn":{"schedule_time":"09:07"}}}"#,
        )
        .unwrap();
        assert_eq!(load_settings(&dir).schedule_time, "10:00");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── 国际版账号的清退（加载时丢弃，不迁移）────────────────────────────

    /// 国际版支持已整体移除：`region:"global"` 的条目在**读**的时候丢弃；
    /// 没有 `region` 字段的条目（新格式保存的、或更老版本的）视为国内版照常读入
    /// —— 不这样「清洗后保存」的文件第二轮就读成空了。
    #[test]
    fn global_accounts_are_dropped_at_load_and_regionless_rows_survive() {
        let dir = temp_dir("drop-global");
        fs::write(
            accounts_file(&dir),
            r#"[
                {"id":"g1","name":"国际版","phone":null,"token":"tg","region":"global","refresh_token":"rt-tg"},
                {"id":"c1","name":"国内版","phone":"191","token":"tc","region":"cn","refresh_token":"rt-tc"},
                {"id":"old","name":"更老的条目","phone":null,"token":"to","refresh_token":"rt-to"}
            ]"#,
        )
        .unwrap();

        let back = load_accounts(&dir);
        let ids: Vec<&str> = back.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "old"], "{back:?}");

        // 往返：清洗后保存（新格式没有 region 字段）→ 再读不丢
        save_accounts(&dir, &back).unwrap();
        let again = load_accounts(&dir);
        assert_eq!(again.len(), 2, "保存后的文件必须原样读回：{again:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    // ── 重复条目自愈（2026-09-19 的「凭空多一个账号」）────────────────────

    fn mk(id: &str, name: &str, phone: Option<&str>, token: &str) -> Account {
        Account {
            id: id.into(),
            name: name.into(),
            phone: phone.map(str::to_string),
            email: None,
            token: token.into(),
            refresh_token: Some(format!("rt-{token}")),
            expires_at: None,
            rt_expires_at: None,
            created_at: "2026-09-19 13:16:24".into(),
            last: None,
            checked_today: None,
            cosy_uid: None,
            invalidated_at: None,
            renew_blocked_until: None,
        }
    }

    /// 幽灵条目的原型：池里的旧 key 认不出本机账号时 `account_from_item` 收养出来的那条
    /// —— 同一份 token、没有手机号、从没签过到。
    #[test]
    fn a_ghost_row_with_the_same_token_is_absorbed() {
        let mut real = mk("real", "nick0494015252", Some("19174256652"), "dt-abc");
        real.last = Some(CheckinRecord {
            success: true,
            already: false,
            inactive: false,
            message: "已领取 100 Credits".into(),
            credit: Some(100.0),
            balance: Some(400.0),
            campaign_key: Some("act-20260918-628".into()),
            expires_at: None,
            at: "2026-09-19 13:16:32".into(),
        });
        real.checked_today = Some(true);
        let ghost = mk("ghost", "nick0494015252", None, "dt-abc");

        let out = dedupe_by_credential(vec![real.clone(), ghost.clone()]);
        assert_eq!(out.len(), 1, "同一份凭证只能留一条记录");
        assert_eq!(out[0].id, "real", "id 必须留在原账号上（积分台账按 id 挂账）");
        assert_eq!(out[0].phone.as_deref(), Some("19174256652"));

        // 顺序反过来（幽灵排在前）结论必须一样 —— 判据是「谁更全」，不是「谁在前面」
        let out = dedupe_by_credential(vec![ghost, real]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "real");
    }

    /// 「谁更全」里的身份标识分要看**邮箱**：手机号为空时它是唯一的身份标识。
    ///
    /// 幽灵条目从池子里收养出来时没有邮箱（`PoolItem` 里就没有这个字段），
    /// 而真实账号有 —— 若邮箱不计分，两条会打平、按「谁在前」决胜，
    /// 幽灵留在原地就把台账挂账的 id 换掉了。
    #[test]
    fn dedupe_prefers_the_row_with_the_email() {
        let mut real = mk("real", "nick", None, "dt-abc");
        real.email = Some("user@example.com".into());
        real.checked_today = Some(true);
        let ghost = mk("ghost", "nick", None, "dt-abc");

        // 幽灵排在前：没有邮箱加分时它会留下
        let out = dedupe_by_credential(vec![ghost.clone(), real.clone()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "real", "有邮箱的那条才是真账号");

        let out = dedupe_by_credential(vec![real, ghost]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "real");
        assert_eq!(out[0].email.as_deref(), Some("user@example.com"));
    }

    /// 被并掉那条的独有字段要补进保留的那条，不能因为「它不是主角」就丢。
    /// 注意方向：**只补空，绝不覆盖** —— 保留方已有的值一律不动。
    #[test]
    fn absorption_keeps_the_loser_s_unique_fields() {
        let mut keeper = mk("keep", "", Some("13800000000"), "dt-t");
        // 造出「空」：保留方没有续签信息，被并方那两份正是缺口
        keeper.refresh_token = None;
        keeper.last = Some(CheckinRecord {
            success: true,
            already: false,
            inactive: false,
            message: "ok".into(),
            credit: None,
            balance: None,
            campaign_key: None,
            expires_at: None,
            at: "2026-09-19 13:16:32".into(),
        });
        let mut loser = mk("lose", "昵称", None, "dt-t");
        loser.refresh_token = Some("rt-new".into());
        loser.rt_expires_at = Some(1_790_898_980_000);
        loser.email = Some("lose@example.com".into());

        // 昵称为空的那条反而更完整（有签到结果）→ 它留下，昵称/续签信息补进来
        let out = dedupe_by_credential(vec![loser, keeper]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "昵称", "空昵称应从被并方补上");
        assert_eq!(out[0].refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(out[0].rt_expires_at, Some(1_790_898_980_000));
        assert_eq!(
            out[0].email.as_deref(),
            Some("lose@example.com"),
            "被并方的邮箱也要补进来"
        );

        // 反过来：保留方有值，被并方也有值 → 保留方的值不许被覆盖
        let mut a = mk("a", "n", Some("138"), "dt-t");
        a.last = Some(CheckinRecord {
            success: true,
            already: false,
            inactive: false,
            message: "ok".into(),
            credit: None,
            balance: None,
            campaign_key: None,
            expires_at: None,
            at: "2026-09-19 13:16:32".into(),
        });
        let mut b = mk("b", "n", Some("139"), "dt-t");
        b.refresh_token = Some("rt-别的".into());
        let out = dedupe_by_credential(vec![a, b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].phone.as_deref(), Some("138"), "已填的字段不被覆盖");
        assert_eq!(out[0].refresh_token.as_deref(), Some("rt-dt-t"));
    }

    /// 不同凭证（两条 token）**绝不能**被合成一条；没 token 的也不参与。
    #[test]
    fn distinct_credentials_and_tokenless_rows_are_left_alone() {
        let a = mk("a", "n1", Some("138"), "dt-one");
        let b = mk("b", "n2", Some("138"), "dt-other");
        assert_eq!(dedupe_by_credential(vec![a, b]).len(), 2, "同手机号的不同凭证是两个账号");

        let mut empty1 = mk("e1", "空1", None, "");
        empty1.refresh_token = None;
        let mut empty2 = mk("e2", "空2", None, " ");
        empty2.refresh_token = None;
        assert_eq!(dedupe_by_credential(vec![empty1, empty2]).len(), 2, "没有凭证的不参与合并");
    }

    /// 幂等：收敛过的列表再收敛一次，结果与第一次完全相同。
    #[test]
    fn dedupe_is_idempotent() {
        let real = mk("real", "n", Some("191"), "dt-t");
        let ghost = mk("ghost", "n", None, "dt-t");
        let once = dedupe_by_credential(vec![real, ghost]);
        let twice = dedupe_by_credential(once.clone());
        assert_eq!(once.len(), twice.len());
        assert_eq!(once[0].id, twice[0].id);
    }

    /// [`write_atomic`] 的三条底线：内容完整、原地替换、**失败要响**。
    ///
    /// 对应修 4 的起因 —— 凭证文件里 refresh token 是一次性的，
    /// 「写了个半截」和「写失败但没人知道」叠在一起就等于这条链悄悄死掉。
    #[test]
    fn write_atomic_replaces_in_place_and_fails_loudly() {
        let dir = temp_dir("write_atomic");
        let target = dir.join("credentials.json");

        write_atomic(&target, r#"{"refresh_token":"rt-1"}"#).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"refresh_token":"rt-1"}"#
        );
        // 原地替换：临时文件必须已经没了，否则下次同名写入会撞上一个残留
        assert!(!dir.join("credentials.json.tmp").exists(), "临时文件不得残留");

        // 覆盖写：不是追加、不是新 inode 意义上的「留一份旧的」
        write_atomic(&target, r#"{"refresh_token":"rt-2"}"#).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"refresh_token":"rt-2"}"#
        );

        // 父目录不存在 → 必须返回 Err，而不是 `let _ =` 那样吞掉
        let bad = dir.join("nope").join("credentials.json");
        assert!(write_atomic(&bad, "{}").is_err(), "写不进去必须报错");
    }

    /// 凭证落盘的两面：**读写往返无损** + **写不进就返回 Err**。
    ///
    /// 后半截才是修 4 的重点 —— refresh token 是一次性的，调用点若用 `let _ =` 吞掉
    /// 失败，磁盘上留着的就还是那枚已经作废的旧票，几天后表现为「要重新登录」。
    /// 这里用「数据目录指向一个普通文件」制造失败（真实世界是磁盘满 / 权限不对，
    /// 但那两者在测试里不可靠），判据一样：`save_accounts` 必须把错误冒出来。
    #[test]
    fn save_accounts_roundtrips_and_propagates_a_failed_write() {
        let dir = temp_dir("save_accounts");
        let a = mk("a", "主号", Some("138"), "dt");
        save_accounts(&dir, &[a.clone()]).expect("落盘应该成功");

        let back = load_accounts(&dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].token, "dt");
        assert_eq!(back[0].refresh_token.as_deref(), Some("rt-dt"));

        let blocker = dir.join("not-a-dir");
        fs::write(&blocker, "").unwrap();
        assert!(
            save_accounts(&blocker, &[a]).is_err(),
            "写不进去必须返回 Err，而不是静默成功"
        );
    }
}
