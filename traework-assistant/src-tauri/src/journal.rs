//! 接管动态（journal）：记录「智能接管」到底做了什么。
//!
//! ## 两条通道 —— 按**受众**分开落盘
//!
//! 2026-09-16 拆分（用户要求）：「接管动态」是给**用户**看的东西，而反代每见到一个新路径就写一条
//! `proxy_path`、认不出会话还要写一份取证 —— 那些是**排查用的噪音**，混在一起会把
//! 「这笔扣费换给了谁」整个淹掉。于是分成两份：
//!
//! - `takeover-journal.jsonl` —— **用户日志**，也是「智能接管」页显示的唯一来源。
//!   只放用户关心的事实：开关动作、补丁、重启、选号、限流换号、名单变更、以及失败原因。
//! - `takeover-trace.jsonl` —— **诊断日志**，不上界面，排查时直接看文件。
//!   路径清单、会话识别取证、上游状态码、WebSocket 细节。
//!
//! 分流**由事件名决定**（见 [`channel_of`] / [`TRACE_EVENTS`]），不由调用点决定 ——
//! 「同一个事件该给谁看」是事件本身的性质，写在调用点只会随人心情飘。
//!
//! ## 清空
//!
//! [`clear`] 把**两条一起清**。两个时机会调它：用户点「清空」按钮，以及**开启接管时**
//! ——「开启接管先清空」是刻意的：开启 = 新的一本账，而上一轮那几百条（尤其路径清单）
//! 会把这一轮埋掉，偏偏用户就是在刚开完接管之后去看它。
//!
//! ## 为什么落盘而不是放在内存
//!
//! 事件由**反代的连接线程**写入（每个 TCP 连接一个线程），界面则通过 Tauri 命令读取。
//! jsonl 追加语义天然支持「一边写一边读」，且助手重启后历史仍在——排障时最想看的
//! 恰恰是**上一次**那批事件。配合每行一个 JSON，单条损坏也不会毁掉整份历史。
//!
//! ## 容量与并发
//!
//! 每条通道各保留最近 [`MAX_EVENTS`] 条（超出丢弃最旧的）。所有写入串行化在一把进程内互斥锁上
//! （否则多线程同时「读全量 + 重写」会互相覆盖），单次事件量级很小（每次写入重写整个文件，
//! 故**不要**在每请求路径上调用 [`append`]——只在会话切换/限流/异常时调用）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 用户日志文件名（「智能接管」页显示的就是它）。
const JOURNAL_FILE: &str = "takeover-journal.jsonl";
/// 诊断日志文件名（**不上界面**）。
const TRACE_FILE: &str = "takeover-trace.jsonl";
/// 每条通道最多保留的事件条数。
const MAX_EVENTS: usize = 500;

/// 一条接管事件。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JournalEvent {
    /// 本地时间字符串（展示用）
    pub at: String,
    /// 毫秒时间戳（排序 / 前端 key 用）
    pub at_ms: i64,
    /// 事件类型，见模块内使用的常量式字符串（`install` / `route_start` / `failover` …）
    pub event: String,
    /// 人类可读说明
    pub detail: String,
}

/// 事件该进哪条通道。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// 给用户看的 —— 界面只显示这一条通道。
    Journal,
    /// 只有排查时才看的。
    Trace,
}

/// 只进诊断日志的事件名。
///
/// ⚠️ **表外的一律进用户日志 —— 「默认可见」是刻意的。** 两者代价不对称：
/// 某个事件被错分进诊断日志，代价是它**永远静默**（用户拿着界面查不出这笔账是怎么发生的，
/// 而这恰恰是本项目反复踩的坑 ——「看不见旧记录」和「没有旧记录」在他那里是同一件事）；
/// 错分进用户日志，代价只是多一行噪音。所以默认取可见的那一边。
const TRACE_EVENTS: &[&str] = &[
    // 反代过程：每见到一个新路径就一条，量最大
    "proxy_path",
    // 会话识别 / 身份头取证
    "unbound_session",
    "unbound_session_hint",
    // 上游响应与连接层异常
    "proxy_error",
    "proxy_upstream_status",
    "proxy_stream_error",
    "proxy_bad_request",
    // WebSocket 细节
    "ws_handshake",
    "ws_swap",
    "ws_open",
];

/// 按事件名分流（判据见 [`TRACE_EVENTS`]）。
fn channel_of(event: &str) -> Channel {
    if TRACE_EVENTS.contains(&event) {
        Channel::Trace
    } else {
        Channel::Journal
    }
}

pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

/// 诊断日志路径（用户日志在 [`journal_path`]）。
pub fn trace_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TRACE_FILE)
}

/// 写入锁：反代每条连接一个线程，并发「读全量 + 重写」会丢事件。
///
/// 两条通道共用一把：单次写入是毫秒级的，为省这点争用而拆成两把锁，只会换来
/// 「两个 `write_all` 各自读全量再各自改名」这类更难查的问题。
fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 读取**用户日志**（界面用的就是它），**最新在前**。
///
/// ⚠️ 这里按 [`channel_of`] **再过滤一遍**，尽管写入端已经保证分流。理由是**契约**而不是
/// 「防御」：本函数的名字就叫「读用户日志」，返回一条诊断事件就是违约。
/// 它顺带解决了一个真实问题 —— 拆分**之前**写下的文件里混着几百行 `proxy_path`，
/// 不过滤的话用户下次打开页面仍旧看到那一片噪音（要等到他下次开启接管才会被清空）。
///
/// [`read_trace`] 刻意**不**过滤：排查时要看的是文件里的真相，而不是我们以为的真相。
pub fn read(data_dir: &Path) -> Vec<JournalEvent> {
    read_file(&journal_path(data_dir))
        .into_iter()
        .filter(|e| channel_of(&e.event) == Channel::Journal)
        .collect()
}

/// 读取**诊断日志**。不上界面 —— 它是排查用的过程记录，量比用户日志大一个量级。
pub fn read_trace(data_dir: &Path) -> Vec<JournalEvent> {
    read_file(&trace_path(data_dir))
}

fn read_file(path: &Path) -> Vec<JournalEvent> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<JournalEvent>(l).ok())
        .collect()
}

fn write_all(path: &Path, events: &[JournalEvent]) -> Result<(), String> {
    let mut buf = String::with_capacity(events.len() * 160);
    for e in events {
        let line = serde_json::to_string(e).map_err(|err| err.to_string())?;
        buf.push_str(&line);
        buf.push('\n');
    }
    // ⚠️ **先写临时文件再改名，绝不能就地截断写**。
    //
    // 这份日志是「读全量 → 重写整个文件」的落盘方式，一次写要持续若干毫秒。进程若正好
    // 在这一窗口里被杀（改 Rust 后 `tauri dev` 每次都会杀掉重启应用），就地截断会把文件
    // 留在 0 字节 —— **历史全丢**，而那恰恰是排查最需要的东西。
    // 2026-09-16 实测踩了两次：一次是好不容易采到的安装证据被 dev 的自动重启抹平。
    // 同目录改名在 Windows 上也是原子的（Rust 的 `rename` 走 `MoveFileEx` + 覆盖已存在）。
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, buf).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// 追加一条事件。**失败只打日志，绝不打断反代主流程。**
pub fn append(data_dir: &Path, event: &str, detail: &str) {
    write_entry(data_dir, event, detail, false)
}

/// 追加一条事件，但**若该通道最新一条与本次完全相同则跳过**。
///
/// 用于「配置型问题」类事件（如账号池为空、上游域名解析不出来）——这类问题会让
/// **每一个**路过反代的请求都产生同一行，不抑制就会把容量瞬间刷满，
/// 把真正有价值的「谁用了哪个账号」挤出去。
pub fn append_dedup(data_dir: &Path, event: &str, detail: &str) {
    write_entry(data_dir, event, detail, true)
}

fn write_entry(data_dir: &Path, event: &str, detail: &str, dedup: bool) {
    let now = chrono::Local::now();
    let entry = JournalEvent {
        at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        at_ms: now.timestamp_millis(),
        event: event.to_string(),
        detail: detail.to_string(),
    };
    let path = match channel_of(event) {
        Channel::Journal => journal_path(data_dir),
        Channel::Trace => trace_path(data_dir),
    };
    let _guard = write_lock().lock();
    let mut events = read_file(&path);
    if dedup {
        if let Some(last) = events.first() {
            if last.event == event && last.detail == detail {
                return;
            }
        }
    }
    events.insert(0, entry);
    if events.len() > MAX_EVENTS {
        events.truncate(MAX_EVENTS);
    }
    if let Err(e) = write_all(&path, &events) {
        eprintln!("[接管动态] 写入失败：{e}");
    }
}

/// 清空**用户日志与诊断日志**（用户点「清空」，以及**开启接管时**）。
pub fn clear(data_dir: &Path) {
    let _guard = write_lock().lock();
    let _ = std::fs::remove_file(journal_path(data_dir));
    let _ = std::fs::remove_file(trace_path(data_dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn appends_newest_first_and_roundtrips() {
        let dir = tmp("twa_journal_basic_test");
        append(&dir, "install", "开启接管");
        append(&dir, "route_start", "会话 abc 开始使用账号「A」");
        let events = read(&dir);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "route_start", "最新事件必须排在最前");
        assert_eq!(events[0].detail, "会话 abc 开始使用账号「A」");
        assert_eq!(events[1].event, "install");
        assert!(events[0].at_ms > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_length_and_keeps_latest() {
        let dir = tmp("twa_journal_cap_test");
        for i in 0..(MAX_EVENTS + 20) {
            append(&dir, "failover", &format!("e{i}"));
        }
        let events = read(&dir);
        assert_eq!(events.len(), MAX_EVENTS, "不得超过上限");
        assert_eq!(events[0].detail, format!("e{}", MAX_EVENTS + 19), "必须保留最新的");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘必须是**先写临时文件再改名**。
    ///
    /// 就地截断写会在「进程被杀」的窗口里留下 0 字节文件、历史全丢，而排查最需要的恰恰是
    /// 那份历史（2026-09-16 被 `tauri dev` 的自动重启抹过两次）。这条测试锁的是两个事实：
    /// 写完没有残留临时文件；历史读得回来。
    #[test]
    fn writes_atomically_and_leaves_no_temp_file() {
        let dir = tmp("twa_journal_atomic_test");
        append(&dir, "install", "开启接管");
        let path = journal_path(&dir);
        assert!(path.exists());
        assert!(
            !path.with_extension("jsonl.tmp").exists(),
            "临时文件必须已被改名掉，不能留在数据目录里"
        );
        assert_eq!(read(&dir).len(), 1, "历史必须读得回来");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_history() {
        let dir = tmp("twa_journal_clear_test");
        append(&dir, "install", "x");
        assert_eq!(read(&dir).len(), 1);
        clear(&dir);
        assert!(read(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **两条通道必须真的分开**（2026-09-16 用户要求「给用户看的」与「其他日志」分家）。
    ///
    /// 分开的意义全在「用户日志里没有噪音」这一点上：一旦诊断事件漏进用户日志，
    /// 界面上那几百行 `proxy_path` 会把「这笔扣费换给了谁」淹掉 —— 那正是这次改动的起因。
    #[test]
    fn diagnostics_never_leak_into_the_user_log() {
        let dir = tmp("twa_journal_split_test");
        append(&dir, "proxy_path", "接管收到 [透传] GET x /a");
        append(&dir, "unbound_session", "认不出会话");
        append(&dir, "proxy_error", "上游 500");
        assert!(read(&dir).is_empty(), "诊断事件不得进入用户日志");
        assert_eq!(read_trace(&dir).len(), 3, "诊断事件必须落在诊断日志");
        assert_eq!(read_trace(&dir)[0].event, "proxy_error", "诊断日志同样最新在前");

        append(&dir, "route_start", "会话 a 开始使用账号「A」");
        assert_eq!(read(&dir).len(), 1, "业务事件只进用户日志");
        assert_eq!(read_trace(&dir).len(), 3, "业务事件不得混进诊断日志");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分类表必须与「谁该被用户看见」一致。
    ///
    /// 名字写错的后果不是编译错误，而是**用户日志里悄悄多一类噪音**（或少一条救命记录）——
    /// 两种都不会有任何测试红，除非有人明确把这两组名单钉下来。
    #[test]
    fn event_classification_matches_intent() {
        for e in TRACE_EVENTS {
            assert_eq!(channel_of(e), Channel::Trace, "{e} 列在诊断表里却没被判成诊断");
        }
        for e in [
            "route_start",
            "failover",
            "install",
            "uninstall",
            "restart_trae",
            "restart_fail",
            "patch_fail",
            "patch_revert_fail",
            "billing_list_changed",
            "billing_list_stale",
            "takeover_fail",
            "token_swap_rejected",
        ] {
            assert_eq!(
                channel_of(e),
                Channel::Journal,
                "{e} 是给用户看的事实，不能进诊断日志"
            );
        }
    }

    /// `read` 的契约是「**用户**日志」：旧文件里混进来的诊断事件必须被挡在界面之外。
    ///
    /// 拆分之前写下的文件是混着的（那正是这次改动的起因），而「开启接管时清空」要等到
    /// 用户下次拨开关才会发生 —— 中间这段时间不该让他继续看那片噪音。
    #[test]
    fn read_hides_diagnostics_left_in_an_old_file() {
        let dir = tmp("twa_journal_legacy_test");
        // 模拟旧格式：直接把两条事件写进**用户日志文件**（含一条诊断事件）
        let legacy = concat!(
            r#"{"at":"2026-09-16 12:00:00","at_ms":1,"event":"proxy_path","detail":"接管收到 [透传] GET x /a"}"#,
            "\n",
            r#"{"at":"2026-09-16 11:59:00","at_ms":0,"event":"route_start","detail":"会话 a 开始使用账号「A」"}"#,
            "\n",
        );
        std::fs::write(journal_path(&dir), legacy).unwrap();

        let events = read(&dir);
        assert_eq!(events.len(), 1, "诊断事件必须被挡在用户日志之外");
        assert_eq!(events[0].event, "route_start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清空一次 = **两条通道一起清**（开启接管与「清空」按钮都依赖这一点）。
    #[test]
    fn clear_wipes_both_channels() {
        let dir = tmp("twa_journal_clear_both_test");
        append(&dir, "install", "x");
        append(&dir, "proxy_path", "y");
        assert_eq!(read(&dir).len(), 1);
        assert_eq!(read_trace(&dir).len(), 1);
        clear(&dir);
        assert!(read(&dir).is_empty(), "用户日志必须被清");
        assert!(read_trace(&dir).is_empty(), "诊断日志必须一起被清");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn survives_corrupt_line() {
        let dir = tmp("twa_journal_corrupt_test");
        append(&dir, "install", "good");
        // 手工塞一行坏数据：单条损坏不应毁掉其余历史
        let p = journal_path(&dir);
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("{ not json\n");
        std::fs::write(&p, text).unwrap();
        assert_eq!(read(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_skips_consecutive_duplicates_only() {
        let dir = tmp("twa_journal_dedup_test");
        append_dedup(&dir, "proxy_error", "账号池为空");
        append_dedup(&dir, "proxy_error", "账号池为空");
        append_dedup(&dir, "proxy_error", "账号池为空");
        assert_eq!(read_trace(&dir).len(), 1, "连续重复必须被抑制（诊断通道）");
        // 中间插一条不同事件后，同一内容可以再记
        append(&dir, "unbound_session", "会话 a 认不出");
        append_dedup(&dir, "proxy_error", "账号池为空");
        assert_eq!(read_trace(&dir).len(), 3);
        // 普通 append 不做抑制（每次都是真实发生的事）
        append(&dir, "install", "x");
        append(&dir, "install", "x");
        assert_eq!(read(&dir).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发写入不得丢事件：反代每个连接一个线程，全靠这把锁。
    #[test]
    fn loses_nothing_under_concurrency() {
        let dir = tmp("twa_journal_concurrency_test");
        let mut handles = Vec::new();
        for t in 0..4 {
            let d = dir.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    append(&d, "route_start", &format!("t{t}-i{i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(read(&dir).len(), 40, "40 次追加必须一条不少");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两条通道同时被并发写也不能互相丢事件（共用一把锁的理由）。
    #[test]
    fn both_channels_survive_concurrent_writes() {
        let dir = tmp("twa_journal_two_channel_test");
        let mut handles = Vec::new();
        for t in 0..2 {
            let d = dir.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    append(&d, "route_start", &format!("j{t}-{i}"));
                    append(&d, "proxy_path", &format!("p{t}-{i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(read(&dir).len(), 20, "用户日志一条不能少");
        assert_eq!(read_trace(&dir).len(), 20, "诊断日志一条不能少");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
