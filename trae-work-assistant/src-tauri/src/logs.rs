//! 签到日志：内存保留最近 N 条，统一时间格式。跨会话不持久化。

use chrono::{DateTime, Local};
use std::sync::{Mutex, OnceLock};

#[derive(serde::Serialize, Clone, Debug)]
pub struct LogEntry {
    pub at: String,
    pub account: String,
    pub message: String,
    pub success: bool,
}

const MAX_LOGS: usize = 200;

fn store() -> &'static Mutex<Vec<LogEntry>> {
    static STORE: OnceLock<Mutex<Vec<LogEntry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn push(account: &str, success: bool, message: impl Into<String>) {
    let now: DateTime<Local> = Local::now();
    if let Ok(mut v) = store().lock() {
        v.push(LogEntry {
            at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
            account: account.to_string(),
            message: message.into(),
            success,
        });
        if v.len() > MAX_LOGS {
            let drop = v.len() - MAX_LOGS;
            v.drain(..drop);
        }
    }
}

pub fn entries() -> Vec<LogEntry> {
    store().lock().map(|v| v.clone()).unwrap_or_default()
}

pub fn clear() {
    if let Ok(mut v) = store().lock() {
        v.clear();
    }
}
