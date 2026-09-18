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
    //      而转发目标是模型网关（`api2-v2.qoder.sh`）。一个既没人读、内容又不对的字段，
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

/// 全局设置
///
/// 新增字段一律带 `#[serde(default)]`，这样老版本写下的 settings.json
/// 仍能反序列化（不会整个文件被判为非法而回退成默认值）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default = "default_base_url")]
    pub default_base_url: String,
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
    /// 通知总开关（关掉后定时与手动都不推送）
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
    #[serde(default = "default_true", alias = "notify_on_report")]
    pub notify_on_briefing: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_base_url: default_base_url(),
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

fn default_base_url() -> String {
    // 模型网关的上游：Qoder CLI 的 `Btn()` 拼的是
    // `https://api2-v2.qoder.sh/model/v1/chat/completions`。
    // 模板里这里是 CodeBuddy 的 `copilot.tencent.com`，已作废
    // （见 `basedata/20260918_Qoder接管机制逆向.md` 第 5 节）。
    crate::qoder_api::INFER_BASE.to_string()
}

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
    serde_json::from_str(&s).unwrap_or_default()
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let target = accounts_file(dir);
    let tmp = dir.join("accounts.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(accounts)?)?;
    fs::rename(&tmp, &target)?;
    set_private_permissions(&target);
    Ok(())
}

pub fn load_settings(dir: &Path) -> Settings {
    let f = settings_file(dir);
    if !f.exists() {
        return Settings::default();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    let mut settings: Settings = serde_json::from_str(&s).unwrap_or_default();
    migrate_settings(&mut settings);
    settings
}

/// 把历史遗留的配置值挪到当前正确的取值。
///
/// **为什么必须做**：`#[serde(default)]` 只在字段**缺失**时才生效，而本机
/// （以及任何从模板改名过来的机器）的 `settings.json` 里已经写死了过时的值。
/// 不迁移的话，那台机器会一直按老规则跑，而新装的机器却是对的 ——
/// 同一份代码两种行为，且没有任何一处报错。
fn migrate_settings(s: &mut Settings) {
    // 1) 上游域名：从模板改名过来的机器里写死了 CodeBuddy 的域名。
    let stale = s.default_base_url.trim().is_empty()
        || s.default_base_url.contains("copilot.tencent.com")
        || s.default_base_url.contains("codebuddy.");
    if stale {
        s.default_base_url = default_base_url();
    }

    // 2) 定时签到时刻：只迁那一个确切的历史默认值。`09:07` 在 Qoder 上落在
    //    活动刷新（10:00 UTC+8）之前的夹缝里，每天都领不到（理由见
    //    `default_schedule_time`）。**用户自己改过的时刻一律不动** ——
    //    拿「不在窗口内」当条件去猜，会把凌晨故意错峰的用户也一起改掉。
    if s.schedule_time.trim() == LEGACY_SCHEDULE_TIME {
        s.schedule_time = default_schedule_time();
    }
}

pub fn save_settings(dir: &Path, settings: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let target = settings_file(dir);
    let tmp = dir.join("settings.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
    fs::rename(&tmp, &target)?;
    set_private_permissions(&target);
    Ok(())
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
                token: "t".into(),
                refresh_token: None,
                expires_at: None,
                rt_expires_at: None,
                created_at: "2026-09-16 09:00:00".into(),
                last: None,
                checked_today: Some(true),
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

    /// 迁移：从模板改名过来的机器里写死了 CodeBuddy 的上游域名 ——
    /// `#[serde(default)]` 对「键在、值过时」无能为力，必须显式迁。
    #[test]
    fn migrate_rewrites_the_stale_codebuddy_base_url() {
        let mut s = Settings {
            default_base_url: "https://copilot.tencent.com".into(),
            ..Settings::default()
        };
        migrate_settings(&mut s);
        assert_eq!(s.default_base_url, crate::qoder_api::INFER_BASE);

        // 用户自己填的地址不能动
        let mut s = Settings {
            default_base_url: "https://my-gateway.example/v1".into(),
            ..Settings::default()
        };
        migrate_settings(&mut s);
        assert_eq!(s.default_base_url, "https://my-gateway.example/v1");
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
        assert_eq!(s.default_base_url, "https://x");
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
}
