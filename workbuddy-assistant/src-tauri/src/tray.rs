//! 系统托盘与后台常驻。
//!
//! 需求：关闭窗口 ≠ 退出进程。定时签到、token 续签、本地反代都依赖进程常驻，
//! 所以点红按钮只隐藏窗口，真正退出必须走托盘菜单「退出」。
//!
//! 交互约定：
//! - **左键单击**托盘图标 = 切换主窗口显隐。主窗口在前台时收起，否则唤回并聚焦。
//! - **右键**托盘图标 = 弹出菜单（显示主窗口 / 退出）。
//! - 只响应按键**抬起**：Windows 的托盘回调会同时派发按下与抬起，两个都处理
//!   会让一次单击被切换两次，看起来像「点了没反应」。

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

/// 两次切换的最小间隔。吞掉双击产生的第二次抬起事件——否则双击等于切换两次，
/// 视觉上什么都没发生。
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

/// 上一次切换的时刻（`None` = 本次进程内还没切换过）。
static LAST_TOGGLE: Mutex<Option<Instant>> = Mutex::new(None);

/// 构建托盘图标 + 菜单。
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("WorkBuddy 助手")
        .menu(&menu)
        // 左键留给「切换显隐」，菜单只在右键弹出
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.to_owned());
    }
    builder.build(app)?;
    Ok(())
}

/// 单击托盘图标：主窗口在前台则收起，否则唤回并聚焦。
pub fn toggle_main(app: &AppHandle) {
    if debounced() {
        return;
    }
    if is_main_on_top(app) {
        if let Some(win) = app.get_webview_window("main") {
            // 先隐藏窗口再摘 Dock 图标：反过来的话，改策略那一刻窗口还看得见，
            // 系统会把这次变更丢掉（详见 set_dock_visible 的说明）。
            let _ = win.hide();
            set_dock_visible(app, false);
        }
    } else {
        show_main(app);
    }
}

/// 主窗口是否「已经在用户眼前」——可见且持有焦点。
///
/// 只看 `is_visible` 是不够的：被别的程序压在下面时窗口依然可见、但不在前台，
/// 此时单击的合理预期是唤回而不是收起（收起会让用户以为点了没反应）。
fn is_main_on_top(app: &AppHandle) -> bool {
    match app.get_webview_window("main") {
        Some(win) => win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false),
        None => false,
    }
}

/// 距上次切换不足 [`TOGGLE_DEBOUNCE`] 则返回 `true`（应当忽略本次事件）。
fn debounced() -> bool {
    let Ok(mut last) = LAST_TOGGLE.lock() else {
        return false;
    };
    let now = Instant::now();
    if last.map_or(false, |prev| now.duration_since(prev) < TOGGLE_DEBOUNCE) {
        return true;
    }
    *last = Some(now);
    false
}

/// 显示并聚焦主窗口（托盘菜单 / macOS Dock 重新激活共用）。
///
/// 顺序与「收进托盘」相反，且**故意的**：先把 Dock 图标还回来（切回 `Regular`），
/// 再 `show()` —— 这样窗口出现时应用已经是个正常应用，能直接拿到焦点、
/// 也进得了 Cmd+Tab。隐藏那侧则是先藏窗口再摘图标，理由见 [`set_dock_visible`]。
pub fn show_main(app: &AppHandle) {
    set_dock_visible(app, true);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// macOS：让 Dock 图标跟随主窗口显隐 —— 窗口在前台时保留，隐藏到托盘后摘掉
/// （`ActivationPolicy::Accessory`，等价于 LSUIElement）。窗口隐藏后 Dock 里没有
/// 图标，唤回只能走菜单栏托盘图标；定时签到 / 续签 / 反代等后台逻辑不受影响。
///
/// 仅当「窗口隐藏」需要才摘：进程退出时 Dock 图标随进程消失，无需干预。
/// Windows 无此概念，编译为空操作。
///
/// # 为什么不是「一句 `set_activation_policy` 就完事」
///
/// 原来就是这么写的，而它在实测里**时灵时不灵**（2026-09-18）：同一份实现，
/// workbuddy 生效（系统记到 `UIElement`），qoder / trae 不生效（仍记
/// `Foreground`）。差别只在**调用时序**，不在代码。
///
/// Tauri 运行时这条路本身是通的（`Message::SetActivationPolicy` →
/// tao 的 `set_activation_policy_at_runtime` → `NSApp.setActivationPolicy`，立即生效），
/// 问题出在**改策略那一刻窗口还看得见**：`hide()` 与改策略都是投递到事件循环的消息，
/// 原来的顺序是「先改策略、后隐藏窗口」，系统在那一瞬看到「Accessory 应用仍有可见窗口」，
/// 于是把这次策略变更丢掉了 —— Dock 图标就这么留了下来。
///
/// 所以这里三道加固：
///
/// 1. **调用方一律先动窗口、再摘图标**（隐藏：先 `hide()`；唤回：先设回 `Regular`
///    再 `show()`）—— 见 [`toggle_main`] / [`show_main`] / `lib.rs` 的 `CloseRequested`。
/// 2. **两条 API 都发**：[`tauri::AppHandle::set_dock_visibility`] 是 tao 专为此场景
///    维护的路径（底层是 Apple 的 `TransformProcessType`，且会先给所有窗口
///    `setCanHide(false)`，避免切 UIElement 时窗口被系统一并隐藏），
///    `set_activation_policy` 则直接改 `NSApp`。二者语义等价、互为兜底。
/// 3. **再排一轮**：`run_on_main_thread` 把同一次设置投递到事件循环下一轮，
///    兜住「当轮变更被系统忽略」的情况。
#[cfg(target_os = "macos")]
pub fn set_dock_visible(app: &AppHandle, visible: bool) {
    apply_dock_policy(app, visible);
    // 兜底：同一次设置再排到下一轮。事件循环里各路消息的先后顺序由投递顺序决定，
    // 窗口动作总是排在前面（调用方负责），所以这一轮执行时窗口状态已经落定。
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || apply_dock_policy(&handle, visible));
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_visible(_app: &AppHandle, _visible: bool) {}

/// 两条 API 一起设，哪条先落地都行（幂等）。
#[cfg(target_os = "macos")]
fn apply_dock_policy(app: &AppHandle, visible: bool) {
    // tao 的官方运行时切换：TransformProcessType(UIElement / Foreground)
    let _ = app.set_dock_visibility(visible);
    // NSApp 策略：与之等价，互为兜底
    let _ = app.set_activation_policy(if visible {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    });
}
