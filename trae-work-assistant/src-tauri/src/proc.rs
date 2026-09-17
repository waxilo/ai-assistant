//! 辅助进程的统一入口 —— 唯一的目的：**不要让 Windows 弹出黑色命令框**。
//!
//! ## 为什么必须有这一层
//!
//! 本助手会调一批**命令行工具**做只读探测：`tasklist`（看某个应用在不在跑）、
//! `netstat`（端口被谁占着）、`pgrep` / `lsof` / `osascript`（macOS 对应物）、
//! `hostname`（OAuth 设备名）等。
//!
//! 在 Windows 上它们是**控制台程序**。当一个本身没有控制台的进程（Tauri 的 GUI 进程
//! 默认就没有）用 `Command` 启动它们、且不带 `CREATE_NO_WINDOW` 时，Windows 会
//! **为子进程新分配一个控制台** —— 屏幕上就闪出一个黑色命令框。
//!
//! 实测症状（2026-09-16 用户上报「切换菜单居然会弹出未知作用的命令框」）：
//! 切到「智能接管」页 → `takeover_status` → `build_status` → **每个应用一次**
//! `AppTarget::running()` → `process_images()` → `tasklist`。而那一页每 5 秒轮询一次，
//! 也就是**每 5 秒闪一个黑框**。用户看到的就是那个「不知道干什么的命令框」。
//!
//! ## 为什么是「统一入口」而不是「在每个调用点加 flag」
//!
//! 加 flag 是一次性的补丁，下一处新增的 `Command::new` 必然漏掉 —— 这个 bug 的成因
//! 正是如此。所以约束改成结构性的：**辅助进程一律用 [`cmd`] 构造**，
//! flag 只在这一个文件里加一次。
//!
//! ⚠️ 例外只有一处：`target::relaunch` 启动的是 TraeWork 本身（GUI 子程序），
//! 它本来就不会分配控制台 —— 但走 [`cmd`] 也无害，保持「全部走同一个入口」更省心。

use std::process::Command;

/// Windows `CREATE_NO_WINDOW`（`winbase.h`）：不为子进程分配控制台窗口。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// 其它平台本来就不会为子进程开窗口，空实现即可 ——
/// 这样调用点不必写 `#[cfg]`，「全部走 proc::cmd」这条约定才守得住。
#[cfg(not(target_os = "windows"))]
fn hide_console(_cmd: &mut Command) {}

/// 构造一个辅助进程命令（已处理「不要弹控制台窗口」）。
///
/// 用法与 `std::process::Command::new` 完全一致，可以继续链式 `.args(..)` / `.output()`：
///
/// ```ignore
/// let out = crate::proc::cmd("tasklist").args(["/fo", "csv"]).output();
/// ```
pub fn cmd(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    hide_console(&mut c);
    c
}
