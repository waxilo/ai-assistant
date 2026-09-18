//! 系统托盘：关闭窗口 = 隐藏到托盘，进程常驻（定时签到 / 智能接管不中断）。
//!
//! 交互约定：
//! - **左键单击**：切换主窗口 —— 窗口可见且已聚焦 ⇒ 隐藏；否则 ⇒ 显示并聚焦。
//! - **右键单击**：弹出菜单（显示/隐藏主窗口、退出）。
//!
//! 图标使用内嵌的 `icons/tray.png`（绿色圆角方块 + 对勾），不依赖系统默认窗口图标，
//! 避免在高 DPI / 小尺寸托盘下糊成一团。

use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

/// 编译期内嵌托盘图标（32×32 RGBA PNG）。
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = tauri::menu::Menu::with_items(
        app,
        &[
            &tauri::menu::MenuItem::with_id(app, "toggle", "显示 / 隐藏主窗口", true, None::<&str>)?,
            &tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
        ],
    )?;

    let icon = Image::from_bytes(TRAY_ICON)?;

    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("TraeWork 助手")
        .menu(&menu)
        // 左键不弹菜单（保留给我们自己做「切换窗口」）；右键弹菜单
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "toggle" => toggle_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main(&tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 左键单击：可见且已聚焦 ⇒ 隐藏到托盘；否则 ⇒ 显示并聚焦。
pub fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let visible = w.is_visible().unwrap_or(false);
        let focused = w.is_focused().unwrap_or(false);
        if visible && focused {
            set_dock_visible(app, false);
            let _ = w.hide();
        } else {
            set_dock_visible(app, true);
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

/// 显示主窗口并聚焦。
///
/// 目前由 `RunEvent::Reopen`（macOS 点击 Dock 图标）调用，Windows 上无调用点，
/// 故放行 dead_code 警告。
#[allow(dead_code)]
pub fn show_main(app: &AppHandle) {
    set_dock_visible(app, true);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// macOS：让 Dock 图标跟随主窗口显隐 —— 窗口在前台时保留，隐藏到托盘后摘掉
/// （`ActivationPolicy::Accessory`，等价于 LSUIElement）。窗口隐藏后 Dock 里没有
/// 图标，唤回只能走菜单栏托盘图标；定时签到 / 智能接管等后台逻辑不受影响。
///
/// 仅当「窗口隐藏」需要才摘：进程退出时 Dock 图标随进程消失，无需干预。
/// Windows 无此概念，编译为空操作。
#[cfg(target_os = "macos")]
pub fn set_dock_visible(app: &AppHandle, visible: bool) {
    let policy = if visible {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    let _ = app.set_activation_policy(policy);
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_visible(_app: &AppHandle, _visible: bool) {}
