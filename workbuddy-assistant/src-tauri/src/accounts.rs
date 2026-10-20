use crate::ledger;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 单次签到结果（用于前端展示与落库）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CheckinRecord {
    pub success: bool,
    pub already: bool,
    pub inactive: bool,
    pub message: String,
    /// 本次获得积分（today_credit）
    pub credit: Option<i64>,
    /// **本次签到那一刻**读到的账号剩余积分（`get-user-resource` 汇总，可能是小数；
    /// 取不到为 None）。
    ///
    /// ⚠️ 它**不是**「当前余额」—— 当前余额只存在于积分台账里（见 [`Account::credits`]），
    /// 因为只有台账同时知道「读到了多少」和「什么时候读到的」。
    ///
    /// 两个出口：签到时间线（`logs::push` 把签到结果连同这条消息写进统一日志流）；
    /// 账户列表在台账还没有该账号读数时兜底（刚升级上来、首次采样还没跑）。
    /// **写点只有签到本身** —— 刷新不再回填它，否则会给这条记录的时刻配上不属于它的读数。
    #[serde(default)]
    pub balance: Option<f64>,
    pub streak: Option<i64>,
    pub host: Option<String>,
    pub at: String,
    pub code: Option<i64>,
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
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last: Option<CheckinRecord>,
    /// 服务端「今日是否已签到」的真实状态（只读查询，持久化）。
    /// None = 尚未查询/查询失败（前端按「未知 / 待签到」处理，
    /// 绝不再把昨天的本地缓存或一次本地报错当成确定状态）。
    #[serde(default)]
    pub checked_today: Option<bool>,
    /// **活性判定**：额度接口对这张 token 返回 401 的时刻（毫秒，见
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
    /// 智能接管总开关（127.0.0.1 反代 + 把 WorkBuddy 端点指向它，按积分过期时间优先路由）。
    /// 这是 WorkBuddy 专用通道：只监听本机、无鉴权 Key、无独立「仅反代」模式。
    #[serde(default)]
    pub proxy_enabled: bool,
    /// 反代监听端口（默认 8787，避开 Clash 的 7897）
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
    "https://copilot.tencent.com".to_string()
}

/// 默认 09:07 —— 与参考脚本 `workbuddy_checkin.py` 的 launchd 定时保持一致
fn default_schedule_time() -> String {
    "09:07".to_string()
}

fn default_true() -> bool {
    true
}

/// 反代默认端口：8787（避开 Clash 的 7897 等常用端口）
fn default_proxy_port() -> u16 {
    8787
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

/// **原子写**：临时文件 + `sync_all` + `rename`，并且**把错误原样还回去**。
///
/// 为什么单独一条路（修 4）：这条路上落盘的是凭证 —— `accounts.json` 里的
/// refresh token 是**一次性**的，换过票之后旧票在服务端就作废了。此时半截落盘等于
/// 「旧票已死、新票没留下」，这条链只能重新登录；而 `let _ = fs::write(..)`
/// 会把这件事变成几周后才被发现的沉默事故（症状是「某个账号突然要重新登录」，
/// 原因藏在几周前的一次写盘失败里）。
///
/// 两点不能反过来：
/// 1. 先 `sync_all` 再 `rename` —— 顺序反了只是「名义上原子」，崩溃时仍可能留下
///    一个名字已经换过去、内容却还空着的文件。
/// 2. 权限在**创建临时文件时**就收到 0600，而不是 rename 之后再补 —— 后者中间有个
///    窗口：新凭证已经落盘、却还是同机其它用户可读的。
pub(crate) fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other(format!("路径没有文件名：{}", path.display())))?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let opened = || -> std::io::Result<fs::File> {
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
    // 失败时把半截临时文件扫掉：它带着一份新凭证的副本，留在那儿既脏又是多余的暴露面。
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    write_atomic(&accounts_file(dir), &serde_json::to_string_pretty(accounts)?)
}

pub fn load_settings(dir: &Path) -> Settings {
    let f = settings_file(dir);
    if !f.exists() {
        return Settings::default();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    let mut settings: Settings = serde_json::from_str(&s).unwrap_or_default();
    // 国际版已不再支持：老配置里的 *.ai 默认域一律退回内置国内域
    if crate::oauth::is_foreign_host(&settings.default_base_url) {
        settings.default_base_url = default_base_url();
    }
    settings
}

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
                token: "t".into(),
                refresh_token: None,
                expires_at: None,
                rt_expires_at: None,
                base_url: None,
                created_at: "2026-09-16 09:00:00".into(),
                last: None,
                checked_today: Some(true),
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

    #[test]
    fn old_settings_file_still_deserializes_with_new_fields_defaulted() {
        // 老版本只写了这两个字段，新增的定时/通知字段必须走默认值而不是让整份配置报废
        let old = r#"{"default_base_url":"https://x","auto_checkin_on_start":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.default_base_url, "https://x");
        assert!(s.auto_checkin_on_start);
        assert!(!s.schedule_enabled);
        assert_eq!(s.schedule_time, "09:07");
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// [`write_atomic`] 的四条底线：内容完整、原地替换、权限收窄、**失败要响并且扫干净**。
    ///
    /// 对应修 4 的起因 —— refresh token 是**一次性**的，「写了个半截」和「写失败但没人
    /// 知道」叠在一起，就等于这条链悄悄死掉，几周后才以「要重新登录」的形式浮出来。
    #[test]
    fn write_atomic_replaces_in_place_and_fails_loudly() {
        let dir = temp_dir("write_atomic");
        let target = dir.join("credentials.json");

        write_atomic(&target, r#"{"refresh_token":"rt-1"}"#).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"refresh_token":"rt-1"}"#
        );
        assert!(!dir.join("credentials.json.tmp").exists(), "临时文件不得残留");

        write_atomic(&target, r#"{"refresh_token":"rt-2"}"#).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"refresh_token":"rt-2"}"#,
            "覆盖写要生效"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "凭证文件必须收窄到 0600，且不能有一瞬间是全局可读的");
        }

        // 「临时文件写得成、换名失败」的场景：把目标占成目录，rename 会失败（EISDIR）。
        // 用它验证失败后那份带新凭证的半截文件被扫掉了。
        let blocker = dir.join("blocked_dir");
        fs::create_dir_all(&blocker).unwrap();
        assert!(write_atomic(&blocker, "{}").is_err(), "目标是目录时必须报错");
        assert!(
            !dir.join("blocked_dir.tmp").exists(),
            "换名失败也要扫掉那份半截临时文件"
        );

        // 父目录不存在同样要报错（真实世界对应磁盘满 / 权限不对，判据一样）
        let bad = dir.join("nope").join("credentials.json");
        assert!(write_atomic(&bad, "{}").is_err(), "写不进去必须返回 Err");
    }

    /// `save_accounts` 必须把写失败**冒给调用方**，而不是静默返回 Ok。
    ///
    /// 反代那条续签路径就靠这个返回值决定是否记日志：token 已经轮换，磁盘上却还是旧票，
    /// 下次重启读回的就是那枚已作废的 refresh token。
    #[test]
    fn save_accounts_roundtrips_and_propagates_a_failed_write() {
        let dir = temp_dir("save_accounts");
        let a = Account {
            id: "a1".into(),
            name: "主号".into(),
            phone: Some("190****9775".into()),
            token: "dt-new".into(),
            refresh_token: Some("rt-new".into()),
            expires_at: Some(2_000),
            rt_expires_at: Some(3_000),
            base_url: None,
            created_at: "2026-09-16 09:00:00".into(),
            last: None,
            checked_today: None,
            invalidated_at: None,
            renew_blocked_until: None,
        };
        save_accounts(&dir, &[a.clone()]).expect("落盘应该成功");
        let back = load_accounts(&dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].refresh_token.as_deref(), Some("rt-new"));

        // 数据目录位置放一个**普通文件**：create_dir_all 必然失败 → Err 要冒出来
        let blocker = dir.join("not-a-dir");
        fs::write(&blocker, "").unwrap();
        assert!(
            save_accounts(&blocker, &[a]).is_err(),
            "写不进去必须返回 Err，而不是静默成功"
        );
    }
}
