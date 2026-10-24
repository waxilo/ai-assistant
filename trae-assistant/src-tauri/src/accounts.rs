//! 账号与设置的持久化层：一个 data 目录下两份 JSON。
//!
//! - `accounts.json`：已导入的 Trae CN 账号列表
//! - `settings.json`：应用设置（签到开关/时刻、智能接管开关/端口、账号白名单、webhook）

use crate::trae_auth::TraeLocalAccount;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    /// 本地记录 id，**由后端分配**（导入时允许缺省，前端不该自己造）。
    /// 界面的账号行、`checkin_one(id)` / `remove_account(id)` / 状态表全靠它匹配。
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub phone: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    pub token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
    /// 设备标识（签到 API 的隐藏必填头 `X-Device-Id` / `X-Machine-Id` 来源）。
    /// 来自本机 storage.json 的 telemetry，或浏览器登录时绑定的设备；缺失时由 user_id/id 派生稳定值。
    ///
    /// ⚠️ 它与「签名头实际用哪个值」解耦了：浏览器登录落库的是**授权时的随机 uuid**，
    /// 服务端不认（`claim` → 9074）。签到头请走 [`crate::checkin::device_id`]。
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub created_at: String,
    /// 积分快照（供「智能接管」选号：先扣谁的额度）。
    /// `#[serde(default)]` 让旧 `accounts.json`（没有该字段）能正常反序列化。
    #[serde(default)]
    pub credit_snapshot: Option<CreditSnapshot>,
    /// **活性判定**：官方接口对这张 token 返回 401 的时刻（毫秒，见 `checkin::post_ent_usage`）。
    ///
    /// 这是唯一可信的「票已被吊销」证据 —— exp 只是票的自我声明，服务端可以提前吊销一张
    /// exp 还在未来的票（链在别处被轮换 / 桌面端自续），那正是 2026-10-08「本地死票锁死、
    /// 云端活票永远采纳不进来」事故的根源。`broker::adopt` 的失效抢救规则用它：
    /// 本机已判死 + 池里那份与本地不同 → 无条件换血，不再比 exp。续签/采纳成功后清除。
    #[serde(default)]
    pub invalidated_at: Option<i64>,
    /// 续签冷却的截止时刻（毫秒）：`needs_login`（注定失败）冷 6 小时，临时失败冷一个巡检周期。
    ///
    /// **持久化**的失败退避 —— 旧实现是内存 HashMap，应用一重启就失忆，死链每轮巡检
    /// 都去打一个必然被拒的接口。续签成功 / 换到新票时清除。
    #[serde(default)]
    pub renew_blocked_until: Option<i64>,
}

/// 账号**已有积分**快照。
/// 数据源是 `POST /trae/api/v2/pay/ide_user_ent_usage`（IDE 版 entitlement 用量），
/// 由 `checkin::parse_ent_usage` 按官方 `hHe()` 汇总出「剩余可用积分」与「最快到期时间」。
/// ⚠️ 这不是 `checkin_credits/status` 里的 `credits`——那个是**签到奖励**，两回事。
///
/// 「智能接管」按「**到期最早优先 → 无到期数据靠后 → 同日则待过期积分少者优先**」
/// 挑账号（见 `proxy::pick_index`）：先消耗快到期的额度，避免浪费。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CreditSnapshot {
    /// 剩余可用积分；未知为 `None`
    pub credits: Option<i64>,
    /// 是否不限量（entitlement 里存在 `credits_limit = -1` 的包）；此时 `credits` 为 `None`
    #[serde(default)]
    pub unlimited: bool,
    /// 「还有余量的额度包」里最早的到期时间（毫秒时间戳）；未知为 `None`
    pub earliest_expiry_ms: Option<i64>,
    /// 抓取时刻（本地 `YYYY-MM-DD HH:MM:SS`），用于判断快照是否过期
    pub fetched_at: String,
    /// 逐额度包明细（名称 / 剩余 / 到期），供前端资源包列表；旧档缺该字段用空数组兜底
    #[serde(default)]
    pub packages: Vec<CreditPackage>,
}

/// 一个额度包的展示快照：名称 + 剩余积分 + 到期时间。
///
/// 给前端「资源包列表」弹窗用。`remaining`（剩余 = size − used）与 `expiry_ms`
/// 都应在解析端折算好，这里只做纯展示结构，不参与计较（避免这里再引入换算口径）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CreditPackage {
    pub name: String,
    #[serde(default)]
    pub remaining: i64,
    #[serde(default)]
    pub expiry_ms: i64,
}

impl CreditSnapshot {
    pub fn now(
        credits: Option<i64>,
        unlimited: bool,
        earliest_expiry_ms: Option<i64>,
        packages: Vec<CreditPackage>,
    ) -> CreditSnapshot {
        CreditSnapshot {
            credits,
            unlimited,
            earliest_expiry_ms,
            fetched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            packages,
        }
    }
}

/// 规范化一条账号记录（**幂等**）：补齐 `id` / `user_id` / `created_at`。
///
/// 这治的是两个已发生的真实问题：
/// 1. **空 `id`**：早期「浏览器登录」在前端把 `id` 写成空串（后端照单全收）。界面的
///    `checkinOne(a.id)` / `removeAccount(a.id)` / `statuses[a.id]` 全靠 id 匹配 ——
///    只有一个空 id 的号时侥幸能用，再加一个就永远命中第一个（签到、删除、状态全串号）。
/// 2. **空 `user_id`**：`GetUserInfo` 对授权码换来的 token 直接 401，于是 uid 落空；
///    而 JWT 载荷里本来就有（见 [`crate::token`]）。uid 也是签到头 `x-device-id` 的首选值。
///
/// 返回「是否发生改动」，调用方据此决定要不要回写。
pub fn normalize(a: &mut Account) -> bool {
    let mut changed = false;
    if a.id.trim().is_empty() {
        a.id = uuid::Uuid::new_v4().to_string();
        changed = true;
    }
    if a.user_id.as_deref().map(str::trim).unwrap_or("").is_empty() {
        if let Some(uid) = crate::token::user_id(&a.token) {
            a.user_id = Some(uid);
            changed = true;
        }
    }
    if a.created_at.trim().is_empty() {
        a.created_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        changed = true;
    }
    changed
}

/// 判定候选账号是否已存在于列表：手机号优先（两端都有且相等即视为同一人），
/// 否则（任一缺少手机号）退化为按 token 比对。用于「按手机号去重、已存在则不重复添加」。
pub fn contains_equivalent(list: &[Account], cand: &Account) -> bool {
    list.iter().any(|x| match (&x.phone, &cand.phone) {
        (Some(p1), Some(p2)) => p1 == p2,
        _ => x.token == cand.token,
    })
}

impl From<TraeLocalAccount> for Account {
    fn from(a: TraeLocalAccount) -> Account {
        Account {
            id: uuid::Uuid::new_v4().to_string(),
            name: a
                .nickname
                .clone()
                .or_else(|| a.phone.clone())
                .unwrap_or_else(|| "未命名账号".into()),
            phone: a.phone,
            user_id: a.user_id,
            token: a.token,
            refresh_token: a.refresh_token,
            expires_at: a.expires_at,
            refresh_expires_at: a.refresh_expires_at,
            device_id: a.device_id,
            machine_id: a.machine_id,
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            credit_snapshot: None,
            invalidated_at: None,
            renew_blocked_until: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub checkin_enabled: bool,
    /// 签到时刻（24h，如 "10:00"）
    pub checkin_time: String,
    /// 智能接管开关：启动本地反代 + 写入 Trae CN 端点覆盖（见 `endpoint.rs` / `proxy.rs`）。
    ///
    /// ## 改道方式只有一种（2026-09-15 收敛）
    ///
    /// 改写 Trae CN 安装目录里的 `product.json`，端点指向 `http://127.0.0.1:{port}`
    /// （[`crate::endpoint::base_url`] 的免证书形态）。
    ///
    /// 曾经还有两条**已整体移除**的路（见 `docs/免证书接管方案.md`）：
    /// - 「经系统代理接管」——写 Trae CN 的 `User/settings.json`，靠 TLS 中间人解密；
    /// - 端点走 `https` 的「证书模式」——本地反代自讲 TLS。
    ///
    /// 两者都必须把自签 CA 装进系统信任库（动的是系统信任设置），而免证书这条不需要，
    /// 代价只是给 Trae CN 打一个可逐字节还原的闸门补丁。既然免证书已验证跑通，
    /// 那两条路就是纯粹的额外风险面，于是不再保留 —— 相关代码（`tls.rs` / `tunnel.rs`）
    /// 也已删除，别为了「留一手」再把开关加回来。
    pub takeover_enabled: bool,
    /// 本机反代监听端口
    pub takeover_port: u16,
    /// 参与接管的账号白名单（空 = 全部）
    pub billing_account_ids: Vec<String>,
    /// 告警 webhook（可选）
    pub webhook_url: String,
    /// **积分简报开关**：开启后后台每小时采样一次（`ide_user_ent_usage`），
    /// 并把已经走完的小时固化成「时条目」（见 `ledger` / `briefing`）。
    /// 默认关：开启那一刻才对齐基线，用户不会在拨开关之前就有一堆历史账。
    pub briefing_enabled: bool,
    /// **已成功推送过简报的日期**（`YYYY-MM-DD`）—— 简报去重的落盘凭据。
    ///
    /// 为什么必须落盘：去重原先只在内存（`scheduler::BriefingState`），而调度线程
    /// 每次启动都带 `at_startup` 补推 —— 于是**每重启一次助手就把最近一天的简报重推一遍**
    /// （2026-09-20 用户报「更新重启 trae 助手，又触发了积分简报推送」）。
    /// 只在推送**成功**后写入：失败不落盘，下次启动还能补上那一天。
    ///
    /// ⚠️ 前端 `Settings` 类型必须带上这个字段：界面走的是**整体覆盖保存**
    /// （`save_settings`），前端不认识它就会被抹掉。
    pub last_briefing_push_date: Option<String>,
    /// 静默自动更新：每天到 `auto_update_time` 后自动检查、下载并安装，装完应用自行重启。
    ///
    /// **默认开启**。关掉只停掉后台这条路径，设置页的「检查更新」按钮照常可用。
    ///
    /// ⚠️ 本结构的其它字段都**不带** `serde(default)`（老配置缺任一键会整份回退默认），
    /// 新增字段必须自带 `default`，否则升级上来的人一次读设置就把签到时间/接管开关全丢了。
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
        Settings {
            checkin_enabled: true,
            checkin_time: "10:00".into(),
            takeover_enabled: false,
            takeover_port: 8788,
            billing_account_ids: Vec::new(),
            webhook_url: String::new(),
            briefing_enabled: false,
            last_briefing_push_date: None,
            // 静默更新默认开：常驻工具的更新不该等人想起来点按钮
            auto_update_enabled: true,
            auto_update_time: default_auto_update_time(),
        }
    }
}

/// 静默更新的默认时刻：凌晨 5 点。
///
/// 挑深夜不是为了「隐藏」，而是因为这一刻**最不容易撞上正在使用的界面/接管**：
/// 自动更新装完会重启整个应用，落在白天就是一次无预警的中断。
fn default_auto_update_time() -> String {
    "05:00".to_string()
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// 展示助手
// ---------------------------------------------------------------------------

/// 「尾号」—— 账号里存的是**已打码**的手机号（`190******75`）。真实尾号只能看
/// **最后一个星号之后**可见的数字（即 `75`）；若把打码串里所有数字拼起来取后 4 位
/// （get `9075`）会把开头的 `190` 也「算进」尾号，从而对不上真实手机尾号。
/// 完整（未打码）手机号则取后 4 位。
///
/// ⚠️ 必须与前端 `TakeoverPage.tsx::shortLabel` 算**同一个数**：
/// 界面那枚 chip 写「尾号 75」，若这里算法不一致，「日志说换给了谁」和
/// 「界面上写着谁」就对不上号，而对账的唯一入口就是这两处对上
/// （2026-09-16 实测踩到，当时算法在 `proxy.rs` 里孤零零地写了一份）。
pub fn tail(phone: Option<&str>) -> Option<String> {
    let phone = phone?;
    // 打码串：取最后一个星号之后可见的数字（真实尾号）。
    if let Some(star) = phone.rfind('*') {
        let visible: String = phone[star + 1..]
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if !visible.is_empty() {
            return Some(visible);
        }
    }
    // 无星号（完整手机号）或星号后没有可读数字：取所有数字的后 4 位。
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    Some(digits[digits.len() - 4..].to_string())
}

/// 「账号 A（尾号 75）」—— 界面上那枚 chip 的文字版。
pub fn label_with_tail(a: &Account) -> String {
    match tail(a.phone.as_deref()) {
        Some(t) => format!("{}（尾号 {t}）", a.name),
        None => a.name.clone(),
    }
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

pub fn accounts_path() -> PathBuf {
    PathBuf::from("accounts.json")
}

pub fn settings_path() -> PathBuf {
    PathBuf::from("settings.json")
}

/// 读取账号列表。
///
/// 读时顺带做一次**幂等的规范化迁移**（补空 `id` / 补 `user_id` / 补 `created_at`，见
/// [`normalize`]），有改动才回写。放在这里而不是每个调用点，理由与 [`load_settings`] 的
/// 旧配置迁移一样：历史遗留的空 `id` 只需修一次，且所有入口（界面、签到、接管反代）
/// 拿到的都是修好的数据。
pub fn load_accounts(dir: &Path) -> Vec<Account> {
    let p = dir.join(accounts_path());
    let mut list: Vec<Account> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Account>>(&s).ok())
        .unwrap_or_default();
    if list.iter_mut().any(normalize) {
        if let Err(e) = save_accounts(dir, &list) {
            crate::logs::push(
                dir,
                crate::logs::CAT_SYSTEM,
                "账号",
                false,
                format!("规范化回写失败：{e}"),
            );
        }
    }
    list
}

/// 原子写：先落同名 `.tmp`（`sync_all` 确认进了盘），再 `rename` 覆盖目标。
///
/// 凭证类文件**必须**走这条路。`RefreshToken` 是一次性的：续签成功的那一瞬间，
/// 盘上那份就同时作废了。直写目标文件时若中途崩溃 / 断电，留下的是一截既换不回旧值、
/// 新值又没落稳的 JSON —— 症状与 `renew.rs` 里那条死链一模一样：只能重新登录。
/// 原子替换保证「要么还是旧的一份，要么就是新的一份」，不会出现第三种。
///
/// `broker.json`（绑池身份）与 `device.json`（签 `DeviceProof` 的那把私钥）同理：
/// 它们半截丢失分别等于「这台机器没绑池」「这条链的钥匙没了」，都不是重试能救的。
pub(crate) fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    use std::io::Write;
    let name = path
        .file_name()
        .ok_or_else(|| format!("路径没有文件名：{}", path.display()))?
        .to_os_string();
    let mut tmp_name = name;
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    // 先 `sync_all` 再 `rename`：顺序反了就只是「名义上原子」—— 内容可能还留在页缓存里，
    // 崩溃时目标文件已指向一个尚未落稳的 inode。
    //
    // 权限在**建临时文件那一刻**就是 0600，而不是 rename 之后再补：后者中间有个窗口，
    // 新凭证已经落盘、却还是同机其它用户可读的。
    let opened = || -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
        }
        #[cfg(not(unix))]
        std::fs::File::create(&tmp)
    };
    let write = || -> std::io::Result<()> {
        let mut f = opened()?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    };
    let result = write();
    // 失败时把半截临时文件扫掉：它带着一份新凭证的副本，留在那儿既脏又是多余的暴露面。
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(accounts).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(accounts_path()), &json)
}

/// 落盘账号，失败就**当场吼一声**给界面日志 —— 给那些「不值得为写失败中断整条调用」的调用点。
///
/// 为什么不能 `let _ =`：这些保存里往往带着**活性结论**（`invalidated_at`）与身份补齐
/// （`user_id`）。前者决定云端那一池会不会把这张死票救回来，后者是「从本机桌面端接新票」
/// 那条路的认人依据（见 [`crate::trae_auth::find_local_session_by_uid`]）。
/// 写失败不致命，但必须让人看得见 —— 否则它的症状要等几周后才以
/// 「这个账号怎么突然要我重新登录」的形式出现，而那时已经查不到是哪次写盘失败了。
pub(crate) fn save_accounts_noisy(dir: &Path, accounts: &[Account], scene: &str) {
    if let Err(e) = save_accounts(dir, accounts) {
        crate::logs::push(
            dir,
            crate::logs::CAT_SYSTEM,
            "账号数据",
            false,
            format!("{scene}：落盘失败（{e}）"),
        );
    }
}

/// 读取设置。
///
/// 兼容早期版本：那时把「本地网关」(`gateway_enabled`) 与「拦截模式」(`intercept_enabled`)
/// 做成两个独立开关、端口叫 `gateway_port`。现在二者合并为「智能接管」，此处做一次性迁移
/// （任一旧开关为真即视为接管开启；端口沿用旧值），避免升级后用户配置丢失。
///
/// 注意：**不能**用 `#[serde(alias)]` 来做这件事——旧配置里两个键可能同时存在，
/// serde 会因「同一字段被赋值两次」而整体反序列化失败，反而丢掉全部设置。
pub fn load_settings(dir: &Path) -> Settings {
    let p = dir.join(settings_path());
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Settings::default();
    };
    let Ok(raw) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Settings::default();
    };
    let mut s: Settings = serde_json::from_value(raw.clone()).unwrap_or_default();
    let legacy_on = |k: &str| raw.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    if legacy_on("intercept_enabled") || legacy_on("gateway_enabled") {
        s.takeover_enabled = true;
    }
    if raw.get("takeover_port").is_none() {
        if let Some(port) = raw.get("gateway_port").and_then(|v| v.as_u64()) {
            s.takeover_port = port.clamp(1, 65535) as u16;
        }
    }
    s
}

pub fn save_settings(dir: &Path, settings: &Settings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(settings_path()), &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(settings_path()));
        dir
    }

    /// 凭证文件必须**原子替换**，且失败要吭声。
    ///
    /// 这条判据是给 `renew.rs` 那条不可逆的一步兜底的：`RefreshToken` 换出去就收不回来，
    /// 「以为写好了」和「没写进去」的差别就是这条链的命。
    #[test]
    fn write_atomic_replaces_in_place_and_fails_loudly() {
        let dir = tmp("twa_write_atomic_test");
        let p = dir.join("atomic_target.json");
        let _ = std::fs::remove_file(&p);
        write_atomic(&p, r#"{"v":1}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"v":1}"#);
        assert!(
            !dir.join("atomic_target.json.tmp").exists(),
            "不该留下 .tmp 残留"
        );
        write_atomic(&p, r#"{"v":2}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"v":2}"#, "覆盖写要生效");
        // 权限：新票不能有一瞬间是「同机任何人可读」的，所以收窄要发生在**建临时文件时**，
        // 而不是 rename 之后再补 —— 这里直接断言最终文件的模式。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "凭证文件的权限必须收窄到 0600");
        }
        // 目标目录不存在必须返回 Err：静默成功等于让调用方以为新票已经落稳了
        let bad = dir.join("definitely-not-a-dir").join("x.json");
        assert!(write_atomic(&bad, "{}").is_err(), "写不进去要返回 Err");
        // 失败不能留下半截临时文件：那是一份没人管的新凭证副本。
        // 这里要造一个「临时文件写得成、但换名失败」的场景：把目标位置占成**目录**，
        // `rename` 对已存在的目录目标会失败（EISDIR），于是能干净地验证清理那一步。
        let blocker = dir.join("blocked_dir");
        std::fs::create_dir_all(&blocker).unwrap();
        assert!(write_atomic(&blocker, "{}").is_err(), "目标是目录时必须报错");
        assert!(
            !dir.join("blocked_dir.tmp").exists(),
            "换名失败也要扫掉那份半截临时文件"
        );
        let _ = std::fs::remove_dir(&blocker);
        let _ = std::fs::remove_file(&p);
    }

    /// 「尾号」必须与前端 `shortLabel` 同算法：取**打码串最后一个星号后可见**的数字。
    ///
    /// 账号里存的是已打码的手机号 `190******75`；真实尾号在**星号之后**，
    /// 即 `75`。若按老算法拼「所有数字后 4 位」会得到 `9075`，把开头的 `190`
    /// 也算进尾号 —— 于是日志与界面 chip 都对不上真实手机尾号。
    #[test]
    fn tail_matches_the_ui_short_label() {
        assert_eq!(tail(Some("190******75")).as_deref(), Some("75"));
        assert_eq!(tail(Some("191******52")).as_deref(), Some("52"));
        assert_eq!(tail(Some("190****9775")).as_deref(), Some("9775"));
        assert_eq!(tail(Some("+86 138-0000-1234")).as_deref(), Some("1234"));
        assert_eq!(tail(Some("12")), None, "数字不够 4 位就别硬凑");
        assert_eq!(tail(None), None);
    }

    #[test]
    fn migrates_legacy_gateway_settings() {
        let dir = tmp("twa_settings_migrate_test");
        let legacy = serde_json::json!({
            "checkin_enabled": true,
            "checkin_time": "09:30",
            "gateway_enabled": false,
            "gateway_port": 9999,
            "intercept_enabled": true,
            "injection_enabled": true,
            "webhook_url": "https://example.invalid/hook",
            "billing_account_ids": []
        });
        std::fs::write(dir.join(settings_path()), legacy.to_string()).unwrap();
        let s = load_settings(&dir);
        assert!(s.takeover_enabled, "旧 intercept_enabled=true 应迁移为接管开启");
        assert_eq!(s.takeover_port, 9999, "旧 gateway_port 应被沿用");
        assert_eq!(s.checkin_time, "09:30", "其余设置不得丢失");
        assert_eq!(s.webhook_url, "https://example.invalid/hook");
        let _ = std::fs::remove_file(dir.join(settings_path()));
    }

    #[test]
    fn defaults_when_settings_absent() {
        let dir = tmp("twa_settings_default_test");
        let s = load_settings(&dir);
        assert!(!s.takeover_enabled);
        assert_eq!(s.takeover_port, 8788);
        assert!(s.checkin_enabled);
    }

    /// 回归（2026-09-14）：浏览器登录落库的账号 `id` 为空串、`user_id` 为 null，
    /// 这里必须一次性补齐，且幂等。
    #[test]
    fn normalize_fills_empty_id_and_uid_from_jwt() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let payload = URL_SAFE_NO_PAD.encode(br#"{"data":{"id":"3225324630062683"}}"#);
        let mut a = Account {
            id: String::new(),
            name: "浏览器登录账号".into(),
            phone: None,
            user_id: None,
            token: format!("eyJhbGciOiJSUzI1NiJ9.{payload}.sig"),
            refresh_token: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: Some("e6fb29121995483988b3f6cff4b9ff1c".into()),
            machine_id: None,
            created_at: String::new(),
            credit_snapshot: None,
            invalidated_at: None,
            renew_blocked_until: None,
        };
        assert!(normalize(&mut a), "空 id/uid/created_at 应判定为有改动");
        assert!(!a.id.trim().is_empty(), "空 id 必须补成 uuid");
        assert_eq!(a.user_id.as_deref(), Some("3225324630062683"), "uid 应能从 JWT 解出");
        assert!(!a.created_at.trim().is_empty());
        assert!(!normalize(&mut a), "规范化必须幂等");
    }

    #[test]
    fn normalize_keeps_existing_values() {
        let mut a = Account {
            id: "keep-me".into(),
            name: "n".into(),
            phone: None,
            user_id: Some("2807498532739475".into()),
            token: "opaque".into(),
            refresh_token: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: None,
            machine_id: None,
            created_at: "2026-09-14 21:21:50".into(),
            credit_snapshot: None,
            invalidated_at: None,
            renew_blocked_until: None,
        };
        assert!(!normalize(&mut a));
        assert_eq!(a.id, "keep-me");
        assert_eq!(a.user_id.as_deref(), Some("2807498532739475"));
        assert_eq!(a.created_at, "2026-09-14 21:21:50");
    }
}
