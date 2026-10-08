//! 凭证池客户端：把**本机这一批账号**整池托管到 cred-broker，续签仍在本机执行。
//!
//! ## 为什么需要它
//!
//! 官方续签是**单链轮换**：续签返回一个新的 refresh token，服务端按账号只认最新那一条。
//! 几台机器各持同一份 RT 的副本时，谁先续签谁赢，其余副本连旧 AT 一起被踢；而「已用过的
//! refresh token 再次出现」还会被判成凭证泄露，把整条链作废。
//!
//! 所以这里做的事不是「同步凭证」，而是**保证同一时刻只有一台机器在续签**。
//!
//! ## 粒度是「一整池」
//!
//! 一池一个 uuid，闸也按池给。上传 = 新建一池并把本机账号放进去；绑定 = 把别处的 uuid
//! 抄过来。**解绑只摘本机绑定，云端那一池不动**，其他机器不受影响；本机与云端重复的
//! 凭证会从本机移除，只留本机独有的。
//!
//! 上传**只在未绑定时可用**：服务端建池永远是新建，不会覆盖，所以「已绑定再上传」会在
//! 云端留下第二池 —— 本机切到新池、旧池原地不删，两台机器于是各持一把闸，正是
//! 「一池一把闸」要消灭的局面。要重新上传，先解绑。
//! ⚠️ 不要改成「一个账号一份 uuid」：那样两台机器可以各自拿着不同账号的闸、同时提交同一批
//! 账号，于是出现「一半新一半旧」这种**谁也没签错**的错状态。
//!
//! ## 一轮同步（三步握手）
//!
//! ```text
//! POST /lease  抢闸（抢到的人**同时拿到那一刻的整池**，别自己再 GET 一次）
//!        ↓     把池并进本地 → 本机执行需要做的续签
//! PUT  /       提交整池（CAS，版本 +1，顺便归还闸）
//!        ↓ 失败
//! POST /abort  释放闸 + 记一段冷却，让所有机器都停手
//! ```
//!
//! 抢闸带回的池内容**必须**用它去签：本地那份 refresh token 可能早就被别的机器换掉了。
//!
//! ## 落盘只有 uuid 一项
//!
//! 版本号、上次同步时刻、上次错误都是**运行时**才知道的东西，写进文件只会多一份可能与云端
//! 对不上的副本。`broker.json` 里只有 `pool_uuid` —— 有它就等于绑定了。

use crate::accounts::{self, Account};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 管家地址：cred-broker 的公网入口（共享网关 cloudflared → nginx → 容器:80）。
/// 容器**不在本机** —— 旧常量 `http://127.0.0.1:7003` 预设「容器与桌面端同机」，
/// 这个前提 2026-09-30 被实测推翻：没装 Docker 的机器永远打不通，症状是凭证池
/// 恒显「还没同步过」、过期票没人救。入口公开后 uuid 即一池凭证的完整权限，
/// 服务端只另有 `x-cred-actor` 审计，没有第二道鉴权 —— 别把它当秘密外发。
/// **编译期常量**，不是配置项：改地址 = 改代码 + 重新构建。
pub const BASE: &str = "https://cred-broker.sloan.dpdns.org";

/// 距上次同步超过这么久才会再问一次管家。
///
/// 太短只是白打接口（接管转发是热路径）；太长会让本地继续用别处已经轮换掉的凭证。
/// 注意它只挡「重复的整池同步」，不挡「续签」本身 —— 续签阈值是 48 小时，
/// 两分钟的窗口相对它完全可以忽略。
pub const SYNC_TTL_MS: i64 = 120_000;

const BROKER_FILE: &str = "broker.json";

// ── 状态 ──────────────────────────────────────────────────────────────────

/// 落盘配置。**只有 uuid 一项** —— 见模块头「落盘只有 uuid 一项」。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct BrokerConfig {
    pub pool_uuid: Option<String>,
}

/// 只存在于内存的运行时状态（进程重启就重来，不影响正确性）
#[derive(Default)]
struct Runtime {
    /// 上次「真的问过管家」的时刻，用于节流
    last_sync_ms: i64,
    /// 上次成功同步的时刻（界面展示「上次同步」）
    last_ok_ms: i64,
    /// 云端那一池的版本号（抢闸 / 提交时得知）
    version: Option<i64>,
    /// 上次失败的原因，成功后清空
    error: Option<String>,
    /// 正在同步（避免界面在长请求里显示「从未同步」）
    syncing: bool,
}

struct State {
    dir: PathBuf,
    cfg: BrokerConfig,
    rt: Runtime,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// 进程启动时加载一次。**只在 `lib.rs` 的 setup 里调**。
pub fn init(dir: &Path) {
    let cfg = load_config(dir);
    let _ = STATE.get_or_init(|| {
        Mutex::new(State {
            dir: dir.to_path_buf(),
            cfg,
            rt: Runtime::default(),
        })
    });
}

fn load_config(dir: &Path) -> BrokerConfig {
    let path = dir.join(BROKER_FILE);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<BrokerConfig>(&t).ok())
        .unwrap_or_default()
}

fn save_config(dir: &Path, cfg: &BrokerConfig) -> std::io::Result<()> {
    let path = dir.join(BROKER_FILE);
    let text = serde_json::to_string_pretty(cfg).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&path, text)?;
    // uuid 等于一池凭证的完整权限，文件权限收到 0600（与 accounts.json 一致）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 读一份快照（uuid + 运行时状态），**不持锁跨 await**。
fn snapshot() -> (Option<String>, Runtime) {
    let Some(lock) = STATE.get() else {
        return (None, Runtime::default());
    };
    match lock.lock() {
        Ok(s) => (s.cfg.pool_uuid.clone(), clone_runtime(&s.rt)),
        Err(_) => (None, Runtime::default()),
    }
}

/// 数据目录（`init` 时写入）。解绑要动本机账号，需要它。
fn data_dir() -> Option<PathBuf> {
    let lock = STATE.get()?;
    match lock.lock() {
        Ok(s) => Some(s.dir.clone()),
        Err(_) => None,
    }
}

fn clone_runtime(rt: &Runtime) -> Runtime {
    Runtime {
        last_sync_ms: rt.last_sync_ms,
        last_ok_ms: rt.last_ok_ms,
        version: rt.version,
        error: rt.error.clone(),
        syncing: rt.syncing,
    }
}

fn mutate<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let lock = STATE.get()?;
    match lock.lock() {
        Ok(mut s) => Some(f(&mut s)),
        Err(_) => None,
    }
}

/// 这台机器绑定到某一池了吗？**续签路径的分岔判据**（见 `commands::ensure_fresh_token`）。
pub fn bound() -> bool {
    snapshot().0.is_some()
}

/// 当前绑定的池 uuid
pub fn uuid() -> Option<String> {
    snapshot().0
}

fn set_uuid(uuid: Option<String>) {
    mutate(|s| {
        s.cfg.pool_uuid = uuid;
        let _ = save_config(&s.dir, &s.cfg);
    });
}

fn note_start() {
    mutate(|s| s.rt.syncing = true);
}

fn note_ok(version: Option<i64>) {
    mutate(|s| {
        s.rt.syncing = false;
        s.rt.error = None;
        if let Some(v) = version {
            s.rt.version = Some(v);
        }
        let now = chrono::Utc::now().timestamp_millis();
        s.rt.last_sync_ms = now;
        s.rt.last_ok_ms = now;
    });
}

fn note_touched() {
    // 被闸拒 / 被节流：也算「问过了」，否则热路径会每一跳都再打一次接口
    mutate(|s| {
        s.rt.syncing = false;
        s.rt.last_sync_ms = chrono::Utc::now().timestamp_millis();
    });
}

fn note_error(message: impl Into<String>) {
    let msg = message.into();
    mutate(|s| {
        s.rt.syncing = false;
        s.rt.error = Some(msg);
        s.rt.last_sync_ms = chrono::Utc::now().timestamp_millis();
    });
}

// ── 发给前端的形状 ────────────────────────────────────────────────────────

/// `inline`：绑定状态。前端只读它，改配置一律走 upload / link / unbind 三个动作。
#[derive(Serialize, Clone, Debug)]
pub struct BrokerStatus {
    pub bound: bool,
    pub uuid: Option<String>,
    pub version: Option<i64>,
    pub last_sync_ms: Option<i64>,
    pub last_ok_ms: Option<i64>,
    pub error: Option<String>,
    pub syncing: bool,
}

pub fn status() -> BrokerStatus {
    let (uuid, rt) = snapshot();
    BrokerStatus {
        bound: uuid.is_some(),
        uuid,
        version: rt.version,
        last_sync_ms: (rt.last_sync_ms > 0).then_some(rt.last_sync_ms),
        last_ok_ms: (rt.last_ok_ms > 0).then_some(rt.last_ok_ms),
        error: rt.error,
        syncing: rt.syncing,
    }
}

/// 上传 / 绑定的结果：**uuid 是唯一要展示给用户复制的东西**
#[derive(Serialize, Clone, Debug)]
pub struct PoolOp {
    pub uuid: String,
    pub account_count: usize,
    pub merged: usize,
    pub message: String,
}

/// 一轮同步的结论（给界面提示用）
#[derive(Serialize, Clone, Debug)]
pub struct SyncReport {
    pub changed: bool,
    pub deferred: bool,
    pub merged: usize,
    pub refreshed: usize,
    pub failed: usize,
    /// 本机**签不动**的条数（票已失效，重试也是同一结论）。它不算 `failed`，
    /// 因为它不该让整池进冷却 —— 见 `sync` 里那条分支。
    #[serde(default)]
    pub blocked: usize,
    pub version: Option<i64>,
    pub message: String,
}

// ── 条目 ──────────────────────────────────────────────────────────────────

/// 池里的一条账号凭证。字段名与 `Account` **故意不同**（`access_token` vs `token`）：
/// 这一层是跨机器的数据契约，不该随本地落盘模型改名而变。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PoolItem {
    /// 跨机身份锚点（手机号 → 昵称 → 本地 id）
    pub key: String,
    pub name: String,
    pub phone: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<i64>,
    pub rt_expires_at: Option<i64>,
    pub updated_at: Option<i64>,
}

/// 跨机认人的锚点：**手机号 → 昵称 → 本地 id**。
///
/// 手机号最稳（token 会轮换、昵称会改、本地 id 每台机器都不一样），所以排第一。
pub fn item_key_of(account: &Account) -> String {
    if let Some(p) = account.phone.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return p.to_string();
    }
    if !account.name.trim().is_empty() {
        return account.name.trim().to_string();
    }
    account.id.clone()
}

pub fn to_item(account: &Account) -> PoolItem {
    PoolItem {
        key: item_key_of(account),
        name: account.name.clone(),
        phone: account.phone.clone().unwrap_or_default(),
        access_token: account.token.clone(),
        refresh_token: account.refresh_token.clone().unwrap_or_default(),
        // 上传**推得出的**到期，而不是本地那个可能为空白的字段：`expires_at: null`
        // 到了别的机器上就是「永不续签」，而真实寿命一直写在这张票里。
        expires_at: acct_expiry(account),
        rt_expires_at: account.rt_expires_at,
        updated_at: Some(chrono::Utc::now().timestamp_millis()),
    }
}

/// 本账号的到期时间：字段优先，空了就从票里的 JWT `exp` 折算（见 `refresh::expiry_ms`）。
fn acct_expiry(account: &Account) -> Option<i64> {
    crate::refresh::expiry_ms(account.expires_at, &account.token)
}

/// 池里那一条的到期时间，判据同 [`acct_expiry`]。
///
/// 别的机器（或更早版本的我们）上传的条目可能是 `"expires_at": null`，
/// 而票是完整的 —— 那种 null 不是「永不过期」，也不是「已过期」，只是**没带上**。
fn item_expiry(item: &PoolItem) -> Option<i64> {
    crate::refresh::expiry_ms(item.expires_at, &item.access_token)
}

/// 池里的一条 → 本机新账号（本地还没有这个 key 时用）
pub fn account_from_item(item: &PoolItem) -> Account {
    let name = if item.name.trim().is_empty() {
        format!(
            "账号-{}",
            &item.key.chars().take(6).collect::<String>()
        )
    } else {
        item.name.clone()
    };
    Account {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        phone: Some(item.phone.clone()).filter(|s| !s.trim().is_empty()),
        token: item.access_token.clone(),
        refresh_token: Some(item.refresh_token.clone()).filter(|s| !s.trim().is_empty()),
        expires_at: item.expires_at,
        rt_expires_at: item.rt_expires_at,
        base_url: None,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        last: None,
        checked_today: None,
        invalidated_at: None,
        renew_blocked_until: None,
    }
}

/// 池里那一条是否**严格新于**本机这一条 —— [`adopt`] 用的判据（云端 → 本地方向）。
///
/// 两个方向必须共用同一套新序判据（这里是对称的两条：[`cloud_is_newer`] / [`local_is_newer`]）：
/// 各写一套就会出现「A 看 B 更新、B 看 A 也更新」，两台机器每轮同步都改一次整池版本、永不收敛。
///
/// 「读不出到期」不等于「更旧」：一侧读得出、另一侧读不出时，读得出的算新；
/// 两侧都读不出（不透明票又没带到期字段）就**谁也不许盖谁**，来历不明的不算更新。
fn cloud_is_newer(item: &PoolItem, account: &Account) -> bool {
    match (item_expiry(item), acct_expiry(account)) {
        (Some(p), Some(l)) => p > l,
        (Some(_), None) => true,
        _ => false,
    }
}

/// 本机这一条是否**严格新于**池里那一份 —— [`cloud_is_newer`] 的反方向，供本地 → 云端回写用。
fn local_is_newer(account: &Account, item: &PoolItem) -> bool {
    match (acct_expiry(account), item_expiry(item)) {
        (Some(l), Some(p)) => l > p,
        (Some(_), None) => true,
        _ => false,
    }
}

/// 把池里那一条采纳到本地账号上。返回是否真的改动了。
///
/// 判据有两层：
///
/// 1. **失效抢救**（最高优先级）：本机这张票已被官方接口判死（`invalidated_at` 有值，
///    见 `checkin::apply_liveness`），而池里那份与本地不同 → **无条件换血**。
///    这是 2026-10-08「本地死票锁死」事故的主修：`expires_at` 比较分不清「更新的轮换」
///    和「到期时间声明得更晚、但已被吊销的死票」，死票会永远挡住池里的活票。
///    池里若是同一张死票，换血无意义，照旧走续签。
/// 2. 常规新序：**`expires_at` 更晚**。续签之后 access token 的有效期必然往后推，
///    所以「更晚」等价于「更新的一次轮换」。反过来（池里那条更旧）绝不能覆盖 ——
///    那正是「先到的旧副本把新凭证盖回去」的来源。
///
/// 不满足「更新」时仍会补本地缺的字段（老账号没有 refresh token、没有手机号），
/// 因为那是从「不知道」变成「知道」，不存在覆盖新值的风险。
pub fn adopt(account: &mut Account, item: &PoolItem) -> bool {
    let has_token = !item.access_token.trim().is_empty() || !item.refresh_token.trim().is_empty();
    if !has_token {
        // 空 / 纯空白 token 一律忽略：采纳它等于把账号弄成登录不上
        return false;
    }
    let differs = item.access_token.trim() != account.token
        || (!item.refresh_token.trim().is_empty()
            && item.refresh_token.trim() != account.refresh_token.as_deref().unwrap_or("").trim());
    let newer = (account.invalidated_at.is_some() && differs) || cloud_is_newer(item, account);
    if !newer {
        let mut touched = false;
        if account.refresh_token.is_none() && !item.refresh_token.trim().is_empty() {
            account.refresh_token = Some(item.refresh_token.clone());
            touched = true;
        }
        // 「补空字段」也算改动：从「不知道」变成「知道」是信息增加，不是覆盖。
        // 但两侧都是 `None` 时**不算** —— 那只是把 `None` 赋给 `None`，
        // 报成改动会让调用方平白多落一次盘，同步提示里也多报一个「并入 N 个」。
        if account.expires_at.is_none() && item.expires_at.is_some() {
            account.expires_at = item.expires_at;
            touched = true;
        }
        if account.rt_expires_at.is_none() && item.rt_expires_at.is_some() {
            account.rt_expires_at = item.rt_expires_at;
            touched = true;
        }
        if account.phone.is_none() && !item.phone.trim().is_empty() {
            account.phone = Some(item.phone.clone());
            touched = true;
        }
        return touched;
    }
    if !item.access_token.trim().is_empty() {
        account.token = item.access_token.clone();
    }
    if !item.refresh_token.trim().is_empty() {
        account.refresh_token = Some(item.refresh_token.clone());
    }
    account.expires_at = item
        .expires_at
        .or_else(|| item_expiry(item))
        .or(account.expires_at);
    account.rt_expires_at = item.rt_expires_at.or(account.rt_expires_at);
    // 新票在手：本机的活性/冷却标记一并清掉 —— 换血本身就是对死锁的解除
    account.invalidated_at = None;
    account.renew_blocked_until = None;
    if account.phone.is_none() && !item.phone.trim().is_empty() {
        account.phone = Some(item.phone.clone());
    }
    if account.name.trim().is_empty() && !item.name.trim().is_empty() {
        account.name = item.name.clone();
    }
    true
}

/// 把池里那份并进本机账号：**并集** —— 本地独有的账号一个都不删。
/// 返回改动的条目数。
pub fn merge_into(accounts: &mut Vec<Account>, items: &[PoolItem]) -> usize {
    let mut changed = 0;
    for item in items {
        if item.key.trim().is_empty() {
            continue;
        }
        match accounts
            .iter_mut()
            .find(|a| item_key_of(a) == item.key.trim())
        {
            Some(a) => {
                if adopt(a, item) {
                    changed += 1;
                }
            }
            None => {
                accounts.push(account_from_item(item));
                changed += 1;
            }
        }
    }
    changed
}

/// 这张本地票**允许回写**云端吗（sync ④ 的守卫）。
///
/// 已被官方接口判死（`invalidated_at` 有值）的票绝不进池：`expires_at` 比较分不清
/// 「更新的轮换」和「到期时间声明得更晚、但已被吊销的死票」，放行会把死票盖在活票上、
/// 污染所有绑池机器（2026-10-08 事故里最危险的一半）。
fn write_back_allowed(account: &Account) -> bool {
    account.invalidated_at.is_none()
}

/// 提交给管家的整池内容 = **云端那份 ∪ 本机账号**。
///
/// 不能只拿云端那份起步：`PUT` 是**整池替换**，漏掉「本机有、池里没有」的账号，就等于
/// 把并集悄悄缩回云端那一份 —— 两台机器各绑同一池时，池里会永远只有第一个上传者的账号，
/// 另一台的账号只进本地、不出本地。
pub fn union_pool(cloud: &[PoolItem], local: &[Account]) -> Vec<PoolItem> {
    let mut pool: Vec<PoolItem> = cloud.to_vec();
    for acct in local {
        let key = item_key_of(acct);
        if key.trim().is_empty() {
            continue;
        }
        if !pool.iter().any(|i| i.key.trim() == key) {
            pool.push(to_item(acct));
        }
    }
    pool
}

// ── HTTP ──────────────────────────────────────────────────────────────────

/// 失败的三态。**必须分开**，否则「池被删」会被当成「网络抖动」，
/// 而前者要求用户重新绑定、后者只要求保持本地凭证继续用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 打不通 / 超时 / 5xx：保持本地凭证继续用，不打扰用户
    Unreachable,
    /// 池不存在（404 gone）：池在别处被解绑了，
    /// **必须清掉本地 uuid** 并让用户重新上传或绑定
    Gone,
    /// 请求被明确拒绝（其它 4xx）：多半是版本对不上，属于要修的错
    Denied,
}

fn classify(status: u16, body: &str) -> Option<FailKind> {
    if (200..300).contains(&status) {
        return None;
    }
    if status == 404 && body.contains("gone") {
        return Some(FailKind::Gone);
    }
    if status >= 500 {
        return Some(FailKind::Unreachable);
    }
    Some(FailKind::Denied)
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(crate::http::TIMEOUT)
        .user_agent(format!("{}-broker", crate::http::UA))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))
}

/// 这台机器的标识。**只进服务端的审计日志，不参与任何判断** ——
/// 所以它是什么名字都不影响正确性，拿不到就用 hostname 兜底。
fn actor() -> String {
    static ACTOR: OnceLock<String> = OnceLock::new();
    ACTOR
        .get_or_init(|| {
            for key in ["HOSTNAME", "COMPUTERNAME"] {
                if let Ok(v) = std::env::var(key) {
                    let v = v.trim();
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
            if let Ok(o) = std::process::Command::new("hostname").output() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            "unknown".to_string()
        })
        .clone()
}

async fn request(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, (FailKind, String)> {
    let c = client().map_err(|e| (FailKind::Unreachable, e))?;
    let url = format!("{BASE}{path}");
    let mut req = c.request(method, &url).header("x-cred-actor", actor());
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (FailKind::Unreachable, format!("连接凭证管家失败：{e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if let Some(kind) = classify(status, &text) {
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("error"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(120).collect());
        return Err((kind, format!("凭证管家返回 HTTP {status}：{detail}")));
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|_| (FailKind::Denied, "凭证管家返回的不是 JSON".to_string()))
}

#[derive(Deserialize, Debug)]
struct LeaseResp {
    #[serde(default)]
    granted: bool,
    #[serde(default)]
    items: Vec<PoolItem>,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    retry_after: i64,
    #[serde(default)]
    lease_owner: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CreateResp {
    uuid: String,
    #[serde(default)]
    version: i64,
}

#[derive(Deserialize, Debug)]
struct VersionResp {
    #[serde(default)]
    version: i64,
}

/// 把 `(FailKind, msg)` 变成给用户看的一句话。`Gone` 额外清掉本地 uuid。
fn on_failure(kind: FailKind, message: String) -> String {
    match kind {
        FailKind::Gone => {
            set_uuid(None);
            "云端那一池已经不存在了（可能已被清理），本机已退回单机运行，请重新上传或绑定"
                .to_string()
        }
        FailKind::Unreachable => format!("{message}（本次继续用本机凭证）"),
        FailKind::Denied => message,
    }
}

// ── 动作 ──────────────────────────────────────────────────────────────────

/// 已绑定就不许再建新池。
///
/// 规则单独拎成纯函数，是为了能直接钉住它 —— 真实 `upload` 会打接口，
/// 而这条规则必须在**发出任何请求之前**生效。
fn upload_guard(already_bound: bool) -> Result<(), String> {
    if already_bound {
        return Err(
            "本机已经绑定了云端凭证池。要重新上传，请先解绑（解绑只摘本机绑定，云端那一池保留）。"
                .to_string(),
        );
    }
    Ok(())
}

/// 把本机这一批账号整体上传：管家颁发一串 uuid 并当场绑定。
///
/// 这是**整台机器**的动作，不是「某个账号」的 —— 所以不收账号参数。
///
/// ⚠️ **已绑定时直接拒绝**（见 [`upload_guard`]）：服务端建池永远是新建、不会覆盖，
/// 放行一次就会在云端留下第二池，让两台机器各持一把闸。
pub async fn upload(dir: &Path) -> Result<PoolOp, String> {
    upload_guard(bound())?;
    let accounts = accounts::load_accounts(dir);
    if accounts.is_empty() {
        return Err("本机还没有账号，先「导入本机账号」或「登录新账号」".to_string());
    }
    let items: Vec<PoolItem> = accounts.iter().map(to_item).collect();
    let v = request(
        reqwest::Method::POST,
        "/v1/pool",
        Some(serde_json::json!({ "items": items })),
    )
    .await
    .map_err(|(k, m)| on_failure(k, m))?;
    let created: CreateResp = serde_json::from_value(v)
        .map_err(|e| format!("凭证管家返回的建池结果看不懂：{e}"))?;

    set_uuid(Some(created.uuid.clone()));
    mutate(|s| {
        s.rt.version = Some(created.version);
        s.rt.error = None;
    });
    note_ok(Some(created.version));

    Ok(PoolOp {
        uuid: created.uuid,
        account_count: items.len(),
        merged: 0,
        message: format!("已把本机的 {} 个账号放上云端", items.len()),
    })
}

/// 绑定别处复制过来的 uuid：先验证这一池真的存在，再并进本地。
///
/// 先验证是必要的防呆：把一串打不通的 uuid 写进本地配置，之后每次同步都失败，
/// 而用户以为已经绑好了。
pub async fn link(dir: &Path, raw_uuid: &str) -> Result<PoolOp, String> {
    let uuid = raw_uuid.trim();
    if uuid.is_empty() {
        return Err("请先粘贴云端凭证池的 uuid".to_string());
    }
    // ⚠️ **不要复用 `on_failure`**：`Gone` 分支里有 `set_uuid(None)`，而这里本机可能
    // 正绑在**另一池**上（用户就是想换过去）。抄错几位 uuid 就把正在用的绑定清掉，
    // 等于把一次打错字升级成一次服务中断。绑定失败就原样失败，本地绑定一动不动。
    let v = match request(reqwest::Method::GET, &format!("/v1/pool/{uuid}"), None).await {
        Ok(v) => v,
        Err((FailKind::Gone, _)) => {
            return Err(
                "云端没有这一池：uuid 可能抄错了，或者它已不存在。\
                 本机的绑定没有变动。"
                    .to_string(),
            )
        }
        Err((_, m)) => return Err(format!("绑定失败，本机的绑定没有变动：{m}")),
    };
    let items: Vec<PoolItem> = v
        .get("items")
        .cloned()
        .and_then(|x| serde_json::from_value(x).ok())
        .unwrap_or_default();
    let version = v.get("version").and_then(|x| x.as_i64());

    set_uuid(Some(uuid.to_string()));
    mutate(|s| {
        s.rt.version = version;
        s.rt.error = None;
        s.rt.last_sync_ms = 0; // 下一次调用立刻做一轮完整同步
    });

    let mut accounts = accounts::load_accounts(dir);
    let merged = merge_into(&mut accounts, &items);
    accounts::save_accounts(dir, &accounts).map_err(|e| e.to_string())?;

    // 绑定后立刻整池同步一轮：把「本地独有的账号」也推上云（并集才是这一池的真相）。
    // 失败不影响绑定本身 —— 已经绑上了，下一跳还会再试。
    let sync_note = match sync(dir, true).await {
        Ok(r) => r.message,
        Err(e) => format!("已绑定，但首次同步未完成：{e}"),
    };

    Ok(PoolOp {
        uuid: uuid.to_string(),
        account_count: items.len(),
        merged,
        message: sync_note,
    })
}

/// 解绑：**只摘掉本地 uuid，云端那一池不动**。
///
/// 同时把本机与云端重复的账号（token）从本地移除 —— 凭证已托管在云端，本机不再持有副本，
/// 免得解绑后本机单机续签把云端那条链轮换掉；本机独有的账号（云端没有的）原样保留。
///
/// 云端那一池还在时，必须先拿到池内容才知道哪些是重复的：拿不到（网络不可达 / 被拒绝）
/// 就返回 Err 且**保留本地绑定** —— 静默只摘 uuid，会让本机继续持有一批与云端相同的凭证，
/// 而用户以为自己已经解绑了。`gone`（池已不存在）例外：云端都没了，本机没有「与云端
/// 相同」的东西，全部保留、照常解绑。
pub async fn unbind() -> Result<BrokerStatus, String> {
    let Some(uuid) = uuid() else {
        return Ok(status());
    };
    let cloud: Vec<PoolItem> = match request(reqwest::Method::GET, &format!("/v1/pool/{uuid}"), None).await {
        Ok(v) => v
            .get("items")
            .cloned()
            .and_then(|x| serde_json::from_value(x).ok())
            .unwrap_or_default(),
        // 池早就不在了：本机没有「与云端相同」的凭证，全部保留、照常解绑
        Err((FailKind::Gone, _)) => Vec::new(),
        Err((_, m)) => {
            return Err(format!("没拿到云端那一池，本机绑定保持不变：{m}"));
        }
    };
    let dir = data_dir().ok_or_else(|| "凭证池状态未初始化".to_string())?;

    let cloud_keys: HashSet<String> = cloud
        .iter()
        .map(|i| i.key.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    let mut accounts = accounts::load_accounts(&dir);
    let before = accounts.len();
    accounts.retain(|a| !cloud_keys.contains(item_key_of(a).trim()));
    if accounts.len() != before {
        // 落盘失败就整体拒绝：摘了 uuid 但账号没删掉，等于静默失败，让用户以为已经解绑了
        accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    }

    set_uuid(None);
    mutate(|s| {
        s.rt = Runtime::default();
    });
    Ok(status())
}

/// 一轮完整同步（三步握手）。**未绑定、被节流、抢不到闸都是正常返回**，不是错误。
pub async fn sync(dir: &Path, force: bool) -> Result<SyncReport, String> {
    let Some(uuid) = uuid() else {
        return Ok(SyncReport {
            changed: false,
            deferred: true,
            merged: 0,
            refreshed: 0,
            failed: 0,
            blocked: 0,
            version: None,
            message: "本机未绑定凭证池".to_string(),
        });
    };

    let now = chrono::Utc::now().timestamp_millis();
    let (_, rt) = snapshot();
    if !force && now - rt.last_sync_ms < SYNC_TTL_MS {
        return Ok(SyncReport {
            changed: false,
            deferred: true,
            merged: 0,
            refreshed: 0,
            failed: 0,
            blocked: 0,
            version: rt.version,
            message: "距上次同步不到两分钟，这次跳过".to_string(),
        });
    }

    note_start();
    let lease_value = match request(
        reqwest::Method::POST,
        &format!("/v1/pool/{uuid}/lease"),
        Some(serde_json::json!({})),
    )
    .await
    {
        Ok(v) => v,
        Err((kind, m)) => {
            let msg = on_failure(kind, m);
            note_error(msg.clone());
            return Err(msg);
        }
    };
    let lease: LeaseResp = serde_json::from_value(lease_value)
        .map_err(|e| format!("凭证管家的抢闸结果看不懂：{e}"))?;

    if !lease.granted {
        // 别的机器正在签。**不更新本地凭证**，但记下「问过了」，
        // 否则热路径每一跳都会再打一次接口。
        note_touched();
        // 「冷却中」比「闸在别人手里」更需要说清楚：前者意味着整池都在等，
        // 后者下一秒可能就好了
        let cooldown_s = (lease.retry_after - chrono::Utc::now().timestamp_millis()) / 1000;
        let message = if cooldown_s > 0 {
            format!("整池在冷静期（还有 {cooldown_s}s），本次跳过")
        } else {
            let owner = lease.lease_owner.unwrap_or_else(|| "另一台机器".to_string());
            format!("{owner} 正在续签，本次跳过")
        };
        return Ok(SyncReport {
            changed: false,
            deferred: true,
            merged: 0,
            refreshed: 0,
            failed: 0,
            blocked: 0,
            version: rt.version,
            message,
        });
    }

    let version = lease.version;

    // ① 池并进本地（并集，本地独有的保留）
    let mut accounts = accounts::load_accounts(dir);
    let merged = merge_into(&mut accounts, &lease.items);

    // ② 本机执行需要做的续签。**只用抢闸带回来的那一份**做判断：
    //    本地那份可能早就被别的机器换掉了。
    //
    //    提交的初值取「云端 ∪ 本机」：`PUT` 是**整池替换**，若从 `lease.items` 起步，
    //    本机独有的账号就永远进不了正文，等于每同步一次都把并集缩回云端那份。
    let mut pool = union_pool(&lease.items, &accounts);
    let mut refreshed = 0usize;
    let mut failed = 0usize;
    let mut blocked = 0usize;
    for acct in accounts.iter_mut() {
        // 「该续」= 进了续签窗口，**或已被上游判死**（吊销票的到期时间还在未来，
        // 按窗口判断它永远轮不到续，而续签是它唯一的自救通道）。
        // 冷却中的账号直接算 blocked：退避已随账号落盘，重启也不会失忆。
        if acct.refresh_token.is_none()
            || (!crate::refresh::refresh_due(acct.expires_at, &acct.token, now)
                && acct.invalidated_at.is_none())
        {
            continue;
        }
        if crate::refresh::renew_blocked(acct, now) {
            blocked += 1;
            continue;
        }
        match crate::commands::refresh_account_in_place(acct).await {
            Ok(()) => {
                refreshed += 1;
                // 把新的凭证写回池里那一份（保持并集：池里别的条目原样带回去）
                if let Some(slot) = pool.iter_mut().find(|i| i.key.trim() == item_key_of(acct)) {
                    *slot = to_item(acct);
                } else {
                    pool.push(to_item(acct));
                }
            }
            // 这条链本机续不动了（票已失效 / 需要重登）。它**不该拖停整池**：
            // 重试一千次也是同一句答复，而冷却是按整池算的 —— 一个等着重登的账号
            // 会把别的还能救的账号一起挡在续签窗口外，最后表现成「一池集体过期」。
            Err(e) if e.dead => {
                blocked += 1;
                crate::scheduler::log_event(
                    dir,
                    &format!("这条链已死，跳过它（不拖停整池）：{}（{e}）", acct.name),
                );
            }
            Err(e) => {
                failed += 1;
                crate::scheduler::log_event(
                    dir,
                    &format!("整池续签失败：{}（{e}）", acct.name),
                );
            }
        }
    }
    accounts::save_accounts(dir, &accounts).map_err(|e| e.to_string())?;

    // ③ 有失败就 abort（记冷却，让所有机器都停手），没有才提交
    if failed > 0 {
        let note = format!("{failed} 个账号续签失败");
        let _ = request(
            reqwest::Method::POST,
            &format!("/v1/pool/{uuid}/abort"),
            Some(serde_json::json!({ "note": note })),
        )
        .await;
        note_error(format!("{note}，已让管家进入冷静期"));
        return Ok(SyncReport {
            changed: merged > 0 || refreshed > 0,
            deferred: false,
            merged,
            refreshed,
            failed,
            blocked,
            version: Some(version),
            message: format!(
                "{refreshed} 个账号续签成功、{failed} 个失败；已暂停整池续签一会儿{}",
                if blocked > 0 {
                    format!("（另有 {blocked} 个是票已失效、重试没用的）")
                } else {
                    String::new()
                }
            ),
        });
    }

    // ④ 本地 → 云端方向：把**本机比池里更新**的票回写进池。上面只回写「本轮续出来的」，
    //    这一步兜住「早就换过新票、但从没被传上去」的 —— 最典型：绑池期间在本机重新登录了一次
    //    （accounts.json 里已是新票，云端还停在旧票，别的机器每轮拿到的都是过期票）。
    //    判据与云端 → 本地方向对称（[`local_is_newer`]），只有严格更新才回写。
    let mut uploaded = 0usize;
    for acct in accounts.iter() {
        // **已被上游判死的票绝不回写**（见 [`write_back_allowed`]）
        if !write_back_allowed(acct) {
            continue;
        }
        let key = item_key_of(acct);
        if key.trim().is_empty() {
            continue;
        }
        let newer = pool
            .iter()
            .find(|i| i.key.trim() == key)
            .is_some_and(|item| local_is_newer(acct, item));
        if newer {
            if let Some(slot) = pool.iter_mut().find(|i| i.key.trim() == key) {
                *slot = to_item(acct);
            }
            uploaded += 1;
        }
    }

    // 提交整池：CAS 由服务端做（`version = ? AND lease_owner = ?`），新版本号就在响应里。
    // **不要再补一次 GET 去问版本** —— 那是热路径上白打的一趟接口。
    let next_version = match request(
        reqwest::Method::PUT,
        &format!("/v1/pool/{uuid}"),
        Some(serde_json::json!({ "version": version, "items": pool })),
    )
    .await
    {
        Ok(v) => serde_json::from_value::<VersionResp>(v)
            .ok()
            .map(|r| r.version)
            .or(Some(version + 1)),
        Err((FailKind::Gone, _)) => return Err(on_failure(FailKind::Gone, String::new())),
        Err((kind, m)) => {
            // 提交失败不改本地：本机的续签结果已经落盘了，下一次同步再推上去。
            // 服务端那份还是旧版本 → 下次抢闸拿到它，接着重走一遍。
            note_error(format!("整池提交失败：{m}"));
            return Err(match kind {
                FailKind::Unreachable => format!("整池提交失败（本机凭证已更新）：{m}"),
                _ => m,
            });
        }
    };
    note_ok(next_version);

    let upload_note = if uploaded > 0 {
        format!("，回写 {uploaded} 个更新的本地票")
    } else {
        String::new()
    };
    Ok(SyncReport {
        changed: merged > 0 || refreshed > 0 || uploaded > 0,
        deferred: false,
        merged,
        refreshed,
        failed: 0,
        blocked,
        version: next_version,
        message: if blocked > 0 {
            format!("整池已同步（并入 {merged} 个，续签 {refreshed} 个{upload_note}；{blocked} 个已死掉、需要重新登录）")
        } else {
            format!("整池已同步（并入 {merged} 个，续签 {refreshed} 个{upload_note}）")
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, phone: Option<&str>, token: &str) -> Account {
        Account {
            id: format!("id-{name}"),
            name: name.into(),
            phone: phone.map(str::to_string),
            token: token.into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            last: None,
            checked_today: None,
            invalidated_at: None,
            renew_blocked_until: None,
        }
    }

    fn item(key: &str, token: &str, expires: Option<i64>) -> PoolItem {
        PoolItem {
            key: key.into(),
            name: key.into(),
            phone: key.into(),
            access_token: token.into(),
            refresh_token: format!("rt-{token}"),
            expires_at: expires,
            rt_expires_at: expires.map(|e| e + 1),
            updated_at: Some(1),
        }
    }

    // ── 身份锚点 ────────────────────────────────────────────────────────

    #[test]
    fn key_prefers_phone_then_name_then_local_id() {
        assert_eq!(item_key_of(&acct("n", Some("138"), "t")), "138");
        assert_eq!(item_key_of(&acct("n", None, "t")), "n");
        assert_eq!(item_key_of(&acct("", None, "t")), "id-");
    }

    #[test]
    fn blank_phone_falls_through_to_name() {
        assert_eq!(item_key_of(&acct("n", Some("   "), "t")), "n");
    }

    // ── 采纳规则 ────────────────────────────────────────────────────────

    #[test]
    fn adopts_the_newer_rotation() {
        let mut a = acct("n", Some("138"), "old");
        a.expires_at = Some(1_000);
        assert!(adopt(&mut a, &item("138", "new", Some(2_000))));
        assert_eq!(a.token, "new");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(a.expires_at, Some(2_000));
    }

    // ── 失效抢救（2026-10-08「本地死票锁死」事故的主修）─────────────────

    /// 本机已被官方判死（invalidated_at）+ 池里那份与本地不同 → **无条件换血**，
    /// 不再比 expires_at：死票的到期时间声明得更晚时，常规判据会让它永远挡住池里的活票。
    #[test]
    fn a_marked_dead_local_token_is_rescued_from_the_pool() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.invalidated_at = Some(5_000);
        // 池里那张 expires_at 更早（别处更早一轮的活票）—— exp 判据会拒绝，活性规则必须放行
        let it = item("138", "pool-live", Some(1_000));
        assert!(adopt(&mut a, &it), "失效抢救不受 expires_at 判据约束");
        assert_eq!(a.token, "pool-live");
        // 换血成功后活性标记清除
        assert_eq!(a.invalidated_at, None);
        assert_eq!(a.renew_blocked_until, None);
    }

    /// 池里若是**同一张**死票（token 与 refresh token 都相同），换血无意义：
    /// 常规判据（expires_at 不更晚）也过不了 → 不采纳，照旧走续签自救。
    #[test]
    fn the_same_dead_token_in_the_pool_is_not_adopted() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.invalidated_at = Some(5_000);
        a.refresh_token = Some("rt-dead-local".into());
        a.expires_at = Some(2_000);
        a.rt_expires_at = Some(2_001);
        assert!(!adopt(&mut a, &item("138", "dead-local", Some(1_000))));
        assert_eq!(a.invalidated_at, Some(5_000), "没换血就不清标记");
    }

    /// 已被判死的本地票**永不回写云端**（sync ④ 的守卫 [`write_back_allowed`]）：
    /// expires_at 更晚的死票一旦回写，会把活票盖掉、污染所有绑池机器。
    #[test]
    fn local_is_newer_refuses_to_overwrite_the_pool_with_a_dead_token() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.invalidated_at = Some(5_000);
        a.expires_at = Some(2_000);
        let it = item("138", "pool-live", Some(1_000));
        // 没有守卫时，expires_at 更晚的死票会被判成「本地更新」——守卫是必需的
        assert!(local_is_newer(&a, &it));
        assert!(!write_back_allowed(&a), "判死的票不许参与本地→云端回写");
        a.invalidated_at = None;
        assert!(write_back_allowed(&a));
    }

    #[test]
    fn never_lets_an_older_copy_win() {
        // 这正是「点刷新积分反而退回去了」那一类症状在凭证上的翻版：
        // 库里那条是更旧的一次轮换，采纳它等于把刚换到的凭证作废。
        // 本地字段都齐（没有「空」可补），所以这一次必须原样返回 false。
        let mut a = acct("n", Some("138"), "fresh");
        a.expires_at = Some(9_000);
        a.rt_expires_at = Some(9_001);
        a.refresh_token = Some("rt-fresh".into());
        let stale = item("138", "stale", Some(1_000));
        assert!(!adopt(&mut a, &stale));
        assert_eq!(a.token, "fresh", "更旧的那次轮换绝不能覆盖");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-fresh"));
        assert_eq!(a.expires_at, Some(9_000));
        assert_eq!(a.rt_expires_at, Some(9_001));
    }

    #[test]
    fn unknown_local_expiry_is_treated_as_older() {
        // 本地「不知道」→ 库里「知道」，那是信息增加，不是覆盖新值
        let mut a = acct("n", Some("138"), "local");
        assert!(adopt(&mut a, &item("138", "cloud", Some(1))));
        assert_eq!(a.token, "cloud");
    }

    #[test]
    fn copies_only_the_gaps_when_not_newer() {
        let mut a = acct("n", None, "local");
        a.expires_at = Some(9_000);
        assert!(adopt(&mut a, &item("138", "older", Some(1_000))), "补空字段也算改动");
        assert_eq!(a.token, "local", "凭证不动");
        assert_eq!(a.phone.as_deref(), Some("138"), "本地缺的手机号补上");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-older"), "本地没有 RT 时补上");
        assert_eq!(a.rt_expires_at, Some(1_001));
    }

    #[test]
    fn blank_tokens_are_ignored_entirely() {
        let mut a = acct("n", Some("138"), "keep");
        let mut empty = item("138", "   ", Some(99_999));
        empty.refresh_token = String::new();
        assert!(!adopt(&mut a, &empty));
        assert_eq!(a.token, "keep");
    }

    #[test]
    fn two_unknown_expiries_do_not_count_as_a_change() {
        // 本地字段都齐、两侧过期时间又都「不知道」时，一个字段都不该动：
        // 把 `None` 赋成 `None` 报成改动，会让调用方平白多落一次盘、
        // 同步提示里也多报一个「并入 N 个」。
        let mut a = acct("n", Some("138"), "keep");
        a.refresh_token = Some("rt-keep".into());
        // item 的 `expires_at` 为 None 时，`rt_expires_at` 也是 None（见本模块的 `item`）
        let blank = item("138", "older", None);
        assert!(!adopt(&mut a, &blank));
        assert_eq!(a.token, "keep");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-keep"));
    }

    /// 两侧字段都是 null、票却读得出到期时，**必须**比出个新旧。
    ///
    /// 以前云端那条 `expires_at: null` 一律判成「不更新」，本机就永远收养不到别的机器
    /// 换到的新链；而上传的那一份又带着 null 下去，下一台机器同样读不出到期。
    #[test]
    fn null_expiry_rows_are_still_compared_by_the_ticket() {
        let jwt = signed_jwt_with_exp(1_800_000_000);
        let mut a = acct("n", Some("138"), &jwt);
        a.refresh_token = Some("rt-local".into());
        let mut cloud = item("138", &signed_jwt_with_exp(1_800_003_600), None);
        cloud.expires_at = None;
        assert!(adopt(&mut a, &cloud), "两边字段都空，也要按票里的 exp 判新旧");
        assert!(a.token.starts_with("eyJ"), "采纳的是云端那张票");
        // 采纳之后本地必须有一个**能拿来判下次续签**的到期时间，不能还是 null
        assert_eq!(a.expires_at, Some(1_800_003_600_000));
        // 上传时同理：票读得出到期就不该交 null 上去
        let mut blank = acct("m", Some("139"), &signed_jwt_with_exp(1_800_007_200));
        blank.expires_at = None;
        assert_eq!(to_item(&blank).expires_at, Some(1_800_007_200_000));
    }

    /// 拼一张本项目真实形态的票（三段式 JWT，载荷里 `exp` 是**秒**）。
    fn signed_jwt_with_exp(exp: i64) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let payload = format!(r#"{{"iss":"https://www.workbuddy.cn","exp":{exp}}}"#);
        let b64 = STANDARD
            .encode(payload)
            .replace('+', "-")
            .replace('/', "_")
            .trim_end_matches('=')
            .to_string();
        format!("eyJhbGciOiJSUzI1NiJ9.{b64}.sig")
    }

    // ── 并集 ────────────────────────────────────────────────────────────

    #[test]
    fn merge_is_a_union_and_never_drops_local_only_accounts() {
        let mut accs = vec![acct("本地独有", Some("111"), "t1")];
        let changed = merge_into(
            &mut accs,
            &[item("111", "t1-new", Some(2_000)), item("222", "t2", Some(1))],
        );
        assert_eq!(accs.len(), 2, "本地独有的一个都不许删");
        assert_eq!(changed, 2, "补一个 + 新增一个");
        assert_eq!(accs[0].token, "t1-new");
        assert_eq!(accs[1].phone.as_deref(), Some("222"));
    }

    #[test]
    fn merge_skips_items_without_a_key() {
        let mut accs = vec![acct("a", Some("111"), "t1")];
        let mut keyless = item("", "t", Some(5));
        keyless.key = String::new();
        assert_eq!(merge_into(&mut accs, &[keyless]), 0);
        assert_eq!(accs.len(), 1);
    }

    #[test]
    fn merge_round_trips_through_item_and_back() {
        let original = {
            let mut a = acct("waxiloao", Some("19098779775"), "at");
            a.expires_at = Some(1_760_000_000_000);
            a.rt_expires_at = Some(1_770_000_000_000);
            a.refresh_token = Some("rt".into());
            a
        };
        let as_item = to_item(&original);
        assert_eq!(as_item.key, "19098779775");
        assert_eq!(as_item.access_token, "at");

        // 另一台机器收到这一条：本地没有 → 建一个新账号，字段逐项相等
        let born = account_from_item(&as_item);
        assert_eq!(born.token, "at");
        assert_eq!(born.refresh_token.as_deref(), Some("rt"));
        assert_eq!(born.expires_at, Some(1_760_000_000_000));
        assert_eq!(born.rt_expires_at, Some(1_770_000_000_000));
        assert_eq!(item_key_of(&born), "19098779775", "两机认的是同一个 key");
        assert_eq!(born.name, "waxiloao");
    }

    #[test]
    fn account_from_item_falls_back_to_a_generated_name() {
        let mut nameless = item("13800000000", "t", Some(1));
        nameless.name = String::new();
        assert_eq!(account_from_item(&nameless).name, "账号-138000");
    }

    #[test]
    fn account_from_item_without_phone_leaves_it_empty_instead_of_blank_string() {
        let mut no_phone = item("x", "t", Some(1));
        no_phone.phone = String::new();
        assert_eq!(account_from_item(&no_phone).phone, None);
    }

    // ── 提交正文必须带全本机账号 ────────────────────────────────────────
    //
    // 「绑定后新增的账号会自动上云」这条承诺就落在这里：`PUT` 是整池替换，
    // 正文里少一个 key，云端那一池就少一个账号。

    #[test]
    fn union_pool_carries_local_only_accounts_into_the_body() {
        let cloud = vec![item("111", "cloud", Some(10))];
        let local = vec![
            acct("交集", Some("111"), "local-a"),
            acct("本机新增", Some("222"), "local-b"),
        ];
        let pool = union_pool(&cloud, &local);
        assert_eq!(pool.len(), 2, "本机新增的账号必须进正文");
        assert_eq!(pool[1].key, "222");
        assert_eq!(pool[1].access_token, "local-b");
    }

    #[test]
    fn union_pool_keeps_the_leased_copy_for_a_shared_key() {
        // 同名 key 保留**闸带回来的**那一条：闸里那份是云端的最新轮换，
        // 本机那份可能在别处已经被换掉了。真正需要覆盖时由续签循环写回。
        let cloud = vec![item("111", "leased", Some(10))];
        let local = vec![acct("交集", Some("111"), "stale-local")];
        let pool = union_pool(&cloud, &local);
        assert_eq!(pool.len(), 1, "同一个 key 不许出现两条");
        assert_eq!(pool[0].access_token, "leased");
    }

    #[test]
    fn union_pool_of_an_empty_cloud_is_just_the_local_accounts() {
        let local = vec![acct("a", Some("1"), "t1"), acct("b", Some("2"), "t2")];
        assert_eq!(union_pool(&[], &local).len(), 2);
    }

    // ── 建池的唯一入口约束 ──────────────────────────────────────────────

    #[test]
    fn upload_is_refused_once_bound() {
        // 服务端建池只会新建、不会覆盖 ⇒ 放行一次就多一池、多一把闸，
        // 于是两台机器能同时续签。错误里必须说清出路（先解绑）。
        assert!(upload_guard(false).is_ok());
        let err = upload_guard(true).unwrap_err();
        assert!(err.contains("先解绑"), "错误文案要给出路：{err}");
    }

    // ── 失败分类 ────────────────────────────────────────────────────────

    #[test]
    fn classifies_the_three_failure_kinds() {
        assert_eq!(classify(200, "{}"), None);
        assert_eq!(classify(201, "{}"), None);
        assert_eq!(classify(404, r#"{"error":"gone"}"#), Some(FailKind::Gone));
        assert_eq!(classify(500, "boom"), Some(FailKind::Unreachable));
        assert_eq!(classify(502, "bad gateway"), Some(FailKind::Unreachable));
        assert_eq!(classify(400, r#"{"error":"bad_uuid"}"#), Some(FailKind::Denied));
        assert_eq!(classify(409, r#"{"error":"stale"}"#), Some(FailKind::Denied));
        // 404 但不是 gone（比如路径写错）不该被当成「池被删」
        assert_eq!(classify(404, r#"{"error":"not_found"}"#), Some(FailKind::Denied));
    }

    #[test]
    fn config_file_only_carries_the_uuid() {
        let cfg = BrokerConfig {
            pool_uuid: Some("abc".into()),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("pool_uuid"));
        assert!(!json.contains("version"), "版本号是运行时状态，不该落盘");

        // 旧版本残留的未知字段要能读进来而不是整份报废
        let restored: BrokerConfig =
            serde_json::from_str(r#"{"pool_uuid":"abc","enabled":true,"url":"x"}"#).unwrap();
        assert_eq!(restored.pool_uuid.as_deref(), Some("abc"));
    }

    /// 真实接口冒烟：上传一池 → 整池同步 → 解绑（云端保留），跑完手动把测试池删干净。
    ///
    /// **这是唯一能验证「客户端与服务端的字段名、响应形状真的对得上」的手段** ——
    /// 两边各写一套类型，字段名写错时单测全绿、跑起来永远 deferred。
    /// token 用的是假的，所以不会碰到任何真实凭证，也不会去打官方续签接口。
    ///
    /// 运行（需要走代理）：
    /// ```text
    /// HTTPS_PROXY=http://127.0.0.1:7897 cargo test --lib -- --ignored broker::tests::smoke --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn smoke_real_broker_roundtrip() {
        let dir = std::env::temp_dir().join("cred-broker-smoke");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::create_dir_all(dir.join("logs")).ok();

        // 有效期放到 60 天后：`should_refresh` 会判成「不用续」，于是这一趟只验证协议，
        // 不会真的拿假 token 去打官方接口（那必然失败并让整池进冷静期）
        let far_future = chrono::Utc::now().timestamp_millis() + 60 * 24 * 3600 * 1000;
        let mut fake = acct("冒烟测试", Some("13900000000"), "smoke-at");
        fake.refresh_token = Some("smoke-rt".into());
        fake.expires_at = Some(far_future);
        fake.rt_expires_at = Some(far_future);
        accounts::save_accounts(&dir, &[fake]).expect("写测试账号");

        init(&dir);
        assert!(!bound(), "测试开始前不该是绑定的");

        let op = upload(&dir).await.expect("上传失败");
        println!("[冒烟] 上传成功：uuid={} 条数={} 说明={}", op.uuid, op.account_count, op.message);
        assert!(bound());
        assert_eq!(op.account_count, 1);

        let rep = sync(&dir, true).await.expect("整池同步失败");
        println!("[冒烟] 同步：{}（deferred={}）", rep.message, rep.deferred);
        assert!(!rep.deferred, "强制同步不该被跳过");
        assert!(rep.version.is_some(), "同步后应拿到云端版本号");

        let st = status();
        println!(
            "[冒烟] 状态：version={:?} last_ok_ms={:?} error={:?}",
            st.version, st.last_ok_ms, st.error
        );
        assert!(st.last_ok_ms.is_some(), "成功同步后必须有 last_ok_ms");
        assert!(st.error.is_none(), "成功路径不该留下错误：{:?}", st.error);

        // 再同步一轮：这次会被节流挡下（两分钟窗口），是正常的
        let throttled = sync(&dir, false).await.expect("节流路径不该报错");
        assert!(throttled.deferred, "两分钟内第二次同步应被节流");

        unbind().await.expect("解绑失败");
        assert!(!bound());
        // 解绑只摘本机绑定、云端保留：本机与云端重复的账号（这里就是那 1 个）应从本机移除
        assert_eq!(
            accounts::load_accounts(&dir).len(),
            0,
            "解绑后本机应移除与云端相同的凭证"
        );
        println!("[冒烟] 已解绑，云端那一池保留");

        // 解绑不再删云端池，这里手动清理，不留孤儿测试池
        request(reqwest::Method::DELETE, &format!("/v1/pool/{}", op.uuid), None)
            .await
            .expect("清理测试池失败");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pool_item_parses_with_every_field_missing() {
        // 服务端将来加字段 / 客户端旧版本读新池，都不该整条失败
        let parsed: PoolItem = serde_json::from_str(r#"{"key":"k"}"#).unwrap();
        assert_eq!(parsed.key, "k");
        assert_eq!(parsed.access_token, "");
        assert_eq!(parsed.expires_at, None);
    }

    // ── 本地 → 云端方向（[`local_is_newer`]，`cloud_is_newer` 的反方向）──

    #[test]
    fn uploads_only_a_strictly_newer_local_ticket() {
        let mut a = acct("n", Some("138"), "new");
        a.expires_at = Some(2_000);
        assert!(local_is_newer(&a, &item("138", "old", Some(1_000))));

        // 反向：本机更旧绝不能盖云端 —— 那等于把别的机器刚续好的凭证作废，
        // 两台机器来回盖就是同步永不收敛。
        a.expires_at = Some(1_000);
        assert!(!local_is_newer(&a, &item("138", "cloud", Some(2_000))));
        // 同一到期时间也不算更新（严格大于）
        a.expires_at = Some(2_000);
        assert!(!local_is_newer(&a, &item("138", "cloud", Some(2_000))));
    }

    /// 单侧读不出到期：读得出的算新（与云端 → 本地方向同一条规则的方向版本）。
    #[test]
    fn a_readable_local_expiry_beats_a_pool_entry_with_no_expiry() {
        let mut a = acct("n", Some("138"), "local");
        a.expires_at = Some(1_000);
        assert!(local_is_newer(&a, &item("138", "cloud", None)));
        // 反过来「不知道」不等于「更旧」：本机读不出到期就不许盖云端
        a.expires_at = None;
        assert!(!local_is_newer(&a, &item("138", "cloud", None)));
    }

    /// 两个方向必须**互斥**：同一对凭证不能既「该采纳云端」又「该回写云端」，
    /// 否则两台机器每轮同步都会改一次整池版本，永远收敛不了。
    /// （用两条方向判据本身，不用 `adopt` 的返回值 —— 它还会因为「补上空字段」返回 true，
    /// 那不是覆盖，不构成收敛问题。）
    #[test]
    fn the_two_directions_never_both_claim_to_be_newer() {
        let cases: [(Account, PoolItem); 4] = [
            (acct("a", Some("1"), "opaque"), item("1", "other", None)),
            ({
                let mut a = acct("b", Some("2"), "x");
                a.expires_at = Some(1_000);
                a
            }, item("2", "y", Some(2_000))),
            ({
                let mut a = acct("c", Some("3"), "x");
                a.expires_at = Some(2_000);
                a
            }, item("3", "y", Some(1_000))),
            ({
                let mut a = acct("d", Some("4"), "x");
                a.expires_at = Some(1_000);
                a
            }, item("4", "y", Some(1_000))),
        ];
        for (a, cloud) in &cases {
            assert!(
                !(cloud_is_newer(cloud, a) && local_is_newer(a, cloud)),
                "「{}」两侧同时认定对方该被覆盖，同步会永不收敛",
                a.name
            );
        }
    }
}
