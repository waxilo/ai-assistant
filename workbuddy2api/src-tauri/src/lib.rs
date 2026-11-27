mod accel;
mod accounts;
mod alerts;
mod atrest;
mod auth_file;
// 静默自动更新：调度线程到点后驱动（见 autoupdate.rs）
mod autoupdate;
mod briefing;
mod broker;
// 上游模型清单整理（能聊天的有哪些、按价格排序）—— 见 catalog.rs
mod catalog;
mod checkin;
// 一键接入 Codex：改写它的 config.toml + 生成模型目录（见 codex.rs）
mod codex;
// 重启 Codex 桌面应用（接入/断开后不重启不生效）
mod codex_restart;
mod commands;
// 对外 API 网关：独立端口 + Key + OpenAI 兼容（见 gateway/mod.rs）
mod gateway;
// HTTP/1.1 服务器原语（网关用；手写是因为 offline 装不了新依赖）
mod http;
mod httpsrv;
mod ledger;
mod logs;
mod notify;
mod oauth;
mod refresh;
mod rng;
// 选号：评分排序 / 限流冷却（从原接管反代抽出来，网关专用）
mod routing;
mod scheduler;
mod tray;
// 上游地址改写与转发客户端
mod upstream;

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
        // 也就不会起第二个调度线程、不会抢反代端口、不会重复改客户端侧的文件。
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
                // 静态加密字段（新版客户端的 `$wbEncrypted`）的解密缓存目录。
                // 只影响「从本机凭证导入」这一条路：拿不到密钥就明确报错，而不是静默导不出。
                atrest::init(dir);
            }
            // 定时自动签到：独立后台线程，与进程同生命周期。
            // 只在应用运行期间生效——桌面端退出后没有守护进程可代为执行。
            scheduler::spawn(app.handle().clone());
            // 对外 API 网关：按设置启停 / 换端口重绑（带 Key、OpenAI 兼容）
            gateway::spawn(app.handle().clone());
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
            commands::refresh_all,
            commands::refresh_account_credits,
            commands::discover_local_accounts,
            commands::oauth_start,
            commands::oauth_poll,
            commands::open_external,
            // 凭证管家：五个都是池级命令，不带账号 id（一池一 uuid、池内一把闸）
            commands::broker_upload,
            commands::broker_link,
            commands::broker_unbind,
            commands::broker_state,
            commands::broker_sync_now,
            commands::broker_claim_renewer,
            commands::broker_release_renewer,
            commands::get_settings,
            commands::save_settings,
            commands::apply_settings,
            commands::test_notify,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_logs,
            commands::clear_logs,
            // 积分简报：日条目 = 当天时条目之和（现算），口径见 briefing 模块。
            // 条目只由后台每小时结算一次，这里没有「手动生成一条」的入口。
            commands::credit_briefing,
            commands::credit_briefing_clear,
            // 开启简报：清历史 + 采一次样**只对齐基线**（断档期增量不记）
            commands::credit_briefing_enable,
            commands::app_version,
            // 对外 API 网关：运行状态 / 重新签发 Key
            commands::gateway_status,
            commands::gateway_key_rotate,
            // 上游可对话模型清单（页面照着它填，也是接入 Codex 的候选集）
            commands::chat_models,
            // 一键接入 Codex：改它的 config.toml，所以有 status/connect/disconnect
            // 三个口子——改用户机器上别人的配置文件，必须能看见状态、也能原样撤回
            commands::codex_status,
            commands::codex_connect,
            commands::codex_disconnect,
            commands::codex_app_running,
            commands::codex_restart,
            // 加速更新下载（多镜像源 + 签名自验，见 accel.rs）
            accel::update_accelerated,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    // 退出时**不需要**清理网关：它只监听自己的端口，进程一退端口自然释放。
    // （助手的接管则必须在退出前摘掉客户端端点配置，否则下次启动会指向一个死端口——
    //  网关没有这类外部状态，这也是它比接管简单的地方。）
    app.run(|handle, event| {
        let _ = &handle;
        match &event {
            // macOS：窗口全隐藏后点 Dock 图标（或 finder 重新打开）→ 唤回主窗口。
            // Reopen 是 macOS 独有变体，Windows 编译时必须条件编译掉。
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => tray::show_main(handle),
            _ => {}
        }
    });
}
