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
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

/// 两次切换的最小间隔。吞掉双击产生的第二次抬起事件——否则双击等于切换两次，
/// 视觉上什么都没发生。
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

/// 上一次切换的时刻（`None` = 本次进程内还没切换过）。
static LAST_TOGGLE: Mutex<Option<Instant>> = Mutex::new(None);

/// 托盘图标 id（状态更新要按 id 找回它）。
const TRAY_ID: &str = "main-tray";
/// 接管未运行时的 tooltip。
const BASE_TOOLTIP: &str = "Qoder 助手";

/// 未叠加状态点的原始托盘图标（`setup` 时存入，关闭接管时原样换回）。
static BASE_ICON: OnceLock<Image<'static>> = OnceLock::new();
/// 接管是否**真的在监听**（不是「开关打开」）：绿点只在运行中显示。
static TAKEOVER_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 接管端口，仅用于运行中的 tooltip。
static TAKEOVER_PORT: AtomicU16 = AtomicU16::new(0);

/// 构建托盘图标 + 菜单。
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tooltip_text())
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
    if let Some(icon) = app.default_window_icon().cloned().map(Image::to_owned) {
        let _ = BASE_ICON.set(icon.clone());
        builder = builder.icon(if TAKEOVER_ACTIVE.load(Ordering::SeqCst) {
            with_status_dot(&icon)
        } else {
            icon
        });
    }
    builder.build(app)?;
    // 反代线程可能先于托盘建立就报告了「接管已启动」：build 之后按静态状态补一次。
    apply_tray_state(app);
    Ok(())
}

/// 接管开始 / 停止监听时更新托盘：右下角绿点 + tooltip 端口。
///
/// 只在**实际监听状态**变化（或运行中端口变化）时重设图标，避免每轮配置轮询都闪一下。
pub fn set_takeover_active(app: &AppHandle, active: bool, port: u16) {
    let was = TAKEOVER_ACTIVE.swap(active, Ordering::SeqCst);
    let prev_port = TAKEOVER_PORT.swap(port, Ordering::SeqCst);
    if was != active || (active && prev_port != port) {
        apply_tray_state(app);
    }
}

fn apply_tray_state(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return; // 托盘还没建好：setup 末尾会按静态状态补一次
    };
    let Some(base) = BASE_ICON.get() else {
        return;
    };
    let icon = if TAKEOVER_ACTIVE.load(Ordering::SeqCst) {
        with_status_dot(base)
    } else {
        base.clone()
    };
    let _ = tray.set_icon(Some(icon));
    let _ = tray.set_tooltip(Some(tooltip_text()));
}

fn tooltip_text() -> String {
    if TAKEOVER_ACTIVE.load(Ordering::SeqCst) {
        format!(
            "{BASE_TOOLTIP} — 接管中 · 127.0.0.1:{}",
            TAKEOVER_PORT.load(Ordering::SeqCst)
        )
    } else {
        BASE_TOOLTIP.to_string()
    }
}

/// 在右下角叠加「运行中」绿点；深色描边让它在浅色 / 深色任务栏上都立得住。
fn with_status_dot(base: &Image<'static>) -> Image<'static> {
    let (w, h) = (base.width(), base.height());
    if w == 0 || h == 0 {
        return base.clone();
    }
    let mut px = base.rgba().to_vec();
    let side = w.min(h) as f32;
    let radius = side * 0.21;
    let ring = (radius * 0.30).max(1.0);
    let margin = side * 0.03;
    let cx = w as f32 - radius - margin;
    let cy = h as f32 - radius - margin;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let outer = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if outer <= 0.0 {
                continue;
            }
            let inner = (radius - ring + 0.5 - dist).clamp(0.0, 1.0);
            let i = ((y * w + x) * 4) as usize;
            blend(&mut px[i..i + 4], [15, 23, 42], outer);
            blend(&mut px[i..i + 4], [34, 197, 94], inner);
        }
    }
    Image::new(&px, w, h).to_owned()
}

/// 不透明色按覆盖率做 source-over 混合（小尺寸托盘靠它拿到平滑边缘）。
fn blend(dst: &mut [u8], color: [u8; 3], coverage: f32) {
    let a = coverage.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    let da = dst[3] as f32 / 255.0;
    let out = a + da * (1.0 - a);
    for c in 0..3 {
        dst[c] = ((color[c] as f32 * a + dst[c] as f32 * da * (1.0 - a)) / out).round() as u8;
    }
    dst[3] = (out * 255.0).round() as u8;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_icon(size: u32) -> Image<'static> {
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for _ in 0..size * size {
            rgba.extend_from_slice(&[128, 128, 128, 255]);
        }
        Image::new(&rgba, size, size).to_owned()
    }

    #[test]
    fn status_dot_is_green_at_bottom_right_and_keeps_the_rest() {
        let base = solid_icon(32);
        let dotted = with_status_dot(&base);
        assert_eq!((dotted.width(), dotted.height()), (32, 32));
        let px = dotted.rgba();
        let idx = |x: usize, y: usize| (y * 32 + x) * 4;
        // 圆心约 (24.3, 24.3)：内圈是绿色
        let center = idx(24, 24);
        assert!(px[center + 1] > px[center], "应有绿色分量");
        assert!(px[center + 1] > px[center + 2], "应是绿点而不是灰点");
        assert_eq!(&px[idx(0, 0)..idx(0, 0) + 4], &[128, 128, 128, 255]);
        assert_eq!(base.rgba()[center + 1], 128, "原始图标不能被改");
    }

    #[test]
    fn status_dot_stays_inside_the_icon() {
        let dotted = with_status_dot(&solid_icon(32));
        let px = dotted.rgba();
        let idx = |x: usize, y: usize| (y * 32 + x) * 4;
        // 四个角都在圆外（右下角贴边但不出界）：像素保持原样
        for (x, y) in [(0, 31), (31, 0), (31, 31)] {
            assert_eq!(&px[idx(x, y)..idx(x, y) + 4], &[128, 128, 128, 255]);
        }
    }
}
