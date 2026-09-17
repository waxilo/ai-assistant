//! 账号资料补全：从服务端取「真昵称 + 脱敏手机号」，替换掉占位 / 派生值。
//!
//! 为什么需要它：这两项**只能从服务端查**（`GetUserInfo` 的 `ScreenName` /
//! `NonPlainTextMobile`），凭证本身不含 —— JWT 载荷里只有 `data.id`（uid）。于是列表里
//! 会出现两种认不出人的账号：
//!
//! - **名字**是占位 / 派生值：浏览器登录时接口拿不到资料 → 前端填占位名
//!   [`PLACEHOLDER_NAME`]；扫描本机导入时登录态里的 `account.nickname` 往往只是
//!   **脱敏手机号**（`190******75`）。
//! - **手机号**缺失：真名形如「用户0044120650」，光看名字根本不知道是哪个号，而手机号
//!   才是人认得的标识 —— 缺了它，界面上就没有可辨识信息。
//!
//! 所以在启动 / 手动刷新时，对**资料不全**的账号回源一次（判据见 [`needs_fetch`]）。
//! 两个判据都是「补上就长期成立」的，补全后不再重复请求（日常零网络开销）。
//!
//! 失败一律静默 —— 离线、限流（9074）时保留原值比清空更好。

use std::path::Path;

use crate::accounts::{self, Account};
use crate::checkin;
use crate::oauth;

/// 浏览器登录流程在拿不到昵称时落的占位名（与前端 `AddAccountModal` 保持一致）。
/// 它出现在 `accounts.json` 里就说明「这个账号还没回源过」。
pub const PLACEHOLDER_NAME: &str = "浏览器登录账号";

/// 这个账号的名字是否只是「占位 / 派生」值，值得用服务端昵称覆盖。
///
/// 三种情况需要补全：
/// 1. 空名字；
/// 2. 占位名 [`PLACEHOLDER_NAME`]（浏览器登录且当时接口没返回资料）；
/// 3. 名字等于脱敏手机号（本机扫描导入时的常见形态，`account.nickname` 就是手机号）。
fn needs_name(a: &Account) -> bool {
    let n = a.name.trim();
    n.is_empty() || n == PLACEHOLDER_NAME || Some(n) == a.phone.as_deref()
}

/// 是否为这个账号回源一次。
///
/// 除了名字需要换（[`needs_name`]），**手机号缺失也要取**：它在列表里承担「哪个号」的
/// 辨识职责，缺了就只能显示 `用户0044120650` 这种认不出的名字。
/// 只认「有值且非空白」为已具备，避免拿空串当地址簿。
pub fn needs_fetch(a: &Account) -> bool {
    let phone_missing = a.phone.as_deref().map_or(true, |p| p.trim().is_empty());
    needs_name(a) || phone_missing
}

/// 按需回源补全账号资料（名字 / 手机号 / 缺失的 uid），返回（可能已更新并落库的）账号列表。
///
/// 只对 [`needs_fetch`] 命中的账号发请求；其它账号零开销。
pub async fn sync_profiles(dir: &Path) -> Vec<Account> {
    let mut list = accounts::load_accounts(dir);
    let mut changed = false;

    for a in list.iter_mut() {
        if !needs_fetch(a) {
            continue;
        }
        let host = checkin::normalize_host(&checkin::host_of(a));
        let Some(info) = oauth::fetch_user_info(&host, &a.token).await else {
            continue;
        };

        if needs_name(a) {
            if let Some(name) = info.nickname.filter(|s| !s.trim().is_empty()) {
                a.name = name;
                changed = true;
            }
        }
        // 空值才补，避免覆盖本机登录态里更准确的值
        if a.phone.as_deref().map_or(true, |p| p.trim().is_empty()) {
            if let Some(p) = info.phone.filter(|s| !s.trim().is_empty()) {
                a.phone = Some(p);
                changed = true;
            }
        }
        if a.user_id.is_none() && !info.uid.is_empty() {
            a.user_id = Some(info.uid);
            changed = true;
        }
    }

    if changed {
        let _ = accounts::save_accounts(dir, &list);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Account;

    fn acc(name: &str, phone: Option<&str>) -> Account {
        Account {
            id: "local".into(),
            name: name.into(),
            phone: phone.map(str::to_string),
            region: None,
            user_id: None,
            token: "t".into(),
            refresh_token: None,
            host: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: None,
            machine_id: None,
            created_at: String::new(),
            credit_snapshot: None,
        }
    }

    #[test]
    fn placeholder_and_phone_like_names_need_backfill() {
        assert!(needs_fetch(&acc(PLACEHOLDER_NAME, None)));
        assert!(needs_fetch(&acc("   ", None)));
        assert!(needs_fetch(&acc("", None)));
        // 名字就是脱敏手机号 → 只是派生值，不是昵称
        assert!(needs_fetch(&acc("190******75", Some("190******75"))));
    }

    /// 真名 + 有手机号 = 资料齐了，不该再打网络。
    #[test]
    fn complete_profile_is_left_alone() {
        assert!(!needs_fetch(&acc("用户0044120650", Some("191******52"))));
        assert!(!needs_fetch(&acc("waxiloao", Some("139******01"))));
    }

    /// 真名但缺手机号 → 仍要回源：手机号是界面上唯一的辨识信息。
    /// 空串按「缺失」处理，否则会永远显示成空白。
    #[test]
    fn missing_phone_triggers_backfill() {
        assert!(needs_fetch(&acc("用户0044120650", None)));
        assert!(needs_fetch(&acc("用户0044120650", Some(""))));
        assert!(needs_fetch(&acc("用户0044120650", Some("  "))));
    }

    /// 真机冒烟：对**真实** `accounts.json` 跑一次资料补全。
    /// `cargo test --lib -- --ignored --nocapture live_sync_real_account_profiles`
    #[test]
    #[ignore]
    fn live_sync_real_account_profiles() {
        let dir = dirs::home_dir()
            .expect("no home")
            .join("Library/Application Support/cn.traework.assistant");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let before = accounts::load_accounts(&dir);
        println!("--- 补全前 ---");
        for a in &before {
            println!("  name={:?} phone={:?} 需回源={}", a.name, a.phone, needs_fetch(a));
        }
        let after = rt.block_on(sync_profiles(&dir));
        println!("--- 补全后 ---");
        for a in &after {
            println!("  name={:?} phone={:?} uid={:?}", a.name, a.phone, a.user_id);
        }
    }
}
