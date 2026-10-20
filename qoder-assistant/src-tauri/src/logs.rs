use crate::accounts::{Account, CheckinRecord};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 一条签到日志（每次签到尝试都落库，便于追溯）。
///
/// 字段与 [`CheckinRecord`] 一一对应 —— 它只是「加上账号标识、并补一个 id」的落盘形态。
/// 旧版的 `code` / `host` 已随签到线重写一起删掉（见 [`CheckinRecord`] 的说明）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CheckinLog {
    pub id: String,
    pub account_id: String,
    pub account_name: String,
    /// 账号手机号（展示用，可空）
    #[serde(default)]
    pub account_phone: Option<String>,
    /// 账号邮箱（展示用，可空）。与手机号一样是「这个人的标识」，
    /// 日志页与账号页因此长得一样。
    #[serde(default)]
    pub account_email: Option<String>,
    pub at: String,
    pub success: bool,
    pub already: bool,
    pub inactive: bool,
    pub message: String,
    /// 本次领取到的额度
    #[serde(default)]
    pub credit: Option<f64>,
    /// 剩余积分（小数）
    #[serde(default)]
    pub balance: Option<f64>,
    /// 领的是哪一场活动（`campaignKey`）
    #[serde(default)]
    pub campaign_key: Option<String>,
    /// 这笔积分的到期时刻（毫秒）；见 [`CheckinRecord::expires_at`]。
    /// 老日志没有这个键（默认 `None`），读取侧按「未知」展示。
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// 最多保留的日志条数，超出后丢弃最旧的，避免无限膨胀
const MAX_LOGS: usize = 2000;

pub fn logs_file(dir: &Path) -> PathBuf {
    dir.join("checkin_logs.json")
}

pub fn load_logs(dir: &Path) -> Vec<CheckinLog> {
    let f = logs_file(dir);
    if !f.exists() {
        return Vec::new();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    serde_json::from_str(&s).unwrap_or_default()
}

fn save_logs(dir: &Path, logs: &[CheckinLog]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    // 走 [`write_atomic`]：这份是「整本重写」的（只留最近 MAX_LOGS 条），
    // 中途崩溃会把历史一起抹掉；另外要 `sync_all`，光 rename 只保证名字不换、不保证内容落盘。
    crate::accounts::write_atomic(&logs_file(dir), &serde_json::to_string_pretty(logs)?)
}

/// 追加一条日志；保留最近 MAX_LOGS 条
pub fn append_log(dir: &Path, log: CheckinLog) -> std::io::Result<()> {
    let mut logs = load_logs(dir);
    logs.push(log);
    if logs.len() > MAX_LOGS {
        let drop = logs.len() - MAX_LOGS;
        logs.drain(0..drop);
    }
    save_logs(dir, &logs)
}

/// 清空日志：`account_id` 为 None 时清空全部，否则只清该账号的日志
pub fn clear_logs(dir: &Path, account_id: Option<&str>) -> std::io::Result<()> {
    match account_id {
        None => save_logs(dir, &[]),
        Some(id) => {
            let mut logs = load_logs(dir);
            logs.retain(|l| l.account_id != id);
            save_logs(dir, &logs)
        }
    }
}

/// 由账号 + 签到结果构造一条日志（复制需要的字段，不移动 rec）
pub fn log_from_record(account: &Account, rec: &CheckinRecord) -> CheckinLog {
    CheckinLog {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account.id.clone(),
        account_name: account.name.clone(),
        account_phone: account.phone.clone(),
        account_email: account.email.clone(),
        at: rec.at.clone(),
        success: rec.success,
        already: rec.already,
        inactive: rec.inactive,
        message: rec.message.clone(),
        credit: rec.credit,
        balance: rec.balance,
        campaign_key: rec.campaign_key.clone(),
        expires_at: rec.expires_at,
    }
}

// ── 统一日志流（「日志管理」页的数据源）─────────────────────────────
//
// ## 为什么是读取侧合并，而不是新建一份写路径
//
// 排查需要的事实**本来就各自有落盘的家**：
// - 签到 → `checkin_logs.json`（本文件上半部，含积分/活动键/到期等富字段）；
// - 续签 / 通知 / 系统 → `scheduler.log`（32 处 `scheduler::log_event` 的唯一出口，
//   文案前缀稳定、由同一份代码写出）；
// - 接管 → `takeover-journal.jsonl`（`stealth` 的对客通知，写入侧已过滤请求级细节）。
//
// 在这三份之上做**只读投影**（[`unified`]）就能得到一条按时间倒序的全量流水，
// 不需要第四份会漂移的副本，也不会漏掉未来新加的 `log_event` 调用点。
// 「报错」不是一个类别，而是横跨全部类别的成败筛选（`success == false`）。
//
// ## 容量
//
// 三路各自截到 [`UNIFIED_PER_SOURCE`] 条再合并——界面要的是「最近发生了什么」，
// 不是全量导出；签到日志本身的 2000 条上限不受影响（账号页回放发放凭据仍读全量）。

/// 界面按这些值分组，**改动会同时改变筛选标签**，不要随手重命名。
pub const CAT_CHECKIN: &str = "checkin";
pub const CAT_RENEW: &str = "renew";
pub const CAT_NOTIFY: &str = "notify";
pub const CAT_TAKEOVER: &str = "takeover";
pub const CAT_SYSTEM: &str = "system";

/// 统一流里每一路最多并入的条数。
const UNIFIED_PER_SOURCE: usize = 500;

/// 统一流的一条：签到行带富字段（credit 等），其余类别只有基本五元组。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LogEntry {
    /// 本地时间 `%Y-%m-%d %H:%M:%S`（展示用）
    pub at: String,
    /// 毫秒时间戳：三路合并时排序用（`at` 只到秒，同秒事件会乱序）
    pub at_ms: i64,
    /// 见 `CAT_*`
    pub category: String,
    /// 账号名；系统 / 接管类事件为空串
    pub account: String,
    pub success: bool,
    /// 签到行专用：`already` 优先于 `success`（与 [`CheckinLog`] 同口径）
    #[serde(default)]
    pub already: bool,
    #[serde(default)]
    pub inactive: bool,
    pub message: String,
    #[serde(default)]
    pub credit: Option<f64>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub campaign_key: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// 本地时间串 → 毫秒时间戳。解析不了（旧格式 / 手改过）返回 0，排序时自然沉底。
fn local_ms(at: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|t| t.and_local_timezone(chrono::Local).single())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

/// `scheduler.log` 的一行 → `(时刻, 正文)`。格式由 [`crate::scheduler::log_event`] 写定：
/// `[yyyy-MM-dd HH:mm:ss] 正文`。不合式的行（空行 / 截断行）直接丢弃。
fn parse_scheduler_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (at, msg) = rest.split_once("] ")?;
    Some((at, msg))
}

/// `scheduler.log` 正文 → `(类别, 是否成功)`。
///
/// 前缀表只认**这份代码自己写出**的文案；顺序有意为之 —— 「续签告警」带「续签」二字，
/// 却是通知通道的结果，必须先于续签类判断。成败按关键词横切推断：
/// `scheduler.log` 的行本来就不带结构化的成败位。
pub fn classify_scheduler(msg: &str) -> (&'static str, bool) {
    const NOTIFY_PREFIXES: &[&str] = &[
        "通知已发送",
        "通知发送失败",
        "预告已发送",
        "预告发送失败",
        "简报通知",
        "续签告警",
    ];
    const RENEW_PREFIXES: &[&str] = &[
        "自动续签",
        "续签成功但",
        "整池续签",
        "已采纳桌面端",
        "这条链已死",
    ];
    const CHECKIN_PREFIXES: &[&str] = &["签到完成", "签到异常", "触发定时签到"];

    let category = if NOTIFY_PREFIXES.iter().any(|p| msg.starts_with(p)) {
        CAT_NOTIFY
    } else if RENEW_PREFIXES.iter().any(|p| msg.starts_with(p)) {
        CAT_RENEW
    } else if CHECKIN_PREFIXES.iter().any(|p| msg.starts_with(p)) {
        CAT_CHECKIN
    } else {
        CAT_SYSTEM
    };
    // 「签到完成」的汇总里恒有「失败 N」字样，N=0 也算成功（别把好日子过成报错）
    let success = if msg.starts_with("签到完成") {
        msg.contains("失败 0")
    } else {
        !["失败", "异常", "放弃", "已死"].iter().any(|k| msg.contains(k))
    };
    (category, success)
}

/// 接管 journal 的事件名 → `(类别, 是否成功)`。带 fail/error 的是真故障，其余按完成算。
fn classify_journal(event: &str) -> (&'static str, bool) {
    (CAT_TAKEOVER, !(event.contains("fail") || event.contains("error")))
}

/// 「日志管理」页的数据源：签到 + 调度（续签/通知/系统）+ 接管动态，按时间倒序。
pub fn unified(dir: &Path) -> Vec<LogEntry> {
    let mut all = Vec::new();

    // 签到：富字段原样带上（界面要展示本次额度 / 活动键 / 到期）
    for l in load_logs(dir).into_iter().rev().take(UNIFIED_PER_SOURCE) {
        let at_ms = local_ms(&l.at);
        all.push(LogEntry {
            at: l.at,
            at_ms,
            category: CAT_CHECKIN.to_string(),
            account: l.account_name,
            success: l.success,
            already: l.already,
            inactive: l.inactive,
            message: l.message,
            credit: l.credit,
            balance: l.balance,
            campaign_key: l.campaign_key,
            expires_at: l.expires_at,
        });
    }

    // 调度流（续签 / 通知 / 系统）：文件尾部是新的，倒序读
    let sched = std::fs::read_to_string(dir.join("scheduler.log")).unwrap_or_default();
    let mut sched_count = 0;
    for line in sched.lines().rev() {
        if sched_count >= UNIFIED_PER_SOURCE {
            break;
        }
        let Some((at, msg)) = parse_scheduler_line(line) else {
            continue;
        };
        let (category, success) = classify_scheduler(msg);
        all.push(LogEntry {
            at: at.to_string(),
            at_ms: local_ms(at),
            category: category.to_string(),
            account: String::new(),
            success,
            already: false,
            inactive: false,
            message: msg.to_string(),
            credit: None,
            balance: None,
            campaign_key: None,
            expires_at: None,
        });
        sched_count += 1;
    }

    // 接管动态：`journal_read` 追加序（旧→新），取最新一段；分类后并入
    for e in crate::stealth::visible_events(crate::stealth::journal_read(dir))
        .into_iter()
        .take(UNIFIED_PER_SOURCE)
    {
        let (category, success) = classify_journal(&e.event);
        all.push(LogEntry {
            at: e.at,
            at_ms: e.at_ms,
            category: category.to_string(),
            account: String::new(),
            success,
            already: false,
            inactive: false,
            message: e.detail,
            credit: None,
            balance: None,
            campaign_key: None,
            expires_at: None,
        });
    }

    all.sort_by(|a, b| b.at_ms.cmp(&a.at_ms));
    all
}

/// 清空**全部**日志：界面上是一张流水，只清一半会让人以为清空按钮坏了。
/// 调试日志（`takeover-debug.log`）故意不清——那是排查材料，不是给界面看的流水。
pub fn clear_unified(dir: &Path) {
    let _ = std::fs::remove_file(logs_file(dir));
    let _ = std::fs::remove_file(dir.join("scheduler.log"));
    let _ = crate::stealth::journal_clear(dir);
}

#[cfg(test)]
mod unified_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scheduler_lines_are_classified() {
        assert_eq!(
            classify_scheduler("自动续签完成：a、b"),
            (CAT_RENEW, true)
        );
        assert_eq!(
            classify_scheduler("自动续签失败：a（HTTP 500）"),
            (CAT_RENEW, false)
        );
        assert_eq!(
            classify_scheduler("这条链已死，跳过它（不拖停整池）：a（expired）"),
            (CAT_RENEW, false)
        );
        // 「续签告警」带「续签」二字，却是通知通道的结果
        assert_eq!(
            classify_scheduler("续签告警：告警已推送：ok"),
            (CAT_NOTIFY, true)
        );
        assert_eq!(
            classify_scheduler("简报通知发送失败（2026-10-20）：timeout"),
            (CAT_NOTIFY, false)
        );
        assert_eq!(classify_scheduler("通知发送失败：timeout"), (CAT_NOTIFY, false));
        // 汇总里「失败 0」不是失败
        assert_eq!(
            classify_scheduler("签到完成：2 个账号，成功 2 / 已签 0 / 活动未开 0 / 失败 0"),
            (CAT_CHECKIN, true)
        );
        assert_eq!(
            classify_scheduler("签到完成：2 个账号，成功 1 / 已签 0 / 活动未开 0 / 失败 1"),
            (CAT_CHECKIN, false)
        );
        assert_eq!(classify_scheduler("签到异常：boom"), (CAT_CHECKIN, false));
        assert_eq!(
            classify_scheduler("触发定时签到（设定 10:00，今日随机目标 10:12，实际 10:12）"),
            (CAT_CHECKIN, true)
        );
        // 兜底归系统
        assert_eq!(
            classify_scheduler("凭证池整池同步失败：boom"),
            (CAT_SYSTEM, false)
        );
        assert_eq!(
            classify_scheduler("积分简报：固化 3 个小时条目"),
            (CAT_SYSTEM, true)
        );
    }

    #[test]
    fn scheduler_line_parse_needs_bracket_shape() {
        assert_eq!(
            parse_scheduler_line("[2026-10-20 14:30:00] hello world"),
            Some(("2026-10-20 14:30:00", "hello world"))
        );
        assert_eq!(parse_scheduler_line("no bracket"), None);
        assert_eq!(parse_scheduler_line("[2026-10-20 14:30:00]"), None);
        // 正文里再出现方括号不影响
        assert_eq!(
            parse_scheduler_line("[2026-10-20 14:30:00] x [y] z"),
            Some(("2026-10-20 14:30:00", "x [y] z"))
        );
    }

    /// 三路合并：都进流、按时间倒序、各自类别正确。
    #[test]
    fn unified_merges_all_three_sources() {
        let dir = tmp("qoder_logs_unified_test");
        // 签到一路：走既有 append_log
        let acc = Account {
            id: "a1".into(),
            name: "小号".into(),
            phone: None,
            email: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            created_at: "2026-10-20 10:00:00".into(),
            last: None,
            checked_today: None,
            cosy_uid: None,
            invalidated_at: None,
            renew_blocked_until: None,
        };
        let rec = CheckinRecord {
            at: "2026-10-20 10:00:05".into(),
            success: true,
            already: false,
            inactive: false,
            message: "签到：success".into(),
            credit: Some(100.0),
            balance: Some(300.0),
            campaign_key: Some("act-1".into()),
            expires_at: None,
        };
        let _ = append_log(&dir, log_from_record(&acc, &rec));
        // 调度一路：直接写 scheduler.log（走 log_event 的格式）
        std::fs::write(
            dir.join("scheduler.log"),
            "[2026-10-20 10:00:10] 自动续签完成：小号\n[垃圾行]\n",
        )
        .unwrap();
        // 接管一路：走 stealth journal
        crate::stealth::journal_append(&dir, "install", "接管已开启");
        crate::stealth::journal_append(&dir, "takeover_fail", "补丁没打上");

        let all = unified(&dir);
        assert_eq!(all.len(), 4, "1 签到 + 1 调度 + 2 接管；坏行丢弃");
        assert!(all.windows(2).all(|w| w[0].at_ms >= w[1].at_ms), "必须倒序");
        assert_eq!(all.last().unwrap().category, CAT_CHECKIN);
        assert_eq!(all.last().unwrap().credit, Some(100.0));
        assert!(all.iter().any(|e| e.category == CAT_RENEW && e.success));
        assert!(all.iter().any(|e| e.category == CAT_TAKEOVER && e.success));
        assert!(all.iter().any(|e| e.category == CAT_TAKEOVER && !e.success));

        // 清空要三路一起清
        clear_unified(&dir);
        assert!(unified(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
