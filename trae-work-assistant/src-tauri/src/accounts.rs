//! 账号与设置的持久化层：一个 data 目录下两份 JSON。
//!
//! - `accounts.json`：已导入的 TraeWork 账号列表
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
    pub region: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    pub token: String,
    pub refresh_token: Option<String>,
    pub host: Option<String>,
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
}

/// 账号**已有积分**快照。
///
/// 数据源是 `POST /trae/api/v2/pay/ide_user_ent_usage`（IDE 版 entitlement 用量），
/// 由 `checkin::parse_ent_usage` 按官方 `hHe()` 汇总出「剩余可用积分」与「最快到期时间」。
/// ⚠️ 这不是 `checkin_credits/status` 里的 `credits`——那个是**签到奖励**，两回事。
///
/// 「智能接管」按「**到期最早优先 → 无到期数据靠后 → 积分多者优先**」挑账号
/// （见 `proxy::pick_index`）：先消耗快到期的额度，避免浪费。
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
}

impl CreditSnapshot {
    pub fn now(
        credits: Option<i64>,
        unlimited: bool,
        earliest_expiry_ms: Option<i64>,
    ) -> CreditSnapshot {
        CreditSnapshot {
            credits,
            unlimited,
            earliest_expiry_ms,
            fetched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
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
            region: a.region,
            user_id: a.user_id,
            token: a.token,
            refresh_token: a.refresh_token,
            host: a.host,
            expires_at: a.expires_at,
            refresh_expires_at: a.refresh_expires_at,
            device_id: a.device_id,
            machine_id: a.machine_id,
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            credit_snapshot: None,
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
    /// 智能接管开关：启动本地反代 + 写入 TraeWork 端点覆盖（见 `endpoint.rs` / `proxy.rs`）。
    ///
    /// ## 改道方式只有一种（2026-09-15 收敛）
    ///
    /// 改写 TraeWork 安装目录里的 `product.json`，端点指向 `http://127.0.0.1:{port}`
    /// （[`crate::endpoint::base_url`] 的免证书形态）。
    ///
    /// 曾经还有两条**已整体移除**的路（见 `docs/免证书接管方案.md`）：
    /// - 「经系统代理接管」——写 TraeWork 的 `User/settings.json`，靠 TLS 中间人解密；
    /// - 端点走 `https` 的「证书模式」——本地反代自讲 TLS。
    ///
    /// 两者都必须把自签 CA 装进系统信任库（动的是系统信任设置），而免证书这条不需要，
    /// 代价只是给 TraeWork 打一个可逐字节还原的闸门补丁。既然免证书已验证跑通，
    /// 那两条路就是纯粹的额外风险面，于是不再保留 —— 相关代码（`tls.rs` / `tunnel.rs`）
    /// 也已删除，别为了「留一手」再把开关加回来。
    pub takeover_enabled: bool,
    /// 本机反代监听端口
    pub takeover_port: u16,
    /// **接管哪些应用**：值是 `target::AppTarget::id`（macOS 下 = `.app` 的名字，
    /// 如 `TRAE SOLO CN` / `Trae CN`）。
    ///
    /// ## 空 = 全部（与「参与扣费的账号」同一套语义）
    ///
    /// 本机可能同时装着几个 Trae shell，默认全接管；用户想只接管其中一个时，这里才写入
    /// 显式名单。这样「没配置过」与「全选」是同一个状态（和 `billing_account_ids` 一致），
    /// 也不会出现「设置里存着一份与本机不符的名单」——名单里不存在的 id 由
    /// `target::missing()` 负责报出来，而不是让它悄悄失效。
    pub takeover_apps: Vec<String>,
    /// 参与接管的账号白名单（空 = 全部）
    pub billing_account_ids: Vec<String>,
    /// 告警 webhook（可选）
    pub webhook_url: String,
    /// **积分简报开关**：开启后后台每小时采样一次（`ide_user_ent_usage`），
    /// 并把已经走完的小时固化成「时条目」（见 `ledger` / `briefing`）。
    /// 默认关：开启那一刻才对齐基线，用户不会在拨开关之前就有一堆历史账。
    pub briefing_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            checkin_enabled: true,
            checkin_time: "10:00".into(),
            takeover_enabled: false,
            takeover_port: 8788,
            takeover_apps: Vec::new(),
            billing_account_ids: Vec::new(),
            webhook_url: String::new(),
            briefing_enabled: false,
        }
    }
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
            crate::logs::push("账号", false, format!("规范化回写失败：{e}"));
        }
    }
    list
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> Result<(), String> {
    let p = dir.join(accounts_path());
    let json = serde_json::to_string_pretty(accounts).map_err(|e| e.to_string())?;
    std::fs::write(&p, json).map_err(|e| e.to_string())
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
    let p = dir.join(settings_path());
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&p, json).map_err(|e| e.to_string())
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
            region: Some("cn".into()),
            user_id: None,
            token: format!("eyJhbGciOiJSUzI1NiJ9.{payload}.sig"),
            refresh_token: None,
            host: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: Some("e6fb29121995483988b3f6cff4b9ff1c".into()),
            machine_id: None,
            created_at: String::new(),
            credit_snapshot: None,
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
            region: None,
            user_id: Some("2807498532739475".into()),
            token: "opaque".into(),
            refresh_token: None,
            host: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: None,
            machine_id: None,
            created_at: "2026-09-14 21:21:50".into(),
            credit_snapshot: None,
        };
        assert!(!normalize(&mut a));
        assert_eq!(a.id, "keep-me");
        assert_eq!(a.user_id.as_deref(), Some("2807498532739475"));
        assert_eq!(a.created_at, "2026-09-14 21:21:50");
    }
}
