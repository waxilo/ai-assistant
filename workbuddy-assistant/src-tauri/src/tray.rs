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
            let _ = win.hide();
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
pub fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}
