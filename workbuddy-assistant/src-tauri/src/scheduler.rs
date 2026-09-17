//! 定时自动签到：应用常驻时，每天在用户设定的时刻执行一次「全部签到」并推送通知。
//!
//! 触发模型：**到点即触发 + 补跑窗口**，而不是「当前分钟恰好等于设定值」。
//! 后者有两个必踩的坑：
//!   1. 轮询粒度做不到精确命中某一分钟（30s 一跳也可能整分钟落在两跳之间）；
//!   2. 应用常常是在预定时刻之后才被打开的（比如 09:10 才开机）。
//! 所以判定条件是「今天还没跑过 && 已过设定时刻但不超过 30 分钟」。
//!
//! 设定时刻是**窗口起点**：`schedule_window_minutes > 0` 时，当天实际触发时刻会在
//! `[设定时刻, +窗口]` 内随机挑一分钟，挑完落盘、当天不再变（见 [`target_time`]）。
//! 固定在同一分钟触发是脚本最好认的特征，窗口把它抹掉；`window = 0` 即回到
//! 「精确到分钟」的老行为。
//!
//! 窗口一开，「设定时刻」就不再是「今天的签到时刻」了 —— 用户配的 08:00 只是窗口起点。
//! 所以摇定当天时刻后会**先推一条预告**（[`preview_time`] + [`notify::target_preview`]），
//! 签到结果的通知里也带上真实触发时刻（[`notify::trigger_line`]）。两处合起来，
//! 「今天到底几点签的」在任何时候都答得出来。
//!
//! 跨启动去重靠 `schedule_state.json`（只记最后一次执行的日期），
//! 这样重启应用不会在补跑窗口内重复签一遍。
//!
//! 自动续签：常驻期间每 12 小时扫一遍账号，**剩余有效期不足 48 小时就静默续一次**；
//! 启动后也会立刻扫一次（久未开应用的情况靠它兜住）。
//! 与定时签到开关无关——续签是保命操作，不该被「没设定时签到」连带关掉。
//! 扫描间隔落盘在 `schedule_state.json`，避免每次启动/每跳都打接口。
//!
//! 通知只在 `notify_enabled && notify_on_schedule` 时发送，且**失败不影响签到**。
//!
//! 除定时签到外，本模块还管两件事，各自有独立开关，互不影响：
//!
//! - 自动续签（见 [`maybe_auto_refresh`]）：剩余有效期不足 48 小时就静默续一次。
//! - **积分简报**（见 [`maybe_seal_briefing`]）：每小时采一次样，把已经走完的小时
//!   固化成「时条目」，并按 `notify_on_briefing` 每天推一条当天汇总。
//!   它**没有随机时间窗** —— 触发时刻就是「哪一个小时」这件事本身，抖动会让同一段
//!   增量前后归到不同的小时里。统计口径见 [`crate::briefing`] 与 [`crate::ledger`]。

use crate::accounts::{self, Settings};
use crate::briefing;
use crate::commands;
use crate::notify;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 轮询间隔：30s 足够（补跑窗口有 30 分钟余量），开销可忽略
const TICK: Duration = Duration::from_secs(30);
/// 补跑窗口：错过设定时刻后仍愿意补签的时长
const CATCH_UP_MINUTES: i64 = 30;
/// 自动续签的扫描间隔：token 有效期 60 天，12 小时一跳足够从容
const REFRESH_SCAN_MS: i64 = 12 * 60 * 60 * 1000;
/// 每小时第几分钟起可以采样。
///
/// 台账把增量归入**采样时刻所属的那个小时**，所以想让整个小时的量落进这一小时，
/// 采样就必须赶在整点之前完成。定成「最后 5 分钟」而不是「最后一分钟」：
/// 30s 一跳只要有一次落在窗口里就够，窗口太窄时一次系统卡顿就会整小时没有采样。
const SAMPLE_AT_MINUTE: i64 = 55;
/// 定时任务进度事件，前端据此刷新列表并提示
pub const EVENT: &str = "checkin-scheduled";
/// 自动续签事件（payload：续签成功的账号数），前端据此刷新列表
pub const REFRESH_EVENT: &str = "auto-refreshed";
/// 积分简报固化完成事件，前端据此刷新简报列表
pub const BRIEFING_EVENT: &str = "credit-briefing-sealed";

/// 当天随机挑定的定时签到时刻。
///
/// 必须落盘：否则每 30s 一跳都会重摇一次（「等时钟走过刚摇出来的那一分钟」这种判定
/// 在每跳都换目标时形同虚设），重启也会把当天已经用过的时刻换掉。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct DayTarget {
    /// 归属日期（`YYYY-MM-DD`）
    date: String,
    /// 当天实际触发时刻（`HH:MM`）
    at: String,
}

#[derive(Serialize, Deserialize, Default)]
struct ScheduleState {
    /// 最后一次定时执行的日期（`YYYY-MM-DD`）
    #[serde(default)]
    last_run_date: Option<String>,
    /// 最后一次自动续签扫描的时刻（毫秒时间戳）
    #[serde(default)]
    last_refresh_scan_ms: Option<i64>,
    /// 今天随机挑定的触发时刻（跨天自动重挑；见 [`target_time`]）
    #[serde(default)]
    today_target: Option<DayTarget>,
    /// 已经推送过预告的那个目标时刻（见 [`preview_time`]）
    ///
    /// 与 `today_target` 分开记：前者是「今天几点签」，这里是「这点**已经告诉过用户**」。
    /// 合并成一个字段就没法区分「摇定了」和「通知过了」，重启或每 30s 一跳都会重推。
    /// 时刻被重新摇过（用户改了设置）时它自然不再相等，于是会补推新时刻。
    #[serde(default)]
    notified_target: Option<DayTarget>,
    /// 最后一次积分简报采样所属的小时（`YYYY-MM-DD HH`）
    ///
    /// 用来保证**每小时只采一次**：采样窗口有 5 分钟、轮询 30s 一跳，
    /// 没有这个标记就会在窗口里连采十来次（每次都打一遍接口）。
    #[serde(default)]
    last_sample_hour: Option<String>,
    /// 最后一次推送简报的日期（`YYYY-MM-DD`）
    ///
    /// 推送粒度是**天**（时条目每小时结算，但「今天花了多少」要等当天结束才有定论），
    /// 所以每天最多一条。先落盘再推送：推送失败也不该让同一份简报反复重推。
    #[serde(default)]
    last_briefing_push_date: Option<String>,
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("schedule_state.json")
}

fn load_state(dir: &Path) -> ScheduleState {
    fs::read_to_string(state_file(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(dir: &Path, st: &ScheduleState) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let target = state_file(dir);
    let tmp = dir.join("schedule_state.json.tmp");
    let Ok(json) = serde_json::to_string_pretty(st) else {
        return;
    };
    if fs::write(&tmp, json).is_ok() && fs::rename(&tmp, &target).is_ok() {
        accounts::set_private_permissions(&target);
    }
}

/// 今天该在几点几分触发。
///
/// - 已经为**同一个日期**挑过、且旧目标仍落在当前窗口内 → 原样复用；
/// - 否则在 `[base, base + window]` 内随机取一分钟；
/// - `window == 0`（或 base 非法）→ 直接用 base，等于关掉随机、保留「精确到分钟」的老行为。
///
/// 窗口会被截断在当天 23:59 之内，绝不跨天——跨天会让 [`due_at`] 的「补跑窗口」
/// 判定失去意义（今天的任务跑到明天零点后触发）。
pub(crate) fn target_time(
    date: &str,
    base: &str,
    window_minutes: u32,
    existing: Option<&DayTarget>,
) -> DayTarget {
    if let Some(t) = existing.filter(|t| t.date == date && in_window(t, base, window_minutes)) {
        return t.clone();
    }
    let at = chrono::NaiveTime::parse_from_str(base.trim(), "%H:%M")
        .ok()
        .filter(|_| window_minutes > 0)
        .map(|t| {
            let base_min = t.hour() * 60 + t.minute();
            // 上限 23:59，保证不跨天
            let span = window_minutes.min(24 * 60 - 1 - base_min);
            let total = base_min + crate::rng::range(0, span as u64) as u32;
            format!("{:02}:{:02}", total / 60, total % 60)
        })
        .unwrap_or_else(|| base.trim().to_string());
    DayTarget {
        date: date.to_string(),
        at,
    }
}

/// 旧目标是否仍落在 `[base, base + window]` 内。
///
/// 用它判断「设置改过了要不要重挑」：只按日期复用的话，用户把签到时刻从 09:00
/// 改到 11:00 之后，那个 10:12 的旧目标既不在新窗口里、又已经过期（按补跑窗口
/// 立刻触发一次），等于改了设置反而提前签了。
fn in_window(target: &DayTarget, base: &str, window_minutes: u32) -> bool {
    let (Ok(at), Ok(b)) = (
        chrono::NaiveTime::parse_from_str(target.at.trim(), "%H:%M"),
        chrono::NaiveTime::parse_from_str(base.trim(), "%H:%M"),
    ) else {
        // 两边都解析不出来时退化成字面比较：不相等就重挑
        return target.at.trim() == base.trim();
    };
    let mins = |t: chrono::NaiveTime| t.hour() as i64 * 60 + t.minute() as i64;
    let (at_m, base_m) = (mins(at), mins(b));
    at_m >= base_m && at_m <= base_m + window_minutes as i64
}

/// 今天是否该推「随机时刻预告」，该预告哪个时刻。
///
/// 只在**窗口真的摇出了另一个时刻、且这个时刻还没到**时推。四条排除理由：
///
/// - `window == 0`，或摇号恰好停在 `base` 上：此时「设定时刻」本身就是答案，
///   再推一条纯属噪音。
/// - `now < base`：窗口还没开。当天的目标是在**零点后第一跳**就算出来的，
///   不加这条就会在半夜给人发推送。
/// - `now >= target`：已经到点（或早过了），这不再是「预告」；真实时刻由签到结果
///   那条通知里的 [`notify::trigger_line`] 交代，不必提前废掉一条。
/// - 同一个目标已经推过：重启、每 30s 一跳都不能重复推。
///
/// 返回 `Some(时刻)` = 该推这条预告。
pub(crate) fn preview_time(
    base: &str,
    window_minutes: u32,
    target: &DayTarget,
    notified: Option<&DayTarget>,
    now: chrono::NaiveDateTime,
) -> Option<String> {
    if window_minutes == 0 || notified == Some(target) {
        return None;
    }
    let (Ok(base_t), Ok(target_t)) = (
        chrono::NaiveTime::parse_from_str(base.trim(), "%H:%M"),
        chrono::NaiveTime::parse_from_str(target.at.trim(), "%H:%M"),
    ) else {
        // 时刻非法（`due_at` 反正也不会触发）：不在这里瞎猜一个点出来
        return None;
    };
    if target_t == base_t {
        return None;
    }
    let cur = now.time();
    if cur < base_t || cur >= target_t {
        return None;
    }
    Some(target.at.clone())
}

/// 距上次自动续签扫描是否已满一个间隔（从未扫过 → 立刻扫）。
fn refresh_scan_due(last_scan_ms: Option<i64>, now_ms: i64) -> bool {
    match last_scan_ms {
        Some(last) => now_ms - last >= REFRESH_SCAN_MS,
        None => true,
    }
}

/// 今天是否该跑了：设定时刻已过（但不超过补跑窗口），且今天还没跑过。
///
/// 抽成纯函数以便单测——时间判断最容易在边界上出错（跨天、未来时刻、重复跑）。
///
/// **只服务定时签到。** 简报不走这里：它按「整点」结算，而 `NaiveTime`
/// （合法范围到 `23:59:59`）表示不了 24:00 这种边界；简报改用「这一小时采过没有」
/// 的判定，见 [`maybe_seal_briefing`]。
pub(crate) fn due_at(
    now: chrono::NaiveDateTime,
    time: &str,
    last_run_date: Option<&str>,
) -> bool {
    let Ok(t) = chrono::NaiveTime::parse_from_str(time.trim(), "%H:%M") else {
        return false; // 时刻非法时宁可不跑
    };
    let today = now.date();
    if let Some(d) = last_run_date {
        if d == today.format("%Y-%m-%d").to_string() {
            return false;
        }
    }
    let mins = (now - today.and_time(t)).num_minutes();
    (0..=CATCH_UP_MINUTES).contains(&mins)
}

/// 启动后台调度线程。线程与进程同生命周期，无需 join。
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        // 启动先扫一次续签：等 app 初始化完成（数据目录就绪）再动手
        std::thread::sleep(Duration::from_secs(5));
        if let Ok(dir) = commands::try_data_dir(&app) {
            maybe_auto_refresh(&app, &dir);
            // 启动就把简报补一遍。固化只读台账、不需要网络，而应用关着的那些小时
            // 只有在这一刻才补得出来；顺带采一次样，让「上次运行到现在」的增量
            // 落进当前小时（用户一打开就能看到今天的最新数字，而不是等下一个整点）。
            let settings = accounts::load_settings(&dir);
            maybe_seal_briefing(&app, &dir, &settings, true);
        }

        loop {
            std::thread::sleep(TICK);

            // 每跳都重读配置：用户在设置里改了时刻/开关即刻生效，不需要重启应用
            let Ok(dir) = commands::try_data_dir(&app) else {
                continue;
            };
            let settings = accounts::load_settings(&dir);

            // 自动续签与「定时签到」开关无关，单独判定
            maybe_auto_refresh(&app, &dir);

            // 积分简报同理：有自己的开关，不受「定时签到」影响。
            // 必须放在下面那个 continue 之前 —— 否则用户一关定时签到，简报也顺带没了
            maybe_seal_briefing(&app, &dir, &settings, false);

            if !settings.schedule_enabled {
                continue;
            }
            let mut state = load_state(&dir);
            let now = chrono::Local::now().naive_local();
            let today = now.date().format("%Y-%m-%d").to_string();
            // 当天目标时刻：第一次算出来后立刻落盘，之后每一跳都复用同一个值，
            // 否则「随机窗口」每跳重摇一次，等于没加（详见 DayTarget）
            let target = target_time(
                &today,
                &settings.schedule_time,
                settings.schedule_window_minutes,
                state.today_target.as_ref(),
            );
            if state.today_target.as_ref() != Some(&target) {
                state.today_target = Some(target.clone());
                save_state(&dir, &state);
                log_event(
                    &dir,
                    &format!(
                        "今日签到目标已定：{}（设定 {}，随机时间窗 {} 分钟）",
                        target.at, settings.schedule_time, settings.schedule_window_minutes
                    ),
                );
            }
            // 窗口一开，「设定时刻」就不再是今天的签到时刻了：摇定的那一刻先推一条，
            // 否则用户只能等签完之后才知道今天定在了几点。
            if let Some(at) = preview_time(
                &settings.schedule_time,
                settings.schedule_window_minutes,
                &target,
                state.notified_target.as_ref(),
                now,
            ) {
                // 通知关着时**不记账**：这样用户中途打开通知仍能收到这条（否则白等一天）。
                // 判定本身是纯函数、不打接口，每 30s 重算一次没有代价。
                if settings.notify_enabled && settings.notify_on_schedule {
                    state.notified_target = Some(target.clone());
                    save_state(&dir, &state);
                    let outcome = match tauri::async_runtime::block_on(notify::send(
                        &settings.notify_webhook,
                        &notify::target_preview(
                            &at,
                            &settings.schedule_time,
                            settings.schedule_window_minutes,
                        ),
                    )) {
                        Ok(resp) => format!("今日签到时刻预告已发送（{at}）：{resp}"),
                        Err(e) => format!("今日签到时刻预告发送失败（{at}）：{e}"),
                    };
                    log_event(&dir, &outcome);
                }
            }
            if !due_at(now, &target.at, state.last_run_date.as_deref()) {
                continue;
            }

            // 先落盘「今天已跑」再执行：万一执行中崩溃，也不会在补跑窗口里反复重试
            state.last_run_date = Some(today.clone());
            save_state(&dir, &state);
            let actual = now.format("%H:%M").to_string();
            log_event(
                &dir,
                &format!(
                    "触发定时签到（设定 {}，今日随机目标 {}，实际 {}）",
                    settings.schedule_time, target.at, actual
                ),
            );
            // 真实触发时刻随通知一起发出去：开了窗口之后，通知里不写这一栏
            // 就没人知道今天究竟是几点签的（见 [`notify::trigger_line`]）
            let trigger = notify::trigger_line(&actual, &target.at, &settings.schedule_time);
            tauri::async_runtime::block_on(run_once(&app, &settings, &dir, &trigger));
        }
    });
}

/// 自动续签扫描：距上次扫描超过 `REFRESH_SCAN_MS` 才真跑。
///
/// 先落盘扫描时刻再执行——续签接口可能耗时/失败，先记账可避免同一时刻被反复触发。
fn maybe_auto_refresh(app: &AppHandle, dir: &Path) {
    let mut state = load_state(dir);
    let now_ms = chrono::Utc::now().timestamp_millis();
    if !refresh_scan_due(state.last_refresh_scan_ms, now_ms) {
        return;
    }
    state.last_refresh_scan_ms = Some(now_ms);
    save_state(dir, &state);

    match tauri::async_runtime::block_on(commands::auto_refresh_all(app)) {
        Ok(report) => {
            if !report.refreshed.is_empty() {
                log_event(dir, &format!("自动续签完成：{}", report.refreshed.join("、")));
                let _ = app.emit(
                    REFRESH_EVENT,
                    serde_json::json!({ "count": report.refreshed.len() }),
                );
            }
            for f in &report.failed {
                log_event(dir, &format!("自动续签失败：{f}"));
            }
        }
        Err(e) => log_event(dir, &format!("自动续签异常：{e}")),
    }
}

/// 这一跳的采样决定。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SamplePlan {
    /// 要不要真去采一次样
    pub sample: bool,
    /// 要不要占掉「本小时已采过」的名额（`Some(key)` = 写进 `last_sample_hour`）
    pub mark_hour: Option<String>,
}

/// 采样判定（纯函数，便于单测）。
///
/// 两条规则合起来才既做到「每小时一次」，又让**这一小时的桶是完整的一小时**：
///
/// 1. **窗口采样**（`minute >= SAMPLE_AT_MINUTE`）：每小时最多一次，并占掉本小时的名额。
///    采样必须赶在整点之前 —— 台账把增量归入「采样时刻所属的小时」，想让整个小时的量
///    落进这一小时就得在本小时里采；采完记下 `last_sample_hour`，否则 5 分钟窗口里
///    会被 30s 一跳连采十来次。
/// 2. **启动补采**（`at_startup`）：应用刚起来顺带采一次，让用户一打开就看到最新数字。
///    它**不占本小时的名额** —— 否则「15:03 启动」会把 15:55 那次窗口采样挡掉，
///    于是 `15:03 → 16:55` 这一大段增量整块落进 16 点的桶，从启动那一小时起就错位。
///    代价只是同一小时里多采一次（增量本来就累加到同一个桶里）。
pub(crate) fn sample_plan(
    at_startup: bool,
    minute: i64,
    cur_key: &str,
    last_sample_hour: Option<&str>,
) -> SamplePlan {
    if minute >= SAMPLE_AT_MINUTE {
        if last_sample_hour == Some(cur_key) {
            // 本小时的窗口采样已经做过了（启动补采若落在窗口内，也会顺带把名额占上）
            return SamplePlan { sample: false, mark_hour: None };
        }
        return SamplePlan {
            sample: true,
            mark_hour: Some(cur_key.to_string()),
        };
    }
    SamplePlan {
        sample: at_startup,
        mark_hour: None,
    }
}

/// 积分简报：**每小时结算一次**（时条目），并按设置每天推一条当天汇总。
///
/// 两件事，各自独立判定：
///
/// 1. **采样**（[`commands::fetch_samples`] + [`commands::apply_samples`]）：安排在每小时的**最后几分钟**
///    （`SAMPLE_AT_MINUTE` 之后，判定见 [`sample_plan`]）。这不是随手定的时刻 ——
///    台账的规则是「增量归入采样时刻所属的那个小时」，所以想让整个小时的量都落进这一小时，
///    采样就必须赶在整点之前完成。`at_startup` 为真时（应用刚起来）在窗口外也采一次：
///    用户一打开就该看到今天的最新数字，而不是干等到下一个整点 —— 但那次**不占**本小时的
///    窗口名额，否则启动那一小时会把整点前的采样挡掉，增量错位到下一小时。
/// 2. **固化**（[`commands::seal_hours`]）：把所有「已经走完、台账里有数据、
///    还没固化」的小时变成时条目。它只读台账，**不依赖网络**，所以即便这一轮采样
///    全失败，之前采到的部分照样能固化；应用关了两天再打开，那两天也补得出来
///    （桶留 60 天）。固化是幂等的，每跳都跑一遍没有副作用。
///
/// 代价与边界：**应用没运行的时段不会采样**，那几格既不会产生时条目、日条目里也没有
/// 那一块。恢复运行后的第一次采样会把这段空白期攒下的增量整块记进「恢复后的那个小时」
/// —— 这是台账的既有口径（界面上的说明照实写了这一条），丢掉它会让总消耗少算。
///
/// 推送只推**已经走完的那一天**：时条目每小时都在结算，但「今天花了多少」要等当天
/// 结束才有定论，每小时推一条只会把通知刷成流水账。
fn maybe_seal_briefing(app: &AppHandle, dir: &Path, settings: &Settings, at_startup: bool) {
    if !settings.briefing_enabled {
        return;
    }
    let Ok(data_dir) = commands::try_data_dir(app) else {
        log_event(dir, "积分简报跳过：数据目录不可用");
        return;
    };
    let accounts = accounts::load_accounts(&data_dir);
    if accounts.is_empty() {
        return;
    }

    let now = chrono::Local::now().naive_local();
    let today = now.date().format("%Y-%m-%d").to_string();
    let hour = now.hour() as u8;
    let now_s = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let cur_key = format!("{today} {hour:02}");

    let mut state = load_state(dir);
    // 采样判定见 [`sample_plan`]：窗口内每小时一次，**启动补采不占本小时的窗口名额**
    let plan = sample_plan(
        at_startup,
        now.minute() as i64,
        &cur_key,
        state.last_sample_hour.as_deref(),
    );
    if let Some(mark) = plan.mark_hour {
        // 先落盘采样标记再真采：采样要打接口、可能耗时或失败，
        // 先记账可避免同一个小时里反复触发（窗口有 5 分钟、轮询 30s 一跳）
        state.last_sample_hour = Some(mark);
        save_state(dir, &state);
    }
    if plan.sample {
        // 采集（打接口）在锁外完成，入账一次性写进**唯一那本内存台账**并广播给界面 ——
        // 见 `commands::fetch_samples` / `commands::apply_samples`。
        // 这里用正常记账（不是 baseline）：断档期攒下的量虽然归不到具体的小时，
        // 但它**是真的消耗**，丢掉会让总账少算。归到「恢复后的那个小时」是最不坏的归属。
        let (at, readings) = tauri::async_runtime::block_on(commands::fetch_samples(&accounts));
        commands::apply_samples(app, &readings, &at, false);
    }

    let sealed = commands::seal_hours(&data_dir, &accounts, &today, hour, &now_s);
    if sealed.is_empty() {
        return;
    }
    log_event(dir, &format!("积分简报：固化 {} 个小时条目", sealed.len()));
    let _ = app.emit(
        BRIEFING_EVENT,
        serde_json::json!({ "hours": sealed.len() }),
    );

    // 每天最多推一条，且只推最近一个**已经走完**的日子。
    // 先落盘推送日期再推：推送失败也不该让同一份简报反复重推。
    let days = briefing::day_entries(&briefing::load(&data_dir), &today);
    let Some(day) = days.iter().find(|d| d.sealed) else {
        return;
    };
    if state.last_briefing_push_date.as_deref() == Some(day.date.as_str()) {
        return;
    }
    state.last_briefing_push_date = Some(day.date.clone());
    save_state(dir, &state);
    if !(settings.notify_enabled && settings.notify_on_briefing) {
        return;
    }
    let outcome = match tauri::async_runtime::block_on(notify::send(
        &settings.notify_webhook,
        &briefing::message(day),
    )) {
        Ok(resp) => format!("简报通知已发送（{}）：{resp}", day.date),
        Err(e) => format!("简报通知发送失败（{}）：{e}", day.date),
    };
    log_event(dir, &outcome);
}

/// 调度日志（`scheduler.log`）：只记「触发 / 通知结果 / 异常」这类有排查价值的事件。
///
/// 定时任务最大的失败模式是**静默不生效**（应用没开、时刻写错、webhook 被墙），
/// 所以留一份可查的记录；逐 tick 的判定不写进来，避免日志噪音与无限增长。
pub(crate) fn log_event(dir: &Path, msg: &str) {
    const MAX_LINES: usize = 200;

    let target = dir.join("scheduler.log");
    let old = fs::read_to_string(&target).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().collect();
    if lines.len() >= MAX_LINES {
        lines.drain(0..lines.len() - MAX_LINES + 1);
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&format!(
        "[{}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        msg
    ));

    if fs::create_dir_all(dir).is_err() {
        return;
    }
    if fs::write(&target, body).is_ok() {
        accounts::set_private_permissions(&target);
    }
}

/// 执行一次「全部签到」并推送结果。
///
/// `trigger` 是本次触发时刻的说明（[`notify::trigger_line`]），拼在通知正文之后 ——
/// 随机窗口 / 补跑都会让真实时刻与用户配置的值对不上，通知里得说清楚是哪个。
async fn run_once(app: &AppHandle, settings: &Settings, dir: &Path, trigger: &str) {
    let _ = app.emit(EVENT, serde_json::json!({ "stage": "start" }));

    let notify_on = settings.notify_enabled && settings.notify_on_schedule;
    match commands::checkin_all_inner(app, true).await {
        Ok(views) => {
            // 与 notify 一致：「已签」与「成功」互斥计数（已签的响应 success 也是 true）。
            // 视图里的账号那一半就是签到结果，积分读数在这里不参与统计。
            let already = views
                .iter()
                .filter(|v| matches!(&v.account.last, Some(r) if r.already))
                .count();
            let ok = views
                .iter()
                .filter(|v| matches!(&v.account.last, Some(r) if r.success && !r.already))
                .count();
            let msg = format!("签到完成：{} 个账号，成功 {ok} / 已签 {already}", views.len());
            log_event(dir, &msg);
            let _ = app.emit(
                EVENT,
                serde_json::json!({ "stage": "done", "count": views.len() }),
            );
            if notify_on {
                let body = format!("{}\n{trigger}", notify::summary_message(&views));
                // 通知失败只记日志，绝不影响签到结果
                let outcome = match notify::send(&settings.notify_webhook, &body).await {
                    Ok(resp) => format!("通知已发送：{resp}"),
                    Err(e) => format!("通知发送失败：{e}"),
                };
                log_event(dir, &outcome);
            }
        }
        Err(e) => {
            log_event(dir, &format!("签到异常：{e}"));
            let _ = app.emit(EVENT, serde_json::json!({ "stage": "error", "message": e }));
            if notify_on {
                let _ = notify::send(
                    &settings.notify_webhook,
                    &format!("WorkBuddy 定时签到异常：{e}\n{trigger}"),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 12)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    #[test]
    fn fires_at_the_configured_minute() {
        assert!(due_at(at(9, 7), "09:07", None));
        // 轮询粒度落在同一分钟内的任意秒都算数
        let t = NaiveDate::from_ymd_opt(2026, 9, 12)
            .unwrap()
            .and_hms_opt(9, 7, 45)
            .unwrap();
        assert!(due_at(t, "09:07", None));
    }

    #[test]
    fn catches_up_when_app_starts_late() {
        // 09:30 才打开应用，仍在 30 分钟补跑窗口内
        assert!(due_at(at(9, 30), "09:07", None));
        // 超出窗口就不再补，避免晚上开应用时突然签一次
        assert!(!due_at(at(9, 38), "09:07", None));
        assert!(!due_at(at(20, 0), "09:07", None));
    }

    #[test]
    fn does_not_fire_before_the_time_or_twice_a_day() {
        assert!(!due_at(at(9, 6), "09:07", None));
        // 同一天已跑过 → 不再补
        assert!(!due_at(at(9, 20), "09:07", Some("2026-09-12")));
        // 昨天的记录不影响今天
        assert!(due_at(at(9, 7), "09:07", Some("2026-09-11")));
    }

    #[test]
    fn tolerates_loose_or_invalid_time() {
        assert!(due_at(at(9, 7), " 09:07 ", None));
        for bad in ["", "9", "24:00", "aa:bb", "09:07:00"] {
            assert!(!due_at(at(9, 7), bad, None), "{bad:?} 不应触发");
        }
    }

    /// 启动补采**不许**占掉本小时的窗口名额。
    ///
    /// 这就是「简报没按每小时一条」的第二处坑：原来 `last_sample_hour` 在启动采样时也会被
    /// 写成本小时，于是 H 点启动（且没到 H:55）会让 H:55 那次窗口采样被自己挡掉，
    /// `H:03 → H+1:55` 整段增量落进 H+1 的桶 —— 从启动那一小时起，每个「小时」都不再是小时。
    #[test]
    fn a_startup_sample_never_consumes_the_hours_window_slot() {
        // 15:03 启动：采一次，但不占「15 点已采过」的名额
        let p = sample_plan(true, 3, "2026-09-16 15", Some("2026-09-16 14"));
        assert!(p.sample, "启动该补采一次");
        assert_eq!(p.mark_hour, None, "但不许占掉 15 点的窗口名额");

        // 因此 15:55 的窗口采样照旧发生 —— 15 点这一格的桶才是完整的
        let p = sample_plan(false, 55, "2026-09-16 15", Some("2026-09-16 14"));
        assert!(p.sample);
        assert_eq!(p.mark_hour.as_deref(), Some("2026-09-16 15"));

        // 窗口内采过一次之后，5 分钟窗口里的后续 tick 不再重复采
        assert!(!sample_plan(false, 56, "2026-09-16 15", Some("2026-09-16 15")).sample);

        // 启动恰好落在窗口内、且本小时还没采过：采一次并占名额（不会紧接着再采一遍）
        let p = sample_plan(true, 57, "2026-09-16 15", Some("2026-09-16 14"));
        assert!(p.sample);
        assert_eq!(p.mark_hour.as_deref(), Some("2026-09-16 15"));
        // 落在窗口内但本小时已采过：不重复采
        assert!(!sample_plan(true, 57, "2026-09-16 15", Some("2026-09-16 15")).sample);

        // 窗口外、非启动：什么都不做（30s 一跳也只有窗口内那一分钟会采）
        let p = sample_plan(false, 20, "2026-09-16 15", Some("2026-09-16 14"));
        assert!(!p.sample && p.mark_hour.is_none());
    }

    #[test]
    fn scans_refresh_at_most_once_per_interval() {
        let now = 1_700_000_000_000;
        assert!(refresh_scan_due(None, now), "从未扫过应立刻扫");
        assert!(!refresh_scan_due(Some(now), now), "刚扫过不应立刻再扫");
        assert!(!refresh_scan_due(Some(now - REFRESH_SCAN_MS + 1), now));
        assert!(refresh_scan_due(Some(now - REFRESH_SCAN_MS - 1), now));
        // 时钟回拨 / 文件被手改成未来时间：视为「刚扫过」，宁可不扫也不刷接口
        assert!(!refresh_scan_due(Some(now + 1000), now));
    }

    #[test]
    fn state_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("wba-sched-{}", uuid::Uuid::new_v4()));
        assert!(load_state(&dir).last_run_date.is_none());
        assert!(load_state(&dir).today_target.is_none());
        save_state(
            &dir,
            &ScheduleState {
                last_run_date: Some("2026-09-12".into()),
                last_refresh_scan_ms: Some(1_700_000_000_000),
                today_target: Some(DayTarget {
                    date: "2026-09-12".into(),
                    at: "10:12".into(),
                }),
                notified_target: Some(DayTarget {
                    date: "2026-09-12".into(),
                    at: "10:12".into(),
                }),
                last_sample_hour: Some("2026-09-12 09".into()),
                last_briefing_push_date: Some("2026-09-13".into()),
            },
        );
        let loaded = load_state(&dir);
        assert_eq!(loaded.last_run_date.as_deref(), Some("2026-09-12"));
        assert_eq!(loaded.last_refresh_scan_ms, Some(1_700_000_000_000));
        // 当天目标必须扛得住重启，否则「随机窗口」会被重启重新摇一次
        assert_eq!(
            loaded.today_target.as_ref().map(|t| t.at.as_str()),
            Some("10:12")
        );
        // 简报的两个标记独立于签到：开关互不影响，去重也必须独立。
        // 采样标记必须扛得住重启，否则每次启动都会重采一遍（多打一轮接口）
        assert_eq!(loaded.last_sample_hour.as_deref(), Some("2026-09-12 09"));
        assert_eq!(loaded.last_briefing_push_date.as_deref(), Some("2026-09-13"));
        // 「预告已推过」也必须扛得住重启，否则每次开应用都会把今天的时刻重推一遍
        assert_eq!(
            loaded.notified_target.as_ref().map(|t| t.at.as_str()),
            Some("10:12")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_time_without_window_stays_on_the_exact_minute() {
        // 窗口 0 = 关掉随机，保持「精确到分钟」的老行为
        assert_eq!(target_time("2026-09-12", "09:07", 0, None).at, "09:07");
        // 非法时刻原样留着（due_at 会拒绝触发），不在这里瞎猜一个点
        assert_eq!(target_time("2026-09-12", "aa:bb", 90, None).at, "aa:bb");
    }

    #[test]
    fn target_time_lands_inside_the_window_and_is_stable_for_the_day() {
        let first = target_time("2026-09-12", "08:00", 90, None);
        assert!(in_window(&first, "08:00", 90), "落在窗口外：{}", first.at);
        for _ in 0..100 {
            assert_eq!(
                target_time("2026-09-12", "08:00", 90, Some(&first)).at,
                first.at,
                "同一天重复调用不该重摇"
            );
        }
        // 换一天就要重挑（值可以碰巧相同，但归属日期必须更新）
        let next = target_time("2026-09-13", "08:00", 90, Some(&first));
        assert_eq!(next.date, "2026-09-13");
        assert!(in_window(&next, "08:00", 90));
    }

    #[test]
    fn target_time_never_crosses_midnight() {
        let lo = chrono::NaiveTime::from_hms_opt(23, 30, 0).unwrap();
        let hi = chrono::NaiveTime::from_hms_opt(23, 59, 0).unwrap();
        for _ in 0..200 {
            let t = target_time("2026-09-12", "23:30", 90, None);
            let at = chrono::NaiveTime::parse_from_str(&t.at, "%H:%M").unwrap();
            assert!(at >= lo && at <= hi, "窗口被截断失败：{}", t.at);
        }
    }

    #[test]
    fn target_time_repicks_when_the_setting_moves_later() {
        // 旧目标 09:40 对应的设置是 09:00 + 90min；用户把时刻改到 11:00 后
        // 旧目标既不在新窗口里、按补跑窗口又会立刻触发一次 → 必须重挑
        let stale = DayTarget {
            date: "2026-09-12".into(),
            at: "09:40".into(),
        };
        let t = target_time("2026-09-12", "11:00", 90, Some(&stale));
        assert!(in_window(&t, "11:00", 90), "旧目标不该被复用：{}", t.at);

        // 反向：设置改早时旧目标仍在新窗口内，继续用，不必重摇
        let keep = DayTarget {
            date: "2026-09-12".into(),
            at: "10:12".into(),
        };
        assert_eq!(
            target_time("2026-09-12", "09:00", 120, Some(&keep)).at,
            "10:12"
        );
    }

    /// 窗口一开，「设定时刻」就不再是今天的签到时刻 —— 摇定后必须先告诉用户真正的那个。
    #[test]
    fn previews_the_random_time_between_window_start_and_the_target() {
        let target = DayTarget {
            date: "2026-09-12".into(),
            at: "09:42".into(),
        };
        let t = |h, m| preview_time("08:00", 90, &target, None, at(h, m));

        // 窗口已开、还没到点 → 推
        assert_eq!(t(8, 0), Some("09:42".into()), "窗口起点整点也该推");
        assert_eq!(t(9, 0), Some("09:42".into()));
        // 窗口还没开（当天的目标在零点后第一跳就算出来了）→ 不推，否则半夜发推送
        assert_eq!(t(0, 1), None);
        assert_eq!(t(7, 59), None);
        // 已经到点 / 过点 → 不推，这不是预告了（真实时刻由结果通知里那一行交代）
        assert_eq!(t(9, 42), None);
        assert_eq!(t(10, 30), None);
        // 同一目标推过一次就不再推（每 30s 一跳、每次重启都会走到这里）
        assert_eq!(
            preview_time("08:00", 90, &target, Some(&target), at(9, 0)),
            None
        );
    }

    /// 用户改了设置导致重摇 → 新时刻要补推一条；窗口关掉/没摇出新时刻 → 一条都不推。
    #[test]
    fn previews_again_only_when_the_target_actually_moves() {
        let old = DayTarget {
            date: "2026-09-12".into(),
            at: "09:42".into(),
        };
        let moved = DayTarget {
            date: "2026-09-12".into(),
            at: "10:05".into(),
        };
        // 时刻变了（用户把窗口起点从 08:00 改到 09:30）：旧的那条已经不作数了
        assert_eq!(
            preview_time("09:30", 90, &moved, Some(&old), at(9, 40)),
            Some("10:05".into())
        );

        // 窗口为 0：设定时刻就是答案，没有「另一个时刻」可预告
        assert_eq!(preview_time("08:00", 0, &target_of("09:42"), None, at(9, 0)), None);
        // 摇号恰好停在设定值上：同上
        assert_eq!(preview_time("08:00", 90, &target_of("08:00"), None, at(8, 30)), None);
        // 时分解析不出来：不瞎猜，交给 `due_at` 去拒绝触发
        for bad in ["", "9", "24:00", "aa:bb"] {
            assert_eq!(
                preview_time(bad, 90, &target_of("09:42"), None, at(9, 0)),
                None,
                "{bad:?} 不该推出预告"
            );
            assert_eq!(
                preview_time("08:00", 90, &target_of(bad), None, at(9, 0)),
                None,
                "目标 {bad:?} 不该推出预告"
            );
        }
    }

    fn target_of(at: &str) -> DayTarget {
        DayTarget {
            date: "2026-09-12".into(),
            at: at.into(),
        }
    }

    /// 把一整天按 30s 一跳跑一遍（摇号 + 预告 + 记账都照真实顺序走）：
    /// 预告只该出现一次，且落在 `[窗口起点, 目标时刻)` 之间。
    ///
    /// 单跳判定已由上面两条覆盖；这条盯的是**记账**——漏掉某次状态回写，
    /// 每 30s 一跳就会变成每跳推一条。
    #[test]
    fn a_full_day_of_ticks_emits_exactly_one_preview() {
        let (base, window) = ("08:00", 90u32);
        let day = NaiveDate::from_ymd_opt(2026, 9, 12).unwrap();
        let mut target: Option<DayTarget> = None;
        let mut notified: Option<DayTarget> = None;
        let mut pushes: Vec<(u32, String)> = Vec::new();

        for min in 0..24 * 60 {
            let now = day.and_hms_opt(min / 60, min % 60, 0).unwrap();
            // 与 tick 循环同一顺序：先算/复用当天目标，再判要不要预告
            let t = target_time("2026-09-12", base, window, target.as_ref());
            target = Some(t.clone());
            if let Some(at) = preview_time(base, window, &t, notified.as_ref(), now) {
                notified = Some(t.clone());
                pushes.push((min, at));
            }
        }

        assert_eq!(pushes.len(), 1, "一天只该推一条预告：{pushes:?}");
        let (min, at) = &pushes[0];
        let t = target_of(at);
        assert!(in_window(&t, base, window), "预告的时刻该在窗口内：{at}");
        let t_at = chrono::NaiveTime::parse_from_str(at, "%H:%M").unwrap();
        let (start, end) = (
            chrono::NaiveTime::parse_from_str(base, "%H:%M").unwrap(),
            t_at,
        );
        let pushed_at = chrono::NaiveTime::from_hms_opt(min / 60, min % 60, 0).unwrap();
        assert!(
            pushed_at >= start && pushed_at < end,
            "预告应在 {base} 之后、{at} 之前推出，实际 {:?}",
            pushed_at
        );
    }

    #[test]
    fn log_event_appends_and_caps_length() {
        let dir = std::env::temp_dir().join(format!("wba-log-{}", uuid::Uuid::new_v4()));
        for i in 0..5 {
            log_event(&dir, &format!("事件 {i}"));
        }
        let body = fs::read_to_string(dir.join("scheduler.log")).unwrap();
        assert_eq!(body.lines().count(), 5);
        assert!(body.contains("事件 4"));
        assert!(body.contains("事件 0"), "未超上限时不应丢历史：{body}");

        // 超过上限后只保留最后 MAX_LINES 行（避免日志无限增长）
        for i in 0..205 {
            log_event(&dir, &format!("填充 {i}"));
        }
        let body = fs::read_to_string(dir.join("scheduler.log")).unwrap();
        let n = body.lines().count();
        assert!((200..=201).contains(&n), "应被裁剪到 ~200 行，实际 {n}");
        assert!(!body.contains("事件 0"), "最旧的记录应被裁掉");
        assert!(body.contains("填充 204"), "最新记录应保留");
        let _ = fs::remove_dir_all(&dir);
    }
}
