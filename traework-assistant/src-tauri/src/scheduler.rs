//! 定时签到线程：按设置里的时刻（HH:MM）每天触发一次全账号签到。
//! 只在应用运行期间生效（与进程同生命周期）。
//!
//! 顺带兼顾一件小事：**端点自愈** —— TraeWork 升级会整份替换 `product.json`，
//! 智能接管的端点改写会随之丢失，这里每 5 分钟确认一次并补写（见 `endpoint::repair`）。
//!
//! ## 为什么要「多轮补签」
//!
//! TraeWork 的领取接口（`/trae/api/v2/ug/checkin_credits/claim`）会返回
//! **9074「当前参与用户太多，请稍后再试」** —— 这是服务端对领取接口的限流，
//! 与请求参数无关，过一段时间才放行（真机实测：状态查询一直正常，
//! 领取可连续十几分钟返回 9074）。只试一次的定时任务会直接失败，
//! 所以这里在单账号内部退避重试之外，再加**整体补签轮次**。

use crate::accounts::{self, Account};
use crate::briefing;
use crate::commands;
use crate::checkin::CheckinResult;
use crate::logs;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use chrono::Timelike;
use tauri::Emitter;

/// 最多 3 轮（首轮 + 2 轮补签）。
const MAX_ROUNDS: usize = 3;
/// 轮与轮之间的间隔。
const ROUND_GAP_SECS: u64 = 600;
/// 端点自愈的常规检查间隔：TraeWork 升级会整份替换 `product.json`，改写随之丢失。
const REPAIR_OK_GAP_SECS: u64 = 300;
/// 还没就绪时的重试间隔（反代可能刚起来、文件刚被覆盖）——短一点，尽快自愈。
const REPAIR_RETRY_GAP_SECS: u64 = 20;

/// 后台每小时固化出新的时条目时后端发的通知（payload: `{ hours: number }`）。
pub const BRIEFING_EVENT: &str = "credit-briefing-sealed";

/// 采样窗口起点（分钟）：每小时的最后几分钟才采一次样。
///
/// 台账的规则是「增量归入采样时刻所属的那个小时」，所以想让整个小时的量都落进
/// 这一小时，采样就必须赶在整点之前完成 —— 见 [`sample_plan`]。
const SAMPLE_AT_MINUTE: i64 = 55;

/// 简报的进程内状态（「本小时已采过」标记 + 「今天已推过」去重）。
///
/// 参考项目把这两项落盘在 `schedule_state.json` 里；本项目没有 state 文件机制，
/// 而两项都是「兜底 / 去重」性质 —— 丢了也只是多采一次、多推一条，内存即可。
static BRIEFING_STATE: OnceLock<Mutex<BriefingState>> = OnceLock::new();

#[derive(Default)]
struct BriefingState {
    last_sample_hour: Option<String>,
    last_briefing_push_date: Option<String>,
}

fn briefing_state() -> &'static Mutex<BriefingState> {
    BRIEFING_STATE.get_or_init(|| Mutex::new(BriefingState::default()))
}

pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_day: Option<chrono::NaiveDate> = None;
        // 启动即检查一次端点改写是否还在
        let mut next_repair = std::time::Instant::now();
        // 第一跳是「启动补采」：应用刚起来就采一次样 + 补一次固化，
        // 用户一打开就能看到最新数字（见 [`sample_plan`] 的 `at_startup`）
        let mut startup = true;
        loop {
            let dir = match commands::try_data_dir(&app) {
                Ok(d) => d,
                Err(_) => {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let settings = accounts::load_settings(&dir);
            let now = chrono::Local::now();
            // 积分简报有自己的开关，不受「定时签到」影响；放最前，别被下面的分支吞掉
            maybe_seal_briefing(&app, &dir, &settings, startup);
            startup = false;
            if settings.takeover_enabled && std::time::Instant::now() >= next_repair {
                // ⚠️ 端点必须与 `commands::enable_endpoint` 用**同一个**来源
                // （`endpoint::base_url`）。端点只有一种形态了，所以这里不再随模式二选一 ——
                // 但「两端共用同一个函数」这条纪律要保住：曾经这里写死过一个值，与安装侧
                // 不一致，于是自愈每 20s 都被 `endpoint::preflight` 以「会让 TraeWork
                // 启动即崩」为由拒绝，journal 里刷屏 `install_blocked`，而 TraeWork 升级
                // 覆盖 `product.json` 后**再也不会有改道**（接管静默失效）。
                let endpoint_base = crate::endpoint::base_url(settings.takeover_port);
                let all = crate::target::discover();
                let selected = crate::target::select(&settings.takeover_apps);
                let proxy_ok = crate::proxy::status().active;

                // ① 名单之外的应用整个放下（端点 + 补丁）。这条同时兜住两件事：
                //    用户在界面里取消勾选（那条路会立即处理，这里是幂等的第二道）、
                //    以及有人直接手改了 `settings.json`。
                crate::endpoint::sweep(&dir, &all, &selected);

                // ② 反代不在监听时，指向本机的端点**必须先还回官方**（补丁不动）。
                //    两条生命周期的分工见 `endpoint::restore_endpoints` 的文档 ——
                //    简单说：补丁跟着「名单」走、端点跟着「连得上」走，否则会有还补丁/重打补丁的抖动。
                if !proxy_ok {
                    crate::endpoint::restore_endpoints(&dir, &all);
                }

                // ③ 名单里的每个应用：补丁必须在位，端点必须指向本机。
                let mut ready = !selected.is_empty();
                for t in &selected {
                    // **持续前提**：该应用的 `out/main.js` 必须是打过补丁的。
                    // 升级会把它整份换掉、补丁随之消失，而端点还写着明文 `http://` ——
                    // **它下一次启动就会崩**。所以每轮都确认一遍，丢了就补回来。
                    let gate_ok = match crate::patch::apply(&dir, t) {
                        Ok(p) => p.patched,
                        Err(e) => {
                            crate::journal::append_dedup(
                                &dir,
                                "patch_gone",
                                &format!(
                                    "免证书模式：「{}」的闸门补丁无法保证（{e}）。\
                                     已放弃对它做端点改写并恢复官方直连 —— \
                                     否则它下次启动会因明文端点崩掉",
                                    t.id
                                ),
                            );
                            false
                        }
                    };

                    let ok = if gate_ok {
                        // 反代没监听时绝不改写端点，否则会把应用指向死端口
                        proxy_ok && crate::endpoint::repair(t, &dir, &endpoint_base)
                    } else {
                        // fail-safe：补丁没保证就**绝不**把明文端点留在它的 product.json 里。
                        // `uninstall` 幂等，不是我们改的就不动。
                        let _ = crate::endpoint::uninstall(t, &dir);
                        false
                    };
                    ready &= ok;
                }
                next_repair = std::time::Instant::now()
                    + Duration::from_secs(if ready {
                        REPAIR_OK_GAP_SECS
                    } else {
                        REPAIR_RETRY_GAP_SECS
                    });
            }
            if settings.checkin_enabled {
                let today = now.date_naive();
                if last_day != Some(today) {
                    let cur = now.format("%H:%M").to_string();
                    if cur == settings.checkin_time {
                        last_day = Some(today);
                        run_checkin(&app, &dir);
                    }
                }
            } else {
                last_day = Some(now.date_naive());
            }
            std::thread::sleep(Duration::from_secs(20));
        }
    });
}

fn run_checkin(app: &tauri::AppHandle, dir: &std::path::Path) {
    let settings = accounts::load_settings(dir);
    // 绑了凭证池：先整池同步一轮再读账号，闸带回来的才是最新凭证（本地可能已被别的机器换掉）
    tauri::async_runtime::block_on(crate::commands::sync_pool_if_bound(dir));
    // 账号列表里已无「启用」概念：所有账号一律参与定时签到。
    let mut pending: Vec<Account> = accounts::load_accounts(dir);
    if pending.is_empty() {
        logs::push("系统", true, "暂无账号，跳过定时签到");
        return;
    }

    let mut done: Vec<(String, CheckinResult)> = Vec::new();
    for round in 0..MAX_ROUNDS {
        if pending.is_empty() {
            break;
        }
        if round > 0 {
            logs::push(
                "系统",
                true,
                format!(
                    "第 {} 轮补签：{} 个账号上一轮被限流（9074），{} 分钟后重试",
                    round + 1,
                    pending.len(),
                    ROUND_GAP_SECS / 60
                ),
            );
            std::thread::sleep(Duration::from_secs(ROUND_GAP_SECS));
        }

        let mut next: Vec<Account> = Vec::new();
        for mut account in pending {
            let name = account.name.clone();
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let _ = tauri::async_runtime::block_on(crate::renew::renew_if_needed(dir, &mut account));
            let r = tauri::async_runtime::block_on(crate::checkin::do_checkin(&account));
            logs::push(
                &name,
                r.success,
                format!("[{}] 第 {} 轮 · {}", now, round + 1, r.message),
            );
            // 仅「服务端限流」值得下一轮再试；鉴权失败/业务错误重试无意义
            if r.transient && round + 1 < MAX_ROUNDS {
                next.push(account);
            } else {
                done.push((name, r));
            }
        }
        pending = next;
    }

    // 兜底：极端情况下仍留在 pending 的账号（不应发生）计入失败
    for account in pending {
        done.push((
            account.name.clone(),
            CheckinResult {
                success: false,
                already: false,
                inactive: false,
                transient: true,
                auth_failed: false,
                message: "限流未放行".into(),
                credit: None,
                host: None,
                at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            },
        ));
    }

    let mut ok = 0usize;
    let mut already = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for (name, r) in &done {
        if r.already {
            already += 1;
        } else if r.success {
            ok += 1;
        } else if !r.inactive {
            failed.push(format!("{}：{}", name, r.message));
        }
    }

    // webhook 通知：仅配置了地址才发，失败只记日志、不影响签到结果
    let webhook = settings.webhook_url.trim();
    if !webhook.is_empty() {
        let title = crate::notify::summary_title(ok, already, failed.len());
        let msg = crate::notify::summary_message(ok, already, &failed);
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let out = match tauri::async_runtime::block_on(crate::notify::send(webhook, &title, &msg)) {
            Ok(r) => r,
            Err(e) => format!("发送失败：{e}"),
        };
        logs::push("系统", true, format!("[{}] Webhook 通知：{}", now, out));
    }
    let _ = app;
}

// ── 积分简报：每小时采样 + 固化 ────────────────────────────────────

/// 这一跳的采样决定。
#[derive(Debug, PartialEq, Eq)]
struct SamplePlan {
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
///    会被 20s 一跳连采十来次。
/// 2. **启动补采**（`at_startup`）：应用刚起来顺带采一次，让用户一打开就看到最新数字。
///    它**不占本小时的名额** —— 否则「15:03 启动」会把 15:55 那次窗口采样挡掉，
///    于是 `15:03 → 16:55` 这一大段增量整块落进 16 点的桶，从启动那一小时起就错位。
///    代价只是同一小时里多采一次（增量本来就累加到同一个桶里）。
fn sample_plan(
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

/// 简报的推送窗口：**跨零点的那一跳**（`hour == 0`，即用户口径里的「24 点」），
/// 或应用刚起来时补一次。
///
/// 为什么是这一跳才推得完整：23 点的桶在 23:5x 就采过样，而 `seal_hours` 只固化
/// 「比当前小时早」的桶 —— 昨天要到 `today` 翻页、`hour` 归 0 的这一跳才收口。
///
/// 为什么不能挂在「这一跳固化出了东西」上（旧写法）：夜里那一小时没动静就固化不出
/// 任何条目，推送跟着被吃掉，简报于是拖到白天下一次有动静才冒出来 —— 用户看到的
/// 推送时刻是随机的。
///
/// `at_startup` 兜住「整夜关着 ⇒ 根本没有零点那一跳」：开机补推一次。桌面端没有
/// 守护进程，应用没运行时刻表本身就无从执行，这是唯一的补偿。
fn briefing_push_due(at_startup: bool, hour: u8) -> bool {
    at_startup || hour == 0
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
/// 推送只推**已经走完的那一天**，且时刻固定在**跨零点那一跳**（即「24 点」，判定见
/// [`briefing_push_due`]）：时条目每小时都在结算，但「今天花了多少」要等当天
/// 结束才有定论，每小时推一条只会把通知刷成流水账。本项目只认 `webhook_url`：
/// 没配地址就不推，配了就推（没有「通知开关」这一层）。
fn maybe_seal_briefing(
    app: &tauri::AppHandle,
    dir: &std::path::Path,
    settings: &accounts::Settings,
    at_startup: bool,
) {
    if !settings.briefing_enabled {
        return;
    }
    let accounts = accounts::load_accounts(dir);
    if accounts.is_empty() {
        return;
    }

    let now = chrono::Local::now().naive_local();
    let today = now.date().format("%Y-%m-%d").to_string();
    let hour = now.hour() as u8;
    let now_s = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let cur_key = format!("{today} {hour:02}");

    let mut st = briefing_state().lock().unwrap_or_else(|e| e.into_inner());
    // 采样判定见 [`sample_plan`]：窗口内每小时一次，**启动补采不占本小时的窗口名额**
    let plan = sample_plan(
        at_startup,
        now.minute() as i64,
        &cur_key,
        st.last_sample_hour.as_deref(),
    );
    if let Some(mark) = plan.mark_hour {
        // 先占掉名额再真采：采样要打接口、可能耗时或失败，
        // 先记账可避免同一个小时里反复触发（窗口有 5 分钟、轮询 20s 一跳）
        st.last_sample_hour = Some(mark);
    }
    // 采集（打接口，可能耗时数十秒）在锁外完成 —— 锁里只做判定和占名额，
    // 否则整段网络 I/O 都压在 `BRIEFING_STATE` 上（现在只有调度线程访问所以无死锁，
    // 但没必要让一次写把状态锁这么久）。
    drop(st);
    if plan.sample {
        // 采集（打接口）在锁外完成，入账一次性写进**唯一那本内存台账** ——
        // 见 `commands::fetch_samples` / `commands::apply_samples`。
        // 这里用正常记账（不是 baseline）：断档期攒下的量虽然归不到具体的小时，
        // 但它**是真的消耗**，丢掉会让总账少算。归到「恢复后的那个小时」是最不坏的归属。
        let (at, readings) = tauri::async_runtime::block_on(commands::fetch_samples(dir, &accounts));
        commands::apply_samples(app, &readings, &at, false);
    }

    let sealed = commands::seal_hours(dir, &accounts, &today, hour, &now_s);
    if !sealed.is_empty() {
        let _ = app.emit(
            BRIEFING_EVENT,
            serde_json::json!({ "hours": sealed.len() }),
        );
    }

    if !briefing_push_due(at_startup, hour) {
        return;
    }

    // 每天最多推一条，且只推最近一个**已经走完**的日子。
    // 先占掉推送日期再推：推送失败也不该让同一份简报反复重推。
    let days = briefing::day_entries(&briefing::load(dir), &today);
    let Some(day) = days.iter().find(|d| d.sealed) else {
        return;
    };
    let mut st = briefing_state().lock().unwrap_or_else(|e| e.into_inner());
    if st.last_briefing_push_date.as_deref() == Some(day.date.as_str()) {
        return;
    }
    st.last_briefing_push_date = Some(day.date.clone());
    drop(st);
    let webhook = settings.webhook_url.trim();
    if webhook.is_empty() {
        return;
    }
    let _ = tauri::async_runtime::block_on(crate::notify::send(
        webhook,
        "积分简报",
        &briefing::message(day),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn briefing_push_is_due_at_midnight_and_on_startup_only() {
        assert!(briefing_push_due(false, 0), "跨零点那一跳（24 点）推");
        assert!(!briefing_push_due(false, 1), "过了零点就不再补推");
        assert!(!briefing_push_due(false, 9), "白天不推（旧写法：有动静才顺带推，时刻随机）");
        assert!(briefing_push_due(true, 9), "整夜没运行 ⇒ 开机补推一次");
    }

    #[test]
    fn sample_plan_samples_once_per_hour_inside_the_window() {
        let key = "2026-09-17 15";
        // 窗口内（>= 55 分）且本小时没采过 → 采，并占名额
        let p = sample_plan(false, 55, key, None);
        assert!(p.sample);
        assert_eq!(p.mark_hour.as_deref(), Some(key));
        // 同一小时再问 → 不采（名额已占）
        let p = sample_plan(false, 58, key, Some(key));
        assert!(!p.sample);
        assert_eq!(p.mark_hour, None);
        // 窗口外 → 不采（除非启动补采）
        let p = sample_plan(false, 10, key, None);
        assert!(!p.sample);
        let p = sample_plan(true, 10, key, None);
        assert!(p.sample);
        assert_eq!(p.mark_hour, None, "启动补采不占本小时窗口名额");
    }
}
