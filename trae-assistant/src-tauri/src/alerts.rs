//! 续签失败告警：把**需要人介入**的失败推到 webhook，并且挡住重复刷屏。
//!
//! ## 为什么要有这一条
//!
//! 2026-10-13 那次事故的全部代价来自「看不见」：绑池的机器上续签心跳空转，票 12:25 过点之后
//! 二十小时没人尝试，而所有失败都只写进内存日志（200 条上限）与巡检日志 —— 用户不主动打开
//! 应用就永远看不到。续签失败的严重程度是**不对称**的：
//!
//! - 临时失败（网络抖动）下一轮大概率自己就好，值得说的只是「连着几轮都不行」；
//! - 死链（`needs_login`）重试一百次都是同一个结论，**只有重新登录能救**，越晚知道损失越大；
//! - 拿到新票却没落盘是**不可逆**的：那枚一次性 refresh token 已经作废，链当场断在我们手里。
//!
//! 所以这里只管「推出去 + 别刷屏」，判定归调用方。
//!
//! ## 冷却（去重）
//!
//! 告警的失败模式是**刷屏**：巡检 30 分钟一跳，一路失败的账号能一天推 48 条，用户很快就把
//! 通道静音 —— 那等于没有告警。于是每个告警带一个 `key`，同一 `key` 在冷却期内只推一次，
//! 记账落在 `alert_state.json`（跨重启有效）。冷却时长由调用方按性质给（见 [`COOLDOWN_MS`]
//! 三档），本模块不做判断。
//!
//! 名额**先占后发**：推送失败不重试、不当轮再刷一遍（与简报推送同一个口径）。失败本身会
//! 进日志与接管动态，所以「没送到」也是可查的，不是静默。
//!
//! ## 开关
//!
//! 与本项目其它通知一致：**只认 `webhook_url`**，配了就推、没配就不推（没有「通知开关」这一层）。
//! 没配地址时**不占冷却名额** —— 否则用户哪天配好 webhook，接下来几小时反而什么都收不到。

use crate::accounts;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 冷却表文件名（数据目录下）。
const STATE_FILE: &str = "alert_state.json";
/// 日志与接管动态里给这类事件用的名字（不是账号名，与「凭证池」「账号数据」同一类）。
const CHANNEL: &str = "续签告警";

/// 普通失败的冷却：与临时失败「一个巡检周期后重试」的节奏相配，一天最多推四条。
pub const COOLDOWN_MS: i64 = 6 * 3600 * 1000;
/// 死链（`needs_login`）的冷却：这类结论不会自己变，一天提醒一次就够。
pub const DEAD_COOLDOWN_MS: i64 = 24 * 3600 * 1000;
/// 不可逆事件（新票没落盘）的冷却：发生概率极低，真出了事就别靠冷却攒批。
pub const CRITICAL_COOLDOWN_MS: i64 = 3600 * 1000;

/// 冷却判定（纯函数，便于单测）。
///
/// 「时间往回走」（`now_ms < last_ms`，改系统时钟 / 时区）算**到期**：宁可多推一条，
/// 也不能因为记账比现在新就永远静默 —— 静默正是这一整条链路要治的病。
pub fn due(now_ms: i64, last_ms: Option<i64>, cooldown_ms: i64) -> bool {
    match last_ms {
        Some(t) if now_ms > t && now_ms - t < cooldown_ms => false,
        _ => true,
    }
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// key → 上次占用告警名额的时刻（毫秒）
    #[serde(default)]
    alerts: HashMap<String, i64>,
}

/// 进程内的名额表：`alert_state.json` 写不进去时仍然挡住同一进程里的重复推送。
///
/// 为什么不能只靠盘上的表：记账失败 → 判定永远「到期」→ 每轮巡检都推一条，
/// 而这恰好发生在磁盘出问题时（也是最不该刷屏的时候）。
fn memory() -> &'static Mutex<HashMap<String, i64>> {
    static CELL: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

fn read_state(dir: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(state_path(dir)) else {
        return State::default();
    };
    // 读坏了当空表：代价是「这条 key 再推一次」，而不是从此静默。
    serde_json::from_str(&text).unwrap_or_default()
}

/// 占用告警名额：`true` 表示本次可以推（并已记账）。
fn claim(dir: &Path, key: &str, now_ms: i64, cooldown_ms: i64) -> bool {
    let mem_last = memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .copied();
    let disk_last = read_state(dir).alerts.get(key).copied();
    // 两份账取**较新**的那个：盘上是跨重启的事实，内存是落盘失败时的兜底
    let last = match (mem_last, disk_last) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if !due(now_ms, last, cooldown_ms) {
        return false;
    }
    if let Ok(mut m) = memory().lock() {
        m.insert(key.to_string(), now_ms);
    }
    let mut st = read_state(dir);
    st.alerts.insert(key.to_string(), now_ms);
    if let Err(e) = write_state(dir, &st) {
        // 落盘失败只影响「重启之后」的去重，本进程的内存表已经占上名额了
        crate::logs::push(CHANNEL, false, format!("告警记账落盘失败：{e}"));
    }
    true
}

fn write_state(dir: &Path, st: &State) -> Result<(), String> {
    let json = serde_json::to_string_pretty(st).map_err(|e| e.to_string())?;
    // 走 [`accounts::write_atomic`]：这份表是「读全量 → 重写整本」的，就地截断写一旦
    // 在窗口里被杀就会把**所有**冷却记录抹平，于是下一轮每个 key 都算「没推过」。
    accounts::write_atomic(&state_path(dir), &json)
}

/// 推一条告警（异步发出，**不阻塞调用方**）。
///
/// 调用方不需要判断开关、冷却，也不需要处理错误 —— 判定只有「该不该说」，
/// 送到没送到由本函数的日志负责。
pub(crate) fn notify(dir: &Path, key: &str, cooldown_ms: i64, title: &str, message: &str) {
    let webhook = accounts::load_settings(dir).webhook_url;
    let webhook = webhook.trim();
    if webhook.is_empty() {
        // 没配地址：不占名额、也不记日志（失败本身已由调用点记过），配好之后立刻能收到
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    if !claim(dir, key, now, cooldown_ms) {
        return;
    }
    let (url, title, body) = (webhook.to_string(), title.to_string(), message.to_string());
    let data_dir = dir.to_path_buf();
    // 通知**永远只是尽力而为**：spawn 出去，绝不 block_on —— 这条链的调用点里有 async 上下文
    // （`broker::sync`），在那里 block_on 会直接把运行时卡住。
    tauri::async_runtime::spawn(async move {
        match crate::notify::send(&url, &title, &body).await {
            Ok(resp) => {
                crate::logs::push(CHANNEL, true, format!("告警已推送（{title}）：{resp}"));
            }
            Err(e) => {
                // 告警自己送不出去是要紧事：这正是用户唯一能知道「链断了」的通道。
                // 除了日志，再补一条接管动态 —— 内存日志只有 200 条上限，等用户想起来查它时
                // 早被巡检噪音刷掉了（2026-10-13 那次就是这么没人看见的）。
                crate::logs::push(CHANNEL, false, format!("告警推送失败（{title}）：{e}"));
                crate::journal::append_dedup(
                    &data_dir,
                    "alert_send_failed",
                    &format!("{title}：{e}"),
                );
            }
        }
    });
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

    /// 冷却判据本身：**到期才允许再推**，而「从没推过」必须算到期。
    ///
    /// 反过来（把 `None` 当成没到期）的结局是新机器永远不会有任何告警 ——
    /// 那正是这次要治的病。
    #[test]
    fn due_treats_never_alerted_and_clock_skew_as_ready() {
        let now = 1_700_000_000_000;
        assert!(due(now, None, COOLDOWN_MS), "没推过就该可以推");
        assert!(!due(now, Some(now - 1000), COOLDOWN_MS), "冷却期内不得重复推");
        assert!(due(now, Some(now - COOLDOWN_MS), COOLDOWN_MS), "冷却满格要放行");
        assert!(due(now, Some(now), COOLDOWN_MS), "同一毫秒算到期（宁可多推）");
        // 系统时钟被往回调：记账比现在新。按「没到期」会永久静默，按「到期」只多推一条
        assert!(due(now, Some(now + 60_000), COOLDOWN_MS), "时钟回拨不得导致静默");
    }

    /// 名额要**跨进程**生效：只记在内存里等于每次重启都把同一件事再推一遍。
    #[test]
    fn claim_persists_and_gates_within_cooldown() {
        let dir = tmp("twa_alerts_claim_test");
        let now = 1_700_000_000_000;
        // key 带本次时刻做后缀：内存表是**进程级**的，同名单元测并行跑会互相占名额
        let a = format!("renew-fail:a1-{now}");
        let b = format!("renew-fail:a2-{now}");
        assert!(claim(&dir, &a, now, COOLDOWN_MS), "第一次必须放行");
        // 直接查盘上的表，确认名额真的落了盘（而不是只在内存里自说自话）
        assert_eq!(
            read_state(&dir).alerts.get(&a).copied(),
            Some(now),
            "名额必须落盘，重启后才挡得住重复"
        );
        // 内存表此时也占着名额；判冷却以两份账里较新的为准
        assert!(
            !claim(&dir, &a, now + 1000, COOLDOWN_MS),
            "冷却期内同一 key 不得再推"
        );
        // 换 key 不受影响：一个账号失败不该把别的账号的告警一起挡掉
        assert!(claim(&dir, &b, now + 1000, COOLDOWN_MS));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没配 webhook 时**不占名额**：否则用户配好地址反而要再等一个冷却期。
    #[test]
    fn notify_without_webhook_claims_nothing() {
        let dir = tmp("twa_alerts_no_webhook_test");
        let _ = std::fs::remove_file(state_path(&dir));
        notify(&dir, "renew-fail:a1", COOLDOWN_MS, "自动续签失败", "正文");
        assert!(
            !state_path(&dir).exists(),
            "没配地址时不该写出冷却表（名额没被占用）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏掉的状态文件不得让整条告警链失效。
    #[test]
    fn corrupt_state_degrades_to_alerting_not_silence() {
        let dir = tmp("twa_alerts_corrupt_test");
        std::fs::write(state_path(&dir), "{ not json").unwrap();
        let now = 1_700_000_000_000;
        let key = format!("renew-fail-corrupt-{}", now);
        assert!(claim(&dir, &key, now, COOLDOWN_MS), "读不懂的账本按空表处理");
        // 重写之后应当恢复正常：同一 key 立刻再 claim 应被冷却挡住
        assert!(!claim(&dir, &key, now + 1000, COOLDOWN_MS));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
