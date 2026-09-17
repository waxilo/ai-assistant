//! Token 续签：让账号的 `Cloud-IDE-JWT` **不过期掉线**。
//!
//! ## 为什么原来的写法不行
//!
//! 旧实现在几个「猜出来」的路径上试 `{"refreshToken": …}`：
//! `/trae/api/v2/user/refresh_token`、`/api/v1/auth/refresh` … —— 实测**全部 404**，
//! 从来没成功过（是「试一次就放弃」的伪续签）。
//!
//! 真实续签接口就是**换 token 那一个**：`POST {host}/trae/api/v3/oauth/ExchangeToken`，
//! `RefreshToken` 授权（官方 `exchangeTokenByRefreshToken`）：
//!
//! ```text
//! { ClientID, ClientSecret:"", RefreshToken,
//!   DeviceInfo: { DeviceID, MachineID, PlatformCode, DevicePublicKey, … },
//!   DeviceProof: { Signature, Timestamp, Nonce }, IDEVersion }
//! 签名消息 = "POST /trae/api/v3/oauth/ExchangeToken <ClientID> <RefreshToken> <ts> <nonce>"
//! ```
//!
//! ## 服务端的设备绑定（2026-09-14 实测，决定成败的唯一因素）
//!
//! | 请求 | 响应 |
//! |---|---|
//! | `DeviceID` 与签发时不一致 | `20403 Token device not match` |
//! | `DeviceID` 一致、**不带** `DeviceProof` | `20405 Device proof required` |
//! | `DeviceID` 一致、`DeviceProof` 用**别的私钥**签 | `20403 Token device not match` |
//!
//! 也就是说 token 绑定「签发时的 `DeviceID` + 那把私钥」。所以续签要成立，必须：
//! 1. `DeviceInfo.DeviceID` 用**账号里存的那个**（`account.device_id`）；
//! 2. `DeviceProof` 用**签发时那把私钥**签 —— 见 [`crate::devicekey`]（这就是为什么
//!    密钥对必须落盘：早期每次进程启动现生成一把，于是签发的 token 一律续不了）。
//!
//! ⚠️ **已经丢过密钥的账号续不了**：服务端会回 `20403`，此时只能重新登录一次
//! （重新登录会绑定新的持久化密钥，之后就一直能续）。这条错误会被明确翻译成
//! 「需重新登录」，而不是含糊的失败。
//!
//! ## 第二条路：本机登录态同步
//!
//! 对「同时登录在本机 TraeWork 里」的账号，还有一条**不需要密钥**的续签路：
//! TraeWork 自己会续签并把新 token 写回 `storage.json`，我们只要在它更新后**同步过来**即可
//! （比签发时间 `iat`，晚者胜）。所以策略是：先同步本机登录态，再走 OAuth 续签。
//!
//! ## 什么时候续（全自动 —— 界面上**没有任何**手动按钮）
//!
//! 按 JWT 载荷里的 `exp` 判断：**到期前 24 小时内**（或已过期）才动手，
//! 避免每轮都打接口。后台线程 [`spawn`] **启动后先巡一遍**、之后每 30 分钟巡一次，
//! 所以是真正的「自动续签」：不需要用户点任何东西
//! （手动入口 `renew_accounts` 与界面上那个按钮都已按需求删除）。
//!
//! 三条兜底，全部服务于同一个目标 —— **别让 token 静默过期**：
//! 1. 应用一启动就巡：可能关了几天才开，那时 token 说不定早就进窗口甚至过期了，
//!    等满一个巡检周期（30 分钟）才动手，那段时间接管拿的就是一张废票；
//! 2. 每次签到（手动单签 / 一键全签 / 定时签到）之前顺手续一次，见 `commands` 与 `scheduler`；
//! 3. 失败冷却**分档**：临时失败（网络抖动、服务端繁忙）只冷一个巡检周期、下一轮就重试；
//!    只有 `needs_login`（refresh token 失效 / 设备密钥丢了 —— 重试多少次都是同一个结论）才冷 6 小时。

use crate::accounts::{self, Account};
use crate::devicekey;
use crate::oauth;
use crate::token;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 到期前多久算「该续签了」。
pub const RENEW_WINDOW_MS: i64 = 24 * 3600 * 1000;
/// 后台巡检间隔。
const TICK: Duration = Duration::from_secs(30 * 60);
/// 启动后多久做第一次巡检：等应用初始化完（托盘、反代、数据目录），别在启动瞬间抢网络。
const START_DELAY: Duration = Duration::from_secs(20);
/// **临时**失败（网络抖动、服务端繁忙、响应看不懂）后的冷却：正好一个巡检周期，下一轮就重试。
/// token 寿命是「小时」量级，冷太久等于放任它过期。
const RETRY_COOLDOWN: Duration = TICK;
/// **注定**失败（`needs_login`：refresh token 已失效 / token 绑定的设备密钥不在本机）后的冷却。
/// 再试一百次也是同一个结论，只留一条日志给人看就够。
const FAIL_COOLDOWN: Duration = Duration::from_secs(6 * 3600);
/// 服务端错误码 → 明确结论
mod code {
    /// token 绑定设备不匹配（含「换了一只密钥」）
    pub const DEVICE_MISMATCH: &str = "20403";
    /// 需要 DeviceProof（缺签名）
    pub const PROOF_REQUIRED: &str = "20405";
    /// refresh token 已失效/非法
    pub const BAD_REFRESH_TOKEN: &str = "20404";
}

/// 续签成功时用的是哪条路
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RenewSource {
    /// OAuth 续签（`ExchangeToken` + `RefreshToken`）
    OAuth,
    /// 同步本机 TraeWork 登录态（TraeWork 自己续签后落盘的新 token）
    LocalSession,
}

/// 一次续签的结果（供界面与日志展示）
#[derive(Serialize, Clone, Debug)]
pub struct RenewOutcome {
    pub id: String,
    pub name: String,
    /// 是否拿到了新 token
    pub renewed: bool,
    pub source: Option<RenewSource>,
    /// 人话说明：成功说明来源，失败说明**下一步该做什么**
    pub message: String,
    /// 续签后 token 的到期时间（毫秒）
    pub expires_at: Option<i64>,
    /// 该账号是否需要重新登录才能自动续签
    pub needs_login: bool,
}

// ---------------------------------------------------------------------------
// 到期时间判定（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// token 到期时间（毫秒）：先读 JWT 载荷里的 `exp`（秒），再退回账号上的 `expires_at`。
///
/// `expires_at` 在不同来源里单位不一致（浏览器登录给毫秒、本机登录态给秒），
/// 这里统一按「小于 1e12 视为秒」归一化。
pub fn expiry_ms(account: &Account) -> Option<i64> {
    if let Some(v) = token::payload(&account.token).and_then(|p| p.get("exp").and_then(Value::as_i64)) {
        return Some(if v < 1_000_000_000_000 { v * 1000 } else { v });
    }
    account.expires_at.map(|v| if v < 1_000_000_000_000 { v * 1000 } else { v })
}

/// 是否到了该续签的窗口（已过期也算）。
///
/// 无到期信息时返回 `false`：没有 `exp` 的不透明 token 无从判断，自动续签会**跳过它** ——
/// 不能因为「不知道」就每 30 分钟盲换一次票。以前这里写着「交给界面上的手动续签」，
/// 而按钮已经删掉了，所以界面必须把这种账号显式标成「到期未知」，
/// 否则会出现一个「永远不会被续、界面上又看不出来」的沉默账号（见 `AccountsPage`）。
pub fn needs_renew(account: &Account, now_ms: i64) -> bool {
    match expiry_ms(account) {
        Some(e) => e - now_ms <= RENEW_WINDOW_MS,
        None => false,
    }
}

/// 候选 token 是否比当前**更新**（按 JWT `iat`，晚签发者胜）。
/// 比较 `iat` 而不是 `exp`：两者同向，但 `iat` 不受客户端时钟/时区写法影响。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let at = |t: &str| token::payload(t).and_then(|p| p.get("iat").and_then(Value::as_i64));
    match (at(candidate), at(current)) {
        (Some(c), Some(cur)) => c > cur,
        // 拿不到 iat 时，退化为「token 不同就算更新」——但要保证不是空串
        _ => !candidate.trim().is_empty() && candidate != current,
    }
}

// ---------------------------------------------------------------------------
// 失败冷却
// ---------------------------------------------------------------------------

/// 失败时间点 + 本次的冷却时长（时长随失败性质变，见 [`mark_attempt`]）。
fn last_attempt() -> &'static Mutex<std::collections::HashMap<String, (Instant, Duration)>> {
    static M: OnceLock<Mutex<std::collections::HashMap<String, (Instant, Duration)>>> =
        OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn cooling(id: &str) -> bool {
    let Ok(mut g) = last_attempt().lock() else {
        return false;
    };
    let now = Instant::now();
    g.retain(|_, (t, d)| within_cooldown(*t, *d, now));
    g.get(id).is_some_and(|(t, d)| within_cooldown(*t, *d, now))
}

/// 是否仍在冷却窗口内。抽成纯函数只为**能在单测里捏时间点**；生产路径传的都是 `Instant::now()`。
fn within_cooldown(marked_at: Instant, cooldown: Duration, now: Instant) -> bool {
    now.saturating_duration_since(marked_at) < cooldown
}

fn mark_attempt(id: &str, cooldown: Duration) {
    if let Ok(mut g) = last_attempt().lock() {
        g.insert(id.to_string(), (Instant::now(), cooldown));
    }
}

fn clear_attempt(id: &str) {
    if let Ok(mut g) = last_attempt().lock() {
        g.remove(id);
    }
}

// ---------------------------------------------------------------------------
// 续签
// ---------------------------------------------------------------------------

/// 按需续签（巡检用）：不在窗口内 / 刚失败过 → 返回 `None`（不发任何请求）。
pub async fn renew_if_needed(dir: &Path, account: &mut Account) -> Option<RenewOutcome> {
    // 绑了凭证池时本地永不续签：整池的续签统一由 `broker::sync` 拿着闸做。
    // 这里若退回本地续签，几台机器会同时打官方接口、各自换一条新链 ——
    // 「谁先签谁把别人踢下线」正是这么来的，所以这条分岔不能省。
    // `renew()` 不加这个判断：`broker::sync` 需要直接调它来续签。
    if crate::broker::bound() {
        return None;
    }
    let now = chrono::Utc::now().timestamp_millis();
    if !needs_renew(account, now) {
        return None;
    }
    if cooling(&account.id) {
        return None;
    }
    let out = renew(dir, account).await;
    if out.renewed {
        clear_attempt(&account.id);
    } else if out.needs_login {
        // 注定失败：冷 6 小时，别每轮都去打一个必然被拒的接口、把日志刷满
        mark_attempt(&account.id, FAIL_COOLDOWN);
    } else {
        // 临时失败：只冷一个巡检周期，下一轮（30 分钟后）就重试 ——
        // 这正是「自动续签」该有的韧性：网络抖一下不该让 token 就这么过期掉
        mark_attempt(&account.id, RETRY_COOLDOWN);
    }
    Some(out)
}

/// 强制续签一次：**忽略续签窗口与失败冷却**。
///
/// ⚠️ 界面上已经没有手动入口了（那个按钮已按要求删除），所以这个函数现在只服务于
/// **真机探针** `live_renew_probe` —— 它是「这个账号到底还能不能续签」的直接验证手段，
/// 别当成死代码顺手删掉。
pub async fn renew(dir: &Path, account: &mut Account) -> RenewOutcome {
    // ① 先试「本机登录态同步」：离线、零风险，且对「同时登录在 TraeWork 里」的账号必然有效
    if let Some(out) = adopt_local_session(dir, account) {
        return out;
    }

    // ② 再试 OAuth 续签
    let Some(refresh_token) = account
        .refresh_token
        .clone()
        .filter(|s| !s.trim().is_empty())
    else {
        return RenewOutcome {
            id: account.id.clone(),
            name: account.name.clone(),
            renewed: false,
            source: None,
            message: "该账号没有 refresh token，无法自动续签 —— 请重新登录该账号。".into(),
            expires_at: expiry_ms(account),
            needs_login: true,
        };
    };

    match exchange_by_refresh_token(dir, account, &refresh_token).await {
        Ok(()) => {
            let _ = accounts::save_accounts(dir, &replace(dir, account));
            let exp = expiry_ms(account);
            RenewOutcome {
                id: account.id.clone(),
                name: account.name.clone(),
                renewed: true,
                source: Some(RenewSource::OAuth),
                message: format!(
                    "已续签，新 token 有效期至 {}。",
                    fmt_ms(exp)
                ),
                expires_at: exp,
                needs_login: false,
            }
        }
        Err(e) => {
            let needs_login = e.needs_login;
            RenewOutcome {
                id: account.id.clone(),
                name: account.name.clone(),
                renewed: false,
                source: None,
                message: if needs_login {
                    format!(
                        "服务端拒绝续签（{e}）—— 该 token 绑定的设备密钥已不在本机，\
                         需重新登录一次；此后即可自动续签。"
                    )
                } else {
                    format!("续签失败：{e}")
                },
                expires_at: expiry_ms(account),
                needs_login,
            }
        }
    }
}

/// 本机 TraeWork 登录态里是否有这个账号**更新的** token；有就采纳并落盘。
fn adopt_local_session(dir: &Path, account: &mut Account) -> Option<RenewOutcome> {
    let uid = account.user_id.clone().filter(|s| !s.trim().is_empty())?;
    let local = crate::trae_auth::find_local_session_by_uid(&uid)?;
    if !is_newer(&local.token, &account.token) {
        return None;
    }
    // 新 token 是 TraeWork 用**它的**设备身份签发的，设备号要一并跟过去，
    // 否则下次续签又会 20403（虽然这条路的正确做法本来就是继续等 TraeWork 同步）
    if let Some(d) = local.device_id.clone().filter(|s| !s.trim().is_empty()) {
        account.device_id = Some(d);
    }
    if let Some(m) = local.machine_id.clone().filter(|s| !s.trim().is_empty()) {
        account.machine_id = Some(m);
    }
    account.token = local.token.clone();
    if let Some(rt) = local.refresh_token.clone() {
        account.refresh_token = Some(rt);
    }
    if let Some(h) = local.host.clone() {
        account.host = Some(h);
    }
    account.expires_at = local.expires_at;
    account.refresh_expires_at = local.refresh_expires_at;
    let _ = accounts::save_accounts(dir, &replace(dir, account));
    let exp = expiry_ms(account);
    Some(RenewOutcome {
        id: account.id.clone(),
        name: account.name.clone(),
        renewed: true,
        source: Some(RenewSource::LocalSession),
        message: format!(
            "已同步 TraeWork 续签后的新 token（有效期至 {}）。",
            fmt_ms(exp)
        ),
        expires_at: exp,
        needs_login: false,
    })
}

/// 用 `RefreshToken` 换新 token（`ExchangeToken` + `DeviceProof`）。成功后就地更新账号字段。
async fn exchange_by_refresh_token(
    dir: &Path,
    account: &mut Account,
    refresh_token: &str,
) -> Result<(), RenewError> {
    let dev = devicekey::load_or_create(
        dir,
        crate::trae_auth::local_device_identity().0,
        crate::trae_auth::local_device_identity().1,
    );

    let host = oauth::normalize_api_host(account.host.as_deref().unwrap_or_default());
    // DeviceID 必须是**签发时**那个（账号里存着）；没有记录时用本机身份兜底，
    // 但那种情况下服务端大概率仍是 20403 —— 错误会被翻译成「需重新登录」。
    let device_id = account
        .device_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| dev.device_id.clone());
    let machine_id = account
        .machine_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| dev.machine_id.clone());

    let path = oauth::EXCHANGE_TOKEN_PATH;
    let ts = chrono::Utc::now().timestamp();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    // 官方 bTe()：`[method, path, clientId, token, ts, nonce].join(" ")`
    let message = format!(
        "POST {path} {} {refresh_token} {ts} {nonce}",
        oauth::CLIENT_ID_SOLO
    );
    let signature = devicekey::sign_der_base64(&dev.private_key_pem, message.as_bytes())
        .map_err(RenewError::other)?;

    let body = serde_json::json!({
        "ClientID": oauth::CLIENT_ID_SOLO,
        "ClientSecret": "",
        "RefreshToken": refresh_token,
        "DeviceInfo": {
            "DeviceID": device_id,
            "MachineID": machine_id,
            "PlatformCode": "SOLO_PC",
            "DeviceType": "PC",
            "DeviceName": device_name(),
            "DeviceModel": "",
            "DeviceBrand": "",
            "DeviceCPU": "",
            "OSInfo": "",
            "OSVersion": "",
            "DevicePublicKey": dev.public_key_pem,
            "ClientVersion": env!("CARGO_PKG_VERSION"),
        },
        "DeviceProof": { "Signature": signature, "Timestamp": ts, "Nonce": nonce },
        "IDEVersion": env!("CARGO_PKG_VERSION"),
    });

    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .build()
        .map_err(|e| RenewError::other(format!("HTTP 客户端初始化失败：{e}")))?
        .post(format!("{host}{path}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| RenewError::other(format!("请求失败：{e}")))?;

    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| RenewError::other(format!("响应非 JSON：{}", excerpt(&text))))?;

    if let Some(err) = v
        .get("ResponseMetadata")
        .and_then(|m| m.get("Error"))
        .filter(|e| !e.is_null())
    {
        let code = err.get("Code").and_then(Value::as_str).unwrap_or("");
        let msg = err.get("Message").and_then(Value::as_str).unwrap_or("");
        return Err(RenewError::server(
            code,
            msg,
            matches!(
                code,
                code::DEVICE_MISMATCH | code::PROOF_REQUIRED | code::BAD_REFRESH_TOKEN
            ),
        ));
    }

    let result = v.get("Result").ok_or_else(|| {
        RenewError::other(format!("响应缺少 Result：{}", excerpt(&text)))
    })?;
    let new_token = dig(result, &["Token", "token", "accessToken"])
        .ok_or_else(|| RenewError::other(format!("响应缺少 Token：{}", excerpt(&text))))?;

    account.token = new_token;
    if let Some(rt) = dig(result, &["RefreshToken", "refreshToken", "refresh_token"]) {
        account.refresh_token = Some(rt);
    }
    if let Some(exp) = dig_num(result, &["TokenExpireAt", "tokenExpireAt", "expiresAt", "expires_at"])
    {
        account.expires_at = Some(if exp < 1_000_000_000_000 { exp * 1000 } else { exp });
    }
    if let Some(exp) = dig_num(result, &["RefreshTokenExpireAt", "refreshTokenExpireAt"]) {
        account.refresh_expires_at = Some(if exp < 1_000_000_000_000 { exp * 1000 } else { exp });
    }
    Ok(())
}

/// 续签错误。`needs_login` 表示「必须重新登录」而不是「临时失败」。
#[derive(Debug)]
struct RenewError {
    text: String,
    needs_login: bool,
}

impl RenewError {
    fn other(text: String) -> RenewError {
        RenewError { text, needs_login: false }
    }
    fn server(code: &str, msg: &str, needs_login: bool) -> RenewError {
        RenewError {
            text: format!("code={code} {msg}"),
            needs_login,
        }
    }
}

impl std::fmt::Display for RenewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把 `account` 覆盖回磁盘上的列表（按 id 匹配），返回新列表。
fn replace(dir: &Path, account: &Account) -> Vec<Account> {
    let mut all = accounts::load_accounts(dir);
    if let Some(a) = all.iter_mut().find(|a| a.id == account.id) {
        *a = account.clone();
    }
    all
}

fn dig(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k))
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn dig_num(v: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| v.get(*k)).and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
    })
}

fn device_name() -> String {
    crate::proc::cmd("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok().map(|x| x.trim().to_string()))
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| "TraeWorkAssistant-Device".into())
}

fn excerpt(s: &str) -> String {
    s.chars().take(240).collect()
}

/// 毫秒时间戳 → `MM-DD HH:MM`（本地时区）
fn fmt_ms(ms: Option<i64>) -> String {
    match ms.and_then(chrono::DateTime::from_timestamp_millis) {
        Some(t) => t
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
        None => "未知".into(),
    }
}

// ---------------------------------------------------------------------------
// 后台自动续签
// ---------------------------------------------------------------------------

/// 后台巡检线程：**启动后先扫一遍**，之后每 30 分钟扫一遍，把临近过期的 token 续掉。
///
/// 与 `scheduler`（定时签到）同生命周期 —— 应用常驻托盘即可一直续签。
///
/// 为什么启动就要扫、而不是先睡 30 分钟：应用可能「关了好几天才开」，那时 token
/// 说不定早就进窗口甚至过期了；而这段时间里智能接管拿的就是一张废票
/// （请求全 401，界面上只看到一片失败，很难联想到是 token 的事）。
pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(START_DELAY);
        loop {
            sweep(&app);
            std::thread::sleep(TICK);
        }
    });
}

/// 巡一遍全部账号：只有**进了续签窗口、且不在冷却里**的才会真的发请求
/// （两个判断都在 [`renew_if_needed`] 里，本函数只负责遍历与记日志）。
fn sweep(app: &tauri::AppHandle) {
    let Ok(dir) = crate::commands::try_data_dir(app) else {
        return;
    };
    let now = chrono::Utc::now().timestamp_millis();
    for mut account in accounts::load_accounts(&dir) {
        if !needs_renew(&account, now) {
            continue;
        }
        let Some(out) = tauri::async_runtime::block_on(renew_if_needed(&dir, &mut account)) else {
            continue;
        };
        // ⚠️ **成功的续签不记日志**，这不是偷懒：token 寿命是「小时」量级而窗口是 24 小时，
        // 也就是说每轮巡检（30 分钟）都可能真的换一次票 —— 每账号每天几十条，
        // 200 条上限的内存日志一天就被刷满，反而把「签到失败」那种真该看的挤掉。
        // 续签成功是**可自证**的：账号列表里那列到期时间被推远了就是它干的。
        if !out.renewed {
            crate::logs::push(
                &out.name,
                false,
                format!("自动续签未成功：{}", out.message),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    fn jwt(iat: i64, exp: i64) -> String {
        let body = URL_SAFE_NO_PAD
            .encode(format!(r#"{{"data":{{"id":"1"}},"iat":{iat},"exp":{exp}}}"#).as_bytes());
        format!("eyJhbGciOiJSUzI1NiJ9.{body}.sig")
    }

    fn acc(token: &str, expires_at: Option<i64>) -> Account {
        Account {
            id: "a1".into(),
            name: "n".into(),
            phone: None,
            region: None,
            user_id: Some("1".into()),
            token: token.into(),
            refresh_token: Some("rt".into()),
            host: None,
            expires_at,
            refresh_expires_at: None,
            device_id: Some("dev".into()),
            machine_id: None,
            created_at: String::new(),
            credit_snapshot: None,
        }
    }

    #[test]
    fn reads_expiry_from_jwt_seconds() {
        let a = acc(&jwt(1_789_000_000, 1_790_601_750), None);
        assert_eq!(expiry_ms(&a), Some(1_790_601_750_000));
    }

    #[test]
    fn normalizes_seconds_and_millis_expires_at() {
        let secs = acc("opaque", Some(1_790_601_750));
        assert_eq!(expiry_ms(&secs), Some(1_790_601_750_000));
        let ms = acc("opaque", Some(1_790_601_750_000));
        assert_eq!(expiry_ms(&ms), Some(1_790_601_750_000));
    }

    #[test]
    fn renew_window_is_24h_before_expiry() {
        let exp_ms = 1_790_601_750_000i64;
        let a = acc(&jwt(1_789_000_000, 1_790_601_750), None);
        assert_eq!(expiry_ms(&a), Some(exp_ms));
        // 距到期 25 小时：还不用续
        assert!(!needs_renew(&a, exp_ms - 25 * 3600 * 1000));
        // 距到期 23 小时：进窗口
        assert!(needs_renew(&a, exp_ms - 23 * 3600 * 1000));
        // 已过期：同样落在窗口内
        assert!(needs_renew(&a, exp_ms + 1000));
    }

    #[test]
    fn unknown_expiry_never_triggers_automatic_renew() {
        let a = acc("not-a-jwt", None);
        assert_eq!(expiry_ms(&a), None, "无法判断到期时间");
        assert!(!needs_renew(&a, 1_790_601_750_000));
    }

    #[test]
    fn newer_token_is_adopted_by_iat() {
        assert!(is_newer(&jwt(2_000, 3_000), &jwt(1_000, 2_000)));
        assert!(!is_newer(&jwt(1_000, 2_000), &jwt(2_000, 3_000)));
        assert!(!is_newer(&jwt(1_000, 2_000), &jwt(1_000, 2_000)));
        // 拿不到 iat：只要不同且非空就算更新
        assert!(is_newer("opaque-new", "opaque-old"));
        assert!(!is_newer("opaque-old", "opaque-old"));
        assert!(!is_newer("", "opaque-old"));
    }

    /// 冷却分档：临时失败只冷一个巡检周期，注定失败才冷 6 小时。
    /// 纯函数，捏时间点即可 —— 「到底该冷多久」由 `renew_if_needed` 按 `needs_login` 选。
    #[test]
    fn cooldown_follows_the_failure_kind() {
        let failed_at = Instant::now();
        // 临时失败：一分钟后就该允许重试（更别说 30 分钟后那一轮了）
        assert!(within_cooldown(
            failed_at,
            RETRY_COOLDOWN,
            failed_at + Duration::from_secs(60)
        ));
        // 临时失败：满一个巡检周期即解冻
        assert!(!within_cooldown(
            failed_at,
            RETRY_COOLDOWN,
            failed_at + RETRY_COOLDOWN
        ));
        // 注定失败：一小时还在冷却里，六小时后才放行
        assert!(within_cooldown(
            failed_at,
            FAIL_COOLDOWN,
            failed_at + Duration::from_secs(3600)
        ));
        assert!(!within_cooldown(
            failed_at,
            FAIL_COOLDOWN,
            failed_at + FAIL_COOLDOWN
        ));
        // 时钟不可能倒退，但真倒退了也不能算「已过期」（saturating 到 0 ⇒ 仍在冷却）
        assert!(within_cooldown(failed_at, RETRY_COOLDOWN, failed_at));
    }

    /// 真机探针：对真实账号打一次续签，打印服务端结论。
    ///
    /// `cargo test --lib -- --ignored --nocapture live_renew_probe`
    #[test]
    #[ignore]
    fn live_renew_probe() {
        let dir = match std::env::var("TWA_DATA_DIR") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => dirs::home_dir()
                .map(|h| h.join("Library/Application Support/cn.traework.assistant"))
                .expect("无法定位数据目录"),
        };
        let mut list = accounts::load_accounts(&dir);
        for account in list.iter_mut() {
            let before = expiry_ms(account);
            println!(
                "\n=== {} (uid={:?}) 到期={} device_id={:?} refresh={} ===",
                account.name,
                account.user_id,
                fmt_ms(before),
                account.device_id,
                account.refresh_token.is_some(),
            );
            let out = tauri::async_runtime::block_on(renew(&dir, account));
            println!("  renewed={} source={:?} needs_login={}", out.renewed, out.source, out.needs_login);
            println!("  message={}", out.message);
        }
    }
}
