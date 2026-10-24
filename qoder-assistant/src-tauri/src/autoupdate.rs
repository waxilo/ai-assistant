//! 静默自动更新：每天在设定时刻自动检查 → 加速下载 → 验签 → 安装。
//!
//! ## 为什么放在后端而不是复用设置页那条 JS 链路
//!
//! 本助手是**托盘常驻**形态，用户可能连着几天不打开窗口；而「凌晨 5 点」这个时刻
//! 本身就意味着没人看着。把更新挂在 webview 上，等于让它依赖「窗口恰好活着」。
//! 后端跑还顺带把结论写进日志页 —— 用户第二天能查到「昨晚有没有自动更新」。
//!
//! 两条路径共用同一套下载/校验/安装实现（[`crate::accel::accelerated_install`]），
//! 区别只在**谁触发**：手动那个由按钮触发并实时画进度条，这个由时钟触发、
//! 只发一条事件让侧边栏挂个「正在自动更新」。
//!
//! ## 触发语义：不早于设定时刻，每天最多一次
//!
//! 不是「恰好等于 HH:MM」—— 轮询有粒度（30s 一跳），后者必然漏。而「不早于」+
//! 当天记账带来一个必要的补做能力：默认的凌晨 5 点对**夜里关机的机器**是永远
//! 不会命中的时刻，于是当天首次轮询就补上（见 [`due`]）。
//!
//! ## 两道保险
//!
//! - **应用运行满 [`MIN_UPTIME`] 才动**：否则「下午三点开机 → 发现今天还没更新 →
//!   开机 30 秒后应用自己重启」会把用户吓一跳。等 5 分钟，至少界面先站住。
//! - **尝试过就记账，失败不当天重试**：下载失败若不留痕，下一跳（30 秒后）会重下
//!   一遍安装包 —— 网断的时候这就成了重试风暴。失败写日志、等明天。

use crate::accel::{self, AccelEvent};
use crate::accounts::{self, Settings};
// qoder 只有一条「调度日志」通道（`scheduler.log`），日志页的类别与成败由
// `logs::classify_scheduler` 按文案前缀/关键词推断 —— 所以这里的措辞要能被它认出来：
// 以「静默更新」开头落进 system 类，正文里出现「失败」即判为失败行。
use crate::scheduler;
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

/// 前端据此在侧边栏挂「正在自动更新」。**不弹 toast、不弹确认框** —— 静默的字面意思。
pub const EVENT: &str = "auto-update";

/// 应用启动后多久之内不执行静默更新。
///
/// 5 分钟：既躲开「刚双击打开就看到它自己重启」，又不至于把补做拖到用户已经忘了这回事。
/// 真正在凌晨 5 点窗口里跑时，应用早就运行了几小时，这个门槛不构成延迟。
const MIN_UPTIME: Duration = Duration::from_secs(300);

/// 进度事件的最小间隔。
///
/// accel 每收到一块数据就回调一次（10MB 包能到几百次），每次都过一趟 IPC 是没必要的开销；
/// 500ms 一跳在侧边栏那条细进度条上完全够顺。
const PROGRESS_THROTTLE: Duration = Duration::from_millis(500);

/// 发给前端的阶段事件。字段名都用单词，`camelCase` 只影响 tag 值（`available` 等）。
#[derive(Clone, Serialize)]
#[serde(tag = "stage", rename_all = "camelCase")]
enum Evt {
    Available { version: String },
    Downloading { downloaded: u64, total: u64 },
    Installing { version: String },
    Failed { message: String },
}

/// 一次静默更新的结局。
pub enum Outcome {
    /// 已是最新，什么都没做（**不写日志**：每天一条「无更新」会把日志页刷成流水账）
    NoUpdate,
    /// 装上了新版本。
    ///
    /// Windows 上基本走不到这里 —— 插件 `install` 会 `ShellExecute` 拉起安装器后
    /// 直接 `exit(0)`，由安装器的 `/R` 负责重启。留着是为了 macOS / Linux 与非
    /// Windows 平台的日志。
    Installed { to: String },
    /// 失败（网络 / 验签 / 安装）。今天不再重试。
    Failed(String),
}

/// 今天是否该跑静默更新。
///
/// 三条都要满足：时刻合法、今天还没跑过、已经过了设定时刻。
/// 抽成纯函数是因为时间判断的边界（跨天、未来时刻、时刻写坏了）最容易出错，
/// 而这套判定一年只被真正验证 365 次 —— 只能靠单测兜。
pub(crate) fn due(now: chrono::NaiveDateTime, time: &str, last_date: Option<&str>) -> bool {
    let Some(t) = accounts::normalize_time(time) else {
        return false; // 时刻写坏了宁可不跑，也别猜一个点出来
    };
    let Ok(t) = chrono::NaiveTime::parse_from_str(&t, "%H:%M") else {
        return false;
    };
    let today = now.date().format("%Y-%m-%d").to_string();
    if last_date == Some(today.as_str()) {
        return false; // 今天已经尝试过（成功或失败）
    }
    now.time() >= t
}

/// 跑一次静默更新（阻塞直到有结论）。
///
/// 调用方（`scheduler`）负责「该不该跑」的判定与当天记账；这里只管跑和写日志。
/// **会阻塞几十秒**（下载 + 安装）—— 凌晨 5 点的调度线程上，这个代价可以接受。
pub fn run(app: &AppHandle, dir: &Path) -> Outcome {
    match tauri::async_runtime::block_on(install_if_available(app, dir)) {
        Ok(None) => Outcome::NoUpdate,
        Ok(Some(to)) => {
            scheduler::log_event(dir, &format!("静默更新完成：已安装 v{to}"));
            Outcome::Installed { to }
        }
        Err(e) => {
            // 失败必须留痕：静默更新最坏的形态是「一直没更新，而界面上没有任何痕迹」。
            emit(app, Evt::Failed { message: e.clone() });
            scheduler::log_event(dir, &format!("静默更新失败（明天再试）：{e}"));
            Outcome::Failed(e)
        }
    }
}

/// 查 → 下 → 验 → 装。`Ok(None)` = 已是最新。
///
/// Windows 上装成功时进程会在这里退出，不会返回 —— 所以「装之前」该写的日志
/// 全部落在 `before_install` 里。
async fn install_if_available(app: &AppHandle, dir: &Path) -> Result<Option<String>, String> {
    let updater = app.updater().map_err(|e| format!("更新器不可用：{e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败：{e}"))?;
    let Some(update) = update else {
        return Ok(None);
    };
    let to = update.version.clone();

    // 先点亮前端的「有新版本」再动手：真到安装那一刻，用户可能正好在看屏幕，
    // 侧边栏那条进度至少能解释「它为什么自己重启了」。
    emit(
        app,
        Evt::Available {
            version: to.clone(),
        },
    );
    scheduler::log_event(dir, &format!("静默更新：发现新版本 v{to}，开始下载"));
    let mut downloaded: u64 = 0;
    let mut total: u64 = 0;
    let mut last_emit: Option<Instant> = None;
    let mut emit_progress = |e: AccelEvent| {
        match e {
            // 换源重下会再发一次 Started：累计量必须归零，否则侧边栏会显示
            // 「已下载 18 MB / 9.6 MB」。口径与前端 `updater.ts` 完全一致。
            AccelEvent::Started { content_length } => {
                downloaded = 0;
                total = content_length.unwrap_or(0);
                last_emit = Some(Instant::now());
                emit(app, Evt::Downloading { downloaded, total });
            }
            AccelEvent::Progress { chunk_length } => {
                downloaded += chunk_length as u64;
                let should = last_emit.map_or(true, |t| t.elapsed() >= PROGRESS_THROTTLE);
                if should {
                    last_emit = Some(Instant::now());
                    emit(app, Evt::Downloading { downloaded, total });
                }
            }
            // Finished 的语义是「这个源下完了」，后面还有验签与安装两步，
            // 进度阶段不在这里结束（安装阶段由 `before_install` 播报）。
            AccelEvent::Finished => {}
        }
    };

    let before_install = || {
        // ⚠️ 这是 Windows 上**最后一次**能写盘的机会（install → exit(0)）。
        scheduler::log_event(
            dir,
            &format!("静默更新：v{to} 下载完成、签名校验通过，正在安装并重启"),
        );
        emit(
            app,
            Evt::Installing {
                version: to.clone(),
            },
        );
    };

    accel::accelerated_install(&update, &mut emit_progress, before_install).await?;

    // Windows 到不了这里（安装器已接管进程）。macOS / Linux 由这里重启到新版本。
    #[cfg(not(windows))]
    {
        app.restart();
    }
    #[cfg(windows)]
    {
        Ok(Some(to))
    }
}

/// 发事件给前端。失败**不算错误**：窗口可能根本没开（托盘常驻），
/// 这件事本身不影响更新，别让它把一条正常的更新路径判成失败。
fn emit(app: &AppHandle, e: Evt) {
    let _ = app.emit(EVENT, e);
}

/// 供调度线程调用：把「该不该跑」的判定与当天记账收在一处，
/// 免得三处调用点各自写一遍 `last_auto_update_date`（漏一处就是重复下载）。
///
/// 返回 `true` 表示本次真的执行了。
pub fn maybe_run(
    app: &AppHandle,
    dir: &Path,
    settings: &Settings,
    last_date: &mut Option<String>,
    started: Instant,
) -> bool {
    if !settings.auto_update_enabled {
        return false;
    }
    if started.elapsed() < MIN_UPTIME {
        return false;
    }
    let now = chrono::Local::now().naive_local();
    if !due(now, &settings.auto_update_time, last_date.as_deref()) {
        return false;
    }
    // **先记账再执行**：Windows 上安装会 exit(0)，回头再写就来不及了。
    // 失败也算跑过（见本文件头「尝试过就记账」）。
    *last_date = Some(now.date().format("%Y-%m-%d").to_string());
    run(app, dir);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn fires_after_the_configured_time_once_per_day() {
        // 到点前后各一秒：判定是「不早于」，所以 04:59 不跑、05:00 跑
        assert!(!due(at("2026-10-24 04:59"), "05:00", None));
        assert!(due(at("2026-10-24 05:00"), "05:00", None));
        // 当天已经跑过（成功或失败都算）：再晚也不重复
        assert!(!due(at("2026-10-24 23:59"), "05:00", Some("2026-10-24")));
        // 昨天跑过：今天照跑
        assert!(due(at("2026-10-24 05:00"), "05:00", Some("2026-10-23")));
    }

    #[test]
    fn night_off_scenarios_catch_up_the_same_day() {
        // 默认凌晨 5 点，而机器整夜关着 —— 补做是这条语义存在的全部理由：
        // 09:30 开机、09:35 这一跳就该跑（前 5 分钟由 MIN_UPTIME 挡住）。
        assert!(due(at("2026-10-24 09:35"), "05:00", None));
        // 但「还没到点」的晚上不会提前跑
        assert!(!due(at("2026-10-23 23:00"), "05:00", Some("2026-10-23")));
        assert!(!due(at("2026-10-24 04:59"), "05:00", Some("2026-10-23")));
    }

    #[test]
    fn bad_clock_setting_never_fires() {
        // 时刻写坏（空串 / 越界 / 非数字）时宁可不跑，也别猜一个点。
        for bad in ["", "  ", "25:00", "05:60", "abc", "5:0:0"] {
            assert!(!due(at("2026-10-24 12:00"), bad, None), "非法时刻 {bad:?} 不该触发");
        }
        // 宽容形态照常识别：`9:7` 与 `09:07` 等价（normalize_time 负责归一）
        assert!(due(at("2026-10-24 12:00"), "9:7", None));
    }
}
