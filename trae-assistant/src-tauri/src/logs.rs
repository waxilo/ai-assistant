//! 运行日志：全应用统一的一份日志流，按**类别**标记，并落盘跨重启保留。
//!
//! ## 为什么要合并
//!
//! 以前只有一张「签到日志」：签到、续签、通知、凭证池、设备身份全挤在同一个 200 条的
//! 内存列表里，重启就没了；而接管动态另有一份持久化的 jsonl。用户想知道「刚才那条通知
//! 到底发出去没有」时，得在三个地方猜。现在统一成一个入口：
//!
//! - 本模块写**应用日志**（签到 / 续签 / 通知 / 系统），落盘、跨重启；
//! - [`unified`] 再把接管动态（`journal.rs`）按类别并进来 —— 接管那一份仍是唯一真相，
//!   本模块不复制它，因此两处永远不会漂移。
//!
//! ## 容量
//!
//! 应用日志保留最近 [`MAX_LOGS`] 条。合并后的列表最多再加 500 条接管动态，
//! 界面上仍是「最近发生的事」，不会无限长。
//!
//! ⚠️ 续签成功**刻意不记日志**（见 `renew.rs::sweep`）：token 每小时都可能轮换，
//! 每轮巡检都记一条会在一天内把所有真正需要看的记录挤掉。

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 应用日志文件名（数据目录下）。接管动态仍在 `takeover-journal.jsonl`，不并进来。
const LOG_FILE: &str = "app-logs.jsonl";
const MAX_LOGS: usize = 500;

/// 类别常量。界面按这些值分组，**改动会同时改变筛选标签**，不要随手重命名。
pub const CAT_CHECKIN: &str = "checkin";
pub const CAT_RENEW: &str = "renew";
pub const CAT_NOTIFY: &str = "notify";
pub const CAT_TAKEOVER: &str = "takeover";
pub const CAT_SYSTEM: &str = "system";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LogEntry {
    pub at: String,
    /// 毫秒时间戳：合并两路日志时排序用（`at` 只到秒，同秒事件会乱序）
    pub at_ms: i64,
    /// 见 `CAT_*`
    pub category: String,
    /// 账号名；系统 / 接管类事件为空串
    pub account: String,
    pub message: String,
    pub success: bool,
}

#[derive(Default)]
struct Store {
    /// 当前缓存对应的数据目录。换了目录（测试 / 多实例）就重新读盘。
    dir: Option<PathBuf>,
    entries: Vec<LogEntry>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join(LOG_FILE)
}

/// 读盘。单行损坏只丢那一行 —— 日志读坏了不该拖死整页。
fn read_file(path: &Path) -> Vec<LogEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LogEntry>(l).ok())
        .collect()
}

/// 整本重写（原子替换）。
///
/// 与 `journal.rs` 同理：这份日志是「读全量 → 重写」的，就地截断写一旦在窗口里被杀，
/// 历史会全丢。日志本身不值钱，但它恰恰是排查「为什么刚出事」的第一手材料。
fn write_file(path: &Path, entries: &[LogEntry]) -> Result<(), String> {
    let mut buf = String::with_capacity(entries.len() * 160);
    for e in entries {
        let line = serde_json::to_string(e).map_err(|err| err.to_string())?;
        buf.push_str(&line);
        buf.push('\n');
    }
    crate::accounts::write_atomic(path, &buf)
}

fn ensure_loaded(st: &mut Store, dir: &Path) {
    if st.dir.as_deref() == Some(dir) {
        return;
    }
    st.entries = read_file(&log_path(dir));
    st.dir = Some(dir.to_path_buf());
}

/// 追加一条应用日志。**失败只打 stderr，绝不打断主流程。**
pub fn push(
    dir: &Path,
    category: &str,
    account: &str,
    success: bool,
    message: impl Into<String>,
) {
    let now: DateTime<Local> = Local::now();
    let entry = LogEntry {
        at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        at_ms: now.timestamp_millis(),
        category: category.to_string(),
        account: account.to_string(),
        message: message.into(),
        success,
    };
    let mut st = store().lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut st, dir);
    // 新的在前：界面直接按存储顺序渲染，不必每帧排序
    st.entries.insert(0, entry);
    st.entries.truncate(MAX_LOGS);
    if let Err(e) = write_file(&log_path(dir), &st.entries) {
        // 不能再走 push（会递归）；落盘失败只影响「重启后还在不在」
        eprintln!("[日志] 写入失败：{e}");
    }
}

/// 只读应用日志（最新在前）。界面用 [`unified`]，这个留给测试与内部调用。
pub fn entries(dir: &Path) -> Vec<LogEntry> {
    let mut st = store().lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut st, dir);
    st.entries.clone()
}

/// 清空应用日志（接管动态由命令层一并清，见 `commands::clear_logs`）。
pub fn clear(dir: &Path) {
    let mut st = store().lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut st, dir);
    st.entries.clear();
    let _ = std::fs::remove_file(log_path(dir));
}

/// 这些 journal 事件在应用日志里已有同一条失败记录（`renew.rs` / `alerts.rs` 同一处
/// 先写日志、再补一条 journal 保命）。合并时跳过 journal 那份，避免界面上同一件事出现两行。
const JOURNAL_DUPLICATED_IN_APP_LOG: &[&str] = &[
    "renew_persist_failed",
    "renew_heartbeat",
    "alert_send_failed",
];

/// journal 事件 → `(类别, 是否成功)`。
///
/// 只把**硬失败**标成 false；`billing_list_stale` / `token_swap_rejected` 这类告警是
/// 「有结论」而不是「坏了」，仍按完成算，免得「报错」筛选被它们常年占满。
fn classify_journal(event: &str) -> (&'static str, bool) {
    if event.starts_with("renew_") {
        return (CAT_RENEW, false);
    }
    if event.starts_with("alert_") {
        return (CAT_NOTIFY, false);
    }
    match event {
        "restart_fail" | "takeover_fail" | "patch_fail" | "patch_revert_fail"
        | "install_blocked" | "takeover_blocked" => (CAT_TAKEOVER, false),
        _ => (CAT_TAKEOVER, true),
    }
}

/// 界面用的**统一日志流**：应用日志 + 接管动态，按时间倒序。
///
/// 接管动态是用户能对账的事实（谁用了哪个账号、补丁有没有打上），和签到 / 续签一样
/// 应该出现在「日志管理」里，而不是只能切到接管页看。
pub fn unified(dir: &Path) -> Vec<LogEntry> {
    let mut all = entries(dir);
    for e in crate::journal::read(dir) {
        if JOURNAL_DUPLICATED_IN_APP_LOG.contains(&e.event.as_str()) {
            continue;
        }
        let (category, success) = classify_journal(&e.event);
        all.push(LogEntry {
            at: e.at,
            at_ms: e.at_ms,
            category: category.to_string(),
            account: String::new(),
            message: e.detail,
            success,
        });
    }
    all.sort_by(|a, b| b.at_ms.cmp(&a.at_ms));
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些用例共用进程级的 `store()`，串行跑，避免互相把目录切走。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 日志要**真的落盘**：重启后打开「日志管理」还能看到上一次的签到失败。
    #[test]
    fn push_persists_and_reloads_from_disk() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp("twa_logs_persist_test");
        let other = tmp("twa_logs_persist_other_test");

        push(&dir, CAT_CHECKIN, "小号", false, "签到：HTTP 401");
        let got = entries(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].category, CAT_CHECKIN);
        assert_eq!(got[0].account, "小号");
        assert!(!got[0].success);

        // 切到另一个目录会强制重新读盘，再切回来即可验证「不是只活在内存里」
        assert!(entries(&other).is_empty());
        let again = entries(&dir);
        assert_eq!(again.len(), 1, "重启后（重新读盘）必须还在");
        assert_eq!(again[0].message, "签到：HTTP 401");
        assert!(again[0].at_ms > 0);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn caps_length_and_keeps_latest() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp("twa_logs_cap_test");
        for i in 0..(MAX_LOGS + 5) {
            push(&dir, CAT_SYSTEM, "系统", true, format!("e{i}"));
        }
        let got = entries(&dir);
        assert_eq!(got.len(), MAX_LOGS);
        assert_eq!(got[0].message, format!("e{}", MAX_LOGS + 4), "必须保留最新的");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_history_and_file() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp("twa_logs_clear_test");
        push(&dir, CAT_NOTIFY, "系统", true, "Webhook 通知：ok");
        assert!(log_path(&dir).exists());
        clear(&dir);
        assert!(entries(&dir).is_empty());
        assert!(!log_path(&dir).exists(), "清空后不该留下旧文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 接管动态要并进统一日志，且同一件事不能出现两行。
    #[test]
    fn unified_merges_journal_without_duplicating_escalations() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp("twa_logs_unified_test");
        push(&dir, CAT_CHECKIN, "小号", true, "签到：success");
        // 应用日志里已有同一件事的 journal 升级副本 → 合并时跳过
        push(&dir, CAT_RENEW, "小号", false, "续签结果落盘失败");
        crate::journal::append(&dir, "renew_persist_failed", "同样的失败");
        crate::journal::append(&dir, "install", "开启接管");
        crate::journal::append(&dir, "takeover_fail", "补丁没打上");

        let all = unified(&dir);
        assert_eq!(all.len(), 4, "2 条应用日志 + 2 条接管动态，升级副本不重复");
        assert!(all.iter().any(|e| e.category == CAT_TAKEOVER && e.success));
        assert!(all.iter().any(|e| e.category == CAT_TAKEOVER && !e.success));
        assert!(!all.iter().any(|e| e.message == "同样的失败"));
        // 最新在前
        assert!(all.windows(2).all(|w| w[0].at_ms >= w[1].at_ms));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_errors_are_categorized_and_marked() {
        assert_eq!(classify_journal("install"), (CAT_TAKEOVER, true));
        assert_eq!(classify_journal("restart_fail"), (CAT_TAKEOVER, false));
        assert_eq!(classify_journal("renew_heartbeat"), (CAT_RENEW, false));
        assert_eq!(classify_journal("alert_send_failed"), (CAT_NOTIFY, false));
        // 告警有结论，但不是「坏了」：不该常驻在「报错」筛选里
        assert_eq!(classify_journal("token_swap_rejected"), (CAT_TAKEOVER, true));
        assert_eq!(classify_journal("billing_list_stale"), (CAT_TAKEOVER, true));
    }

    #[test]
    fn survives_corrupt_line() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp("twa_logs_corrupt_test");
        push(&dir, CAT_SYSTEM, "系统", true, "good");
        let p = log_path(&dir);
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("{ not json\n");
        std::fs::write(&p, text).unwrap();
        // 当前内存缓存仍只有 1 条；切目录强制重读，坏行只丢自己
        let other = tmp("twa_logs_corrupt_other_test");
        let _ = entries(&other);
        assert_eq!(entries(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }
}
