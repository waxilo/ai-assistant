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
//! ⚠️ **谁是那台设备，决定这一枪打不打得中**。本机同时可能存在好几套设备身份：
//! assistant 自己的 `device.json`，以及**每个 TraeWork 安装各一把** —— 桌面端登录时注册的
//! `DeviceID` 是数字 id（本机 `1687098020335299`），公钥也是它自己的（落盘在同一个
//! `storage.json` 的 `iCubeAuthInfo://icube-dc:<DeviceID>` 里，见
//! [`crate::trae_auth::local_desktop_devices`]）。所以续签按 [`candidates`] 的顺序把本机
//! 所有身份挨个试一遍，只有 `20403` 才换下一把 —— 两台机器一起 20403 不是「密钥丢了」，
//! 是当时还只会拿 `device.json` 签。
//!
//! 只剩两种真救不了的情况：链是**旧版 assistant 那把没落盘的临时密钥**签的（2026-09-14 之前
//! 登录的一批，密钥随进程没了），或服务端那条 refresh token 已失效（`20404`）。
//! 这两种会被明确翻译成「需重新登录」，而不是含糊的失败。
//!
//! ## 第二条路：本机登录态同步
//!
//! 对「同时登录在本机 TraeWork 里」的账号，还有一条**零请求**的路：TraeWork 自己会续签并
//! 把新 token 写回 `storage.json`，我们只要在它更新后**同步过来**即可（比签发时间 `iat`，
//! 晚者胜）。它不等价于第一条路的备份 —— 桌面端换到新票也可能不落盘（2026-09-28 实测：
//! SOLO 09:37 换票成功却报 `UserInfoNotMatchError`，票被丢掉、旧 refresh token 已作废），
//! 所以两条路都要走。策略是先同步本机登录态，再走 OAuth 续签；但**桌面端自己还续得动的时候
//! 不抢**（一条链只能续一次，抢了的代价见上面那个例子）。
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
use std::time::Duration;

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
    /// 本轮**故意不动**：这条链本机桌面端还续得动，我们等它换完再同步。
    /// 它既不是成功也不是失败，所以不记日志、不进冷却、也不算整池失败。
    #[serde(default)]
    pub deferred: bool,
}

// ---------------------------------------------------------------------------
// 到期时间判定（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// 时间戳归一：服务端各处混发秒与毫秒，小于 1e12 一律当秒。
pub fn norm_ms(v: i64) -> i64 {
    if v < 1_000_000_000_000 {
        v * 1000
    } else {
        v
    }
}

/// token **自己声明**的到期时间（JWT `exp`，毫秒）；不是 JWT 就读不出来。
///
/// 这是唯一可信的到期来源：`account.expires_at` 这个字段经常是空的（浏览器登录换来的票、
/// 从池里采纳的票都不填它），而凭证的真实寿命就写在票里。以前凡拿 `expires_at` 判新旧的地方
/// 都会被这些空值带错 —— 2026-09-28「池里那张还能用的票被当成不更新丢掉」就是这么来的。
pub fn token_expiry_ms(token: &str) -> Option<i64> {
    token::payload(token)
        .and_then(|p| p.get("exp").and_then(Value::as_i64))
        .map(norm_ms)
}

/// token 到期时间（毫秒）：先读 JWT 载荷里的 `exp`（秒），再退回账号上的 `expires_at`。
pub fn expiry_ms(account: &Account) -> Option<i64> {
    token_expiry_ms(&account.token).or_else(|| account.expires_at.map(norm_ms))
}

fn iat(token: &str) -> Option<i64> {
    token::payload(token).and_then(|p| p.get("iat").and_then(Value::as_i64))
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
    match (iat(candidate), iat(current)) {
        (Some(c), Some(cur)) => c > cur,
        // 拿不到 iat 时，退化为「token 不同就算更新」——但要保证不是空串
        _ => !candidate.trim().is_empty() && candidate != current,
    }
}

/// 严格版：两边都必须是读得出 `iat` 的 JWT，且候选签发更晚。
///
/// 与 [`is_newer`] 的差别就是那条退化分支。这一条要拿去决定**要不要用云端副本盖掉本地凭证**，
/// 所以「两个不透明 token 长得不一样」不能算更新 —— 来历不明的那份不许赢。
pub fn newer_by_iat(candidate: &str, current: &str) -> bool {
    matches!((iat(candidate), iat(current)), (Some(c), Some(cur)) if c > cur)
}

/// 「谁的票更新」的**唯一**判据：候选（token, 到期时间）是否新于在用的那一份。
///
/// 两个方向必须共用它 —— 云端 → 本地的采纳（`broker::adopt`）和本地 → 云端的回写
/// （`broker::local_is_newer`）一旦各写一套，就会出现「A 看 B 更新、B 看 A 也更新」，
/// 两台机器每轮同步都改一次整池版本，永远收敛不了。
///
/// 规则（2026-10-08 起改为 **iat 优先**）：
/// 1. 两侧都是读得出 `iat` 的 JWT → **晚签发者胜**。`iat` 是「轮换顺序」的本义；
///    exp 只是它的投影（受各家 TTL、时钟写法影响），过去拿 exp 当主键的缺陷在于
///    exp 无法区分「更新的一次轮换」和「一张 exp 声明得更晚、但已被吊销的死票」。
/// 2. 两侧都读不出 `iat` → 按到期时间比：都读得出时晚到期者胜；一侧读不出，
///    读得出的算新（**「不知道」不等于「更旧」**）；两侧都读不出 → 不透明 token
///    「长得不一样」不构成更新的理由，来历不明的不许赢。
pub fn ticket_is_newer(
    cand_expiry: Option<i64>,
    cand_token: &str,
    cur_expiry: Option<i64>,
    cur_token: &str,
) -> bool {
    match (iat(cand_token), iat(cur_token)) {
        (Some(c), Some(cur)) => return c > cur,
        _ => {}
    }
    match (cand_expiry, cur_expiry) {
        (Some(c), Some(cur)) => c > cur,
        (Some(_), None) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 失败冷却（**持久化**：写在 `Account::renew_blocked_until` 上、随 accounts.json 落盘）
// ---------------------------------------------------------------------------

/// 该账号是否仍在续签冷却期内。
///
/// 冷却写在账号上并**落盘**：`needs_login`（refresh token 失效 / 设备密钥不在本机 ——
/// 重试多少次都是同一个结论）冷 6 小时，临时失败冷一个巡检周期。
/// 旧实现是内存 HashMap，应用一重启就失忆，死链每轮巡检都去打一个必然被拒的接口。
pub fn renew_blocked(account: &Account, now_ms: i64) -> bool {
    account.renew_blocked_until.is_some_and(|until| until > now_ms)
}

/// 是否该**尝试**续签：进了续签窗口（或已过期），**或者已被上游判死**。
///
/// 「已被判死」必须单列：被吊销的票 exp 可能还在未来，按窗口判断它永远轮不到续 ——
/// 链若在本机续得动，续签就是它唯一的自救通道（2026-10-08 死锁事故的另一半）。
pub fn renew_due(account: &Account, now_ms: i64) -> bool {
    needs_renew(account, now_ms) || account.invalidated_at.is_some()
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
    if !renew_due(account, now) {
        return None;
    }
    if renew_blocked(account, now) {
        return None;
    }
    let out = renew(dir, account).await;
    if out.deferred {
        // 交给桌面端：既不记日志也不进冷却 —— 它每轮都该重新看一眼（桌面端可能就是下一轮换的）
        return None;
    }
    // 成功与失败的冷却/清除都在 [`renew`] 里就地生效并落盘，这里不用再补一次写。
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

    // ② 本机桌面端还轮得到它自己续 —— **那就先别抢**。
    //     `RefreshToken` 是一次性的：谁先打谁把对方的那条链作废。桌面端账号（用户日常在用的
    //     Trae IDE）一旦被这里续掉，它手里存的 refresh token 当场失效，等它那张票到期就只能
    //     重新登录 —— 那正是 `用户696006185260` 这次死掉的样子。所以只在桌面端**已经续不动**
    //     （本机没有它的登录态 / 它那张票已经过期）时，才动用它的密钥替它续。
    if desktop_can_self_renew(account, chrono::Utc::now().timestamp_millis()) {
        return RenewOutcome {
            id: account.id.clone(),
            name: account.name.clone(),
            renewed: false,
            source: None,
            message: "这个账号本机桌面端还登录着、票也没到期，续签交给它（它换完新票会被本机同步接住）—— 不打接口，免得把桌面端那条链顶掉。".into(),
            expires_at: expiry_ms(account),
            needs_login: false,
            deferred: true,
        };
    }

    // ③ 再试 OAuth 续签
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
            deferred: false,
        };
    };

    match exchange_by_refresh_token(dir, account, &refresh_token).await {
        Ok(()) => {
            // 新票在手：活性/冷却两个标记自然失效（这张票刚被官方亲自签发，必然活着）
            account.invalidated_at = None;
            account.renew_blocked_until = None;
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
                deferred: false,
            }
        }
        Err(e) => {
            let needs_login = e.needs_login;
            // 失败退避**随账号落盘**：needs_login（注定失败）冷 6 小时，临时失败冷一个巡检周期。
            // 值没变就不写 —— 临时失败每 30 分钟都会走到这里，别白落一次盘。
            let cooldown = if needs_login { FAIL_COOLDOWN } else { RETRY_COOLDOWN };
            let until = chrono::Utc::now().timestamp_millis() + cooldown.as_millis() as i64;
            if account.renew_blocked_until != Some(until) {
                account.renew_blocked_until = Some(until);
                let _ = accounts::save_accounts(dir, &replace(dir, account));
            }
            RenewOutcome {
                id: account.id.clone(),
                name: account.name.clone(),
                renewed: false,
                source: None,
                message: if needs_login {
                    format!(
                        "续签被服务端拒绝（{e}）：本机每一套设备身份（assistant 的 device.json \
                         加上本机各套桌面端密钥）都被判定不匹配，或那条 refresh token 已失效。\
                         出路两条：在这个账号还登录着的桌面端 TraeWork 里让它自续一次（本机同步会接住），\
                         或重新登录一次。"
                    )
                } else {
                    format!("续签失败：{e}")
                },
                expires_at: expiry_ms(account),
                needs_login,
                deferred: false,
            }
        }
    }
}

/// 本机桌面端**自己还续得上**这个账号：它登录着、手里有 refresh token、票还没过期。
///
/// 判据见 [`renew`] 的第②步 —— 一条链只能续一次，谁续谁把另一边的 refresh token 顶掉。
fn desktop_can_self_renew(account: &Account, now_ms: i64) -> bool {
    let uid = match account.user_id.as_deref().map(str::trim) {
        Some(u) if !u.is_empty() => u,
        _ => return false,
    };
    let Some(local) = crate::trae_auth::find_local_session_by_uid(uid) else {
        return false;
    };
    desktop_is_live(
        local.refresh_token.as_deref(),
        token_expiry_ms(&local.token).or_else(|| local.expires_at.map(norm_ms)),
        now_ms,
    )
}

/// 纯函数版判据。到期时间**读不出来时算「还活着」**：宁可多等桌面端一轮，
/// 也不能凭「不知道」就把一条在用的链顶掉（那正是这次要把账号弄死的做法）。
fn desktop_is_live(refresh_token: Option<&str>, expiry_ms: Option<i64>, now_ms: i64) -> bool {
    refresh_token.map(str::trim).is_some_and(|s| !s.is_empty())
        && expiry_ms.map_or(true, |e| e > now_ms)
}

/// 本机 TraeWork 登录态里是否有这个账号**更新的** token；有就采纳并落盘。
///
/// `pub` 是给 `broker::sync` 用的：桌面端自续与新票落盘这两件事**不该绑在续签窗口上**
/// （见 `broker::sync` 第①步）。
pub fn adopt_local_session(dir: &Path, account: &mut Account) -> Option<RenewOutcome> {
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
    // 新票在手：活性/冷却标记一并清掉（可能正是靠「桌面端换的新票」才能救活一条死链）
    account.invalidated_at = None;
    account.renew_blocked_until = None;
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
        deferred: false,
    })
}

/// 用 `RefreshToken` 换新 token（`ExchangeToken` + `DeviceProof`）。成功后就地更新账号字段。
///
/// 一次打不定能成：服务端只认**签发那条票的设备身份**，而本机同时可能存在好几套
/// （每个 TraeWork 安装一把自己的密钥 + assistant 自己的 `device.json`）。所以按
/// [`candidates`] 的顺序逐个试，只有 `20403`（=「这把钥匙不对」）才继续往下试，
/// 其余错误直接停 —— 重试同一把钥匙没有意义。
async fn exchange_by_refresh_token(
    dir: &Path,
    account: &mut Account,
    refresh_token: &str,
) -> Result<(), RenewError> {
    let host = oauth::normalize_api_host(account.host.as_deref().unwrap_or_default());
    let mut mismatch: Option<RenewError> = None;
    for c in candidates(dir, account) {
        match exchange_once(&host, account, refresh_token, &c).await {
            Ok(()) => {
                // 记住这把真的签动了：下次直接是它（`adopt_local_session` 之后可能被覆盖回
                // `telemetry.devDeviceId`，所以覆盖不到也只是多试一轮，不影响正确性）
                account.device_id = Some(c.device_id.clone());
                return Ok(());
            }
            Err(e) if e.wrong_device => mismatch = mismatch.or(Some(e)),
            Err(e) => return Err(e),
        }
    }
    Err(mismatch.unwrap_or_else(|| RenewError::other("本机没有任何可用的设备身份".into())))
}

/// 一套候选的设备身份：`DeviceInfo` 的 `DeviceID` / `MachineID` / `DevicePublicKey` + 签 `DeviceProof` 的私钥。
struct Candidate {
    device_id: String,
    machine_id: String,
    public_key_pem: String,
    private_key_pem: String,
}

/// 候选设备身份，**优先桌面端**（现网绝大多数票是桌面端签的，assistant 自己那把排最后）。
fn candidates(dir: &Path, account: &Account) -> Vec<Candidate> {
    let uid = account
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let fallback_machine = account
        .machine_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| crate::trae_auth::local_device_identity().1)
        .unwrap_or_default();

    let mut out: Vec<Candidate> = crate::trae_auth::local_desktop_devices(uid)
        .into_iter()
        .map(|d| Candidate {
            device_id: d.device_id,
            machine_id: d.machine_id.unwrap_or_else(|| fallback_machine.clone()),
            public_key_pem: d.public_key_pem,
            private_key_pem: d.private_key_pem,
        })
        .collect();

    let dev = devicekey::load_or_create(
        dir,
        crate::trae_auth::local_device_identity().0,
        crate::trae_auth::local_device_identity().1,
    );
    out.push(Candidate {
        device_id: dev.device_id.clone(),
        machine_id: dev.machine_id.clone(),
        public_key_pem: dev.public_key_pem.clone(),
        private_key_pem: dev.private_key_pem.clone(),
    });

    out.retain(|c| !c.device_id.trim().is_empty() && !c.private_key_pem.trim().is_empty());
    for i in (1..out.len()).rev() {
        if out[..i]
            .iter()
            .any(|p| p.device_id == out[i].device_id && p.public_key_pem == out[i].public_key_pem)
        {
            out.remove(i);
        }
    }
    out
}

/// 用一套身份打一次 `ExchangeToken`。
async fn exchange_once(
    host: &str,
    account: &mut Account,
    refresh_token: &str,
    c: &Candidate,
) -> Result<(), RenewError> {
    let path = oauth::EXCHANGE_TOKEN_PATH;
    let ts = chrono::Utc::now().timestamp();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    // 官方 bTe()：`[method, path, clientId, token, ts, nonce].join(" ")`
    let message = format!(
        "POST {path} {} {refresh_token} {ts} {nonce}",
        oauth::CLIENT_ID_SOLO
    );
    let signature = devicekey::sign_der_base64(&c.private_key_pem, message.as_bytes())
        .map_err(RenewError::other)?;

    let body = serde_json::json!({
        "ClientID": oauth::CLIENT_ID_SOLO,
        "ClientSecret": "",
        "RefreshToken": refresh_token,
        "DeviceInfo": {
            "DeviceID": c.device_id,
            "MachineID": c.machine_id,
            "PlatformCode": "SOLO_PC",
            "DeviceType": "PC",
            "DeviceName": device_name(),
            "DeviceModel": "",
            "DeviceBrand": "",
            "DeviceCPU": "",
            "OSInfo": "",
            "OSVersion": "",
            "DevicePublicKey": c.public_key_pem,
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
        return Err(RenewError::server(code, msg));
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

/// 续签错误。`needs_login` 表示「必须重新登录」而不是「临时失败」；
/// `wrong_device` 只表示「这把钥匙不对」，换下一把候选接着试。
#[derive(Debug)]
struct RenewError {
    text: String,
    needs_login: bool,
    wrong_device: bool,
}

impl RenewError {
    fn other(text: String) -> RenewError {
        RenewError { text, needs_login: false, wrong_device: false }
    }
    fn server(code: &str, msg: &str) -> RenewError {
        RenewError {
            text: format!("code={code} {msg}"),
            needs_login: matches!(
                code,
                code::DEVICE_MISMATCH | code::PROOF_REQUIRED | code::BAD_REFRESH_TOKEN
            ),
            wrong_device: code == code::DEVICE_MISMATCH,
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
        if !renew_due(&account, now) {
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
            invalidated_at: None,
            renew_blocked_until: None,
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

    /// 严格版（跨机覆盖用的那条）：**没有**上面那条退化分支 —— 读不出 `iat` 就不算更新。
    #[test]
    fn newer_by_iat_refuses_opaque_tokens() {
        assert!(newer_by_iat(&jwt(2_000, 3_000), &jwt(1_000, 2_000)));
        assert!(!newer_by_iat(&jwt(1_000, 2_000), &jwt(2_000, 3_000)));
        assert!(!newer_by_iat("opaque-new", "opaque-old"));
        assert!(!newer_by_iat(&jwt(2_000, 3_000), "opaque-old"));
    }

    /// 判新主键现在是 **iat**（轮换顺序的本义）：exp 相同/死票 exp 更晚都不能翻盘；
    /// 读不出 iat 的票才退化到按 exp 比。
    #[test]
    fn ticket_newness_is_decided_by_iat_first() {
        // iat 更晚者胜，即便它的 exp 更早（TTL 变短了也是更新的轮换）
        assert!(ticket_is_newer(Some(1_000), &jwt(9_000, 10_000), Some(2_000), &jwt(1_000, 9_000)));
        // 两侧都读得出 iat 时，exp 不参与：死票 exp 更晚也赢不了
        assert!(!ticket_is_newer(
            Some(9_000),
            &jwt(1_000, 9_000),
            Some(1_000),
            &jwt(2_000, 3_000)
        ));
        // 候选读不出 iat（不透明票）：退化到 exp 比较
        assert!(ticket_is_newer(Some(3_000), "opaque-new", Some(2_000), &jwt(1_000, 9_000)));
        assert!(!ticket_is_newer(Some(1_000), "opaque-old", Some(2_000), &jwt(1_000, 9_000)));
        // 候选有 exp、在用票什么都没有：读得出的算新
        assert!(ticket_is_newer(Some(3_000), "opaque-new", None, "opaque-cur"));
        // 两侧都没 exp 且都读不出 iat：来历不明的不许赢
        assert!(!ticket_is_newer(None, "opaque-new", None, "opaque-cur"));
    }

    /// 到期时间的首选来源必须是票自己（`exp`），字段只是兜底，且秒/毫秒要归一。
    #[test]
    fn token_expiry_prefers_jwt_and_normalizes_units() {
        assert_eq!(token_expiry_ms(&jwt(1_000, 2_000)), Some(2_000_000));
        assert_eq!(token_expiry_ms("opaque"), None);
        assert_eq!(norm_ms(2_000), 2_000_000);
        assert_eq!(norm_ms(2_000_000_000_000), 2_000_000_000_000);
        // 票读不出来时退回字段
        let a = acc("opaque", Some(1_790_601_750));
        assert_eq!(expiry_ms(&a), Some(1_790_601_750_000));
    }

    /// 冷却与「该不该续」的判定：冷却看持久化的 `renew_blocked_until`；
    /// `renew_due` = 进窗口 **或已被判死**（吊销票的 exp 还在未来也必须试）。
    #[test]
    fn renew_due_and_blocked() {
        let now = 1_000_000i64;
        // 冷却期内不放行，期满放行
        let mut a = acc(&jwt(1, 2), None);
        a.renew_blocked_until = Some(now + 1);
        assert!(renew_blocked(&a, now));
        assert!(!renew_blocked(&a, now + 1));
        // 进窗口 → 该续
        let exp_ms = 1_790_601_750_000i64;
        let b = acc(&jwt(1_789_000_000, 1_790_601_750), None);
        assert!(renew_due(&b, exp_ms - 23 * 3600 * 1000));
        // 票还早，但已被上游判死 → 同样该续（否则永远轮不到它自救）
        let mut c = acc(&jwt(1_789_000_000, 1_790_601_750), None);
        c.invalidated_at = Some(now);
        assert!(!renew_due(&acc(&jwt(1_789_000_000, 1_790_601_750), None), exp_ms - 25 * 3600 * 1000));
        assert!(renew_due(&c, exp_ms - 25 * 3600 * 1000));
    }

    /// 只有 `20403` 是「这把钥匙不对」→ 换下一把候选接着试；`20404`（refresh token 已失效）
    /// 换钥匙没有意义，必须立刻停手，别对同一条死链连打三枪。
    #[test]
    fn only_device_mismatch_moves_to_the_next_identity() {
        assert!(RenewError::server(code::DEVICE_MISMATCH, "Token device not match.").wrong_device);
        assert!(!RenewError::server(code::BAD_REFRESH_TOKEN, "invalid").wrong_device);
        assert!(!RenewError::server(code::PROOF_REQUIRED, "required").wrong_device);
        assert!(!RenewError::other("请求失败".into()).wrong_device);
        for c in [
            code::DEVICE_MISMATCH,
            code::BAD_REFRESH_TOKEN,
            code::PROOF_REQUIRED,
        ] {
            assert!(RenewError::server(c, "").needs_login, "{c} 该报成需重登");
        }
        assert!(!RenewError::server("500", "busy").needs_login);
    }

    /// 候选身份必须去重、不能有空钥匙，且本机兜底那套（`device.json`）排最后 ——
    /// 桌面端签发的票占多数，先试它对，少打几次必然被拒的请求。
    #[test]
    fn candidates_are_unique_and_end_with_the_assistant_identity() {
        let dir = std::env::temp_dir().join("twa_renew_candidates");
        let _ = std::fs::create_dir_all(&dir);
        let a = acc(&jwt(1_000, 2_000), None);
        let list = candidates(&dir, &a);
        assert!(!list.is_empty());
        assert!(list
            .iter()
            .all(|c| !c.device_id.trim().is_empty() && !c.private_key_pem.trim().is_empty()));
        for (i, x) in list.iter().enumerate() {
            for y in &list[i + 1..] {
                assert!(
                    !(x.device_id == y.device_id && x.public_key_pem == y.public_key_pem),
                    "候选重复：{}",
                    x.device_id
                );
            }
        }
        let own = devicekey::load_or_create(&dir, None, None);
        assert_eq!(list.last().unwrap().device_id, own.device_id);
    }

    /// 只列「本机有哪几套设备身份、按什么顺序试」，**不打任何网络请求**、不打印私钥。
    /// `cargo test --lib -- --ignored --nocapture print_renew_candidates`
    #[test]
    #[ignore]
    fn print_renew_candidates() {
        let dir = match std::env::var("TWA_DATA_DIR") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => dirs::home_dir()
                .map(|h| h.join("Library/Application Support/cn.traework.assistant"))
                .expect("无法定位数据目录"),
        };
        for account in accounts::load_accounts(&dir) {
            println!("=== {} (uid={:?})", account.name, account.user_id);
            for (i, c) in candidates(&dir, &account).iter().enumerate() {
                println!(
                    "  #{} DeviceID={} 公钥sha={}",
                    i + 1,
                    c.device_id,
                    &format!("{:x}", sha256(c.public_key_pem.as_bytes()))[..12]
                );
            }
        }
    }

    fn sha256(bytes: &[u8]) -> impl std::fmt::LowerHex {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize()
    }

    /// 「抢不抢桌面端那条链」的判据：没 RT / 票已过期才轮到本机替它续；读不出到期算「还活着」。
    #[test]
    fn leaves_the_chain_alone_while_the_desktop_can_still_renew() {
        let now = 1_790_601_750_000i64;
        assert!(desktop_is_live(Some("rt"), Some(now + 1), now));
        // 到期时间未知 = 宁可等，不误顶（把在用的链续掉就是这次的事故本身）
        assert!(desktop_is_live(Some("rt"), None, now));
        // 票已过期 / 手里没有 refresh token = 桌面端续不动了，该我们上
        assert!(!desktop_is_live(Some("rt"), Some(now - 1), now));
        assert!(!desktop_is_live(Some("   "), Some(now + 1), now));
        assert!(!desktop_is_live(None, Some(now + 1), now));
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
