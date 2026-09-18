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
            // 先隐藏窗口再摘 Dock 图标：反过来的话，改策略那一刻窗口还看得见，
            // 系统会把这次变更丢掉（详见 set_dock_visible 的说明）。
            let _ = w.hide();
            set_dock_visible(app, false);
        } else {
            // 唤回这侧顺序相反，且**故意的**：先把 Dock 图标还回来再 show，
            // 窗口出现时应用已经是个正常应用，能直接拿到焦点。
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
///
/// 先把 Dock 图标还回来再 `show()` —— 顺序与「收进托盘」相反，理由见
/// [`set_dock_visible`]。
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
