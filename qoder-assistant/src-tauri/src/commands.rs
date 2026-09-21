use crate::accounts::{self, Account, Settings};
use crate::auth_file::{self, LocalScan};
use crate::briefing;
use crate::broker;
use crate::checkin;
use crate::ledger;
use crate::logs::{self, CheckinLog};
use crate::notify;
use crate::oauth;
use crate::refresh;
use crate::region::Region;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;

/// 应用数据目录。取不到时返回 Err（而不是 panic）——后台调度线程也走这里，
/// 一旦 panic 在 release（`panic = "abort"`）下会直接把整个应用带崩。
pub(crate) fn try_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录（请检查系统权限）：{e}"))
}

fn data_dir(app: &AppHandle) -> PathBuf {
    try_data_dir(app).expect("无法获取应用数据目录（请检查系统权限）")
}

/// 一条「积分事实」：某个账号此刻还剩多少、什么时候读到的。
///
/// 带上 `id` 才能随事件广播给界面 —— 前端持有的是「账号 id → 事实」的一张表
/// （见 `src/credits.ts`），收到推送就地合并，不必再回头请求一次。
///
/// 事实本体直接复用 [`ledger::CreditFact`]：「资源包快照」只有一种形状，
/// 各处不再各写一份会漂移的字段表（`#[serde(flatten)]` 保证发出去的 JSON 仍是扁平的）。
#[derive(serde::Serialize, Clone, Debug)]
pub struct CreditRow {
    pub id: String,
    #[serde(flatten)]
    pub fact: ledger::CreditFact,
}

/// 积分事实更新事件。**所有采集路径结束时都会发**（每小时采样 / 手动刷新 / 签到 /
/// 开启简报 / 接管路由补拉），前端据此更新那份唯一的内存对象。
///
/// 触发点是 [`ledger::Store`] 的写入口本身，而不是这里的调用点：见 [`ledger::on_change`]。
pub const CREDITS_EVENT: &str = "credits-updated";

/// 全部账号的积分事实 —— 事件广播的载荷。
fn credit_rows(dir: &Path) -> Vec<CreditRow> {
    let accounts = accounts::load_accounts(dir);
    let store = ledger::store(dir);
    store.read(|led| {
        accounts
            .iter()
            .filter_map(|a| {
                ledger::fact(led, &a.id).map(|fact| CreditRow {
                    id: a.id.clone(),
                    fact,
                })
            })
            .collect()
    })
}

/// 把最新积分事实广播给界面。
///
/// 界面持有一份「积分内存对象」（`src/credits.ts`）：账号页的「剩余积分 / 总积分」与
/// 简报页的「当前剩余」读的都是它。于是**采集一次、两个页面同时更新** ——
/// 不会再出现「一个页面是新数、另一个还是旧数」这种要用户自己刷新才能对齐的状态。
pub(crate) fn emit_credits(app: &AppHandle) {
    let rows = credit_rows(&data_dir(app));
    let _ = app.emit(CREDITS_EVENT, rows);
}

#[tauri::command]
pub fn list_accounts(app: AppHandle) -> Result<Vec<accounts::AccountView>, String> {
    // 积分读数从**唯一的内存台账**（`ledger::Store`）配上：账户管理与简报同源
    Ok(accounts::load_account_views(&data_dir(&app)))
}

/// 单个账号的自动续签结果（供调度线程写日志 / 发事件）
#[derive(serde::Serialize, Clone, Debug)]
pub struct AutoRefreshReport {
    /// 实际完成续签的账号名
    pub refreshed: Vec<String>,
    /// 需要续签但失败的（账号名 + 原因）
    pub failed: Vec<String>,
}

/// 就地做一次本地续签（拿 refresh token 换新的一对 token）。
///
/// **它不含「该不该续」的判断**，那是调用方的事：单机路径由 [`ensure_fresh_token`] 判，
/// 整池路径由 [`crate::broker::sync`] 判。判断有两处，动作只有这一份 ——
/// 反过来（各处自己写一遍续签）就会出现「同一个接口几套行为」，那正是最难受的错法。
///
/// 两个 token 的过期时间**都要收下来**：refresh token 那条是「这条续签链还能活多久」
/// 的判据，而只有它能换来新的 access token —— 漏掉它，界面上的「剩余有效期」就只剩
/// 一个看起来还很新、实际已经换不出东西的数字。
pub(crate) async fn refresh_account_in_place(account: &mut Account) -> Result<(), String> {
    let rt = account
        .refresh_token
        .clone()
        .ok_or_else(|| "没有 refresh token，无法续签".to_string())?;
    let r = refresh::refresh(account.region, &account.token, &rt).await?;
    account.token = r.token.clone();
    let next_rt = r.refresh_token.clone();
    if let Some(nrt) = &next_rt {
        account.refresh_token = Some(nrt.clone());
    }
    account.expires_at = r.expires_at.or(account.expires_at);
    account.rt_expires_at = r.rt_expires_at.or(account.rt_expires_at);
    // 续签成功 → 同窗口原子写回 auth.v1.dat（尽力而为，失败只读模式静默）
    let _ = crate::auth_file::apply_refresh(
        account.region,
        &r.token,
        next_rt.as_deref(),
        r.expires_at,
        r.rt_expires_at,
    );
    Ok(())
}

/// 阈值内自动续签。返回值 `Ok(true)` = **账号字段有更新，调用方需要落盘**。
///
/// 账号所属区域绑了凭证池时**恒为「没动」**：那池的续签统一由 [`sync_pool_if_bound`]
/// 拿着闸做。这里若退回本地续签，几台机器会同时打官方接口、各自换一条新链 ——
/// 「谁先签谁把别人踢下线」正是这么来的，所以这条分岔不能省。
/// 判据按**账号自己的区域**算：两套部署各绑各的池，国际版绑了不代表国内版账号
/// 也要停掉本地续签。
pub(crate) async fn ensure_fresh_token(account: &mut Account) -> Result<bool, String> {
    if crate::broker::bound_in(account.region) {
        return Ok(false);
    }
    if account.refresh_token.is_none() {
        return Ok(false);
    }
    let now = chrono::Utc::now().timestamp_millis();
    if !refresh::should_refresh(account.expires_at, now) {
        return Ok(false);
    }
    refresh_account_in_place(account).await?;
    Ok(true)
}

/// 账号缺手机号时补一次（**只补空，绝不覆盖**）。
///
/// 为什么需要它：旧版认为 `/api/v1/userinfo` 不含手机号，于是「登录新账号」进来的账号
/// 手机号恒空（更正见 `oauth::parse_userinfo`）。而手机号是跨机合并（`broker` 的身份锚点
/// 第一顺位）与界面展示都要的东西，缺了就只能按昵称认人 —— 昵称是会改的。
/// 新登录的账号现在登录那一刻就带手机号，这个函数是给**升级上来的老账号**补的。
///
/// 三条边界：
/// - **只在为空时打这一下**：有值就不动（那个值可能正是云端合并键在用的）；
/// - **失败静默**：拿不到就保持原样，绝不写入空串或占位；
/// - **与续签各管各的**：它只认 `security_mobile`。
///
/// 返回值约定与 [`ensure_fresh_token`] 一致：`Ok(true)` = 账号字段有更新，调用方需落盘。
pub(crate) async fn fill_phone_if_missing(account: &mut Account) -> Result<bool, String> {
    if account.phone.as_deref().is_some_and(|p| !p.trim().is_empty()) {
        return Ok(false);
    }
    let Some(v) =
        crate::qoder_api::get_json(account.region, &account.token, "/api/v1/userinfo", &[]).await
    else {
        return Ok(false);
    };
    match oauth::parse_userinfo(&v) {
        (_, _, Some(phone)) => {
            account.phone = Some(phone);
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// 绑定凭证池时先整池同步一轮再让调用方去读账号（区域 = 当前选中的区域）。
///
/// ⚠️ **必须在 `load_accounts` 之前调用**：它会把云端那一份并进 `accounts.json`，
/// 而闸带回来的才是最新凭证（本地那份可能早被别的机器换掉了）。
/// 先读后同步 = 整轮都在用已作废的 token，症状是「绑定之后签到反而全失败」。
///
/// 未绑定、距上次同步不到两分钟、闸在别的机器手里，都是**常态**，静默返回；
/// 真出错也只写进 `broker::status().error`。它挂在签到 / 刷新 / 接管路由这些主流程上，
/// 不该因为管家不可达就把主流程拦住 —— 本地凭证本来就还能用。
pub(crate) async fn sync_pool_if_bound(dir: &Path) {
    sync_pool_if_bound_in(dir, accounts::load_settings(dir).takeover_region).await;
}

/// 同上，但区域由调用方指定：反代路由与批量动作都各只碰一个区域，
/// 而自动续签是跨区域的 —— 绑了池的区域每一处都要先同步一轮。
pub(crate) async fn sync_pool_if_bound_in(dir: &Path, region: Region) {
    if !crate::broker::bound_in(region) {
        return;
    }
    let _ = crate::broker::sync(dir, false, region).await;
}

/// 这个账号接下来会真的发出**官方续签请求**吗？
///
/// 判定条件与本地路径开头的守卫一致，并且共用同一个 `refresh::should_refresh`，
/// 所以阈值只有一处定义、不会各写一套。之所以要提前问一次，是为了让批量循环**只为真正
/// 会发生的请求**留间隔——续签本就是少数事件，若「全都不需要续签」的空转也被逐个账号拖住，
/// 一次后台自检就要凭空多花十几秒。
fn will_refresh(account: &Account) -> bool {
    account.refresh_token.is_some()
        && refresh::should_refresh(account.expires_at, chrono::Utc::now().timestamp_millis())
}

/// 自动续签全部账号（调度线程 / 启动自检调用）。
///
/// 只在确实有账号被续签时才落盘；单个账号失败不中断其它账号，也不让整体返回 Err
/// （续签失败是常态化的旁路事件，不该被当成「签到异常」）。
pub async fn auto_refresh_all(app: &AppHandle) -> Result<AutoRefreshReport, String> {
    let dir = data_dir(app);
    // 先整池同步（绑了池才有动作）—— 必须在 load_accounts 之前，见它的注释。
    // 续签本身是跨区域的，所以**每个绑了池的区域**都要先同步一轮。
    for r in Region::ALL {
        sync_pool_if_bound_in(&dir, r).await;
    }

    let mut accounts = accounts::load_accounts(&dir);
    let mut refreshed = Vec::new();
    let mut failed = Vec::new();
    let mut changed = false;
    // 上一个账号是否真的产生过出站请求：用来决定本账号前要不要让一拍。
    // 两个条件同时成立才等（上次发过 + 这次也要发），缺一个等待就纯属拖延
    let mut sent = false;
    for acct in accounts.iter_mut() {
        let due = will_refresh(acct);
        if sent && due {
            // 自动续签是后台静默循环，用户完全看不见它连发——这条路径反而更该有节奏
            crate::http::account_gap().await;
        }
        match ensure_fresh_token(acct).await {
            Ok(true) => {
                sent = true;
                refreshed.push(acct.name.clone());
                changed = true;
            }
            Ok(false) => {}
            Err(e) => {
                // 走到这里说明请求已经发出去了（只是失败），流量一样算数
                sent = true;
                failed.push(format!("{}：{e}", acct.name));
            }
        }
    }
    if changed {
        accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    }
    Ok(AutoRefreshReport { refreshed, failed })
}

/// 一条导入项：来自「导入本机账号」或「登录新账号」。
///
/// `region` 是**必填语义、可省字段**：来源一定知道它属于哪套部署（登录时用户点的
/// 那个入口；导入本机账号时来自凭据所在的 profile 目录），前端原样带回来即可。
/// 缺省落成国际版 —— 没有这个字段的那些请求都是「两套部署出现之前」的旧前端，
/// 而那时能导进来的只有国际版账号。
///
/// 旧版这里还有个 `host`，会被写进 `Account.base_url`。那条路已经删掉了：
/// 授权域（`qoder.com`）≠ 模型网关（`api2-v2.qoder.sh`），一个恒错的字段没人该读。
#[derive(serde::Deserialize)]
pub struct ImportItem {
    #[serde(default)]
    pub region: Region,
    pub token: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// refresh token 过期时间（毫秒时间戳）；本地登录文件与授权响应都可能有
    #[serde(default)]
    pub rt_expires_at: Option<i64>,
}

/// 导入结果：新增 / 更新各多少个（前端据此提示）
#[derive(serde::Serialize)]
pub struct ImportReport {
    pub added: usize,
    pub updated: usize,
}

/// 批量导入账号：已存在的账号**合并补全**而不是跳过。
///
/// 识别规则：**同一区域**内，手机号相同或 token 相同即视为同一账号
/// （token 会轮换，手机号更稳定）。
///
/// 区域必须先相等：同一个手机号在两套部署里是**两个不同的账号**（两套后端、两份
/// token、各自的活动权益）。只按手机号匹配，会把国内版那条合并进国际版记录里 ——
/// token 一换就再也签不到到，而界面上看不出哪里错了。
/// 合并时以新来源为准：token、refresh token 以及**两个 token 各自的过期时间**都用新值覆盖
/// （本机登录文件 / 授权响应是权威来源），没给的字段则保留本地那份；昵称仅在原名为空时补。
/// 这样早期导入、缺续签字段的账号重新导入一次即可获得自动续签能力，也不会产生重复条目。
///
/// 绑了凭证池时还会**当场把这一批推上云**（见下方 `broker::sync` 那段）。
#[tauri::command]
pub async fn import_accounts(
    app: AppHandle,
    items: Vec<ImportItem>,
) -> Result<ImportReport, String> {
    let dir = data_dir(&app);
    // 导入项落在哪些区域（要在 `merge_import` 消费 items 之前取，见下面推云那段）
    let regions: HashSet<Region> = items.iter().map(|i| i.region).collect();
    let mut accounts = accounts::load_accounts(&dir);
    let report = merge_import(&mut accounts, items);
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;

    // 绑了池：新增 / 改动的账号**立刻**推上云，不等下一次签到或接管路由。
    // 这里必须 `force` —— 常规路径有两分钟节流，而导入是用户看得见的动作，
    // 卡在节流窗口里会让「刚加的账号没上去」看起来像丢了。
    // 每个区域各绑一池：导入项落在哪个区域，就推哪个区域的池。
    // 推不动（闸在别的机器手里 / 管家不可达）**不算导入失败**：整池同步每次提交的
    // 都是「云端 ∪ 本机」，下一次同步照样会把它带上，所以这里只吞掉错误、不改结果。
    for r in regions {
        if broker::bound_in(r) {
            let _ = broker::sync(&dir, true, r).await;
        }
    }
    Ok(report)
}

/// 合并逻辑本体（纯函数，便于单测）：见 `import_accounts`。
pub(crate) fn merge_import(accounts: &mut Vec<Account>, items: Vec<ImportItem>) -> ImportReport {
    let mut added = 0;
    let mut updated = 0;
    for it in items {
        let token = it.token.trim().to_string();
        if token.is_empty() {
            continue;
        }
        let phone = it
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let region = it.region;
        let found = accounts.iter_mut().find(|a| {
            a.region == region
                && (a.token == token
                    || phone.is_some() && a.phone.as_deref() == phone.as_deref())
        });
        match found {
            Some(a) => {
                a.token = token;
                if let Some(rt) = it.refresh_token.filter(|s| !s.trim().is_empty()) {
                    a.refresh_token = Some(rt);
                }
                if it.expires_at.is_some() {
                    a.expires_at = it.expires_at;
                }
                // 两个 token 的过期时间各自独立覆盖：新来源没给就保留本地那份，
                // 别把「不知道」写成「没有」（那会让界面直接判成已失效）。
                if it.rt_expires_at.is_some() {
                    a.rt_expires_at = it.rt_expires_at;
                }
                if phone.is_some() {
                    a.phone = phone;
                }
                if a.name.trim().is_empty() {
                    if let Some(n) = it.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        a.name = n.to_string();
                    }
                }
                // 导入 = 重新同步：丢弃本地缓存的签到结果，避免陈旧的「今天失败」记录
                // 在状态列直接显示「签到失败」（真实状态由导入后的 refreshAll 以服务端为准重写）
                a.last = None;
                updated += 1;
            }
            None => {
                let name = it
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!("账号-{}", &token.chars().take(6).collect::<String>())
                    });
                accounts.push(Account {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    phone,
                    region,
                    token,
                    refresh_token: it.refresh_token.filter(|s| !s.trim().is_empty()),
                    expires_at: it.expires_at,
                    rt_expires_at: it.rt_expires_at,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    last: None,
                    checked_today: None,
                    cosy_uid: None,
                });
                added += 1;
            }
        }
    }
    ImportReport { added, updated }
}

// ── 账号导入的补全规则 ──────────────────────────────────────────────────────



#[cfg(test)]
mod import_tests {
    use super::*;

    fn acct_in(region: Region, name: &str, phone: Option<&str>, token: &str) -> Account {
        Account {
            region,
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            phone: phone.map(str::to_string),
            token: token.into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            created_at: String::new(),
            checked_today: None,
            cosy_uid: None,
            last: None,
        }
    }

    /// 国际版账号 —— 绝大多数用例只关心一套部署，用这个短名字
    fn acct(name: &str, phone: Option<&str>, token: &str) -> Account {
        acct_in(Region::Global, name, phone, token)
    }

    fn item_in(region: Region, token: &str, phone: Option<&str>) -> ImportItem {
        ImportItem {
            region,
            token: token.into(),
            name: None,
            phone: phone.map(str::to_string),
            refresh_token: Some("rt-new".into()),
            expires_at: Some(123456),
            rt_expires_at: Some(234567),
        }
    }

    fn item(token: &str, phone: Option<&str>) -> ImportItem {
        item_in(Region::Global, token, phone)
    }

    #[test]
    fn merges_existing_account_by_phone_and_fills_credentials() {
        let mut accs = vec![acct("waxiloao", Some("19098779775"), "old-token")];
        let r = merge_import(&mut accs, vec![item("new-token", Some("19098779775"))]);
        assert_eq!((r.added, r.updated), (0, 1), "同手机号应识别为同一账号");
        assert_eq!(accs.len(), 1, "不应产生重复条目");
        let a = &accs[0];
        assert_eq!(a.token, "new-token");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(a.expires_at, Some(123456));
        assert_eq!(a.rt_expires_at, Some(234567), "refresh token 的有效期也要落库");
        assert_eq!(a.name, "waxiloao", "已有名字不应被覆盖");
    }

    #[test]
    fn import_keeps_local_expiries_when_the_source_does_not_know_them() {
        // 新来源只说了一半的事实（给了 token、没给有效期）。那不该把本地已知的有效期
        // 抹成 None —— 界面会立刻把账号显示成「已失效」，而凭证其实是好的。
        let mut accs = vec![acct("a", Some("111"), "t1")];
        accs[0].expires_at = Some(999);
        accs[0].rt_expires_at = Some(888);
        let mut partial = item("t2", Some("111"));
        partial.expires_at = None;
        partial.rt_expires_at = None;
        merge_import(&mut accs, vec![partial]);
        assert_eq!(accs[0].expires_at, Some(999));
        assert_eq!(accs[0].rt_expires_at, Some(888));
    }

    #[test]
    fn adds_new_account_when_phone_and_token_unknown() {
        let mut accs = vec![acct("a", Some("111"), "t1")];
        let r = merge_import(&mut accs, vec![item("t2", Some("222"))]);
        assert_eq!((r.added, r.updated), (1, 0));
        assert_eq!(accs.len(), 2);
        assert_eq!(accs[1].name, "账号-t2", "无名导入项用默认名");
    }

    #[test]
    fn empty_tokens_are_ignored() {
        let mut accs = vec![acct("a", Some("111"), "t1")];
        let r = merge_import(&mut accs, vec![item("  ", Some("111"))]);
        assert_eq!((r.added, r.updated), (0, 0));
        assert_eq!(accs.len(), 1);
    }

    /// 同一个手机号在两套部署里是**两个不同的账号**。只按手机号合并，国内版那条会
    /// 被合并进国际版记录里 —— token 一换就再也签不到到，而界面上看不出哪里错了。
    #[test]
    fn the_same_phone_in_two_regions_stays_two_accounts() {
        let mut accs = vec![acct("g", Some("13800000000"), "t-global")];

        let r = merge_import(
            &mut accs,
            vec![item_in(Region::Cn, "t-cn", Some("13800000000"))],
        );
        assert_eq!((r.added, r.updated), (1, 0), "跨区域的同手机号必须新建而不是合并");
        assert_eq!(accs.len(), 2);
        assert_eq!(accs[1].region, Region::Cn);
        assert_eq!(accs[0].token, "t-global", "国际版那条不该被动到");
        assert_eq!(accs[0].region, Region::Global);

        // 同一区域内仍然按手机号合并（老行为不能变）
        let r = merge_import(
            &mut accs,
            vec![item_in(Region::Cn, "t-cn2", Some("13800000000"))],
        );
        assert_eq!((r.added, r.updated), (0, 1));
        assert_eq!(accs[1].token, "t-cn2");
    }

    /// 缺省区域 = 国际版：没有 `region` 字段的导入项来自「两套部署出现之前」的前端，
    /// 而那时能导进来的只有国际版账号。
    #[test]
    fn an_import_item_without_a_region_lands_on_global() {
        let raw = serde_json::json!({ "token": "t" });
        let it: ImportItem = serde_json::from_value(raw).unwrap();
        assert_eq!(it.region, Region::Global);
    }
}

#[tauri::command]
pub fn remove_account(app: AppHandle, id: String) -> Result<(), String> {
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    let before = accounts.len();
    accounts.retain(|a| a.id != id);
    if accounts.len() == before {
        return Err("账号不存在".into());
    }
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    // 一并清理该账号的签到日志，避免留下无归属的孤儿记录
    let _ = logs::clear_logs(&dir, Some(&id));
    Ok(())
}

/// 本次真的签到过的账号里，那些带回了余额读数的记录 → `(账号 id, 余额, 读数时刻)`。
///
/// 抽成纯函数是为了把「**只认本次签到的账号**」这条规则钉在单测里 —— 它是本函数
/// 唯一容易写错、且写错了不会报错的地方（见 [`record_checkin_credits`]）。
fn checkin_credit_readings(
    accounts: &[Account],
    checked: &[String],
) -> Vec<(String, f64, String)> {
    accounts
        .iter()
        .filter(|a| checked.iter().any(|id| id == &a.id))
        .filter_map(|a| {
            let rec = a.last.as_ref()?;
            // `at` 为空说明这条记录没带时刻，写进台账只会让读数无法归到某一小时
            if rec.at.is_empty() {
                return None;
            }
            let b = rec.balance?;
            Some((a.id.clone(), b, rec.at.clone()))
        })
        .collect()
}

/// 本次签到带回的**发放凭据** → 台账的逐笔记账入参。
///
/// 签到为什么能拿到它：领取接口的响应里就有这一笔积分自己的 `expiresAt`，
/// 而且**已领取时靠幂等回放同样能拿到**（见 `qoder_api::ClaimReceipt`、
/// `checkin::do_checkin` 的 `Plan::Done` 分支）。它是「积分过期」列对**免费号**
/// 唯一的日期来源 —— `/sash/api/v2/me/usage` 对免费号给的是「无期限」哨兵。
///
/// `credits` 取的是这次领到的量：领取成功那条记录里是实际发放量，
/// 回放路径（当天已领）通常只有日期 —— 台账两个都收（`None` 就不显示量）。
///
/// 与 [`checkin_credit_readings`] 同一套「只认本次签到过的账号」规则：`last` 里那条
/// 记录属于某次具体签到，拿别的账号（或很旧）的 `last` 去写台账同样是「用旧记录覆盖新读数」。
fn checkin_grant_writes(accounts: &[Account], checked: &[String]) -> Vec<ledger::GrantWrite> {
    accounts
        .iter()
        .filter(|a| checked.iter().any(|id| id == &a.id))
        .filter_map(|a| {
            let last = a.last.as_ref()?;
            let expires_ms = last.expires_at?;
            Some(ledger::GrantWrite {
                account_id: a.id.clone(),
                // 落在「附加额度」那一栏：签到领的 100 Credits 在 usage 响应里就是那一格。
                // 键取自 `usage`（唯一知道包结构的地方），不在这里写第二份字面量。
                pkg_key: crate::usage::KEY_ADDON.to_string(),
                expires_ms,
                credits: last.credit,
            })
        })
        .collect()
}

/// 把签到顺手读到的余额写进**积分台账**（唯一来源）。
///
/// 单个签到与批量签到共用这一份 —— 否则「余额」又会多出一条各写各的路径，
/// 而那正是这次重构要消掉的东西。返回值由调用方用 [`accounts::AccountView::of`]
/// 组装：这里只管写台账，不管「发给界面长什么样」，两件事不混在一个函数里。
///
/// ⚠️ **`checked` 必须传「本次真的签到过」的账号 id**，不能图省事传全部：
/// `last` 里那条记录的时刻是**签到时刻**，而列表里其它账号的 `last` 可能很旧，
/// 把它们一起写进台账，等于拿一个旧时刻去覆盖台账里更新、更准的读数
/// （曾经就是这样：某账号 9 点签到、10 点刷新、11 点给别的账号签到时被一起重写，
/// 于是「10 点的余额」被盖上 9 点的时刻 —— 而积分简报的余额列只认「读数时刻
/// 与被固化小时同小时」的读数，那一刻钟的余额就错了）。
///
/// 写两样东西，都是签到这条路径独有的产物：
/// - **余额读数**（`apply_credits`）：只更新读数，不产生小时桶；
/// - **发放凭据**（`note_grants`）：逐笔记在「附加额度」包上，并让 `earliest_expiry_ms`
///   把这份也算进去（接管路由按那个排序）。口径是「接口那份与凭据那份**取更早者**」，
///   所以采样报的那份不会被弄丢；一个包都没有时不写，也不会凭空造出一个最早到期时间。
fn record_checkin_credits(dir: &Path, accounts: &[Account], checked: &[String]) {
    let readings = checkin_credit_readings(accounts, checked);
    if !readings.is_empty() {
        // 落盘失败不影响签到结果：下一次任意拉取都会再写一遍
        let _ = ledger::store(dir).apply_credits(&readings);
    }
    let grants = checkin_grant_writes(accounts, checked);
    if !grants.is_empty() {
        let _ = ledger::store(dir).note_grants(&grants);
    }
}

/// 启动时的**补录**：把签到日志里每一笔领取登记进台账的逐笔发放。
///
/// 为什么要补：逐笔明细上线之前签到的那些笔，只有 `checkin_logs.json` 记得
/// （那份日志从签到功能第一天就在记 `expires_at`），台账里只有一个「最晚一笔」的标量。
/// 不补的话，界面上「附加额度」的明细要从零攒起 —— 而本机可能已经领了几十笔。
///
/// **幂等**：台账按到期时刻去重（见 `ledger::note_grant`），日志里的老记录在每次启动
/// 都会被重放一遍，但只会留下一条 —— 所以这里不需要「补过了吗」的标记，
/// 也无所谓每次启动都跑。写失败不致命：下次启动再试。
///
/// 与 [`record_checkin_credits`] 的分工：那条路是「签到当场」写，这条是「把历史补齐」；
/// 两者最终都收口到同一个写点（`note_grants`），口径不会分叉。
pub(crate) fn backfill_grants_from_logs(dir: &Path) {
    let items = grant_writes_from_logs(logs::load_logs(dir));
    if items.is_empty() {
        return;
    }
    let _ = ledger::store(dir).note_grants(&items);
}

/// 签到日志 → 待补录的逐笔发放：只取**成功的**行、且确实记着到期时刻的。
///
/// 失败行本来就没有领取发生（`checkin::claim` 的失败分支把 `expires_at` 留空），
/// 所以「成功」与「有日期」两道筛选各管各的，谁都不用替对方兜底。
fn grant_writes_from_logs(rows: Vec<CheckinLog>) -> Vec<ledger::GrantWrite> {
    rows.into_iter()
        .filter(|l| l.success)
        .filter_map(|l| {
            let expires_ms = l.expires_at?;
            Some(ledger::GrantWrite {
                account_id: l.account_id,
                pkg_key: crate::usage::KEY_ADDON.to_string(),
                expires_ms,
                credits: l.credit,
            })
        })
        .collect()
}

#[tauri::command]
pub async fn checkin_one(app: AppHandle, id: String) -> Result<accounts::AccountView, String> {
    let dir = data_dir(&app);
    sync_pool_if_bound(&dir).await;
    let mut accounts = accounts::load_accounts(&dir);
    let idx = accounts
        .iter()
        .position(|a| a.id == id)
        .ok_or("账号不存在")?;
    // 只签当前区域的账号（与「全部签到」同一条界线）。前端本来就只展示这一区域的行，
    // 这道闸拦的是「拿着过期列表来点」：区域切走了，按钮还悬在旧账号上。
    let region = accounts::load_settings(&dir).takeover_region;
    if accounts[idx].region != region {
        return Err("该账号不属于当前区域".into());
    }
    // 临期先续签。
    // 失败不阻断：仍用旧 token 试一次，由签到结果给出明确提示
    let _ = ensure_fresh_token(&mut accounts[idx]).await;
    let rec = checkin::do_checkin(&accounts[idx]).await;
    accounts[idx].last = Some(rec.clone());
    let _ = logs::append_log(&dir, logs::log_from_record(&accounts[idx], &rec));
    // 刚读到的余额也写进台账（与批量签到同一条路径、同一份数据来源）。
    // 只报这一个账号：其余账号的 `last` 是它们各自上一次签到留下的，此刻没有新读数
    record_checkin_credits(&dir, &accounts, std::slice::from_ref(&id));
    // 视图必须在台账写完**之后**取：签到顺手读到的余额正是从台账投影出去的
    let view = accounts::AccountView::of(accounts[idx].clone(), &dir);
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(view)
}

#[tauri::command]
pub async fn checkin_all(app: AppHandle) -> Result<Vec<accounts::AccountView>, String> {
    let dir = data_dir(&app);
    let settings = accounts::load_settings(&dir);
    // 手动「全部签到」走短间隔档（manual_stagger_*）：同样打散顺序与节奏，只是等待短得多
    let results = checkin_all_inner(&app, false).await?;
    // 手动批量签到是否推送由设置决定（默认关，免得连点几下就把通知刷屏）
    if settings.notify_enabled && settings.notify_on_manual {
        let _ = notify::send(&settings.notify_webhook, &notify::summary_message(&results)).await;
    }
    Ok(results)
}

/// 一键刷新全部账号：**拉一次额度接口，把结果写进唯一的数据来源（积分台账）**。
///
/// 1. **积分事实**：剩余积分 + 最早过期时间 + 逐包明细 → 全部交给台账（见 [`crate::ledger`]）。
///    账户列表的「剩余积分」列、智能接管路由、积分简报因此读的都是同一份数；
/// 2. **签到状态**：只读查询活动权益的 `claimStatus`（不打领取接口、零副作用），
///    写入 `checked_today`。
///
/// **刻意不碰 `last`**：`last` 是「上一次真实签到」的记录，刷新不是签到，往里面写余额
/// 等于给一条旧时刻的记录塞一个新读数 —— 而那个读数本该只以台账里的时刻存在。
/// 账户列表的积分一律读台账投影（`attach_credits`），`last.balance` 只作为
/// 「签到那一刻读到多少」留给签到时间线，所以刷新这条路径不再产生第二个写点。
///
/// 不打领取接口——已领的活动再打只会拿到 409，看最新状态没必要绕这一圈。
/// 逐账号查询、单个失败不改原值（界面保留旧数），最后整体保存一次。
///
/// **只刷当前区域**（与「全部签到」同一条界线）：刷新是「把界面上看得见的数对齐」，
/// 另一区域的行根本不在这一屏上，替它打一轮接口既多一倍请求，也把风控面摊宽。
/// 凭证续签（`auto_refresh_all`）不受这条限制 —— 那是保命操作，跨区域照跑。
#[tauri::command]
pub async fn refresh_all(app: AppHandle) -> Result<Vec<accounts::AccountView>, String> {
    let dir = data_dir(&app);
    sync_pool_if_bound(&dir).await;
    let mut accounts = accounts::load_accounts(&dir);
    let region = accounts::load_settings(&dir).takeover_region;
    let idx: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.region == region)
        .map(|(i, _)| i)
        .collect();
    if idx.is_empty() {
        return Ok(Vec::new());
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    // 采集与记账分开：先把读数攒齐（网络在锁外），最后一次性并入内存台账。
    // 这样既不会跨 `await` 持锁，也不会出现「改到一半被别的路径那份旧副本覆盖」。
    let mut readings = Vec::with_capacity(idx.len());
    for (step, &i) in idx.iter().enumerate() {
        // 账号之间留抖动：每账号三四个请求，N 个账号零间隔打出去就是脚本形态。
        // 首个账号不等（它前面本就没有请求，也让首条结果尽快回到界面）
        if step > 0 {
            crate::http::account_gap().await;
        }
        // 凭证临期的先续签，避免拿着过期 token 把「没积分」误判成「查不到」。
        let _ = ensure_fresh_token(&mut accounts[i]).await;
        // 顺手给老账号补手机号（只在为空时打一下，见 [`fill_phone_if_missing`]）。
        // 放在这里而不是签到路径：刷新本来就是「把该对齐的都对齐」的那一次，
        // 而签到要多打一个请求、又不一定会被点到。
        let _ = fill_phone_if_missing(&mut accounts[i]).await;
        // 1) 资源视图：剩余积分 + 最早过期时间 + 逐包明细 —— 一次拉取，全部进台账
        let view = checkin::fetch_resource_view(accounts[i].region, &accounts[i].token).await;
        readings.push(ledger::Reading {
            id: accounts[i].id.clone(),
            packages: view.packages,
            credits: view.credits,
            expiry_ms: view.earliest_expiry_ms,
        });
        // 2) 签到状态：只读查询活动权益的 claimStatus（持久化 checked_today）
        accounts[i].checked_today = checkin::query_checked_today(&accounts[i]).await;
    }
    // 台账落盘失败不影响刷新结果（它只是统计，丢了下次采样会重新累积）
    if let Err(e) = ledger::store(&dir).apply(&readings, &now, ledger::Mode::Normal) {
        crate::scheduler::log_event(&dir, &format!("积分台账落盘失败：{e}"));
    }
    // 积分读数从刚写好的台账配上账号一起返回（界面口径与台账同源）；
    // 落盘只写账号本身 —— 积分是台账的投影，不进 accounts.json。
    // 返回只含本区域（前端按 id 合并）；落盘是全量（另一区域的条目原样写回）。
    let refreshed = idx.into_iter().map(|i| accounts[i].clone()).collect();
    let views = accounts::view_accounts(refreshed, &dir);
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(views)
}

// ── 只读命令的取舍（2026-09-18 改版） ──────────────────────────────────────
//
// 这里曾经有三个纯只读命令：`account_overview`（套餐 + 活跃度 + 近一年汇总）、
// `account_heatmap`（近一年消耗热力图）、`account_campaigns`（当天活动权益状态）。
// 它们各自对应账号页上的一块：指标卡 / 热力图 / 「今日权益」那一栏。
//
// 2026-09-18 按产品决定改版 —— 顶部只留「账号总数 / 已签到 / 未签到 / 剩余额度」，
// 热力图不做，「今日权益」那一栏撤掉 —— 三个命令**同时失去全部消费方**，一并删除。
//
// 注意「签到」本身**没有**受影响：它走 [`checkin::do_checkin`] / [`checkin::query_checked_today`]，
// 不经过 `account_campaigns`。那个命令只是给被撤掉的那一栏做展示用的。
//
// 响应形态与实测证据都还在 `basedata/20260918_Qoder缺失接口逆向.md` 与
// `basedata/20260918_Qoder活动权益接口逆向.md`，将来要接照那两份接。

/// 「全部签到」的实际实现：逐账号签到、逐条落库，最后整体保存。
///
/// ⚠️ **签到是写操作**（领取活动权益），但每个账号**只尝试一次**：没有可领的就直接
/// 落成 `inactive`，不会重试、也不会为了「确认一下」再打一遍写接口。重复打同一个
/// 写接口既是风控信号，也没有任何意义 —— 想知道最新状态，只读的活动接口就够了。
///
/// 抽成独立函数是因为定时调度（`scheduler`）与手动命令共用同一套逻辑——
/// 后台线程走不了 Tauri 的 invoke，只能直接调它。
///
/// **只签当前区域**（`settings.takeover_region`，即左下角选择器选中的那一套部署）：
/// 另一区域的账号有自己的活动权益与刷新点，跟着这批签既没意义、又替用户做了他没选的写操作。
/// 落盘仍是**全量账号**（只改了本区域那几条的 `last`），返回视图只含本区域 ——
/// 前端按 id 合并回列表，另一区域的行保持原样。
///
/// `scheduled` 决定用哪一档「风控节奏」（见 [`gap_seconds`]）：
/// - `true`：定时/自动触发，两次请求之间随机歇 `stagger_max_seconds` 以内（默认 45s 档）；
/// - `false`：用户主动点（含启动即签到），走 `manual_stagger_max_seconds`（默认 8s 档）——
///   一样要等，只是等得短，避免手动路径成为全程唯一的瞬时连发入口。
///
/// 访问顺序由 [`visit_order`] 决定：默认打乱。**落盘与返回值仍按原顺序**，
/// 所以列表不会因为打乱而跳来跳去。
pub(crate) async fn checkin_all_inner(
    app: &AppHandle,
    scheduled: bool,
) -> Result<Vec<accounts::AccountView>, String> {
    let dir = data_dir(app);
    sync_pool_if_bound(&dir).await;
    let mut accounts = accounts::load_accounts(&dir);
    let settings = accounts::load_settings(&dir);
    // 本区域账号在原列表里的下标：签到只碰这一批，其余条目连 `last` 都不动
    let idx: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.region == settings.takeover_region)
        .map(|(i, _)| i)
        .collect();
    if idx.is_empty() {
        return Ok(Vec::new());
    }
    let order = visit_order(idx.len(), settings.shuffle_checkin_order);
    for (step, &o) in order.iter().enumerate() {
        if let Some(secs) = gap_seconds(&settings, scheduled, step) {
            tokio::time::sleep(std::time::Duration::from_secs(secs as u64)).await;
        }
        let i = idx[o];
        // 临期先续签：失败了也不阻断，仍用旧 token 试一次，由签到结果给出明确提示
        let _ = ensure_fresh_token(&mut accounts[i]).await;
        let rec = checkin::do_checkin(&accounts[i]).await;
        accounts[i].last = Some(rec.clone());
        let _ = logs::append_log(&dir, logs::log_from_record(&accounts[i], &rec));
    }
    // 签到顺手把刚读到的余额写进台账 —— 那是「积分事实」的唯一来源，
    // 账户管理与积分简报都读它，所以这里不用、也不许另存一份。
    // 本区域的每个账号都真的签到过（`visit_order` 是本批下标的一个排列），所以这批都算新读数。
    let checked: Vec<String> = idx.iter().map(|&i| accounts[i].id.clone()).collect();
    record_checkin_credits(&dir, &accounts, &checked);
    // 视图在台账写完**之后**取，才能带上刚读到的余额；落盘仍只写账号本身（全量，含没动的另一区域）
    let signed = idx.into_iter().map(|i| accounts[i].clone()).collect();
    let views = accounts::view_accounts(signed, &dir);
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(views)
}

// ── 凭证管家：四个命令都是**池级**的，都不带账号 id ────────────────────────
//
// 一池一个 uuid，闸也按池给 —— 所以这里的动作作用范围是「一池」，而不是「某个账号」。
// **每个区域各绑一池**：四个命令都取「当前选中的区域」为作用域，界面上也因此
// 没有逐个账号的开关。前端切了区域（set_region）后再来问，拿到的就是另一池的状态。

/// 把本机**当前区域**的账号整体上传：管家颁发一串 uuid 并当场绑定该区域。
///
/// 返回的 uuid 是**唯一要展示给用户复制**的东西（另一台机器靠它接上同一池）。
#[tauri::command]
pub async fn broker_upload(app: AppHandle) -> Result<broker::PoolOp, String> {
    let dir = data_dir(&app);
    let region = accounts::load_settings(&dir).takeover_region;
    broker::upload(&dir, region).await
}

/// 绑定别处复制过来的 uuid（绑定到当前区域）。绑定后立刻整池同步一轮，
/// 把本地独有的账号也推上去。
#[tauri::command]
pub async fn broker_link(app: AppHandle, uuid: String) -> Result<broker::PoolOp, String> {
    let dir = data_dir(&app);
    let region = accounts::load_settings(&dir).takeover_region;
    broker::link(&dir, region, &uuid).await
}

/// 解绑当前区域：摘掉本地 uuid，**云端那一池保留**；本机移除与云端重复的凭证，
/// 只留本机独有的。拿不到云端那一池时返回 Err 且保留本地绑定 —— 见 `broker::unbind` 的注释。
#[tauri::command]
pub async fn broker_unbind(app: AppHandle) -> Result<broker::BrokerStatus, String> {
    let dir = data_dir(&app);
    let region = accounts::load_settings(&dir).takeover_region;
    broker::unbind(region).await
}

/// 只读状态：当前区域那池的绑没绑、uuid、云端版本、上次同步时刻、上次错误。无副作用。
#[tauri::command]
pub fn broker_state(app: AppHandle) -> broker::BrokerStatus {
    let dir = data_dir(&app);
    broker::status(accounts::load_settings(&dir).takeover_region)
}

/// 首选通道：直接读本机 Qoder 写在磁盘上的凭据文件（`auth.v1.dat`，OSCrypt 加密）。
///
/// 不需要应用处于运行状态、不需要调试端口，且一次就能拿到 token + 昵称 + 手机号。
/// 两个区域各读一条（同机可以各登录一个），并**逐区域**回报读取结果 ——
/// 「没读到」的原因（未登录 / 钥匙串没条目 / 解密失败）必须能显示出来。
#[tauri::command]
pub fn discover_local_accounts() -> Result<LocalScan, String> {
    Ok(auth_file::discover_local_accounts())
}

/// 「无感登录」第一步：申请 state + 授权链接（不重启应用、不打断正在跑的客户端）。
///
/// `region` = 用户在界面上点的那个登录入口（国际版 / 国内版）。它决定授权页打哪个域，
/// 并随 [`oauth::OAuthStart::region`] 原样回给前端 —— 导入时再带回来，
/// 账号才认得出自己属于哪一套部署。
///
/// 旧版这里是 `oauth_start(host)`，那个参数从来没被用过（那时只有一套域）。
/// 现在它不是「可选覆盖」，而是**唯一的区域来源**：授权链接本身不含区域信息。
#[tauri::command]
pub async fn oauth_start(region: Region) -> Result<oauth::OAuthStart, String> {
    oauth::start(region).await
}

/// 「无感登录」第二步：轮询一次授权结果。
///
/// 返回 `done=false` 表示用户还没完成授权（继续轮询即可，**不是错误**）；
/// `done=true` 且带 `token` 表示授权完成，`nickname` / `phone` / `uid` 一并带回。
#[tauri::command]
pub async fn oauth_poll(login_id: String) -> Result<oauth::OAuthPoll, String> {
    oauth::poll(&login_id).await
}

/// 在系统默认浏览器打开链接（用于打开无感登录的授权页）
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    oauth::open_in_browser(&url)
}

/// macOS 系统设置里「App 管理」授权面板的深链（`kTCCServiceSystemPolicyAppBundles`）。
///
/// **不能走 [`open_external`]**：那条路只放行 http(s)（有意收窄，免成跳板），
/// 而这里要用系统私有的 `x-apple.systempreferences:` scheme。
/// URL 写死在函数里、前端不传参 —— 单开一个入口比放宽那条白名单安全。
///
/// 存在的理由见 [`crate::patch::write_hint`]：本应用用**固定自签证书**签名，写官方
/// 客户端的产物受「App 管理」管辖，而**未带授权签名的应用永远不会弹授权框**
/// （系统只往 tccd 记一条拒绝，`authValue=0 authReason=2`）→ 用户唯一的出路就是
/// 手动去这个面板打开开关。这一步既然是绕不过的，就别再让他们去搜「在哪」。
#[tauri::command]
pub fn open_app_management() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        const PANE: &str = "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AppBundles";
        std::process::Command::new("open")
            .arg(PANE)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统设置失败：{e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("「App 管理」是 macOS 专有的授权项，当前系统不需要这一步。".to_string())
    }
}

/// 一条区域的可展示信息（登录弹窗、账号标签、接管页下拉都用它）
#[derive(serde::Serialize)]
pub struct RegionOption {
    /// 落盘 / IPC 的稳定标识（`global` / `cn`），前端原样回传
    pub key: &'static str,
    /// 中文名（「国际版」/「国内版」）
    pub label: &'static str,
    /// 一句话说明差异（域在哪）
    pub hint: &'static str,
}

/// 区域清单：**界面上「国际版 / 国内版」的唯一来源**。
///
/// 由后端给而不是前端各写一份：中文名、以及「OpenAPI 在哪个域」这类事实只该有
/// 一处定义（见 [`crate::region`]），否则界面说的和实际请求打的会各走各的。
#[tauri::command]
pub fn regions() -> Vec<RegionOption> {
    Region::ALL
        .iter()
        .map(|r| RegionOption {
            key: r.key(),
            label: r.label(),
            hint: r.hint(),
        })
        .collect()
}

/// 切换「当前区域」（左下角区域选择器的落点）。返回切换后的设置视图。
///
/// 为什么不走 [`save_settings`]：那份视图带着**离开区域**的全部取值，保存进
/// 「进入区域」会把对方刚配好的定时/简报/风控设置整个盖掉 —— 切区域不是存设置
/// （细节见 [`accounts::set_region`]，那里只动「当前指向哪个区域」这一个指针）。
///
/// **接管开着时拒绝切换**（产品决定）：接管是一端装着真实注入的活拓扑，
/// 「先关再切」把「A 的客户端还指着我们的代理、代理却按 B 的账号扣费」这种
/// 中间态彻底排除在外；安全切换流程（`apply_settings`）只服务接管页自己的启停。
#[tauri::command]
pub fn set_region(app: AppHandle, region: Region) -> Result<Settings, String> {
    let dir = data_dir(&app);
    if accounts::load_settings(&dir).proxy_enabled {
        return Err("接管开启中不能切换区域：请先在「智能接管」页关闭接管。".into());
    }
    accounts::set_region(&dir, region).map_err(|e| e.to_string())?;
    Ok(accounts::load_settings(&dir))
}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> Result<Settings, String> {
    Ok(accounts::load_settings(&data_dir(&app)))
}

// ⛔ 这里曾有一整套「退出客户端 → 收割长驻 CLI host → 重新拉起」的进程操作
// （`process_pids` / `wait_processes_gone` / `kill_processes` / `quit_qoder_and_wait`
// / `open_qoder` / `restart_qoder_process` / `running_apps`），用来在切换接管拓扑时
// 把官方客户端整个重启一遍。它建立在两个**在 Qoder 上并不成立**的前提上：
//
// 1. 「CLI 是长驻 host，它进程环境里的端点值是 spawn 那一刻定死的，所以只有重启客户端
//    才能纠正」——那是 CodeBuddy 时代的形态。Qoder 0.3.3（CLI 1.1.53）的推理进程是
//    **每次会话按需 spawn 的一次性 `--print` 进程**：证据在
//    `~/.qoder[-cn]/logs/runs/<时间戳>-p<桌面端pid>/manifest.json`（`argv` 就是
//    `.../app.asar.unpacked/node_modules/@qoder-ai/qoder-*-agent-sdk/.../qoder-worker-runtime.obf.mjs
//    --print ...`），跑完即退，`ps` 里几乎抓不到。没有长驻 host ⇒ 没有「要重启的东西」。
//
//    ⚠️ 但**别**由此推出「改配置就等于生效」：那个一次性进程的 env 是**桌面端 spawn 时
//    构造**的（`new Worker(runtime,{argv,env})`，`env` 来自 SDK 调用方、**不继承桌面端自己的
//    process.env**），而 `~/.qoder[-cn]/settings.json` 的 `env` 块**没有任何消费者**。
//    所以「写配置 → 下次会话读到」这条链根本不存在。现在改的是**产物本身**
//    （见 [`crate::patch`]）：端点跟着产物一起被那次会话读到，所以「不重启客户端」这条
//    结论仍然成立 —— 它是靠「根本不需要重启」而不是靠「改了配置就会生效」站住的。
// 2. 顺带，`open -a` 紧跟在 AppleScript `quit` 之后会撞上 LaunchServices 的注册竞态，
//    返回 `exit status: 1`；而 `.status()` 把 stderr 一并丢掉，只剩一个状态码。
//    旧实现在这一步失败后**不回滚**，于是磁盘说「已开启」、界面还停在「已关闭」——
//    用户报的「接管报错、随后又显示接管成功」正是这条链路。
//
// 所以接管现在只做**写配置**这一件事，对正在登录、正在对话的客户端零打扰：
// 唯一的事实源是 `settings.json`（见 [`crate::stealth`]）与落盘设置。

/// 当前**实际装着**端点的是哪个区域。
///
/// 取租约而不是取设置：一次保存里用户可能既换了区域又关了开关，而写进磁盘的是
/// 租约上那个区域 —— 要摘掉的正是它。租约丢了（数据目录被清理过）才退回旧设置里的值。
fn installed_region(dir: &Path, fallback: Region) -> Region {
    crate::stealth::load_lease(dir)
        .map(|l| l.region)
        .unwrap_or(fallback)
}

fn normalize_settings(mut settings: Settings) -> Result<Settings, String> {
    settings.schedule_time = accounts::normalize_time(&settings.schedule_time)
        .ok_or_else(|| "定时签到时刻格式应为 HH:MM（例如 09:07）".to_string())?;
    settings.notify_webhook = settings.notify_webhook.trim().to_string();
    // 风控间隔上限：钳制到合理区间，防手滑填 0（退化成无间隔）或填超大值
    settings.stagger_max_seconds = settings.stagger_max_seconds.clamp(2, 600);
    settings.manual_stagger_max_seconds = settings.manual_stagger_max_seconds.clamp(2, 600);
    // 定时签到的随机窗口：0 = 关闭随机，上限 12 小时（再宽就会把签到推到半夜）
    settings.schedule_window_minutes = settings.schedule_window_minutes.min(720);
    // 扣费备选账号：去空格、去空项、去重，保持原有顺序（多选池；空 = 全部可用）
    let mut seen = std::collections::HashSet::new();
    settings.billing_account_ids = settings
        .billing_account_ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect();
    if settings.notify_enabled && settings.notify_webhook.is_empty() {
        return Err("已开启签到通知，请填写 webhook 地址".into());
    }
    if settings.proxy_enabled && settings.proxy_port == 0 {
        return Err("反代端口不能为 0".into());
    }
    Ok(settings)
}

/// 等端点真正就位（监督线程装完 + 租约心跳起来）。
///
/// 不需要 `home`：端点现在是注入到客户端的 worker 产物里，不再依赖家目录下的配置文件，
/// 「装没装上」由产物里的注入标记回答。
fn wait_for_takeover(region: Region, dir: &std::path::Path, port: u16) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let status = crate::stealth::status(region, dir);
        if status.installed && status.alive && status.port == port {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// 接管的「拓扑」= 决定**装不装、装在哪个区域、转发到哪个端口**的那三项。
///
/// 只有它变化时才需要动官方客户端的产物与进程。定时时刻、webhook、限流清单
/// 那些改了就存 —— 不该为了改一个通知地址去动用户正开着的客户端。
///
/// 区域属于拓扑：换区域 = 换一个客户端接管，产物要跟着换。
#[derive(PartialEq, Eq)]
struct TakeoverTopology {
    enabled: bool,
    port: u16,
    region: Region,
}

/// 租约里查不到失败原因时的**就地诊断**。
///
/// 有两类失败发生在 [`crate::stealth::install`] **写租约之前**，因此租约的
/// `last_error` 里必然什么都读不到：客户端找不到（产物路径解析不出来）、以及该区域
/// 根本不支持端点覆盖。它们恰恰是最常见的两种 —— 于是界面只能端出那句
/// 「检查端口是否被占用、客户端是否装在 /Applications」，而这两句在 Windows 上
/// 都是错的（那里的客户端装在 `%LOCALAPPDATA%\Programs`，端口也从没被占）。
fn diagnose_enable_failure(region: Region) -> Option<String> {
    if region.endpoint_env_key().is_none() {
        return Some(format!(
            "{}的端点覆盖尚未支持：该区域的客户端不读这个键。",
            region.label()
        ));
    }
    if crate::patch::worker_path(region).is_none() {
        return Some(format!(
            "找不到{}客户端的 worker 产物：请确认官方客户端已安装在 {}。",
            region.label(),
            region.install_hint()
        ));
    }
    None
}

fn topology(s: &Settings) -> TakeoverTopology {
    TakeoverTopology {
        enabled: s.proxy_enabled,
        port: s.proxy_port,
        region: s.takeover_region,
    }
}

/// 把旧端点**原样装回去**（回滚用）。
///
/// CA 必须一起给：端点被客户端强制成 https，缺了证书的注入等于把对话打断 ——
/// 回滚反而比不回滚更糟。CA 取不到时宁可什么都不做，让调用方如实报错。
fn reinstate(region: Region, dir: &std::path::Path, port: u16) {
    match crate::certs::ensure(dir).and_then(|c| c.ca_pem()) {
        Ok(ca) => {
            if let Err(e) = crate::stealth::install(region, dir, port, &ca) {
                eprintln!("[commands] 回滚接管端点失败：{e}");
            }
        }
        Err(e) => eprintln!("[commands] 回滚时取不到本地 CA，端点未恢复：{e}"),
    }
}

/// 关闭接管撞「写不回官方原样」时的报错。
///
/// 这里**必须**继续拒掉这次保存：注入还在、反代却停了，客户端的对话就全部连向一个
/// 没人监听的端口 —— 比开关拨不动恶劣得多。但只报「权限不够」等于把用户关在门外，
/// 所以顺手把那条能直接粘进终端的还原命令一起端出去（路径里带空格，已引好）。
fn disable_blocked(region: Region, e: String) -> String {
    match crate::patch::restore_hint(region) {
        Some(hint) => format!("{e}\n执行之后回到本页再点一次「关闭接管」。\n{hint}"),
        None => e,
    }
}

pub(crate) fn apply_settings_inner(app: &AppHandle, settings: Settings) -> Result<Settings, String> {
    let dir = data_dir(app);
    let old = accounts::load_settings(&dir);
    let next = normalize_settings(settings)?;
    // 区域不在这条路里改（理由见 [`save_settings`] 命令里的同一条守卫）。
    // 这里必须**另外**拦一遍：安全切换流程自己就能「换区域 + 重装注入」（`(true, true)`
    // 那一支），而产品决定是接管开着不给切、关着走 [`set_region`] —— 两个入口都开着，
    // 「按区域存」的切片就会被带着上一区域取值的视图盖掉。
    if old.takeover_region != next.takeover_region {
        return Err("切换区域请使用左下角的区域选择器。".into());
    }
    if topology(&old) == topology(&next) {
        accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
        return Ok(next);
    }

    // 旧的那份端点装在哪个区域：租约说了算（设置里的区域可能在同一次保存里被改过）
    let old_region = installed_region(&dir, old.takeover_region);

    match (old.proxy_enabled, next.proxy_enabled) {
        (false, true) => {
            // 先落盘：反代监督线程据此监听端口并注入端点（**必须先监听成功才注入**）。
            accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
            if !wait_for_takeover(next.takeover_region, &dir, next.proxy_port) {
                // 失败原因**必须在 uninstall 之前取走**：注入的真实原因活在租约的
                // `last_error` 里，而紧接的 uninstall 会把租约删掉 —— 读晚了就只剩
                // 一句与事实无关的「端口被占用」，把用户送去查一个没坏的东西。
                //
                // 这条路径最常撞上的原因恰恰是唯一需要用户动手的那一项：macOS
                // 「App 管理」未授权，而自签应用**永远不会弹授权框**（系统只往 tccd
                // 记一条拒绝）。原样端出去，用户才知道该开哪个开关。
                //
                // 租约只覆盖「注入那一步失败」；**客户端找不到 / 区域不支持**发生在
                // 写租约之前 —— 那里什么都不会留下，所以补一遍就地诊断（见
                // [`diagnose_enable_failure`]），别让这两类也端出那句误导的兜底。
                let cause = crate::stealth::last_error(&dir)
                    .or_else(|| diagnose_enable_failure(next.takeover_region));
                // 端点没装上（端口被占 / 客户端没装 / 未授权）→ 连设置一起退回去。
                // 绝不留下「设置说已开启、磁盘上却没有端点」的半成品：那种状态会让开关与
                // 事实长期相反，而且此后这一页的每次保存都会被拓扑守卫拒掉 ——
                // 用户报的「切换账号失败」正是它的下游症状。
                let _ = crate::stealth::uninstall(next.takeover_region, &dir);
                let _ = accounts::save_settings(&dir, &old);
                return Err(match cause {
                    Some(c) => format!("接管没能开启（设置已回滚）。\n{c}"),
                    None => format!(
                        "无法在 127.0.0.1:{} 上就位接管（设置已回滚）。\
                         请检查端口是否被占用、以及本应用有没有权限改客户端文件。",
                        next.proxy_port
                    ),
                });
            }
            // 客户端一个进程都不动（也不需要动）：Qoder 每次会话自己起一次性进程，
            // 没有长驻 host 可重启。
        }
        (true, false) => {
            // 摘掉注入即可 —— 不碰任何客户端进程，正在进行的对话不受影响
            if let Err(e) = crate::stealth::uninstall(old_region, &dir) {
                return Err(disable_blocked(old_region, e));
            }
            if let Err(e) = accounts::save_settings(&dir, &next) {
                reinstate(old_region, &dir, old.proxy_port);
                return Err(e.to_string());
            }
        }
        (true, true) => {
            // 换端口 / 换区域：摘掉旧区域的注入 → 落盘 → 等新端点就位；任一环节失败整体回滚
            crate::stealth::uninstall(old_region, &dir)?;
            accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
            if !wait_for_takeover(next.takeover_region, &dir, next.proxy_port) {
                // 原因同样要在回滚**之前**取走（回滚会把新区域的租约摘掉），
                // 且租约只覆盖注入那一步 —— 客户端找不到这类走就地诊断。
                let cause = crate::stealth::last_error(&dir)
                    .or_else(|| diagnose_enable_failure(next.takeover_region));
                let _ = accounts::save_settings(&dir, &old);
                let _ = wait_for_takeover(old_region, &dir, old.proxy_port);
                return Err(match cause {
                    Some(c) => format!(
                        "无法把接管切换到{}（127.0.0.1:{}），已回滚。\n{c}",
                        next.takeover_region.label(),
                        next.proxy_port
                    ),
                    None => format!(
                        "无法把接管切换到{}（127.0.0.1:{}），已回滚。",
                        next.takeover_region.label(),
                        next.proxy_port
                    ),
                });
            }
        }
        // 两端都关着：那说明变的是**区域**（关着的时候端口本来就能随手改，
        // 那条路走 `save_settings`）。此时没有任何端点装着、也没有客户端受影响 ——
        // 只是一次普通的落盘。
        //
        // ⚠️ 这里此前是 `unreachable!()`：拓扑只有「启停 + 端口」时确实到不了，
        // 但区域进了拓扑之后，「接管关着的时候换区域」是**合法操作**，
        // 一旦命中就会把整个应用 panic 掉（release 构建里 panic = abort）。
        (false, false) => {
            accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
        }
    }
    Ok(next)
}

#[tauri::command]
pub fn apply_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    apply_settings_inner(&app, settings)
}

/// 拓扑守卫：接管**开着**的时候，启停 / 端口 / 区域这三项只能经安全切换流程
/// （`apply_settings`）改；关着的时候端口与区域都只是普通配置（那时没有任何端点装着）。
///
/// 抽成纯函数是为了能被测 —— 它是「前端拿着过期值来保存」这类事故的第一道闸，
/// 但它必须**只**拦拓扑项：拦宽一点（比如拿整份设置做比对）就会把正常的扣费池 /
/// 限流清单保存一起拒掉，而那正是用户能直接摸到的故障。
///
/// 返回 `Some(文案)` 表示这次保存必须被拒。
fn topology_conflict(current: &Settings, next: &Settings) -> Option<&'static str> {
    const MSG: &str = "接管启停、换端口或换区域必须使用安全切换流程。";
    if current.proxy_enabled != next.proxy_enabled {
        return Some(MSG);
    }
    if current.proxy_enabled
        && (current.proxy_port != next.proxy_port
            || current.takeover_region != next.takeover_region)
    {
        return Some(MSG);
    }
    None
}

#[tauri::command]
pub fn save_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let dir = data_dir(&app);
    let current = accounts::load_settings(&dir);
    let settings = normalize_settings(settings)?;
    // 「区域」只能由 [`set_region`] 改：视图带着**当前区域**的全部取值，
    // 允许保存请求顺手改它，等于每次切换都把「进入区域」的切片用「离开区域」的设置盖掉。
    // 这是按区域存储之后最难发现的写坏路径 —— 症状要等切回去才看得见，报错却是零。
    if current.takeover_region != settings.takeover_region {
        return Err("切换区域请使用左下角的区域选择器。".into());
    }
    // 拓扑（启停 / 端口 / 区域）里任何一项变了，都必须走 `apply_settings`：
    // 区域同样属于拓扑 —— 换区域要「摘掉旧区域的端点 + 把端点装进新区域」，
    // 直接落盘会留下「A 区域的客户端还指着我们的代理，代理却按 B 区域的账号扣费」。
    if let Some(msg) = topology_conflict(&current, &settings) {
        return Err(msg.into());
    }
    accounts::save_settings(&dir, &settings).map_err(|e| e.to_string())?;
    Ok(settings)
}

/// 发送一条测试通知，直接返回推送服务的原始响应（便于用户自查配置）
#[tauri::command]
pub async fn test_notify(webhook: String) -> Result<String, String> {
    // 品牌前缀由 notify::send 统一施加，这里只写正文
    notify::send(&webhook, "【测试】通知配置正常").await
}

/// 是否已注册开机自启动。
///
/// 以操作系统为准（登录项 / LaunchAgent），不写进 Settings——
/// 用户可能在系统设置里手动关掉，我们不能拿一份本地缓存自欺欺人。
#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("读取开机自启动状态失败：{e}"))
}

/// 开启/关闭开机自启动，返回落定后的真实状态
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let autolaunch = app.autolaunch();
    let res = if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    res.map_err(|e| format!("设置开机自启动失败：{e}"))?;
    autolaunch
        .is_enabled()
        .map_err(|e| format!("读取开机自启动状态失败：{e}"))
}

/// 查询签到日志：默认倒序（最新在前），最多 200 条；可按账号筛选
#[tauri::command]
pub fn get_checkin_logs(
    app: AppHandle,
    limit: Option<usize>,
    account_id: Option<String>,
) -> Result<Vec<CheckinLog>, String> {
    let mut logs = logs::load_logs(&data_dir(&app));
    if let Some(id) = account_id {
        logs.retain(|l| l.account_id == id);
    }
    logs.reverse();
    if let Some(n) = limit {
        logs.truncate(n);
    }
    Ok(logs)
}

/// 清空签到日志：`account_id` 为空则清空全部，否则只清该账号的日志
#[tauri::command]
pub fn clear_checkin_logs(app: AppHandle, account_id: Option<String>) -> Result<(), String> {
    logs::clear_logs(&data_dir(&app), account_id.as_deref()).map_err(|e| e.to_string())
}

// ── 积分简报 ───────────────────────────────────────────────────
//
// 简报的口径、存储与聚合都在 `briefing` 模块里，这里只提供两个动作：
// **采样一次**（写台账）与**固化已经走完的小时**。界面上没有任何「手动生成一条」
// 的入口 —— 条目只由后台每小时跑一次（`scheduler::maybe_seal_briefing`）产生。

/// 采集阶段：逐账号打一次额度接口，把读数攒起来 —— **不碰台账**。
///
/// 采一次要几秒到几十秒（账号之间还要留抖动），而台账锁只能在内存操作期间持有，
/// 所以「打接口」与「记账」必须分开：这里只负责把读数拿回来，入不入账、按什么口径
/// 入账由调用方交给 [`ledger::Store::apply`] 决定。
///
/// 返回第二项是**采样时刻**：增量按它归属到「采样时刻所属的那个小时」，
/// 所以必须在这里定下来（而不是入账那一刻），否则跨整点时归属会漂。
pub(crate) async fn fetch_samples(accounts: &[Account]) -> (String, Vec<ledger::Reading>) {
    let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut readings = Vec::with_capacity(accounts.len());
    for (i, a) in accounts.iter().enumerate() {
        // 账号之间留抖动：这是后台按小时跑的批量请求，零间隔连发就是脚本形态
        if i > 0 {
            crate::http::account_gap().await;
        }
        let view = checkin::fetch_resource_view(a.region, &a.token).await;
        readings.push(ledger::Reading {
            id: a.id.clone(),
            packages: view.packages,
            credits: view.credits,
            expiry_ms: view.earliest_expiry_ms,
        });
    }
    (at, readings)
}

/// 记账阶段：把采集到的读数并入**唯一的内存台账**。
///
/// 广播不在这里做 —— 台账的写入口自己会通知界面（见 [`ledger::on_change`]）：
/// 这样「签到 / 刷新 / 采样 / 接管补拉」各条路径都不必记得收尾，也就不会漏。
///
/// 简报采样与「开启简报」两条路径共用这个收尾；按小时的定时采样走的是
/// [`crate::scheduler`] 里同一套动作（它还要接着固化时条目）。
///
/// `baseline = true` 时改走「只对齐基线」（[`ledger::Mode::Baseline`]）：
/// 用于「开启简报」—— 那时要的是「从现在开始算」，断档期攒下的量既不归任何小时，
/// 也不该算进开启后的第一个小时。
pub(crate) fn apply_samples(
    app: &AppHandle,
    readings: &[ledger::Reading],
    at: &str,
    baseline: bool,
) {
    let dir = data_dir(app);
    let mode = if baseline {
        ledger::Mode::Baseline
    } else {
        ledger::Mode::Normal
    };
    // 落盘失败不回滚：内存里的值是对的，下一次任意写入都会再落一遍
    if let Err(e) = ledger::store(&dir).apply(readings, at, mode) {
        crate::scheduler::log_event(&dir, &format!("积分台账落盘失败：{e}"));
    }
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
/// 为什么累计值不能清：`CapacityUsed` 是接口侧的**装机以来累计量**，清掉它下次采样
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
pub fn credit_briefing(app: AppHandle) -> Result<Vec<briefing::DayEntry>, String> {
    let dir = data_dir(&app);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    Ok(briefing::day_entries(&briefing::load(&dir), &today))
}

/// 清空简报历史（时条目 + 台账里的小时桶；逐包累计值保留）
#[tauri::command]
pub fn credit_briefing_clear(app: AppHandle) -> Result<(), String> {
    clear_briefing_history(&data_dir(&app)).map_err(|e| e.to_string())
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
pub async fn credit_briefing_enable(app: AppHandle) -> Result<(), String> {
    let dir = data_dir(&app);
    clear_briefing_history(&dir).map_err(|e| e.to_string())?;
    let accounts = accounts::load_accounts(&dir);
    if accounts.is_empty() {
        return Ok(());
    }
    // 采集在锁外完成，入账一次性做完（Baseline 口径），并顺带把新读数广播给界面
    let (at, readings) = fetch_samples(&accounts).await;
    apply_samples(&app, &readings, &at, true);

    // 起点条目用刚采到的余额读数（rebaseline 把它们写进了台账，且读数时刻就是此刻 ⇒ 同小时）
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let hour = chrono::Timelike::hour(&now.naive_local()) as u8;
    let now_s = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let entry = ledger::store(&dir)
        .read(|led| briefing::build_baseline(&led.accts, &accounts, &today, hour, &now_s));
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

/// 当前应用版本（用于“关于/更新”展示）
#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 批量签到的访问顺序：返回「第 n 个被签到的账号」在列表里的下标。
///
/// `shuffle = false` 时就是顺序访问。打乱只作用于**请求次序**——调用方按它取账号，
/// 但结果照旧写回原下标、按原顺序落盘与返回，所以界面列表不会跟着跳。
fn visit_order(n: usize, shuffle: bool) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if shuffle {
        crate::rng::shuffle(&mut order);
    }
    order
}

/// 批量签到的「下一个账号之前等几秒」。返回 None = 不等。
///
/// - 第一个账号前永远不等（上来先等一段反而更像排队脚本）；
/// - 定时/自动触发走 `stagger_checkin` + `stagger_max_seconds`（默认 2..=45s）；
/// - 交互触发走 `manual_stagger` + `manual_stagger_max_seconds`（默认 2..=8s）。
///
/// 两档共用 [`stagger_seconds`]，只是上限不同：打散节奏靠的是「随机且非零」，
/// 不是某个特定秒数。
fn gap_seconds(settings: &Settings, scheduled: bool, step: usize) -> Option<u32> {
    if step == 0 {
        return None;
    }
    let (enabled, max) = if scheduled {
        (settings.stagger_checkin, settings.stagger_max_seconds)
    } else {
        (settings.manual_stagger, settings.manual_stagger_max_seconds)
    };
    stagger_seconds(enabled, max)
}

/// 返回某账号签到前应等待的秒数：未开启、或上限 < 2 时返回 None（不等待），
/// 否则在 2..=max（秒）内取值。
fn stagger_seconds(enabled: bool, max: u32) -> Option<u32> {
    if !enabled || max < 2 {
        return None;
    }
    Some(crate::rng::range(2, max as u64) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拓扑守卫只该拦「启停 / 端口 / 区域」这三项 —— 拦宽了就会把正常的扣费池、
    /// 限流清单保存一起拒掉。
    ///
    /// 用户 2026-09-19 报的「切换扣费账号失败」正是这个形状：前端带着一份**过期的**
    /// `proxy_enabled` 来保存扣费池，守卫于是按「你在改拓扑」拒收，而此后这一页的
    /// 每一次保存都会一直被拒。前端那一侧已改成以 props 为唯一事实源（见 TakeoverPage），
    /// 这条测试则把后端闸门的边界钉死：**只拦拓扑项**。
    #[test]
    fn topology_guard_blocks_only_topology_changes() {
        let base: Settings = serde_json::from_str("{}").expect("Settings 应能由空对象反序列化");
        let on = || Settings {
            proxy_enabled: true,
            proxy_port: 8789,
            takeover_region: Region::Cn,
            ..base.clone()
        };

        // ── 纯粹的非拓扑字段：一律放行 ──
        let cur = on();
        let mut next = cur.clone();
        next.billing_account_ids = vec!["a".into()];
        assert!(topology_conflict(&cur, &next).is_none());
        let mut next = cur.clone();
        next.rate_limit_models = vec!["m".into()];
        assert!(topology_conflict(&cur, &next).is_none());
        let mut next = cur.clone();
        next.failover_on_rate_limit = !cur.failover_on_rate_limit;
        assert!(topology_conflict(&cur, &next).is_none());

        // ── 拓扑三项：接管开着时必须拦 ──
        let mut next = cur.clone();
        next.proxy_enabled = false;
        assert!(topology_conflict(&cur, &next).is_some());
        let mut next = cur.clone();
        next.proxy_port = 8899;
        assert!(topology_conflict(&cur, &next).is_some());
        let mut next = cur.clone();
        next.takeover_region = Region::Global;
        assert!(topology_conflict(&cur, &next).is_some());

        // ── 接管关着时，端口与区域只是普通配置：放行（启停本身仍然要拦）──
        let off = Settings {
            proxy_enabled: false,
            ..cur.clone()
        };
        let mut next = off.clone();
        next.proxy_port = 8899;
        next.takeover_region = Region::Global;
        assert!(topology_conflict(&off, &next).is_none());
        let mut next = off.clone();
        next.proxy_enabled = true;
        assert!(topology_conflict(&off, &next).is_some());
    }

    #[test]
    fn stagger_seconds_bounds_and_disabled() {
        assert_eq!(stagger_seconds(false, 45), None);
        assert_eq!(stagger_seconds(true, 0), None);
        assert_eq!(stagger_seconds(true, 2), Some(2));
        for _ in 0..50 {
            let s = stagger_seconds(true, 45).unwrap();
            assert!((2..=45).contains(&s), "间隔越界：{s}");
        }
    }

    #[test]
    fn visit_order_covers_everyone_exactly_once() {
        // 关掉打乱时必须原样顺序访问（老行为不能变）
        assert_eq!(visit_order(4, false), vec![0, 1, 2, 3]);
        assert!(visit_order(0, true).is_empty());
        assert_eq!(visit_order(1, true), vec![0]);

        // 打乱后仍是 0..n 的一个排列：不重不漏
        let mut changed = false;
        for _ in 0..200 {
            let order = visit_order(8, true);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..8).collect::<Vec<usize>>(), "下标被弄丢了：{order:?}");
            if order != (0..8).collect::<Vec<usize>>() {
                changed = true;
            }
        }
        assert!(changed, "200 次都没打乱顺序，说明打乱没生效");
    }

    #[test]
    fn gap_seconds_uses_the_right_dial_and_never_waits_for_the_first() {
        let mut s = Settings::default();
        // 两档上限不同：自动 45s、手动 8s（默认值）
        s.stagger_checkin = true;
        s.stagger_max_seconds = 45;
        s.manual_stagger = true;
        s.manual_stagger_max_seconds = 8;

        assert_eq!(gap_seconds(&s, true, 0), None, "第一个账号前不应等待");
        assert_eq!(gap_seconds(&s, false, 0), None, "第一个账号前不应等待");
        for _ in 0..50 {
            let auto = gap_seconds(&s, true, 1).unwrap();
            assert!((2..=45).contains(&auto), "自动档越界：{auto}");
            let manual = gap_seconds(&s, false, 1).unwrap();
            assert!((2..=8).contains(&manual), "手动档越界：{manual}");
        }

        // 各自关掉后互不影响
        s.manual_stagger = false;
        assert_eq!(gap_seconds(&s, false, 1), None);
        assert!(gap_seconds(&s, true, 1).is_some());
        s.stagger_checkin = false;
        assert_eq!(gap_seconds(&s, true, 1), None);
    }

    #[test]
    fn normalize_settings_clamps_the_guard_dials() {
        // 手滑填 0 或天文数字都不该生效（0 会让间隔退化成「无间隔」）
        let mut s = Settings::default();
        s.stagger_max_seconds = 0;
        s.manual_stagger_max_seconds = 9_999;
        s.schedule_window_minutes = 100_000;
        let n = normalize_settings(s).unwrap();
        assert_eq!(n.stagger_max_seconds, 2);
        assert_eq!(n.manual_stagger_max_seconds, 600);
        assert_eq!(n.schedule_window_minutes, 720);

        // 窗口 0 是**合法值**（= 精确到设定时刻，回到老行为），不能被当成非法输入拒掉
        let mut s = Settings::default();
        s.schedule_window_minutes = 0;
        assert_eq!(normalize_settings(s).unwrap().schedule_window_minutes, 0);
    }

    // ── 签到读数 → 台账：只认本次签到的账号 ────────────────────────

    fn acct_with_last(id: &str, at: &str, balance: Option<f64>) -> Account {
        Account {
            region: Region::Global,
            id: id.into(),
            name: id.into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            created_at: String::new(),
            cosy_uid: None,
            last: Some(accounts::CheckinRecord {
                success: true,
                already: false,
                inactive: false,
                message: String::new(),
                credit: Some(100.0),
                balance,
                campaign_key: Some("act-20260918-899".into()),
                expires_at: None,
                at: at.into(),
            }),
            checked_today: None,
        }
    }

    /// 只有「本次真的签到过」的账号，才允许把自己的 `last` 写进台账。
    ///
    /// 这是本文件里最容易悄悄写错、且写错了不报错的一条规则：`last.at` 是**签到时刻**，
    /// 而没签到过的账号的 `last` 可能是几天前的 —— 跟着一起写进台账就会拿旧时刻覆盖
    /// 更新的读数，而积分简报的余额列只认「读数时刻与被固化小时同小时」的读数，
    /// 结果是那一刻钟的余额出错。
    #[test]
    fn checkin_credit_readings_only_covers_the_accounts_that_just_signed_in() {
        let accounts = vec![
            acct_with_last("a1", "2026-09-16 11:00:00", Some(450.0)),
            // 它 9 点签到过，但这一次没签 —— 那个旧时刻绝不能跟着进台账
            acct_with_last("a2", "2026-09-16 09:00:00", Some(470.0)),
        ];

        assert_eq!(
            checkin_credit_readings(&accounts, &["a1".to_string()]),
            vec![("a1".to_string(), 450.0, "2026-09-16 11:00:00".to_string())]
        );

        // 批量签到：整批都是刚读到的（`visit_order` 保证每个账号都被访问一次）
        let all: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
        assert_eq!(checkin_credit_readings(&accounts, &all).len(), 2);

        // 一个都没签到 → 一条都不写（不能退化成「全都写」）
        assert!(checkin_credit_readings(&accounts, &[]).is_empty());
    }

    /// 没带时刻、或没读到余额的记录都不进台账。
    #[test]
    fn checkin_credit_readings_skip_records_without_time_or_balance() {
        let accounts = vec![
            // `at` 为空：写进去只会让这个读数归不到任何一小时
            acct_with_last("a1", "", Some(100.0)),
            // 没读到余额：读不到 ≠ 余额变成 0
            acct_with_last("a2", "2026-09-16 11:00:00", None),
        ];
        let all: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
        assert!(checkin_credit_readings(&accounts, &all).is_empty());
    }

    /// 签到带回来的**发放凭据**：只挑本次签到过的账号，且只挑真有到期时刻的；
    /// 金额一并带上（回放路径没有量 → `None`，台账照收）。
    ///
    /// 与上面那个函数同一套「只认本次签到」的规则 —— 拿没签到账号的旧记录去写台账，
    /// 等于把**上一笔**积分的到期时间当成这一笔记进去（那会让逐笔明细里多出一条
    /// 根本不是这轮发生的记录）。
    #[test]
    fn checkin_grant_writes_only_take_this_rounds_receipts() {
        const MS: i64 = 1_792_384_855_460; // 2026-10-19T04:40:55.460Z（实测凭据）
        let mut signed = acct_with_last("a1", "2026-09-19 13:16:49", Some(400.0));
        signed.last.as_mut().unwrap().expires_at = Some(MS);
        // 签到过，但这条记录没带回凭据（老响应 / 回放失败）→ 不写，也不编一个日期
        let no_receipt = acct_with_last("a2", "2026-09-19 13:16:50", Some(100.0));
        // 这一次没签到：它的 `last` 属于上一轮
        let mut stale = acct_with_last("a3", "2026-09-18 09:00:00", Some(300.0));
        stale.last.as_mut().unwrap().expires_at = Some(1_700_000_000_000);
        let accounts = vec![signed, no_receipt, stale];

        let got: Vec<(String, String, i64, Option<f64>)> =
            checkin_grant_writes(&accounts, &["a1".to_string(), "a2".to_string()])
                .into_iter()
                .map(|g| (g.account_id, g.pkg_key, g.expires_ms, g.credits))
                .collect();
        assert_eq!(
            got,
            vec![(
                "a1".to_string(),
                crate::usage::KEY_ADDON.to_string(),
                MS,
                Some(100.0)
            )]
        );
        assert!(checkin_grant_writes(&accounts, &[]).is_empty());
    }

    /// 启动补录的纯函数部分：日志 → 逐笔发放。成功、且带着日期的才补 ——
    /// 逐笔记录上线之前的老行没有日期（补不出东西来），失败行压根没有领取发生。
    #[test]
    fn grant_backfill_takes_only_dated_successes_from_the_log() {
        const MS: i64 = 1_792_460_000_000;
        let row = |success: bool, expires_at: Option<i64>| CheckinLog {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: "a1".into(),
            account_name: "甲".into(),
            account_phone: None,
            at: "2026-09-20 14:10:00".into(),
            success,
            already: false,
            inactive: false,
            message: String::new(),
            credit: Some(100.0),
            balance: Some(500.0),
            campaign_key: Some("act-x".into()),
            expires_at,
        };
        let got = grant_writes_from_logs(vec![
            row(true, Some(MS)),
            row(true, None),
            row(false, Some(MS)),
        ]);
        let one: Vec<(String, String, i64, Option<f64>)> = got
            .into_iter()
            .map(|g| (g.account_id, g.pkg_key, g.expires_ms, g.credits))
            .collect();
        assert_eq!(
            one,
            vec![(
                "a1".to_string(),
                crate::usage::KEY_ADDON.to_string(),
                MS,
                Some(100.0)
            )]
        );
    }

    /// 真实数据**干跑**：拿本机那份 `checkin_logs.json` + `credit_ledger.json`
    /// 在临时目录里演一遍启动补录 —— 只读本机数据，写的是临时目录，
    /// 不碰正在运行的那个应用手里那份台账。
    ///
    /// 钉住的是这次改造的要点：补录之后聚合包（「附加额度」）的到期日变成
    /// **最早一笔未过期的领取**（而不是台账里那个「最晚一笔」的标量），
    /// 且逐笔明细与日志里的成功领取一一对应（同一天的回放只留一条）。
    ///
    /// 运行：`cargo test --lib -- --ignored --nocapture backfill_dry_run`
    #[test]
    #[ignore = "读本机真实数据（只读），写临时目录"]
    fn backfill_dry_run_on_this_machines_real_data() {
        use std::fs;
        let live = dirs::data_dir()
            .expect("应有系统数据目录")
            .join("com.waxilo.qoder-assistant");
        let tmp = std::env::temp_dir().join(format!("wba-dryrun-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        for f in ["checkin_logs.json", "credit_ledger.json"] {
            let src = live.join(f);
            if src.exists() {
                fs::copy(&src, tmp.join(f)).unwrap();
            }
        }

        // 补录前的投影（读的是拷贝过来的那份老台账）
        let before = ledger::load_ledger(&tmp);
        backfill_grants_from_logs(&tmp);
        let store = ledger::store(&tmp);

        let mut checked = 0;
        for id in before.accts.keys() {
            let Some(old) = ledger::fact(&before, id) else { continue };
            let Some(new) = store.fact_of(id) else { continue };
            for (o, n) in old.packages.iter().zip(new.packages.iter()) {
                println!(
                    "{id} 包 {}：剩余 {}｜过期 {:?} → {:?}｜逐笔 {:?}",
                    n.name,
                    n.remaining,
                    o.expiry_ms,
                    n.expiry_ms,
                    n.grants
                        .iter()
                        .map(|g| (g.expires_ms, g.credits))
                        .collect::<Vec<_>>()
                );
                if n.grants.is_empty() {
                    continue;
                }
                checked += 1;
                // 逐笔列表自身：升序、无重复、都不早于此刻
                let now = crate::timeutil::now_ms();
                let ms: Vec<i64> = n.grants.iter().map(|g| g.expires_ms).collect();
                let mut sorted = ms.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(ms, sorted, "逐笔必须升序且无重复");
                assert!(ms.iter().all(|m| *m >= now), "过期的笔不该留下");
                // 展示口径 = 最早一笔（而不是最晚一笔）
                assert_eq!(n.expiry_ms, Some(ms[0]), "到期日要取最早一笔");
                // 老台账那个标量是「最晚一笔」，所以新值只会更早或持平
                if let Some(old_ms) = o.expiry_ms {
                    assert!(ms[0] <= old_ms, "最早一笔不可能晚于旧口径");
                }
            }
        }
        println!("共校验 {checked} 个带逐笔明细的包");
        let _ = fs::remove_dir_all(&tmp);
    }
}
