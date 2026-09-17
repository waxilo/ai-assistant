//! Tauri 命令层：账号管理、签到、设置、智能接管。

use crate::accounts::{self, Account, Settings};
use crate::briefing;
use crate::broker;
use crate::checkin;
use crate::ledger;
use crate::oauth;
use crate::trae_auth;
use tauri::Manager;
use std::path::{Path, PathBuf};

/// 解析应用数据目录（macOS: ~/Library/Application Support/<identifier>）
pub fn try_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位数据目录：{e}"))?;
    let _ = std::fs::create_dir_all(&dir);
    Ok(dir)
}

fn settings(app: &tauri::AppHandle) -> Settings {
    match try_data_dir(app) {
        Ok(dir) => accounts::load_settings(&dir),
        Err(_) => Settings::default(),
    }
}

/// 绑定凭证池时先整池同步一轮再让调用方去读账号。
///
/// ⚠️ **必须在 `load_accounts` 之前调用**：它会把云端那一份并进 `accounts.json`，
/// 而闸带回来的才是最新凭证（本地那份可能早被别的机器换掉了）。
/// 先读后同步 = 整轮都在用已作废的 token，症状是「绑定之后签到反而全失败」。
///
/// 未绑定、距上次同步不到两分钟、闸在别的机器手里，都是**常态**，静默返回；
/// 真出错也只写进 `broker::status().error`。它挂在签到 / 刷新 / 接管路由这些主流程上，
/// 不该因为管家不可达就把主流程拦住 —— 本地凭证本来就还能用。
pub(crate) async fn sync_pool_if_bound(dir: &Path) {
    if !broker::bound() {
        return;
    }
    let _ = broker::sync(dir, false).await;
}

// ---------------------------------------------------------------------------
// 账号
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_accounts(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    Ok(accounts::load_accounts(&try_data_dir(&app)?))
}

/// 按需回源补全账号资料（占位名 / 脱敏手机号 → 服务端 `ScreenName` / `NonPlainTextMobile`），
/// 返回更新后的列表。
///
/// 只对资料不全的账号发请求，失败静默保留原值 —— 见 [`crate::profile`]。
#[tauri::command]
pub async fn refresh_account_profiles(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    sync_pool_if_bound(&dir).await;
    Ok(crate::profile::sync_profiles(&dir).await)
}

/// 导入账号（去重后追加）。
///
/// 前端只负责「它知道的东西」——不该也不需要在浏览器里编 `id`：候选记录一律先过
/// [`accounts::normalize`] 补齐 `id` / `user_id` / `created_at`，再按手机号或 token 去重。
#[tauri::command]
pub async fn import_accounts(app: tauri::AppHandle, accounts: Vec<Account>) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    for mut a in accounts {
        accounts::normalize(&mut a);
        // 按手机号（优先）或 token 去重；已存在则不重复添加
        if accounts::contains_equivalent(&list, &a) {
            continue;
        }
        list.push(a);
    }
    accounts::save_accounts(&dir, &list)?;
    // 绑了凭证池就立刻整池同步一轮，把新账号推上去，别的机器才能接手
    if broker::bound() {
        let _ = broker::sync(&dir, true).await;
    }
    Ok(list)
}

#[tauri::command]
pub fn remove_account(app: tauri::AppHandle, id: String) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let list = accounts::load_accounts(&dir);
    let kept: Vec<_> = list.into_iter().filter(|a| a.id != id).collect();
    accounts::save_accounts(&dir, &kept)?;
    Ok(kept)
}

/// 扫描本机 TraeWork 已登录账号，返回「可导入」列表（不含已在库里的）。
#[tauri::command]
pub fn discover_local(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let existing = accounts::load_accounts(&dir);
    Ok(trae_auth::discover_local_accounts()
        .into_iter()
        .map(Account::from)
        .filter(|a| !accounts::contains_equivalent(&existing, a))
        .collect())
}

// ---------------------------------------------------------------------------
// 签到
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn checkin_one(app: tauri::AppHandle, id: String) -> Result<checkin::CheckinResult, String> {
    let dir = try_data_dir(&app)?;
    sync_pool_if_bound(&dir).await;
    let list = accounts::load_accounts(&dir);
    let mut account = list
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| "账号不存在".to_string())?;
    // 临近过期先续签（只在到期窗口内才发请求，见 `renew::needs_renew`）
    if let Some(out) = crate::renew::renew_if_needed(&dir, &mut account).await {
        crate::logs::push(&account.name, out.renewed, format!("续签：{}", out.message));
    }
    let result = checkin::do_checkin(&account).await;
    crate::logs::push(&account.name, result.success, format!("签到：{}", result.message));
    Ok(result)
}

#[tauri::command]
pub async fn checkin_all(app: tauri::AppHandle) -> Result<Vec<checkin::CheckinResult>, String> {
    let dir = try_data_dir(&app)?;
    sync_pool_if_bound(&dir).await;
    let list = accounts::load_accounts(&dir);
    let mut results = Vec::new();
    for mut account in list {
        if let Some(out) = crate::renew::renew_if_needed(&dir, &mut account).await {
            crate::logs::push(&account.name, out.renewed, format!("续签：{}", out.message));
        }
        let result = checkin::do_checkin(&account).await;
        crate::logs::push(&account.name, result.success, format!("签到：{}", result.message));
        results.push(result);
    }
    Ok(results)
}

// ⚠️ 这里原来有个 `renew_accounts`（手动续签）命令。2026-09-15 按用户要求**整体删除**：
// 续签必须全自动，界面上不提供按钮，也就不需要一个「点一下才续」的后端入口。
// 自动路径完全覆盖它 —— 后台线程启动即巡、之后每 30 分钟一轮（`renew::spawn`），
// 并且每次签到之前都会顺手续一次（`checkin_one` / `checkin_all` / `scheduler::run_checkin`）。
// 想「立刻验证某个账号到底还能不能续签」，用真机探针（它是删掉按钮后唯一的手动手段）：
//     cargo test --lib -- --ignored --nocapture live_renew_probe

#[tauri::command]
pub async fn checkin_status(app: tauri::AppHandle) -> Result<Vec<checkin::AccountStatus>, String> {
    let dir = try_data_dir(&app)?;
    sync_pool_if_bound(&dir).await;
    let mut list = accounts::load_accounts(&dir);
    let mut out = Vec::new();
    let mut dirty = false;
    for account in list.iter_mut() {
        // ① 今日是否已签到（签到状态接口）
        let status = checkin::query_status(account).await;
        // ② 账号已有积分（entitlement 用量接口）；拉到就落盘，供界面展示与接管选号
        if let Some(u) = checkin::fetch_ent_usage(account).await {
            let next = accounts::CreditSnapshot::now(u.remaining, u.unlimited, u.earliest_expiry_ms);
            let prev = account.credit_snapshot.as_ref();
            let changed = prev.map(|p| (p.credits, p.unlimited, p.earliest_expiry_ms))
                != Some((next.credits, next.unlimited, next.earliest_expiry_ms));
            if changed {
                account.credit_snapshot = Some(next);
                dirty = true;
            }
        }
        // 拉不到（限流 9074 / 掉线）时沿用上次已知的积分，而不是把已有数字抹成未知
        let snap = account.credit_snapshot.as_ref();
        out.push(checkin::AccountStatus {
            id: account.id.clone(),
            checked_in: status.as_ref().map(checkin::is_checked_in).unwrap_or(false),
            message: status.as_ref().map(checkin::message_of).unwrap_or_default(),
            credits: snap.and_then(|s| s.credits),
            unlimited: snap.map(|s| s.unlimited).unwrap_or(false),
            earliest_expiry_ms: snap.and_then(|s| s.earliest_expiry_ms),
        });
    }
    if dirty {
        let _ = accounts::save_accounts(&dir, &list);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_logs() -> Result<Vec<crate::logs::LogEntry>, String> {
    Ok(crate::logs::entries())
}

#[tauri::command]
pub fn clear_logs() -> Result<(), String> {
    crate::logs::clear();
    Ok(())
}

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    Ok(settings(&app))
}

#[tauri::command]
pub fn save_settings(app: tauri::AppHandle, settings: Settings) -> Result<Settings, String> {
    let dir = try_data_dir(&app)?;
    // ⚠️ **「参与扣费」的改动必须留痕。**
    //
    // 它和别的设置不一样：改了它，接下来每一笔扣费换给谁都会变。而这条通路原来只是纯写盘，
    // 于是接管动态里只有**结果**（`route_start`：这笔用了谁），没有**前提**
    // （「名单在什么时候被改成了两个都参与」）——
    // 2026-09-16 用户连着两次问「为什么扣的不是我勾的那个」，第一次能查到的证据正是
    // 被这个盲区挡住的。**对账要成立，前提和结果都得在。**
    //
    // 只在**真的变了**时才写（`update()` 是整体覆盖保存，别的字段一改也会走到这里）。
    let before = accounts::load_settings(&dir).billing_account_ids;
    accounts::save_settings(&dir, &settings)?;
    if before != settings.billing_account_ids {
        crate::journal::append(
            &dir,
            "billing_list_changed",
            &format!(
                "「参与扣费」名单已改为 {} —— 下一次请求就按新名单选号（名单内**额度到期最早的先用**；\
                 空名单 = 全部账号，且**新加的账号会自动参与**）",
                billing_list_text(&dir, &settings.billing_account_ids)
            ),
        );
    }
    Ok(settings)
}

/// 把「参与扣费」名单渲染成人读的说法：`尾号 9075、尾号 9152`。
///
/// 空列表**不能**只说「空」：它的语义是「全部账号」，而且**后加的账号会自动进来** ——
/// 这正是最容易被误解的一处，必须写出来。
fn billing_list_text(dir: &Path, ids: &[String]) -> String {
    if ids.is_empty() {
        return "全部账号（今后新加的账号也会自动参与）".to_string();
    }
    let all = accounts::load_accounts(dir);
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        match all.iter().find(|a| &a.id == id) {
            Some(a) => out.push(format!("尾号 {}", accounts::tail(a.phone.as_deref()).unwrap_or_else(|| "-".into()))),
            None => out.push(format!("已不存在的账号 {}", &id[..id.len().min(8)])),
        }
    }
    out.join("、")
}

// ---------------------------------------------------------------------------
// 浏览器登录（OAuth）
// ---------------------------------------------------------------------------

/// 「浏览器登录」第一步：申请 state + 授权链接。
///
/// `host` 省略时默认国内版 `https://api.trae.cn`；国际版传 `https://api.trae.ai`。
/// 端点需抓包逆向确认后才可用（见 [`crate::oauth`] 顶部说明）。
#[tauri::command]
pub async fn oauth_start(host: Option<String>) -> Result<oauth::OAuthStart, String> {
    oauth::start(host).await
}

/// 「浏览器登录」第二步：轮询一次授权结果。
///
/// 返回 `done=false` 表示用户还没完成授权（继续轮询即可，**不是错误**）；
/// `done=true` 且带 `token` 表示授权完成，`nickname`/`phone`/`uid` 一并带回。
#[tauri::command]
pub async fn oauth_poll(login_id: String) -> Result<oauth::OAuthPoll, String> {
    oauth::poll(&login_id).await
}

/// 在系统默认浏览器打开链接（用于打开浏览器登录的授权页）
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    oauth::open_in_browser(&url)
}

// ---------------------------------------------------------------------------
// 智能接管：本地反代 + TraeWork 端点覆盖（一体开关）
// ---------------------------------------------------------------------------

/// **单个**目标应用的接管状态。
///
/// 界面按这个数组渲染「接管哪些应用」的多选，以及每个应用自己那句「挡着你的话」。
#[derive(serde::Serialize, Clone)]
pub struct AppStatus {
    /// 稳定 id（macOS 下 = `.app` 名），也是设置里记录选择用的键。
    pub id: String,
    /// 显示名（当前与 id 相同：`.app` 名本来就够清楚，多一层映射只会多一处会漂的东西）。
    pub label: String,
    /// 应用包（macOS）/ 安装目录（Windows）—— 排障时要能一眼看到在改谁。
    pub bundle: String,
    pub app_dir: String,
    /// 是否在接管名单里（名单为空 = 全部 ⇒ 这里恒 `true`）。
    pub selected: bool,
    /// 当前是否在运行。
    pub running: bool,
    /// `product.json` 的端点是否已指向本机反代。
    pub installed: bool,
    /// 上述改写是不是本助手写的（只有带标记才敢还原）。
    pub ours: bool,
    /// 它的安装目录是否**真能写** —— 不能写就没有任何一步能成。
    pub writable: bool,
    /// 它自己的闸门补丁状态。
    pub patch: crate::patch::PatchStatus,
    pub upstream_http: Option<String>,
    pub upstream_ws: Option<String>,
    /// **只在有事要说时非空**（版本不认识 / 不可写 / 被别人改过 / 端点丢了）。
    /// 一切正常时是空串 —— 界面上一行文字都不该出现（见 `TakeoverPage` 的取向）。
    pub message: String,
}

/// 智能接管的完整状态（供界面一次性渲染）。
#[derive(serde::Serialize, Clone)]
pub struct TakeoverStatus {
    /// 用户是否开启了智能接管（对应 `Settings.takeover_enabled`）。
    pub enabled: bool,
    /// 本地反代监听端口。
    pub port: u16,
    /// 本地反代是否正在监听。
    pub proxy_active: bool,
    /// 反代启动失败原因（端口占用等）。
    pub proxy_error: Option<String>,
    /// 本机反代端点基址（写进 `product.json` 的那个值）：
    /// 恒为 `http://127.0.0.1:PORT`（免证书形态，唯一来源见 `endpoint::base_url`）。
    pub endpoint_base: String,
    /// 当前生效的接管规则（`proxy-rules.json`，热加载）。
    pub rules: crate::rules::Rules,
    /// 端点覆盖租约是否新鲜（反代的心跳）。
    pub lease_fresh: bool,
    /// 本机发现到的**全部** Trae 应用（含未勾选的）—— 界面据此渲染「接管应用」多选。
    pub apps: Vec<AppStatus>,
    /// 接管名单里、但本机已经不存在的 id（应用卸载了 / 改名了）。
    pub missing_apps: Vec<String>,
    /// 总状态那句话（成功时也可以是陈述句；界面只在有东西挡路时才显示）。
    pub message: String,
}

/// 组装对 UI 的状态。**`message` 只描述「现在挡在你面前的是什么」**，措辞按「谁知道得最准」分配：
///
/// - 文件层的事实（目录找不到 / 不可写 / 端点被谁改的）→ 用 `endpoint.rs` 探针给出的那句
///   （尤其「不可写」：权限位 / macOS「App 管理」TCC / 只读卷的成因只有探针分得清，且它带了处置办法）；
/// - 补丁层的状态 → 用 `patch.rs` 探针那句（它自带「怎么打补丁」这个下一步）；
/// - 运行层的事实（反代在不在监听）→ 只有这里知道，自己出话。
///
/// ⚠️ 现在是**按应用**各给一句（`AppStatus::message`），顶层的 `message` 只是「最先要说的那句」。
/// 之所以要拆开：本机可能有两个 Trae shell，一个能接管、另一个版本不认识 ——
/// 合成一句话必然要说谎，用户也无从知道该点掉哪一个。
fn build_status(dir: &std::path::Path, s: &Settings) -> TakeoverStatus {
    let endpoint_base = crate::endpoint::base_url(s.takeover_port);
    let px = crate::proxy::status();
    let rules = crate::rules::Rules::load(dir);

    let targets = crate::target::discover();
    // 进程表只取**一次**供全部应用共用。Windows 下每问一个应用就是 spawn 一次
    // `tasklist`（几百毫秒 + 一个控制台窗口，见 `proc.rs`），而这函数每 5 秒被调一次。
    let images = crate::target::process_images();
    let apps: Vec<AppStatus> = targets
        .iter()
        .map(|t| {
            let ep = crate::endpoint::status(t, dir, &endpoint_base);
            let patch = crate::patch::status(t);
            let selected = crate::target::is_selected(&s.takeover_apps, &t.id);
            let message = app_message(s, &px, selected, &ep, &patch, &t.id);
            AppStatus {
                id: t.id.clone(),
                label: t.id.clone(),
                bundle: t.bundle.display().to_string(),
                app_dir: ep.app_dir.clone().unwrap_or_default(),
                selected,
                running: t.running_in(&images),
                installed: ep.installed,
                ours: ep.ours,
                writable: ep.writable,
                patch,
                upstream_http: ep.upstream_http,
                upstream_ws: ep.upstream_ws,
                message,
            }
        })
        .collect();

    let missing_apps = crate::target::missing(&s.takeover_apps);

    // 顶层那句 = 「最先要说的」：先挑真的挡着路的（被选中的应用里的第一条），
    // 都没有则给一句陈述 —— 界面只在有东西挡路时才把它显示出来（见 `TakeoverPage`）。
    let message = if apps.is_empty() {
        "本机没有发现可接管的 Trae 应用（在应用目录里找不到带 bootConfig 的 product.json）。"
            .to_string()
    } else if let Some(first) = apps.iter().find(|a| a.selected && !a.message.is_empty()) {
        first.message.clone()
    } else if !missing_apps.is_empty() {
        format!(
            "接管名单里的这些应用本机已不存在：{}。取消勾选即可（不影响其它应用）。",
            missing_apps.join("、")
        )
    } else if s.takeover_enabled {
        "接管已生效：端点走明文回环，不需要任何证书。".to_string()
    } else {
        "未接管：应用仍直连官方。".to_string()
    };

    TakeoverStatus {
        enabled: s.takeover_enabled,
        port: s.takeover_port,
        proxy_active: px.active,
        proxy_error: px.error,
        endpoint_base,
        rules,
        lease_fresh: crate::endpoint::lease_fresh(dir),
        apps,
        missing_apps,
        message,
    }
}

/// 某个应用此刻「挡着路的是什么」。**空串 = 没事要说**。
///
/// 顺序是刻意排的：
/// 1. **打不了补丁**（版本不认识 / 目录不可写）必须排最前 —— 那时开关是灰的，
///    而这句话就是「为什么开不了」。与开关当前状态无关，所以**永远要说**。
///    连「目录不可写」也由 `patch.message` 来说（它已含 TCC 指引），
///    否则会被端点话术抢答成一句与当下无关的话。
/// 2. **能打、但还没打** —— 只在**接管开着**时才是问题（见下面的长注释）。
/// 3. 端点被**别人**改过 → 说清楚我们为什么不动它。
/// 4. 开着但反代没监听 / 端点没指过来 → 说下一步做什么。
fn app_message(
    s: &Settings,
    px: &crate::proxy::ProxyStatus,
    selected: bool,
    ep: &crate::endpoint::EndpointStatus,
    patch: &crate::patch::PatchStatus,
    id: &str,
) -> String {
    // 没被选中的应用不需要说任何话（用户明确不让接管它）
    if !selected {
        return String::new();
    }
    // ① 打不了（版本不认识 / 目录不可写）：这是「开关为什么是灰的」的答案，永远要说。
    if !(patch.recognized && patch.writable) {
        return patch.message.clone();
    }
    // ② 能打、但还没打：**只在接管开着时才是问题**。
    //
    //    接管开着 ⇒ 这个应用的端点已经指向本机明文回环，而闸门还没放开 ⇒ 它启动就会崩，
    //    所以必须当场喊出来（升级应用会把 out/main.js 换回去，这条真的会发生）。
    //
    //    接管**没开**时它是最正常不过的状态 —— 谁都没打过补丁，补丁本来就不该在。
    //    这里曾经无条件 `if !patch.patched { return patch.message }`，于是全新安装的页面上
    //    永远挂着一个红框：「未打补丁（识别正常：闸门 2 处 / 身份头数组在位），可以打……」
    //    —— 一句既不是故障、也不是下一步的话（2026-09-16 用户上报要求去掉）。
    //    真正的失败不在这里说：`patch::apply` 打不成时会写进**接管动态**。
    if s.takeover_enabled && !patch.patched {
        return patch.message.clone();
    }
    if ep.installed && !ep.ours {
        return format!(
            "「{id}」的端点已指向本机反代，但不是本助手改的——为免误伤，助手不会动它。"
        );
    }
    if s.takeover_enabled && !px.active {
        return format!("已开启接管，但本地反代未在监听，所以「{id}」用不上账号池——请检查端口是否被占用。");
    }
    if s.takeover_enabled && !ep.installed {
        return format!(
            "本地反代已就绪，但「{id}」的端点还没指过来（升级后会丢失）。重新开启一次开关即可。"
        );
    }
    String::new()
}

/// 只读：当前智能接管状态（不修改任何文件）。
///
/// ⚠️ **必须是 `async` + `spawn_blocking`**，不能是同步命令。
///
/// Tauri 里同步命令**在主线程上执行**，而这条命令要：
/// ① `target::discover()` 扫应用目录、② `patch::status()` 逐个应用读文件算校验、
/// ③ `process_images()` spawn 一次 `tasklist`（Windows 上几百毫秒）。
/// 三件事加起来轻松超过一帧的时间预算 —— 接管页每 5 秒轮询一次，于是**窗口被反复冻住**，
/// 表现就是「切菜单很卡」。放到 blocking 线程池后主线程只负责收结果。
///
/// 顺带一个坑：`spawn_blocking` 的闭包里不能再持有 `&app`（`AppHandle` 的借用活不过
/// 这个 await），所以路径必须在切线程**之前**算好。
#[tauri::command]
pub async fn takeover_status(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let s = accounts::load_settings(&dir);
        build_status(&dir, &s)
    })
    .await
    .map_err(|e| format!("读取接管状态任务异常：{e}"))
}

/// 开启智能接管：**先给名单里的应用逐个打免证书补丁** → 只读预检 → 预检端口 →
/// 启用本地反代 → 确认已监听 → 改写各应用端点并重启它们。
///
/// 顺序是刻意排的：前几步都发生在「动任何开关之前」，任何一步不通过，
/// 开关 / 反代 / 应用进程**一个都没动** —— 不留半开状态。
#[tauri::command]
pub async fn takeover_enable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || enable_endpoint(dir))
        .await
        .map_err(|e| format!("开启接管任务异常：{e}"))?
}

/// 解析「这次要接管哪些应用」。
///
/// 名单为空 = 全部已发现（与「参与扣费的账号」`billing_account_ids` 同一套语义）。
/// 两种失败都**自带出路**：一个都没发现（本机没装 / 装在别处）、名单里的应用都不在了。
fn resolve_targets(s: &Settings) -> Result<Vec<crate::target::AppTarget>, String> {
    let all = crate::target::discover();
    if all.is_empty() {
        return Err(
            "本机没有发现可接管的 Trae 应用（在应用目录里找不到带 bootConfig 的 product.json），\
             没有可以改道的对象。"
                .to_string(),
        );
    }
    let targets = crate::target::select(&s.takeover_apps);
    if targets.is_empty() {
        return Err(format!(
            "接管名单里的应用本机都不存在了（{}）。请在接管页重新勾选要接管的应用。",
            crate::target::missing(&s.takeover_apps).join("、")
        ));
    }
    Ok(targets)
}

/// 把**本次刚打上的**补丁还回去（只在开启流程中途失败时调用）。
///
/// 为什么需要它：本模块的明文约定是「补丁的生命周期完全跟着开关走 ——
/// 不存在『接管关了、应用还带着补丁』这种只靠人记住的状态」。多目标之后出现了新窗口：
/// 第一个应用的补丁已经打上，而第二个应用在打补丁 / 预检 / 端口预检那一步失败。
/// 不回滚的话，开关还是关的、第一个应用却带着补丁 —— 正好落进那个被否掉的状态。
fn rollback_patches(dir: &Path, applied: &[crate::target::AppTarget]) {
    for t in applied {
        if let Err(e) = crate::patch::revert(dir, t) {
            crate::journal::append(
                dir,
                "patch_revert_fail",
                &format!(
                    "回滚「{}」的免证书补丁失败（接管并未开启，不影响使用；下次开启会重打）：{e}",
                    t.id
                ),
            );
        }
    }
}

/// 端口预检。被占就**当场**说清「谁占着、该换到哪个」，绝不先把开关打开再回滚 ——
/// 回滚是对的，但用户拿到的只有一句 `Address already in use`，等于死路。
/// 已经在监听同一端口的（例如重复点开关）不算冲突，放行给后面的幂等逻辑。
fn ensure_port_free(port: u16) -> Result<(), String> {
    let st = crate::proxy::status();
    if st.active && st.port == port {
        return Ok(());
    }
    if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
        let hint = crate::portcheck::busy_hint(port);
        return Err(format!("端口 {port} {hint}。改好端口再开接管即可。"));
    }
    Ok(())
}

/// 等本地代理真正在监听（最多 ~3s）。不就绪则**回滚开关**并返回原因。
fn wait_proxy_ready(dir: &std::path::Path, port: u16) -> Result<(), String> {
    for _ in 0..30 {
        let st = crate::proxy::status();
        if st.active && st.port == port {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut s2 = accounts::load_settings(dir);
    s2.takeover_enabled = false;
    accounts::save_settings(dir, &s2)?;
    let err = crate::proxy::status().error.unwrap_or_default();
    let tail = if err.is_empty() { String::new() } else { format!("（{err}）") };
    // 事件名用 `takeover_fail`（=「开关拨了但这一趟没生效」）而**不是** `proxy_error`：
    // 后者是技术诊断通道，不上界面。而这是**开启接管失败的唯一原因记录**，
    // 用户必须能在接管动态里看到它。
    crate::journal::append(
        dir,
        "takeover_fail",
        &format!("本地代理未能在 127.0.0.1:{port} 监听{tail}，已回滚，各应用未被改动"),
    );
    Err(format!("本地代理未能在 127.0.0.1:{port} 监听{tail}，已回滚，未改动任何应用。"))
}

/// 开启「端点改道」（改各应用的 `product.json`）—— **现在唯一的改道方式**。
///
/// 顺序是**刻意**的，前几步都发生在「动开关之前」：
/// 0. **清空两种日志**（用户日志 + 诊断日志）——「开启接管先清空」：开启 = 新的一本账。
/// 0a. 只读前置检查：**每个**目标都得「版本认识 + 目录可写」，否则先在什么都不动时报错。
/// 0b. 逐个打闸门补丁 —— 这是「明文端点」能成立的前提。中途失败会把已打上的那些还回去
///     （见 [`rollback_patches`]），所以「关了开关却带着补丁」这个状态不会出现。
/// 0c. **端点预检**——写进去的值必须能通过**每个应用自己**的 URL pattern 校验，
///     否则它会启动即崩。不通过就当场报错。
/// 1. 端口预检：见 [`ensure_port_free`]。
/// 2. 必须先确认反代在监听，才允许改端点，否则应用的请求会打到死端口。
/// 3. 改写 + **只重启被接管的那几个应用**（`target::with_restart`）。
fn enable_endpoint(dir: PathBuf) -> Result<TakeoverStatus, String> {
    let mut s = accounts::load_settings(&dir);
    let port = s.takeover_port;
    let endpoint_base = crate::endpoint::base_url(port);
    let targets = resolve_targets(&s)?;

    // 0) **开启接管 = 新的一本账** ⇒ 两种日志一起清空（用户日志 + 诊断日志）。
    //
    //    日志回答的是「刚才这一轮发生了什么」，而用户恰恰是在**刚开完接管之后**去看它。
    //    不清的话，上一轮那几百条（其中大半是「接管收到 [透传] …」这类路径清单）会把
    //    这一轮真正有用的几条埋掉 —— 那正是把日志拆成两条通道要解决的问题。
    //
    //    ⚠️ 必须发生在**写任何一条日志之前**（下面 0a 就可能写 `rules_write`）。
    //    ⚠️ 位置在 `resolve_targets` **之后**：连目标都解析不出来时这一轮压根没开始，
    //       不该把上一轮的历史毁掉。
    crate::journal::clear(&dir);

    // 0a) 把规则文件落地（已存在则不动）：让「现在到底用哪张表」随时可见可改。
    //     ⚠️ 这个动作原先挂在已移除的「经系统代理」开启流程里，不能跟着一起丢掉：
    //     接管一旦出问题，第一件要确认的就是「用的是哪张表」，而那时文件必须在。
    if let Err(e) = crate::rules::Rules::write_default_if_absent(&dir) {
        crate::journal::append(&dir, "rules_write", &format!("规则文件落地失败：{e}"));
    }

    // 0b) **只读**前置检查：版本不认识 / 目录不可写这类「必然失败」，要在动任何东西之前说。
    //     `patch::apply` 自己也是 fail-closed 的，但那已经是**写操作途中**了。
    for t in &targets {
        let p = crate::patch::status(t);
        if !p.recognized || !p.writable {
            // ⚠️ **必须留痕**：上面第 0 步刚把两种日志清空了，而这里是「清空之后、写第一条
            //    日志之前」唯一一条早退路径 —— 不留痕就成了「拨了开关、日志一片空白、
            //    也不知道为什么」。旧版这里直接 `return Err`，页面的红框切页即没。
            crate::journal::append(
                &dir,
                "patch_fail",
                &format!("「{}」无法接管：{}", t.id, p.message),
            );
            return Err(p.message);
        }
    }

    // 0c) 打补丁。`patch::apply` 幂等，所以重复点开关不会重复写盘。
    let mut freshly_patched: Vec<crate::target::AppTarget> = Vec::new();
    for t in &targets {
        if crate::patch::is_patched(t) {
            continue;
        }
        match crate::patch::apply(&dir, t) {
            Ok(p) if p.patched => freshly_patched.push(t.clone()),
            Ok(p) => {
                // 「apply 回来说打了、可文件不是补丁后形态」也是失败。**必须落进接管动态**：
                // 页面上的红框只在这次操作期间存在（而且失败后开关会弹回去），
                // 接管动态是用户事后回看「刚才到底发生了什么」的唯一地方。
                crate::journal::append(
                    &dir,
                    "patch_fail",
                    &format!("「{}」的免证书补丁没打上：{}", t.id, p.message),
                );
                rollback_patches(&dir, &freshly_patched);
                return Err(p.message);
            }
            Err(e) => {
                crate::journal::append(&dir, "patch_fail", &format!("打免证书补丁失败：{e}"));
                rollback_patches(&dir, &freshly_patched);
                return Err(e);
            }
        }
    }

    // 0d) 端点预检（只读）。写进去的值若会被某个应用拼成非法 URL pattern，它会启动即崩
    //     —— 所以这一步必须在**动开关之前**，失败就是「什么都没发生」。
    //     它还负责拦住「没打补丁却写了明文端点」这条最危险的路径。
    for t in &targets {
        if let Err(e) = crate::endpoint::preflight(t, &endpoint_base) {
            rollback_patches(&dir, &freshly_patched);
            return Err(e);
        }
    }

    // 1) 预检端口
    if let Err(e) = ensure_port_free(port) {
        rollback_patches(&dir, &freshly_patched);
        return Err(e);
    }

    // 2) 先开启：让反代线程开始监听（反代只在 takeover_enabled 时绑定端口）
    if !s.takeover_enabled {
        s.takeover_enabled = true;
        accounts::save_settings(&dir, &s)?;
    }

    // 3) 等反代真正就绪；不就绪则回滚，绝不留下指向死端口的覆盖
    wait_proxy_ready(&dir, port)?;

    // 4) 改写各应用的端点 + 重启它们使其生效
    let dir2 = dir.clone();
    let targets2 = targets.clone();
    match crate::target::with_restart(&targets, || {
        for t in &targets2 {
            crate::endpoint::install(t, &dir2, &endpoint_base)?;
        }
        Ok(())
    }) {
        Ok((_unit, outcome)) => note_restart(
            &dir,
            &outcome,
            "为让端点改写生效，已重启（未保存的输入请自行确认）",
            "开启接管时被接管的那些应用都没在运行，所以**没有应用重启** —— 这不是失败",
        ),
        Err(e) => {
            // 这里可能有两种原因，**都要留痕**：界面上那行红字是一次性的（切页即没），
            // 而「开关拨了却没生效」恰恰是最该事后能查清的一件事 —— 2026-09-16 用户
            // 报「开关接管不重启应用」时，能查的只有那行随时会消失的红字。
            //   ① 闸门在写盘那一刻才拦住（正常应在第 0c 步就拦住）；
            //   ② **应用退不掉** —— `with_restart` 在动手之前就放弃了，配置一个字没改。
            // 事件名取「动作 + 成败」的通用形态，具体是哪一条在 detail 里（本来就是完整句子）。
            crate::journal::append(&dir, "takeover_fail", &e);
            // 无论哪种，反代都已起来、开关都已打开 —— 必须把开关回滚成关闭，不留半开状态。
            let mut s2 = accounts::load_settings(&dir);
            if s2.takeover_enabled {
                s2.takeover_enabled = false;
                let _ = accounts::save_settings(&dir, &s2);
            }
            rollback_patches(&dir, &freshly_patched);
            return Err(e);
        }
    }

    // 5) 收尾：把**名单之外**的应用放下（它们可能刚被取消勾选，或上次接管留下的痕迹）
    let keep = crate::target::select(&accounts::load_settings(&dir).takeover_apps);
    crate::endpoint::sweep(&dir, &crate::target::discover(), &keep);

    let s3 = accounts::load_settings(&dir);
    Ok(build_status(&dir, &s3))
}

/// 记一条「重启」动态 —— **三种结局都要留痕**：真重启了 / 本该重启但没在跑 / 关了却拉不起来。
///
/// [`crate::target::with_restart`] 只处理**那一刻正在运行**的应用：没在跑的跳过，也刻意
/// **不替你打开**（改配置跟「现在就要用这个应用」是两件事，见该函数的注释）。
/// 所以「开了接管、应用毫无动静」是完全正常的 —— 但它是**静默**的正常：
/// 2026-09-16 用户就来问「为什么我开启接管不会重启 Trae」。
///
/// ⚠️ 更糟的一种是「退出成功、拉起失败」：用户的窗口被我们关掉了，而且不会自己回来。
/// 以前它被 `spawn().is_ok()` 吞成 `false`，于是和「本来就没在跑」共用同一句话 ——
/// **一句话把真相盖住**。现在它有自己的事件（`restart_fail`）和自己的原因。
fn note_restart(
    dir: &Path,
    outcome: &crate::target::RestartOutcome,
    done: &str,
    why: &str,
) {
    // 最坏的一种先说（红色）：应用被退出了，却没拉回来。
    for (id, err) in &outcome.failed {
        crate::journal::append(
            dir,
            "restart_fail",
            &format!(
                "「{id}」已被退出，但**没能重新启动**：{err}。请手动打开它 —— \
                 配置已经写好，重开即生效"
            ),
        );
    }
    if !outcome.restarted.is_empty() {
        crate::journal::append(
            dir,
            "restart_trae",
            &format!("{done}：{}", outcome.restarted.join("、")),
        );
    }
    // 一个都没碰过 ⇒ 这句才是真的：不是失败了，是本来就没在跑
    if !outcome.touched() {
        crate::journal::append_dedup(
            dir,
            "restart_skipped",
            &format!("{why}（按进程表判断；改过的配置会在它下次启动时自然生效，不会替你打开它）"),
        );
    }
}

/// 关闭智能接管：还原各应用 `product.json` + **自动还原免证书补丁** + 重启它们恢复官方直连，
/// 再停本地反代。**关开关 = 把应用侧改过的东西全部还回去**。
///
/// 顺序不能反：先恢复应用（端点不再指向本机，「指向死端口」的风险即消失），再停反代 ——
/// 反过来会留下「反代已停、应用仍指向它」的死状态。
///
/// 端点与补丁在**同一次重启**里一起还原：先端点、后补丁。
/// 反过来则中途会出现「明文端点 + 未打补丁的应用」这个组合，那正是让它**启动即崩**的组合。
///
/// **只碰真的被我们动过的应用**（带标记的端点 / 带标记的补丁）—— 关闭接管不该去
/// 重启一个从头到尾没参与过的应用。
///
/// 顺带做一次**残留清扫**：老版本的「经系统代理接管」会把应用的 `User/settings.json`
/// 指到本机回环代理，而那条路已整体移除（本端点对 CONNECT 一律 405）。
/// 只要那个设置还在，整个应用就不可用 —— 所以关接管时**逐个应用**确认一遍并清掉。
/// `traework::uninstall` 按**回环指纹**判定，绝不碰用户自己配的非环代理。
#[tauri::command]
pub async fn takeover_disable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let pre = accounts::load_settings(&dir);
        if crate::traework::applied(pre.takeover_port) {
            match crate::traework::uninstall(&dir) {
                Ok(msg) => crate::journal::append(&dir, "legacy_proxy_clear", &msg),
                Err(e) => crate::journal::append(
                    &dir,
                    "legacy_proxy_clear",
                    &format!("清理遗留代理设置失败：{e}"),
                ),
            }
        }

        let touched: Vec<crate::target::AppTarget> = crate::target::discover()
            .into_iter()
            .filter(|t| crate::endpoint::is_ours(t) || crate::patch::is_patched(t))
            .collect();

        // 先恢复应用（端点不再指向本机，「指向死端口」的风险即消失），再停反代。
        // 补丁还原塞进**同一个 op** 里 —— 于是每个应用只重启一次，且中间态永远安全。
        let dir2 = dir.clone();
        let mut endpoint_failures: Vec<String> = Vec::new();
        let (_, outcome) = crate::target::with_restart(&touched, || {
            for t in &touched {
                // ① 端点必须回到官方：这是「应用可用」的硬前提，失败要如实上报。
                match crate::endpoint::uninstall(t, &dir2) {
                    Ok(_) => {}
                    Err(e) => {
                        // 端点没能还回去 ⇒ **绝不能**碰它的补丁（明文端点 + 未打补丁 = 启动即崩）
                        endpoint_failures.push(format!("{}：{e}", t.id));
                        continue;
                    }
                }
                // ② 再还原补丁（次序理由见函数文档）。
                //    ⚠️ 这一步**故意不算致命**：端点已经回官方、应用完全可用，只是它还带着补丁。
                //    若把它算成失败，用户会看到「点了关闭、开关又弹回去」—— 比「补丁没还原」难懂得多
                //    （而且下次开启接管会重新打，留下的补丁也不影响任何行为）。
                if let Err(e) = crate::patch::revert(&dir2, t) {
                    crate::journal::append(
                        &dir2,
                        "patch_revert_fail",
                        &format!(
                            "还原「{}」的免证书补丁失败（端点已恢复官方直连、不影响使用，下次开启接管会重打）：{e}",
                            t.id
                        ),
                    );
                }
            }
            Ok(())
        })?;
        if !endpoint_failures.is_empty() {
            return Err(format!(
                "以下应用的端点没能还原：{}。它们仍指向本机反代，请重试或手工检查。",
                endpoint_failures.join("；")
            ));
        }

        let mut s = accounts::load_settings(&dir);
        s.takeover_enabled = false;
        accounts::save_settings(&dir, &s)?;
        note_restart(
            &dir,
            &outcome,
            "为恢复官方直连已重启（端点改写与免证书补丁一并还原）",
            "关闭接管时被接管的那些应用都没在运行，所以**没有应用重启**",
        );
        Ok(build_status(&dir, &s))
    })
    .await
    .map_err(|e| format!("关闭接管任务异常：{e}"))?
}

/// 改「接管哪些应用」——**必须先关闭接管**。
///
/// 语义与界面一致：
/// - `ids` 为空 = **全部接管**（与「参与扣费的账号」同一套语义：空 = 没配置 = 全选）；
/// - 非空 = 只接管这些（本机不存在的 id 会被丢掉，不会写进设置）。
///
/// ## 为什么开着接管时直接拒绝（2026-09-16 按用户要求）
///
/// 改名单的代价是**重启刚刚被勾上 / 被放下的那些应用**，而用户此刻正在用它们（编辑器里
/// 可能还有没保存的东西）。这正是「端口」那条规矩的同一套道理：**会动到别人应用的事，
/// 先关开关再改**。界面上那两枚 chip 在开启时也是灰的，所以这条拒绝正常情况下不会被触到 ——
/// 把它放在后端，是因为**规则只该有一个出处**：界面只是显示它，不是定义它。
///
/// ⚠️ **反面：「参与扣费」不受这条约束** —— 它不碰任何应用，所以开着也能改、下一请求即生效。
/// 两者看着像同类的「勾选表」，代价却完全不同，**别把它们统一**。
///
/// ## 顺带说明：原来的「增量协调」为什么没了
///
/// 它做过「新勾的打补丁 + 改道 + 重启、放下的还原 + 重启、没变的一个字节都不碰」。
/// 既然现在开着就不许改，那条路径**永远走不到**了 —— 而本项目对「走不到的状态」的态度是不留
/// （同「还原补丁」按钮的删除）。现在改名单只有一条路径：**关接管 → 改 → 开接管**，
/// 由 `enable_endpoint` 统一负责补丁、改道与重启（它本来就做这件事）。
/// 代码可随时从 git 历史取回。
#[tauri::command]
pub async fn takeover_set_apps(
    app: tauri::AppHandle,
    ids: Vec<String>,
) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || set_apps(dir, ids))
        .await
        .map_err(|e| format!("保存接管应用失败：{e}"))?
}

fn set_apps(dir: PathBuf, ids: Vec<String>) -> Result<TakeoverStatus, String> {
    let mut s = accounts::load_settings(&dir);
    if s.takeover_enabled {
        return Err(
            "接管开着的时候不能改「接管应用」—— 改它会重启那些应用。请先关闭接管，改完再打开。"
                .to_string(),
        );
    }

    // 只接受本机**真实存在**的 id（界面上勾的就是这些），并排序让设置文件稳定。
    let all = crate::target::discover();
    let mut ids: Vec<String> = ids
        .into_iter()
        .filter(|i| all.iter().any(|t| &t.id == i))
        .collect();
    ids.sort();
    ids.dedup();

    let before: Vec<String> = crate::target::select(&s.takeover_apps)
        .into_iter()
        .map(|t| t.id)
        .collect();
    s.takeover_apps = ids;
    accounts::save_settings(&dir, &s)?;
    let after: Vec<String> = crate::target::select(&s.takeover_apps)
        .into_iter()
        .map(|t| t.id)
        .collect();

    // **名单是前提，必须留痕** —— 与「参与扣费」同一条理由：日志里只有结果（下次开启时
    // 重启了谁）而没有前提（名单什么时候被改的）时，「为什么它被重启了」就查不到起因。
    // 空名单不能只说「空」：它的语义是「全部」，而且**后装的应用会自动进来**。
    if before != after {
        crate::journal::append(
            &dir,
            "takeover_apps_changed",
            &format!(
                "接管名单已改为「{}」（原为「{}」）—— 下次开启接管时生效",
                if after.is_empty() {
                    "全部应用".to_string()
                } else {
                    after.join("、")
                },
                if before.is_empty() {
                    "全部应用".to_string()
                } else {
                    before.join("、")
                }
            ),
        );
    }

    Ok(build_status(&dir, &accounts::load_settings(&dir)))
}

// ---------------------------------------------------------------------------
// TraeWork 主进程补丁（免证书的唯一前提）—— 生命周期已**并入「智能接管」开关**
//
// 补丁把 TraeWork 的 URL pattern 闸门从「只认 https」改成「认任何 scheme」，
// 于是本地端点可以走**明文回环**，自签 CA 与钥匙串那一整套都可以不要 ——
// 连同「把 CA 装进系统信任库」这个动作一起消失了（2026-09-15 按用户要求移除）。
//
// ⚠️ 2026-09-15 按用户要求：「还原补丁」与开关**合并**，不再有独立的打/还原命令与按钮：
//   · **开启接管** → `enable_endpoint()` 第 0 步 `patch::apply`（幂等）；
//   · **关闭接管** → `takeover_disable()` 恢复端点后**自动还原补丁**（best-effort，
//     与端点还原合并在**同一次重启**里完成）。
// 这么做的直接好处：不可能再出现「接管关了、TraeWork 还带着补丁」这种只靠人记住的状态；
// 也不会出现「关了开关又得手动点一次还原」的两步操作。
//
// 补丁改的是**别的应用**的**可执行文件**，所以那套保护一个都不能少：
// fail-closed（版本不认识就拒打）、逐字节还原、以及还原后的 sha256 指纹自证（见 `patch.rs`）。
// ---------------------------------------------------------------------------

/// 读接管规则（`proxy-rules.json`，热加载，不用重编译）。
#[tauri::command]
pub fn takeover_rules(app: tauri::AppHandle) -> Result<crate::rules::Rules, String> {
    let dir = try_data_dir(&app)?;
    Ok(crate::rules::Rules::load(&dir))
}

/// 写接管规则。**立即生效**（读侧 1s 缓存）。
#[tauri::command]
pub fn takeover_save_rules(
    app: tauri::AppHandle,
    rules: crate::rules::Rules,
) -> Result<crate::rules::Rules, String> {
    let dir = try_data_dir(&app)?;
    let path = crate::rules::Rules::path(&dir);
    let text = serde_json::to_string_pretty(&rules).map_err(|e| format!("序列化规则失败：{e}"))?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    crate::journal::append(
        &dir,
        "rules_save",
        &format!(
            "接管规则已更新：观察模式={} · 换号前缀 {} 条 · 换 WS 凭据={} · 强制透传 {} 条",
            rules.observe_only,
            rules.swap_http_prefixes.len(),
            rules.swap_ws,
            rules.never_swap_prefixes.len()
        ),
    );
    // 绕过 1s 缓存，让调用方立刻看到新值（界面需要立即回显）
    crate::rules::invalidate();
    Ok(crate::rules::Rules::load(&dir))
}

// ---------------------------------------------------------------------------
// 接管动态（journal）
// ---------------------------------------------------------------------------

/// 读取接管动态（最新在前）：谁在什么时候用了哪个账号、有没有被限流换号、代理有没有报错。
#[tauri::command]
pub fn takeover_events(app: tauri::AppHandle) -> Result<Vec<crate::journal::JournalEvent>, String> {
    Ok(crate::journal::read(&try_data_dir(&app)?))
}

/// 清空接管动态（不可恢复）。
#[tauri::command]
pub fn clear_takeover_events(app: tauri::AppHandle) -> Result<(), String> {
    crate::journal::clear(&try_data_dir(&app)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// 云端凭证池（broker）
// ---------------------------------------------------------------------------

/// 上传：创建 / 复用本机凭证池并返回 uuid。
/// 返回的 uuid 是**唯一要展示给用户复制**的东西（另一台机器靠它接上同一池）。
#[tauri::command]
pub async fn broker_upload(app: tauri::AppHandle) -> Result<broker::PoolOp, String> {
    broker::upload(&try_data_dir(&app)?).await
}

/// 绑定别处复制过来的 uuid。绑定后立刻整池同步一轮，把本地独有的账号也推上去。
#[tauri::command]
pub async fn broker_link(app: tauri::AppHandle, uuid: String) -> Result<broker::PoolOp, String> {
    broker::link(&try_data_dir(&app)?, &uuid).await
}

/// 解绑：摘掉本地 uuid，并删掉本机「与云端一致」的账号（**云端那一池保留**）。
/// 见 `broker::unbind` 的注释。
#[tauri::command]
pub async fn broker_unbind(app: tauri::AppHandle) -> Result<broker::BrokerStatus, String> {
    broker::unbind(&try_data_dir(&app)?).await
}

/// 只读状态：绑没绑、uuid、云端版本、上次同步时刻、上次错误。无副作用。
#[tauri::command]
pub fn broker_state() -> broker::BrokerStatus {
    broker::status()
}

// ---------------------------------------------------------------------------
// 积分简报
// ---------------------------------------------------------------------------
//
// 简报的口径、存储与聚合都在 `briefing` 模块里，这里只提供两个动作：
// **采样一次**（写台账）与**固化已经走完的小时**。界面上没有任何「手动生成一条」
// 的入口 —— 条目只由后台每小时跑一次（`scheduler::maybe_seal_briefing`）产生。

/// 采集阶段：逐账号打一次 `ide_user_ent_usage`，把读数攒起来 —— **不碰台账**。
///
/// 采一次要几秒到几十秒（账号之间还要留抖动），而台账锁只能在内存操作期间持有，
/// 所以「打接口」与「记账」必须分开：这里只负责把读数拿回来，入不入账、按什么口径
/// 入账由调用方交给 [`ledger::Store::apply`] 决定。
///
/// 返回第二项是**采样时刻**：增量按它归属到「采样时刻所属的那个小时」，
/// 所以必须在这里定下来（而不是入账那一刻），否则跨整点时归属会漂。
///
/// ⚠️ 顺带一个副作用：把每次采到的「剩余 / 不限量 / 最早到期」回写到账号列表的
/// `credit_snapshot`（有变化才落盘）。账号页的积分快照与简报页的「当前剩余」读的
/// 都是这份数据 —— 不加这一刀，后台每小时采样后「当前剩余」会停在打开应用时
/// 那个旧值上，简报页开着也不会动。
pub(crate) async fn fetch_samples(
    dir: &std::path::Path,
    accounts: &[Account],
) -> (String, Vec<ledger::Reading>) {
    let client = reqwest::Client::new();
    let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut readings = Vec::with_capacity(accounts.len());
    let mut list = accounts::load_accounts(dir);
    let mut dirty = false;
    for (i, a) in accounts.iter().enumerate() {
        // 账号之间留抖动：这是后台按小时跑的批量请求，零间隔连发就是脚本形态
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        }
        let view = checkin::fetch_resource_view_with(&client, a).await;
        readings.push(ledger::Reading {
            id: a.id.clone(),
            packages: view.packages,
            credits: view.credits,
            expiry_ms: view.earliest_expiry_ms,
        });
        // 与 `checkin_status` 同一套回写口径：拉不到（限流/掉线）沿用上次，不抹成未知。
        // 失败时 `view` 是 Default（credits/unlimited/expiry 全空）—— 必须拦在这一步，
        // 否则会把已有快照整体覆盖成 `None`（账号页/简报页的「当前剩余」变「—」）。
        // unlimited 账号的 credits 恒为 `None`，所以不能只看 credits，三个字段任一有效即算拿到。
        if let Some(acct) = list.iter_mut().find(|x| x.id == a.id) {
            let got =
                view.credits.is_some() || view.unlimited || view.earliest_expiry_ms.is_some();
            if got {
                let next = accounts::CreditSnapshot::now(
                    view.credits.map(|c| c as i64),
                    view.unlimited,
                    view.earliest_expiry_ms,
                );
                let prev = acct.credit_snapshot.as_ref();
                let changed = prev.map(|p| (p.credits, p.unlimited, p.earliest_expiry_ms))
                    != Some((next.credits, next.unlimited, next.earliest_expiry_ms));
                if changed {
                    acct.credit_snapshot = Some(next);
                    dirty = true;
                }
            }
        }
    }
    if dirty {
        let _ = accounts::save_accounts(dir, &list);
    }
    (at, readings)
}

/// 记账阶段：把采集到的读数并入**唯一的内存台账**。
///
/// 简报采样与「开启简报」两条路径共用这个收尾；按小时的定时采样走的是
/// [`crate::scheduler`] 里同一套动作（它还要接着固化时条目）。
///
/// `baseline = true` 时改走「只对齐基线」（[`ledger::Mode::Baseline`]）：
/// 用于「开启简报」—— 那时要的是「从现在开始算」，断档期攒下的量既不归任何小时，
/// 也不该算进开启后的第一个小时。
///
/// 落盘失败不回滚：内存里的值是对的，下一次任意写入都会再落一遍，这里静默即可。
pub(crate) fn apply_samples(
    app: &tauri::AppHandle,
    readings: &[ledger::Reading],
    at: &str,
    baseline: bool,
) {
    let dir = try_data_dir(app).unwrap_or_default();
    let mode = if baseline {
        ledger::Mode::Baseline
    } else {
        ledger::Mode::Normal
    };
    let _ = ledger::store(&dir).apply(readings, at, mode);
}

/// 把「已经走完、还没固化」的小时从台账固化成时条目（幂等），返回本次新固化的条目。
///
/// 输入只有台账（桶 + 余额读数），所以**不依赖网络**：补算历史时余额显示「—」，
/// 因为台账里那个读数是「此刻」的，拿去标注两天前的小时就是编数字（见 [`briefing::build_hour`]）。
/// 这也意味着「应用关了两天再打开」不会丢明细 —— 桶还在（留 60 天），补得出来。
pub(crate) fn seal_hours(
    dir: &std::path::Path,
    accounts: &[Account],
    today: &str,
    hour: u8,
    generated_at: &str,
) -> Vec<briefing::HourEntry> {
    let store = ledger::store(dir);
    let mut all = briefing::load(dir);
    // 待固化的小时 = 台账桶里「已经走完、还没固化」的那些（起点条目不参与去重）
    let pending = store.read(|led| briefing::unsealed_hours(&led.accts, &all, today, hour));
    if pending.is_empty() {
        return Vec::new();
    }
    // 建条目也在锁内一次做完：它是纯内存聚合，而每小时的桶随时可能被采样写进来
    let built: Vec<briefing::HourEntry> = store.read(|led| {
        pending
            .iter()
            .filter_map(|(date, h)| {
                briefing::build_hour(&led.accts, accounts, date.as_str(), *h, generated_at)
            })
            .collect()
    });
    if built.is_empty() {
        return Vec::new();
    }
    let mut sealed = Vec::with_capacity(built.len());
    for e in built {
        briefing::upsert(&mut all, e.clone());
        sealed.push(e);
    }
    if sealed.is_empty() {
        return Vec::new();
    }
    // 落盘前必须收口：这里是「读全量 → 就地改 → 整体写回」，不经过任何单条写入函数，
    // 漏了这一刀文件就会无限增长（每加一条就多一条，永远没人删）。
    briefing::normalize(&mut all);
    // 落盘失败只意味着「这次没记住」：固化是幂等的，下一跳会重算一遍，
    // 所以这里吞掉错误（返回的 sealed 是给界面刷新用的，不是「已持久化」的凭据）。
    let _ = briefing::save(dir, &all);
    sealed
}

/// 清空简报：时条目 + 台账里的小时桶。**逐包累计值必须留着。**
///
/// 为什么连桶一起清：时条目是「从桶固化出来的」，只清条目的话下一次调度就会把它们
/// 原样重建出来 —— 用户会看到「清空」在 30 秒后自己撤销。
///
/// 为什么累计值不能清：`credits_amount` 是接口侧的**装机以来累计量**，清掉它下次采样
/// 就会把这一整笔总量算成「此刻这一小时的消耗」，清空反而凭空多出一笔账。
fn clear_briefing_history(dir: &std::path::Path) -> std::io::Result<()> {
    briefing::clear(dir)?;
    // 清的是「明细」，逐包累计值必须原样留着（见上）：改的仍是唯一那份内存台账
    ledger::store(dir).update(|led| {
        for a in led.accts.values_mut() {
            a.clear_hours();
        }
    })?;
    Ok(())
}

/// 积分简报的日条目（新的在前）。
///
/// 日条目**不落盘**，由时条目现算（[`briefing::day_entries`]）：这样「日 = 时之和」
/// 是结构上的事实，不可能出现「日条目和它下面的时条目对不上」这种最没得解释的 bug。
#[tauri::command]
pub fn credit_briefing(app: tauri::AppHandle) -> Result<Vec<briefing::DayEntry>, String> {
    let dir = try_data_dir(&app)?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    Ok(briefing::day_entries(&briefing::load(&dir), &today))
}

/// 清空简报历史（时条目 + 台账里的小时桶；逐包累计值保留）
#[tauri::command]
pub fn credit_briefing_clear(app: tauri::AppHandle) -> Result<(), String> {
    clear_briefing_history(&try_data_dir(&app)?).map_err(|e| e.to_string())
}

/// 「开启积分简报」：清历史 → 立刻采一次样 → **只对齐基线**（不记这一段增量）
/// → 把「开启这一刻」落成一条**起点条目**。
///
/// 顺序不能反：先采样再清的话，刚采出来的那一段增量会被清掉，而基线已经推到「采完」
/// 的位置 —— 那一段消耗就永久消失了。
///
/// 采样走 [`ledger::Mode::Baseline`] 而不是正常记账，理由见 [`fetch_samples`]。
///
/// 为什么要立刻落一条起点条目：拨完开关页面上什么都没有的话，用户既看不出「生效了没有」，
/// 也少了一个可以对照的起点（下一条要等到下一个整点过后的固化）。这条条目带 `baseline`
/// 标记，规则见 [`briefing::HourEntry::baseline`] —— 尤其**它不参与去重**，
/// 这一小时走完后的真实固化会把它覆盖掉，所以那一小时的消耗不会被它挡掉。
#[tauri::command]
pub async fn credit_briefing_enable(app: tauri::AppHandle) -> Result<(), String> {
    let dir = try_data_dir(&app)?;
    clear_briefing_history(&dir).map_err(|e| e.to_string())?;
    let accounts = accounts::load_accounts(&dir);
    if accounts.is_empty() {
        return Ok(());
    }
    // 采集在锁外完成，入账一次性做完（Baseline 口径）
    let (at, readings) = fetch_samples(&dir, &accounts).await;
    apply_samples(&app, &readings, &at, true);

    // 起点条目用刚采到的余额读数（Baseline 把它们写进了台账）。
    // 归属的日期/小时必须与读数入账所用的**同一个 `at`**（`ledger::parse_at` 的规则），
    // 不能用采集完成的 `now` —— 跨整点时两者会差一小时，基线条目会落错小时、
    // balance 因 `credits_in_hour` 校验取不到值而变 `None`。
    let (today, hour) = ledger::parse_at(&at)
        .unwrap_or_else(|| (chrono::Local::now().format("%Y-%m-%d").to_string(), 0));
    let entry = ledger::store(&dir)
        .read(|led| briefing::build_baseline(&led.accts, &accounts, &today, hour, &at));
    if let Some(entry) = entry {
        let mut all = briefing::load(&dir);
        briefing::upsert(&mut all, entry);
        // 与固化路径同一刀：这次是「读全量 → 改 → 整体写回」，不 normalize 就没人收口
        briefing::normalize(&mut all);
        // 落盘失败不影响「开启」这件事本身：台账已经写好，下一跳照样会固化出真实条目
        let _ = briefing::save(&dir, &all);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // 「页面上此刻该不该说话」—— 这条判据曾经错得很显眼，所以锁住它
    // -----------------------------------------------------------------------

    /// `#[serde(default)]` 让空对象就能造出一份默认设置，不必把 7 个字段抄一遍。
    fn settings(takeover_enabled: bool) -> Settings {
        let mut s: Settings = serde_json::from_str("{}").expect("Settings 应能从空对象构造");
        s.takeover_enabled = takeover_enabled;
        s
    }

    /// 接管开着时**拒绝**改接管名单，而且设置一个字节都不动。
    ///
    /// 理由：改名单意味着**重启那些应用**（编辑器里可能还有没保存的东西），
    /// 跟改端口是同一类事 —— 先关开关再改。
    /// 界面那两枚 chip 在开启时也是灰的，但**规则放在后端才有唯一出处**：
    /// 界面过期（另一个窗口拨过开关、load 还没回来）也拦得住。
    #[test]
    fn set_apps_is_refused_while_takeover_is_on() {
        let dir = std::env::temp_dir().join(format!("twa-setapps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = settings(true);
        s.takeover_apps = vec!["A".into()];
        accounts::save_settings(&dir, &s).unwrap();

        // 不写 `unwrap_err()`：那要求 `TakeoverStatus: Debug`，为一条测试给状态类型加 derive
        // 不划算 —— `match` 更直白，失败信息也更好读。
        let err = match set_apps(dir.clone(), vec!["B".into()]) {
            Ok(_) => panic!("接管开着时不该允许改接管名单"),
            Err(e) => e,
        };
        assert!(err.contains("先关闭接管"), "话术必须自带下一步：{err}");
        assert_eq!(
            accounts::load_settings(&dir).takeover_apps,
            vec!["A".to_string()],
            "被拒绝时设置不得被改动"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn proxy(active: bool) -> crate::proxy::ProxyStatus {
        crate::proxy::ProxyStatus { active, port: 8788, error: None }
    }

    fn endpoint(installed: bool, ours: bool) -> crate::endpoint::EndpointStatus {
        crate::endpoint::EndpointStatus {
            supported: true,
            app_dir: Some("C:/x/resources/app".into()),
            installed,
            ours,
            writable: true,
            endpoint_base: "http://127.0.0.1:8788".into(),
            upstream_http: Some("https://api.trae.cn".into()),
            upstream_ws: None,
            lease_fresh: true,
            message: String::new(),
        }
    }

    /// `recognized && writable` = 打得了；`patched` = 已经打了。
    fn patch(recognized: bool, writable: bool, patched: bool) -> crate::patch::PatchStatus {
        crate::patch::PatchStatus {
            supported: true,
            target: Some("C:/x/resources/app/out/main.js".into()),
            writable,
            patched,
            recognized,
            gates: 2,
            identity_patterns: true,
            // 真实探针在「能打但还没打」时给的就是这句话（用户截图里那条）。
            message: "未打补丁（识别正常：闸门 2 处 / 身份头数组在位），可以打。".into(),
        }
    }

    /// 用户上报的那条：**接管没开**时，一个「能打但还没打」的应用不该在页面上说话。
    /// 补丁本来就不该在（谁都没开启过），说它等于给全新安装的用户挂一个红框。
    #[test]
    fn unpatched_app_stays_silent_while_takeover_is_off() {
        let msg = app_message(
            &settings(false),
            &proxy(false),
            true,
            &endpoint(false, false),
            &patch(true, true, false),
            "Trae CN",
        );
        assert_eq!(msg, "", "未接管 + 未打补丁是最正常的状态，不该有任何提示");
    }

    /// 反过来：**接管开着**却查不到补丁 ⇒ 端点已经指向本机明文回环、闸门还没放开，
    /// 这个应用启动就会崩，必须当场喊出来（应用升级会真的造成这个状态）。
    #[test]
    fn unpatched_app_shouts_while_takeover_is_on() {
        let msg = app_message(
            &settings(true),
            &proxy(true),
            true,
            &endpoint(true, true),
            &patch(true, true, false),
            "Trae CN",
        );
        assert!(msg.contains("未打补丁"), "接管开着却没补丁必须报：{msg}");
    }

    /// 打不了补丁（版本不认识 / 目录不可写）**与开关状态无关** ——
    /// 它是「开关为什么是灰的」的答案，关着时更要说。
    #[test]
    fn unpatachable_app_always_speaks() {
        for recognized in [true, false] {
            for writable in [true, false] {
                if recognized && writable {
                    continue; // 这一格是「打得了」，不属于本用例
                }
                let msg = app_message(
                    &settings(false),
                    &proxy(false),
                    true,
                    &endpoint(false, false),
                    &patch(recognized, writable, false),
                    "Trae CN",
                );
                assert!(
                    !msg.is_empty(),
                    "recognized={recognized} writable={writable} 是开关灰着的理由，关着也要说"
                );
            }
        }
    }

    /// 没被勾选的应用永远不吭声 —— 用户明确不让接管它。
    #[test]
    fn unselected_app_never_speaks() {
        let msg = app_message(
            &settings(true),
            &proxy(false),
            false,
            &endpoint(false, false),
            &patch(false, false, false),
            "Trae CN",
        );
        assert_eq!(msg, "");
    }

    /// 真机诊断：把「开启接管」这条路径上所有会说话的东西一次性打出来 ——
    /// 发现了哪些应用、每个应用的端点基址/闸门判定/补丁状态。
    ///
    /// 跑法：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture dump_takeover_enable_on_this_machine
    /// ```
    ///
    /// **默认只读**：不动 `product.json`、不动开关、不重启任何应用。要在本机真跑一遍
    /// 完整路径（会改写端点并重启被接管的应用），必须显式加环境变量：
    /// ```text
    /// TWA_REAL_TAKEOVER=1 cargo test --lib -- --ignored --nocapture dump_takeover_enable_on_this_machine
    /// ```
    #[test]
    #[ignore = "真机诊断：读真实配置；只有设 TWA_REAL_TAKEOVER=1 才会真的改写端点并重启应用"]
    fn dump_takeover_enable_on_this_machine() {
        let Some(base) = dirs::data_dir() else {
            eprintln!("[skip] 无法定位系统数据目录");
            return;
        };
        let dir = base.join("cn.traework.assistant");
        let s = accounts::load_settings(&dir);
        let endpoint_base = crate::endpoint::base_url(s.takeover_port);

        println!("\n===== 真机「开启接管」诊断 =====");
        println!("数据目录  : {}", dir.display());
        println!(
            "开关/端口 : takeover_enabled={} port={}",
            s.takeover_enabled, s.takeover_port
        );
        println!("接管名单  : {:?}（空 = 全部）", s.takeover_apps);
        println!("端点基址  : {endpoint_base}");

        println!("\n--- ① 本机发现到的应用 ---");
        let all = crate::target::discover();
        if all.is_empty() {
            println!("（一个都没有 —— 接管无从谈起）");
        }
        for t in &all {
            println!(
                "  {:<16} bundle={} running={}",
                t.id,
                t.bundle.display(),
                t.running()
            );
            println!("      product.json = {}", t.product_path().display());
            // 明文端点能不能过闸，取决于**这个目标**的补丁状态（那条双向不变量）。
            let gate = crate::endpoint::preflight(t, &endpoint_base);
            println!("      preflight(当前端点) = {gate:?}");
            let p = crate::patch::status(t);
            println!(
                "      补丁: supported={} patched={} recognized={} writable={} gates={} identity_patterns={}",
                p.supported, p.patched, p.recognized, p.writable, p.gates, p.identity_patterns
            );
            println!("      补丁说明: {}", p.message);
        }

        println!("\n--- ② 当前状态（界面看到的那一份）---");
        let st = build_status(&dir, &s);
        println!(
            "proxy_active={} error={:?} lease_fresh={}",
            st.proxy_active, st.proxy_error, st.lease_fresh
        );
        for a in &st.apps {
            println!(
                "  {:<16} selected={} installed={} ours={} writable={} running={}",
                a.id, a.selected, a.installed, a.ours, a.writable, a.running
            );
            if !a.message.is_empty() {
                println!("      要说的话: {}", a.message);
            }
        }
        if !st.missing_apps.is_empty() {
            println!("名单里本机不存在的: {:?}", st.missing_apps);
        }
        println!("顶层那句话: {}", st.message);

        if std::env::var("TWA_REAL_TAKEOVER").as_deref() != Ok("1") {
            println!("\n（只读模式：未开启接管。要真跑一遍请设 TWA_REAL_TAKEOVER=1）");
            return;
        }

        // ── 真跑：会改写 product.json 并重启被接管的应用 ────────────────
        println!("\n--- ③ 真跑 enable_endpoint（会改写端点 + 重启被接管的应用）---");
        match enable_endpoint(dir.clone()) {
            Ok(st) => println!("成功：{}", st.message),
            Err(e) => println!("返回错误：{e}"),
        }
        let after = build_status(&dir, &accounts::load_settings(&dir));
        println!("收尾：enabled={} proxy_active={}", after.enabled, after.proxy_active);
        for a in &after.apps {
            println!("  {:<16} installed={} ours={}", a.id, a.installed, a.ours);
        }
        println!("（要还原请调用 takeover_disable，或重启助手让它自愈）");
    }

    // -----------------------------------------------------------------------
    // 免证书模式（A′）真机驱动：不依赖界面，直接走生产代码路径
    // -----------------------------------------------------------------------

    fn real_data_dir() -> Option<PathBuf> {
        dirs::data_dir().map(|b| b.join("cn.traework.assistant"))
    }

    /// 只读报告：每个应用的补丁状态 + 端点模式 + 接管状态。**不动任何文件。**
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_takeover_report
    /// ```
    #[test]
    #[ignore = "真机只读报告"]
    fn live_takeover_report() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let s = accounts::load_settings(&dir);
        let st = build_status(&dir, &s);
        println!("\n===== 接管 / 补丁 现状 =====");
        println!("数据目录    : {}", dir.display());
        println!(
            "开关        : enabled={} port={}",
            s.takeover_enabled, s.takeover_port
        );
        println!("接管名单    : {:?}（空 = 全部）", s.takeover_apps);
        println!("扣费白名单  : {:?}", s.billing_account_ids);
        println!("端点基址    : {}", st.endpoint_base);
        println!("反代        : active={} error={:?}", st.proxy_active, st.proxy_error);
        println!("租约新鲜    : {}", st.lease_fresh);
        for a in &st.apps {
            println!(
                "\n--- {} ---\n  路径      : {}",
                a.id, a.bundle
            );
            println!(
                "  选中={} 运行={} 端点改写={} 我们的={} 可写={}",
                a.selected, a.running, a.installed, a.ours, a.writable
            );
            println!(
                "  补丁      : supported={} patched={} recognized={} writable={} gates={} identity_patterns={}",
                a.patch.supported, a.patch.patched, a.patch.recognized,
                a.patch.writable, a.patch.gates, a.patch.identity_patterns
            );
            println!("  补丁说明  : {}", a.patch.message);
            println!("  上游      : http={:?} ws={:?}", a.upstream_http, a.upstream_ws);
            if !a.message.is_empty() {
                println!("  要说的话  : {}", a.message);
            }
        }
        if !st.missing_apps.is_empty() {
            println!("\n名单里本机不存在的: {:?}", st.missing_apps);
        }
        println!("\n界面那句话  : {}", st.message);
    }

    /// 真机：给**本机发现的每个**应用打「免证书补丁」。**只改各自的 `out/main.js`**，
    /// 不碰开关、不重启进程。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_patch_apply
    /// ```
    ///
    /// 故意写成「直接调 `patch::apply`」而不是走界面：这是**生产用的同一段代码**，
    /// 而且没有 Tauri 上下文也能跑，便于在真机上把补丁这一步单独验证干净。
    #[test]
    #[ignore = "真机：会修改各应用的 out/main.js（可逐字节还原）"]
    fn live_patch_apply() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let targets = crate::target::discover();
        assert!(!targets.is_empty(), "本机没有发现可接管的 Trae 应用");
        for t in &targets {
            let before = crate::patch::status(t);
            println!("\n===== 「{}」打补丁前 =====", t.id);
            println!("{}", before.message);
            assert!(before.supported, "{}", before.message);
            // fail-safe：版本不认识就到此为止，绝不写盘
            assert!(before.recognized, "{}", before.message);
            if !before.writable {
                // 正常现象：`cargo test` 跑在**终端/工具**的进程里，macOS 的「App 管理」TCC
                // 只授权给过用户点头的 App。要真打补丁请从助手本体走
                // （免证书模式下「开启接管」会自动调用同一段 `patch::apply`）。
                eprintln!("[skip] 当前进程无权重写「{}」：{}", t.id, before.message);
                eprintln!("       这不是 bug —— 用助手本体开接管即可（它走的正是这段代码）。");
                continue;
            }
            if before.patched {
                println!("已经打过补丁，跳过（幂等）");
                continue;
            }
            let after = crate::patch::apply(&dir, t).expect("打补丁失败");
            assert!(after.patched, "打完补丁后状态仍不是 patched：{}", after.message);
            println!("===== 打补丁后 =====\n{}", after.message);
            println!(
                "指纹记录 : {}",
                dir.join(format!("patch_main_js.{}.json", t.id)).display()
            );
        }
    }

    /// 真机：还原**每个**应用的补丁（会先确保接管已关闭，因为明文端点遇上未打补丁的应用
    /// 会让它启动即崩）。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_patch_revert
    /// ```
    #[test]
    #[ignore = "真机：还原各应用 out/main.js；若接管开着会先还原端点并重启应用"]
    fn live_patch_revert() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let mut s = accounts::load_settings(&dir);
        let targets = crate::target::discover();
        if s.takeover_enabled {
            println!("接管开着，先还原端点并重启受影响的应用");
            let dir2 = dir.clone();
            let touched: Vec<crate::target::AppTarget> = targets
                .iter()
                .filter(|t| crate::endpoint::is_ours(t))
                .cloned()
                .collect();
            crate::target::with_restart(&touched, || {
                for t in &touched {
                    crate::endpoint::uninstall(t, &dir2)?;
                }
                Ok(())
            })
            .expect("还原端点失败");
            s.takeover_enabled = false;
            accounts::save_settings(&dir, &s).expect("回写设置失败");
        }
        for t in &targets {
            let changed = crate::patch::revert(&dir, t).expect("还原补丁失败");
            let st = crate::patch::status(t);
            println!(
                "\n「{}」还原动作 : {}",
                t.id,
                if changed { "已还原" } else { "无需还原" }
            );
            println!("当前形态 : patched={} recognized={}", st.patched, st.recognized);
            println!("{}", st.message);
        }
    }

    // -----------------------------------------------------------------------
    // 真机驱动：不依赖界面，直接走生产代码路径
    //
    // 真机排障时往往只能对着终端，所以这些测试把「点按钮」换成了「敲命令」。
    // ⚠️ 「经系统代理接管」与「证书模式」那一组（`live_ca_install` /
    // `live_proxy_route_on` / `live_proxy_route_off`）已随两条路一起删除。
    // -----------------------------------------------------------------------

    /// 真机：把本机 TraeWork 的登录账号导入账号池。
    ///
    /// **只读 TraeWork 的文件，只写助手自己的 `accounts.json`** —— 不碰应用包、不碰网络设置。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_pool_import_local
    /// ```
    #[test]
    #[ignore = "真机：读本机登录态并写入助手 accounts.json"]
    fn live_pool_import_local() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let mut list = accounts::load_accounts(&dir);
        let before = list.len();
        println!("\n===== 导入本机登录态到账号池 =====");
        println!("数据目录    : {}", dir.display());

        for cand in crate::trae_auth::discover_local_accounts() {
            let mut a = Account::from(cand);
            accounts::normalize(&mut a);
            if accounts::contains_equivalent(&list, &a) {
                println!("  跳过（已在池里）：{}", a.name);
                continue;
            }
            println!(
                "  导入：name={} phone={} region={:?} token={} refresh={}",
                a.name,
                a.phone.clone().unwrap_or_else(|| "-".into()),
                a.region,
                if a.token.is_empty() { "无" } else { "有" },
                if a.refresh_token.is_some() { "有" } else { "无" }
            );
            list.push(a);
        }

        if list.len() != before {
            accounts::save_accounts(&dir, &list).expect("写入 accounts.json 失败");
        }
        println!("\n池内账号    : {} 个（本次新增 {}）", list.len(), list.len() - before);
        if list.len() == before && before == 0 {
            println!("⚠️ 一个都没扫到：TraeWork 是否已登录？（登录态在它的 User/globalStorage 里）");
        }
    }

}
