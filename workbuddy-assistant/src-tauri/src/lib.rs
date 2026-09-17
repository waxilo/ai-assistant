mod accounts;
mod auth_file;
mod briefing;
mod broker;
mod checkin;
mod commands;
mod http;
mod ledger;
mod logs;
mod netfix;
mod notify;
mod oauth;
mod proxy;
mod refresh;
mod rng;
mod scheduler;
mod stealth;
mod tray;

use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Tauri updater 的 reqwest 默认走系统代理；本机 Clash 代理对 GitHub release-assets
    // CDN 不稳定（HTTP 000 / 502），导致检查更新连 latest.json 都拉不下来。
    // 启动时把 GitHub 相关域名加入 NO_PROXY，让更新器直连 GitHub。
    const GITHUB_NO_PROXY: &str = "github.com,.github.com,githubusercontent.com,.githubusercontent.com";
    match std::env::var("NO_PROXY") {
        Ok(v) if !v.is_empty() => {
            std::env::set_var("NO_PROXY", format!("{}, {}", v, GITHUB_NO_PROXY));
        }
        _ => {
            std::env::set_var("NO_PROXY", GITHUB_NO_PROXY);
        }
    }

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        // 用 LaunchAgent 而非 AppleScript，登录时静默启动、不弹窗
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 台账（进程内唯一的内存副本）与界面那份积分对象之间只留这一条线：
            // 往台账里写一次，界面就更新一次 —— 签到 / 刷新 / 整点采样 / 接管路由补拉 /
            // 开启简报，谁都不必再各自记得广播一次。接管那条路径手上根本没有
            // `AppHandle`，正是「各自记得」必然漏掉的那一类。
            let handle = app.handle().clone();
            if let Ok(dir) = commands::try_data_dir(app.handle()) {
                ledger::store(&dir).on_change(Box::new(move || commands::emit_credits(&handle)));
                // 凭证池：只读一次 broker.json 里那串 uuid。
                // **必须在调度线程与反代起来之前** —— 两者都会调 `sync_pool_if_bound`，
                // 而它读的就是这份内存状态（读到「未绑定」会静默跳过整池同步）。
                broker::init(&dir);
            }
            // 定时自动签到：独立后台线程，与进程同生命周期。
            // 只在应用运行期间生效——桌面端退出后没有守护进程可代为执行。
            scheduler::spawn(app.handle().clone());
            // 本地反代（按积分过期时间优先路由）+ 智能接管的装卸与心跳
            proxy::spawn(app.handle().clone());
            // 系统托盘：后台常驻入口
            tray::setup(app.handle())
                .expect("初始化系统托盘失败");
            Ok(())
        })
        // 关闭窗口 = 隐藏到托盘，进程常驻（签到/续签/反代不中断）。
        // 真正退出走托盘菜单「退出」（app.exit 触发 RunEvent::Exit 的清理逻辑）。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_accounts,
            commands::import_accounts,
            commands::remove_account,
            commands::checkin_one,
            commands::checkin_all,
            commands::refresh_all,
            commands::discover_local_accounts,
            commands::oauth_start,
            commands::oauth_poll,
            commands::open_external,
            // 凭证管家：四个都是池级命令，不带账号 id（一池一 uuid、池内一把闸）
            commands::broker_upload,
            commands::broker_link,
            commands::broker_unbind,
            commands::broker_state,
            commands::get_settings,
            commands::save_settings,
            commands::apply_settings,
            commands::test_notify,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_checkin_logs,
            commands::clear_checkin_logs,
            // 积分简报：日条目 = 当天时条目之和（现算），口径见 briefing 模块。
            // 条目只由后台每小时结算一次，这里没有「手动生成一条」的入口。
            commands::credit_briefing,
            commands::credit_briefing_clear,
            // 开启简报：清历史 + 采一次样**只对齐基线**（断档期增量不记）
            commands::credit_briefing_enable,
            commands::app_version,
            // 网络急救：扫出「调试残留的全局服务端点」并一键清除（含关闭本地反代）
            netfix::net_diagnose,
            netfix::net_restore,
            netfix::reveal_path,
            // 智能接管：状态查询 / 事件流 / 限流切换支持模型（弹窗展示 + 手动刷新）
            stealth::stealth_status,
            stealth::takeover_events_clear,
            stealth::takeover_events,
            proxy::free_models,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    // 退出时安全关闭接管。仅摘配置不够：WorkBuddy 的长驻 CLI host 会把旧值留在
    // process.env，必须在代理仍存活时让它退出，之后才能停止监听。
    app.run(|handle, event| {
        static CLEANED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        match &event {
            // macOS：窗口全隐藏后点 Dock 图标（或 finder 重新打开）→ 唤回主窗口。
            // Reopen 是 macOS 独有变体，Windows 编译时必须条件编译掉。
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => tray::show_main(handle),
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. } => {
                if CLEANED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                if let Ok(dir) = commands::try_data_dir(handle) {
                    let mut settings = accounts::load_settings(&dir);
                    if settings.proxy_enabled {
                        settings.proxy_enabled = false;
                        let _ = commands::apply_settings_inner(handle, settings);
                    }
                }
            }
            _ => {}
        }
    });
}
