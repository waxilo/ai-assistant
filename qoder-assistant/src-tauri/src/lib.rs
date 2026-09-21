mod accel;
mod accounts;
mod auth_file;
mod briefing;
mod broker;
// 智能接管的本地 TLS 材料（自签 CA + 叶证书）：端点被客户端强制成 https origin，
// 反代必须真的能终止 TLS —— 见 certs 模块说明。
mod certs;
mod checkin;
mod commands;
mod cosy;
mod http;
mod ledger;
mod logs;
mod models;
mod netfix;
mod notify;
mod oauth;
// 智能接管的注入补丁器：直接改客户端 `app.asar.unpacked` 里那个**真正被执行**的
// worker 产物（在 asar 之外，每次会话新起进程 → 改完下一次对话即生效，无需重启）。
// 端点键与本地 CA 都从这里写进去 —— 见 patch 模块说明。
mod patch;
mod proxy;
mod qoder_api;
mod refresh;
mod region;
mod rng;
mod scheduler;
mod stealth;
mod timeutil;
mod tray;
mod usage;

use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 更新下载对齐 traework：不再向 NO_PROXY 注入 GitHub 域名来强制「绕开代理直连」。
    // updater 的 reqwest 默认读系统代理（system-proxy 特性），本机直连
    // release-assets 会被掐/超时；走系统代理反而更快更稳。真正需要提速时，
    // 用 accel::update_accelerated 走加速镜像下载（见 accel.rs）。

    let app = tauri::Builder::default()
        // **必须第一个注册**：守卫生效时后起的实例在这里直接退出，走不到 `setup()`，
        // 也就不会起第二个调度线程、不会抢反代端口、不会重复打客户端的接管补丁。
        // 两个实例共享同一份 `settings.json` 和同一个反代端口，谁后起谁把前一个的状态盖掉。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 再次点击图标 = 「把已有窗口拿到前面来」，而不是再开一份
            tray::show_main(app);
        }))
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
                // 把签到日志里的历史领取补录成台账的逐笔发放（幂等，每次启动都跑）：
                // 逐笔明细上线之前的那些笔，只有日志记得它们的到期日。
                commands::backfill_grants_from_logs(&dir);
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
                // macOS：窗口收进托盘后摘掉 Dock 图标，唤回时再由托盘恢复。
                // 顺序不能反 —— 先 hide 再改策略，否则改策略那一刻窗口还看得见，
                // 系统会忽略这次变更（详见 tray::set_dock_visible 的说明）。
                let _ = window.hide();
                tray::set_dock_visible(window.app_handle(), false);
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_accounts,
            commands::import_accounts,
            commands::remove_account,
            commands::checkin_one,
            commands::checkin_all,
            // 账号只读面：额度读数（剩余积分 / 逐额度包过期）由 refresh_all 采集后落台账，
            // 界面读台账而不单独打只读命令。签到（= 领取活动权益）走上面的 checkin_*。
            commands::refresh_all,
            commands::discover_local_accounts,
            commands::oauth_start,
            commands::oauth_poll,
            commands::open_external,
            commands::open_app_management,
            // 凭证管家：四个都是池级命令，不带账号 id（一池一 uuid、池内一把闸）
            commands::broker_upload,
            commands::broker_link,
            commands::broker_unbind,
            commands::broker_state,
            commands::get_settings,
            // 区域清单（国际版 / 国内版）：界面下拉的唯一来源
            commands::regions,
            // 切换「当前区域」（左下角选择器的落点）：只动指针，不碰任何区域的设置
            commands::set_region,
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
            // 界面上只讲对客通知，请求级细节在调试日志文件里 —— 这是打开它的入口
            stealth::reveal_debug_log,
            proxy::free_models,
            // 加速更新下载（多镜像源 + 签名自验，见 accel.rs）
            accel::update_accelerated,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    // 退出时安全关闭接管：摘掉端点、停掉监听，不给用户留下「配置指向一个已经不在
    // 的本地端口」这种残留。不用碰 Qoder 的进程 —— 它的每一次推理都是新起的
    // 一次性 `--print` 进程，下一次会话读到的就是还原后的配置。
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
