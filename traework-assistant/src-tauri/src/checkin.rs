//! TraeWork 签到：查询状态 + 领取。
//!
//! ## 端点与请求形态（逆向 `TRAE SOLO CN` 的 `out/main.js` 逐字核对）
//!
//! 官方实现在 `CNCommercialService`：
//!
//! ```js
//! async eb(path, method, apiName) {
//!   const s = await getAuthUserInfo();
//!   const url = await getApi(ugApi, path);              // bootConfig.ugApi + path
//!   const o = this.cb(s.token);                          // {headers:{Content-Type, Authorization:`Cloud-IDE-JWT <token>`}}
//!   this.fb(o.headers);                                  // + x-device-id / x-device-brand / x-device-type / x-os-version / x-app-version
//!   const a = Pr(this.P) ? 2 : 1;                        // req_source：Lite=2 / IDE=1
//!   return fetchWithHeaders({ url, method, data: {req_source: a}, timeout: 30000, ...o });
//! }
//! fetchCheckinCreditsStatus() -> eb("/trae/api/v2/ug/checkin_credits/status", "POST", "checkin_status")
//! claimCheckinCredits()       -> eb("/trae/api/v2/ug/checkin_credits/claim",  "POST", "checkin_claim")
//! ```
//!
//! 三个**关键细节**（漏掉任一个都会被服务端按「非法客户端」处理）：
//!
//! 1. **域名走 `bootConfig.ugApi`**，本机 `product.json` 里 `ug.trae.normal = https://api.trae.cn`
//!    （与 `account.trae.normal` 同值，所以沿用账号 `host` 亦等价）。
//! 2. **body 必须是 `{"req_source":2}`**（Lite 客户端），不是 `{}`。
//! 3. **鉴权头是 `Cloud-IDE-JWT <token>`**（非 Bearer）；设备头用小写 `x-*` 系列，
//!    `x-device-id` = 本机设备标识，`x-app-version` = 客户端版本号。
//!
//! ## 响应形态（真机实测）
//!
//! ```text
//! status: {"checked_in":false,"code":0,"credits":150,"did_checked_in":false,
//!          "enable":true,"extra_credits":50,"message":"success"}
//! claim : {"code":0,"message":"success"}            // 成功
//! claim : {"code":9074,"message":"当前参与用户太多，请稍后再试"}   // 服务端限流
//! ```
//!
//! ## 9074 的处理（这是「签到老是失败」的真正原因）
//!
//! ⚠️ **2026-09-14 更正**：9074 的提示语（「当前参与用户太多，请稍后再试」）有误导性，
//! 它**不只是**排队/限流，更是服务端对**不认识的设备号**的通用拒绝。实测对照（同一 token、
//! 同一请求体、只换 `x-device-id`）：
//!
//! ```text
//! x-device-id = 授权时随机生成的 uuid  → {"code":9074,...}   ❌ 持续失败
//! x-device-id = 账号 uid（同一秒发）   → {"code":0,"message":"success"} ✅
//! ```
//!
//! 因此设备号必须取「服务端认识的账号身份」或「本机真实设备号」，见 [`device_id`]。
//! 另外**不要**因为 9074 就无限等待：它既可能是真排队、也可能是设备号不被接受，
//! 所以这里仍是「递增退避 + 多轮重试」（5 次 / 约 75 秒）而不是试一次就写失败——
//! 真排队时会等到放行，设备号错时失败信息里会带上实际用的 `x-device-id` 便于定位。
//!
//! 注：领取成功后重复调用同样返回 `{"code":0,"message":"success"}`（幂等），
//! 所以重试不会重复发放。

use crate::accounts::Account;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

/// 状态查询路径（官方唯一路径）。
const STATUS_PATH: &str = "/trae/api/v2/ug/checkin_credits/status";
/// 领取路径（官方唯一路径）。
const CLAIM_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";
/// **账号已有积分**（额度用量）路径。
///
/// 官方实现（`out/main.js` 的 `pb()`）：
/// `db("/trae/api/v2/pay/ide_user_ent_usage", {require_usage:true, req_source:2|1}, cb(token))`。
/// ⚠️ 这跟 [`STATUS_PATH`] 返回的 `credits`（**签到奖励**）完全是两回事。
const ENT_USAGE_PATH: &str = "/trae/api/v2/pay/ide_user_ent_usage";
/// `req_source`：Lite 客户端 = 2（IDE = 1）。
const REQ_SOURCE_LITE: i64 = 2;
/// 缺省客户端版本（读不到本机 TraeWork `product.json` 时兜底）。
const FALLBACK_APP_VERSION: &str = "1.107.1";

/// 领取失败时的重试退避（秒）。共 5 次尝试，覆盖约 75 秒。
const CLAIM_BACKOFF_SECS: &[u64] = &[0, 3, 8, 20, 45];

static ALREADY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("已签到|已领取|already\\s*(?:checked[- ]?in|claimed)|daily.*already").unwrap()
});
static INACTIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("未开启|未开始|未开放|无.*活动|活动.*(?:结束|关闭|暂停)").unwrap()
});

/// 签到结果
#[derive(serde::Serialize, Clone, Debug)]
pub struct CheckinResult {
    pub success: bool,
    pub already: bool,
    pub inactive: bool,
    /// 瞬时失败（服务端限流 9074 / 网络错误）——调用方可稍后再试一轮
    pub transient: bool,
    /// 鉴权失败（token 失效，需要重新登录或刷新）
    pub auth_failed: bool,
    pub message: String,
    /// 签到积分余额（服务端 `credits` 原值）
    pub credit: Option<i64>,
    pub host: Option<String>,
    pub at: String,
}

/// 账号状态（给界面用）：今日签到情况 + **账号已有积分**。
///
/// ⚠️ `credits` 来自 entitlement 用量接口的剩余额度，**不是**签到奖励
/// （签到奖励在 [`CheckinResult::credit`]）。
#[derive(serde::Serialize, Clone, Debug)]
pub struct AccountStatus {
    pub id: String,
    pub checked_in: bool,
    /// 账号已有积分；未知为 `None`
    pub credits: Option<i64>,
    /// 不限量
    pub unlimited: bool,
    /// 「还有余量的额度包」里最早的到期时间（毫秒）；未知为 `None`。
    ///
    /// 它就是「智能接管」选号的**第一排序键**（到期最早者优先，见 `proxy::pick_index`），
    /// 所以必须露到界面上，否则用户无法核对这条规则是否生效。
    pub earliest_expiry_ms: Option<i64>,
    /// 逐额度包明细（名称 / 剩余 / 到期）：与 `Account.credit_snapshot.packages` 同一份，
    /// 随实时状态一并下发，前端不必再合并两处取数。
    #[serde(default)]
    pub packages: Vec<crate::accounts::CreditPackage>,
    pub message: String,
}

/// 签到状态响应里「今日是否已签到」。
pub fn is_checked_in(v: &Value) -> bool {
    status_fields(v).0
}

/// 响应里的提示语。
pub fn message_of(v: &Value) -> String {
    msg_of(v)
}

pub(crate) fn host_of(account: &Account) -> String {
    account
        .host
        .clone()
        .unwrap_or_else(|| "https://api.trae.cn".into())
}

pub(crate) fn normalize_host(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{}", u)
    }
}

/// 本机 TraeWork 的客户端版本号（`x-app-version`）。
/// 读 `resources/app/product.json` 的 `version`；失败则用兜底值。结果进程内缓存。
pub fn app_version() -> String {
    static CACHE: LazyLock<String> = LazyLock::new(|| {
        crate::endpoint::app_dir()
            .and_then(|d| std::fs::read_to_string(d.join("product.json")).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                v.get("version")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| FALLBACK_APP_VERSION.to_string())
    });
    CACHE.clone()
}

/// 设备标识 —— `x-device-id` 的唯一来源。
///
/// 顺序（2026-09-14 逐项实测确定，**不是**拍脑袋的优先级）：
///
/// | `x-device-id` 取值 | `claim` 结果 |
/// | --- | --- |
/// | 账号 uid（`user_id` / JWT `data.id`） | `{"code":0,"message":"success"}` ✅ |
/// | 本机 TraeWork 的 `telemetry.devDeviceId` | 官方客户端同源，可信 |
/// | 浏览器登录时随机生成的 uuid | `{"code":9074,"message":"当前参与用户太多，请稍后再试"}` ❌ |
///
/// 也就是说：**9074 不是「稍后再试」的排队，而是服务端不认识这个设备号**。
/// 新账号（浏览器登录）此前正是因为落库里存着那个随机 uuid，签到永远签不上。
///
/// ⚠️ 所以这里**刻意不读 `account.device_id`**：它对「扫描本机登录」导入的账号是真实的
/// `devDeviceId`，对「浏览器登录」的账号却是随机值，两者无法区分。改用下面的
/// [`crate::trae_auth::local_device_identity`] 取「本机真实设备号」，语义与账号来源无关。
pub fn device_id(account: &Account) -> String {
    let clean =
        |s: Option<String>| s.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    clean(account.user_id.clone())
        // uid 缺失（GetUserInfo 对授权码换来的 token 直接 401）时，从 JWT 载荷里解
        .or_else(|| crate::token::user_id(&account.token))
        // 本机真实设备号：与官方客户端同一个值，一台机器一个
        .or_else(|| clean(crate::trae_auth::local_device_identity().0))
        .or_else(|| clean(account.machine_id.clone()))
        .unwrap_or_else(|| account.id.clone())
}

fn device_type() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    }
}

/// 请求头：`Cloud-IDE-JWT` 鉴权 + 官方设备头（小写 `x-*`）。
///
/// `dev` 由调用方传入（一次操作里只解析一次，见 [`device_id`]）。
fn headers(account: &Account, dev: &str) -> (String, Vec<(String, String)>) {
    let hdrs: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        ("x-device-id".into(), dev.to_string()),
        ("x-device-type".into(), device_type().into()),
        ("x-app-version".into(), app_version()),
    ];
    (format!("Cloud-IDE-JWT {}", account.token), hdrs)
}

/// 请求体：`{"req_source":2}`。
fn claim_body() -> String {
    format!("{{\"req_source\":{REQ_SOURCE_LITE}}}")
}

/// 服务端限流（9074）/ 网络错误的瞬时失败。
fn is_transient(code: Option<i64>, msg: &str) -> bool {
    if code == Some(9074) {
        return true;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("9074")
        || m.contains("繁忙")
        || m.contains("busy")
        || m.contains("too many")
        || m.contains("rate limit")
        || m.contains("participants")
        || m.contains("稍后再试")
}

/// 鉴权失败（token 不可用）。
fn is_auth_error(code: Option<i64>, msg: &str) -> bool {
    if code == Some(1001) {
        return true;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("authenticate")
        || m.contains("unauthor")
        || m.contains("token")
        || m.contains("1001")
}

/// 从 status/claim 响应取值：`checked_in`（今日是否已签）、`enable`、`credits`
fn status_fields(v: &Value) -> (bool, bool, Option<i64>) {
    let checked_in = v
        .get("checked_in")
        .or_else(|| v.get("did_checked_in"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let enable = v.get("enable").and_then(Value::as_bool).unwrap_or(true);
    let credits = parse_credit(v);
    (checked_in, enable, credits)
}

/// 从响应里取**签到奖励**积分。服务端顶层为 `credits`，另有 `extra_credits`（会员加量）；
/// 官方客户端展示的是 `credits`，此处保持一致。
///
/// ⚠️ 这是「签到给了多少分」，**不是**账号已有积分——后者见 [`parse_ent_usage`]。
fn parse_credit(body: &Value) -> Option<i64> {
    let keys = ["credits", "credit", "gain_credit", "today_credit"];
    for r in [body, body.get("data").unwrap_or(&Value::Null)] {
        for key in keys {
            if let Some(v) = r.get(key) {
                if let Some(n) = v.as_i64() {
                    return Some(n);
                }
                if let Some(s) = v.as_str() {
                    if let Ok(n) = s.trim().parse::<i64>() {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

fn msg_of(body: &Value) -> String {
    body.get("msg")
        .or_else(|| body.get("message"))
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 查询签到状态（best-effort）。返回响应体本体（含 `checked_in`/`credits`/`enable`）。
pub async fn query_status(account: &Account) -> Option<Value> {
    let client = reqwest::Client::new();
    query_status_with(&client, account).await
}

/// 同 [`query_status`]，但复用外部 `client`（反代路由每 10 分钟要批量取一次，
/// 复用连接池避免每个账号都新建一次客户端）。
pub async fn query_status_with(client: &reqwest::Client, account: &Account) -> Option<Value> {
    let host = normalize_host(&host_of(account));
    let (auth_hdr, hdrs) = headers(account, &device_id(account));
    let url = format!("{host}{STATUS_PATH}");
    let mut req = client
        .post(&url)
        .header("Authorization", &auth_hdr)
        .body(claim_body())
        .timeout(Duration::from_secs(15));
    for (k, v) in &hdrs {
        req = req.header(k, v);
    }
    let resp = req.send().await.ok()?;
    let v = resp.json::<Value>().await.ok()?;
    if v.get("code").and_then(Value::as_i64) == Some(0) {
        Some(v)
    } else {
        None
    }
}

/// 把数字/字符串时间统一成毫秒时间戳（秒会被识别并放大）。
fn to_ms(x: &Value) -> Option<i64> {
    if let Some(n) = x.as_i64() {
        // 启发式：小于 1e12 视为秒级时间戳（约公元 33658 年才是 1e12 秒）
        return Some(if n < 1_000_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        });
    }
    let s = x.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<i64>() {
        return to_ms(&Value::from(n));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t.and_utc().timestamp_millis());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// 一个账号的「已有积分」画像（来自 entitlement 用量接口）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EntUsage {
    /// 剩余可用积分（Σ max(credits_limit − credits_amount, 0)，四舍五入到整数）；
    /// 不限量、或没有任何额度包时为 `None`
    pub remaining: Option<i64>,
    /// 不限量（存在 `credits_limit = -1` 的包）
    pub unlimited: bool,
    /// 「**还有余量**的额度包」里最早的到期时间（毫秒）；未知为 `None`
    pub earliest_expiry_ms: Option<i64>,
}

/// 额度包的到期时间（毫秒）：包级 `expire_time` → `end_time` → `yearly_expire_time`。
/// 真实数据里 `expire_time` 是**秒**级时间戳且与 `end_time` 同值，由 [`to_ms`] 统一。
fn pack_expiry_ms(pack: &Value) -> Option<i64> {
    for node in [Some(pack), pack.get("entitlement_base_info")].into_iter().flatten() {
        for key in ["expire_time", "end_time", "yearly_expire_time"] {
            if let Some(ms) = node.get(key).and_then(to_ms) {
                if ms > 0 {
                    return Some(ms);
                }
            }
        }
    }
    None
}

/// 汇总 `ide_user_ent_usage` 响应 —— 逐行照搬官方 `hHe()`（`out/main.js`）：
/// 遍历 `user_entitlement_pack_list`，对 `credits_limit > 0` 的包累加
/// `max(credits_limit − usage.credits_amount, 0)`；`credits_limit == -1` 视为不限量。
///
/// 真实响应（2026-09-14 实机）：`credits_amount` 是**小数**（如 579.3308），
/// `usage` 可能为空对象（视为 0），`code` 字段不存在。
///
/// 到期时间官方 `hHe()` 不计算，是本项目补的：**只有还剩额度的包**才参与
/// 「到期最早」比较 —— 一个已经用光的包到期再早也没有意义，不该把排序带偏。
pub fn parse_ent_usage(v: &Value) -> EntUsage {
    let Some(packs) = v.get("user_entitlement_pack_list").and_then(Value::as_array) else {
        return EntUsage::default();
    };
    let mut remaining: f64 = 0.0;
    let mut unlimited = false;
    let mut has_quota = false;
    let mut earliest: Option<i64> = None;
    for pack in packs {
        let limit = pack
            .get("entitlement_base_info")
            .and_then(|b| b.get("quota"))
            .and_then(|q| q.get("credits_limit"))
            .and_then(Value::as_f64);
        let Some(limit) = limit else { continue };
        if limit < 0.0 {
            // -1 = 不限量（真实数据里只有 -1；防御性地把任何负数都当不限量）
            unlimited = true;
            has_quota = true;
            continue;
        }
        if limit == 0.0 {
            continue;
        }
        let used = pack
            .get("usage")
            .and_then(|u| u.get("credits_amount"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let left = (limit - used).max(0.0);
        has_quota = true;
        remaining += left;
        if left > 0.0 {
            if let Some(ms) = pack_expiry_ms(pack) {
                earliest = Some(earliest.map_or(ms, |e: i64| e.min(ms)));
            }
        }
    }
    EntUsage {
        remaining: if has_quota && !unlimited {
            Some(remaining.round() as i64)
        } else {
            None
        },
        unlimited,
        earliest_expiry_ms: earliest,
    }
}

/// 发一次 `ide_user_ent_usage` 请求并返回原始 JSON（best-effort）。
///
/// 官方判定：响应存在且 `code` 缺省或为 0 才算成功（正常响应里没有 `code` 字段）。
/// 汇总（[`parse_ent_usage`]）与逐包明细（[`parse_packages`]）都吃这一份 body，
/// 所以请求只发一次、解析两遍 —— 简报台账不额外打接口。
async fn post_ent_usage(client: &reqwest::Client, account: &Account) -> Option<Value> {
    let host = normalize_host(&host_of(account));
    let (auth_hdr, hdrs) = headers(account, &device_id(account));
    let url = format!("{host}{ENT_USAGE_PATH}");
    let mut req = client
        .post(&url)
        .header("Authorization", &auth_hdr)
        .body(format!(
            "{{\"require_usage\":true,\"req_source\":{REQ_SOURCE_LITE}}}"
        ))
        .timeout(Duration::from_secs(15));
    for (k, v) in &hdrs {
        req = req.header(k, v);
    }
    let resp = req.send().await.ok()?;
    let v = resp.json::<Value>().await.ok()?;
    match v.get("code").and_then(Value::as_i64) {
        None | Some(0) => Some(v),
        Some(_) => None,
    }
}

/// 抓取账号「已有积分」（best-effort）。⚠️ 会发一次网络请求，调用方负责缓存/TTL。
pub async fn fetch_ent_usage(account: &Account) -> Option<EntUsage> {
    let client = reqwest::Client::new();
    fetch_ent_usage_with(&client, account).await
}

/// 同 [`fetch_ent_usage`]，但复用外部 `client` 的连接池。
pub async fn fetch_ent_usage_with(client: &reqwest::Client, account: &Account) -> Option<EntUsage> {
    post_ent_usage(client, account).await.map(|v| parse_ent_usage(&v))
}

/// 一次拉取「剩余积分 + 最早过期时间 + **逐包明细**」（同一份 `ide_user_ent_usage` 响应）。
///
/// 积分简报（[`crate::ledger`]）需要的逐包明细就附在同一份响应里，多解析几个字段而已，
/// 不额外打接口。`credits` 与 [`EntUsage::remaining`] 同口径（四舍五入到整数后转 f64）。
#[derive(Debug, Clone, Default)]
pub struct ResourceView {
    /// 逐包明细（台账输入）
    pub packages: Vec<crate::ledger::PkgView>,
    /// 剩余积分；不限量 / 未知为 `None`
    pub credits: Option<f64>,
    /// 不限量（存在 `credits_limit = -1` 的包）—— 随采样回写账号快照用
    pub unlimited: bool,
    /// 最早过期时间（毫秒）；未知为 `None`
    pub earliest_expiry_ms: Option<i64>,
}

/// 抓取 `ResourceView`（best-effort）。⚠️ 会发一次网络请求。
pub async fn fetch_resource_view(account: &Account) -> ResourceView {
    let client = reqwest::Client::new();
    fetch_resource_view_with(&client, account).await
}

/// 同 [`fetch_resource_view`]，但复用外部 `client` 的连接池。
pub async fn fetch_resource_view_with(client: &reqwest::Client, account: &Account) -> ResourceView {
    let Some(v) = post_ent_usage(client, account).await else {
        return ResourceView::default();
    };
    let u = parse_ent_usage(&v);
    ResourceView {
        packages: parse_packages(&v),
        credits: u.remaining.map(|r| r as f64),
        unlimited: u.unlimited,
        earliest_expiry_ms: u.earliest_expiry_ms,
    }
}

/// 逐包解析 `ide_user_ent_usage` 响应 → 台账输入（[`crate::ledger::PkgView`]）。
///
/// 适配要点（与参考项目 billing 的 `get-user-resource` 不同）：
/// - `size` = `entitlement_base_info.quota.credits_limit`（授予量）
/// - `used` = `usage.credits_amount`（已用量，缺省视为 0）
/// - **键 = `entitlement_id`**（`source_id`；`product_id` 208/209 的区分字段）。这是
///   **实测逼出来的**：同一批「老用户福利」（产品 208 与 209，`expire_time` 相同、
///   描述相同）里，一个包的 `credits_amount` 是 2000（已用满）、另一个是 1288.93（还在
///   消耗）。若拿「描述@到期」当键，台账合并取最大值会把唯一那条钉在用满的包上，
///   另一个继续消耗的包的增量永远算不进来 → 简报不再更新（见 [`crate::ledger::PkgView::key`]）。
///   响应里没有 `entitlement_id` 的包（低版本接口）无法构成稳定唯一键，直接跳过
///   （宁可不记，也不能让同一个包在两次采样间「换了个键」而重复记账）。
/// - `cycle_start` 恒为空串：本项目无周期概念，「翻周期归档」分支不会触发。
pub fn parse_packages(v: &Value) -> Vec<crate::ledger::PkgView> {
    let Some(packs) = v.get("user_entitlement_pack_list").and_then(Value::as_array) else {
        return Vec::new();
    };
    packs
        .iter()
        .filter_map(|pack| {
            let desc = pack
                .get("display_desc")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let base = pack.get("entitlement_base_info");
            let limit = base
                .and_then(|b| b.get("quota"))
                .and_then(|q| q.get("credits_limit"))
                .and_then(Value::as_f64)?;
            // 不限量（-1）或零额度包不进台账：size 为负/0 会让「新增」失真
            if limit <= 0.0 || desc.is_empty() {
                return None;
            }
            let used = pack
                .get("usage")
                .and_then(|u| u.get("credits_amount"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            // 键必须逐包唯一：优先 `entitlement_base_info.entitlement_id`，
            // 退而求其次包级 `source_id`（真实响应里两者同值）。
            let id = base
                .and_then(|b| b.get("entitlement_id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| pack.get("source_id").and_then(Value::as_str).filter(|s| !s.is_empty()))?;
            Some(crate::ledger::PkgView {
                key: id.to_string(),
                name: desc.to_string(),
                size: limit,
                used,
                cycle_start: String::new(),
                expiry_ms: pack_expiry_ms(pack),
            })
        })
        .collect()
}

/// 把解析出的逐包台账输入投影成前端展示用的 `CreditPackage` 列表。
///
/// 只保留**还有余量**（size − used > 0）的包：用光的包到期再早也没有意义，
/// 不该出现在「资源包列表」里让用户去盯一对没用的数字。到期时间直接取台账条目的
/// `expiry_ms`（不再从键里反解 —— 键现在是 `entitlement_id`，不含到期时间）。
pub fn to_credit_packages(packages: Vec<crate::ledger::PkgView>) -> Vec<crate::accounts::CreditPackage> {
    packages
        .into_iter()
        .filter_map(|p| {
            let remaining = (p.size - p.used).max(0.0) as i64;
            if remaining <= 0 {
                return None;
            }
            let expiry_ms = p.expiry_ms?;
            Some(crate::accounts::CreditPackage {
                name: p.name,
                remaining,
                expiry_ms,
            })
        })
        .collect()
}

/// 抓取积分快照（best-effort）：**账号已有积分** + 到期时间 + 逐包明细，供界面展示与接管选号。
///
/// **失败也要落一个空快照**，否则每次新会话都会重打一次接口。
/// 调用方若已有快照，应保留原有数值（见 `commands::checkin_status` / `proxy::choose_account`）。
pub async fn fetch_credit_snapshot(
    client: &reqwest::Client,
    account: &Account,
) -> crate::accounts::CreditSnapshot {
    let view = fetch_resource_view_with(client, account).await;
    crate::accounts::CreditSnapshot::now(
        view.credits.map(|c| c.round() as i64),
        view.unlimited,
        view.earliest_expiry_ms,
        to_credit_packages(view.packages),
    )
}

/// 对一个账号执行签到。
pub async fn do_checkin(account: &Account) -> CheckinResult {
    let client = reqwest::Client::new();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let host = normalize_host(&host_of(account));
    let dev = device_id(account);
    let (auth_hdr, hdrs) = headers(account, &dev);
    let body = claim_body();

    // ---- 1) 先查状态：今日已签 → 幂等成功；活动未开启 → 直接返回 ----
    if let Some(v) = query_status(account).await {
        let (checked_in, enable, credits) = status_fields(&v);
        if checked_in {
            return CheckinResult {
                success: true,
                already: true,
                inactive: false,
                transient: false,
                auth_failed: false,
                message: "今日已签到".into(),
                credit: credits,
                host: Some(host.clone()),
                at: now,
            };
        }
        if !enable {
            return CheckinResult {
                success: false,
                already: false,
                inactive: true,
                transient: false,
                auth_failed: false,
                message: "签到活动未开启".into(),
                credit: credits,
                host: Some(host.clone()),
                at: now,
            };
        }
    }

    // ---- 2) 领取：递增退避重试（9074 是服务端限流，需等它放行）----
    let url = format!("{host}{CLAIM_PATH}");
    let mut last_msg = String::new();
    let mut last_code: Option<i64> = None;
    let mut last_transient = false;
    let mut last_auth = false;

    for delay in CLAIM_BACKOFF_SECS.iter() {
        if *delay > 0 {
            tokio::time::sleep(Duration::from_secs(*delay)).await;
        }
        let mut req = client
            .post(&url)
            .header("Authorization", &auth_hdr)
            .body(body.clone())
            .timeout(Duration::from_secs(25));
        for (k, v) in &hdrs {
            req = req.header(k, v);
        }

        let (http_ok, code, msg, raw) = match req.send().await {
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let code = parsed.get("code").and_then(Value::as_i64);
                let msg = msg_of(&parsed);
                (status.is_success(), code, msg, text)
            }
            Err(e) => (false, None, e.to_string(), String::new()),
        };

        // 成功（含幂等重复领取）
        if code == Some(0) {
            let credit = serde_json::from_str::<Value>(&raw).ok().and_then(|v| parse_credit(&v));
            let msg = if msg.is_empty() { "success".into() } else { msg };
            return CheckinResult {
                success: true,
                already: false,
                inactive: false,
                transient: false,
                auth_failed: false,
                message: msg,
                credit,
                host: Some(host.clone()),
                at: now,
            };
        }

        // 已签到文案（业务上用非 0 code 表达）
        if ALREADY_RE.is_match(&msg) {
            return CheckinResult {
                success: true,
                already: true,
                inactive: false,
                transient: false,
                auth_failed: false,
                message: msg,
                credit: None,
                host: Some(host.clone()),
                at: now,
            };
        }

        // 活动未开启/已结束：非错误，直接返回
        if INACTIVE_RE.is_match(&msg) {
            return CheckinResult {
                success: false,
                already: false,
                inactive: true,
                transient: false,
                auth_failed: false,
                message: msg,
                credit: None,
                host: Some(host.clone()),
                at: now,
            };
        }

        last_code = code;
        last_msg = if msg.is_empty() {
            format!("HTTP {} {}", if http_ok { 200 } else { 0 }, raw.trim())
        } else {
            msg.clone()
        };
        last_transient = is_transient(code, &last_msg) || !http_ok;
        last_auth = is_auth_error(code, &last_msg);

        // 鉴权失败重试没有意义，直接结束
        if last_auth {
            break;
        }
        // 非瞬时失败（真业务错误）也没必要继续退避
        if !last_transient {
            break;
        }
    }

    // ---- 3) 组装失败结果（带可读原因 + 是否需要重试）----
    // 9074 有两副面孔：真排队（等一会儿会放行）与「设备号不被接受」（重试永远无效）。
    // 提示语里带上实际用的 x-device-id，下次排障一眼就能看出是哪种。
    let message = if last_auth {
        format!("鉴权失败（code {:?}）：{}；请重新登录该账号以刷新 token", last_code, last_msg)
    } else if last_transient {
        format!(
            "服务端未放行（code {:?}）：{}；已按 5 次退避重试仍未成功（x-device-id={}）",
            last_code, last_msg, dev
        )
    } else {
        format!("[code={:?}] {}（x-device-id={}）", last_code, last_msg, dev)
    };

    CheckinResult {
        success: false,
        already: false,
        inactive: false,
        transient: last_transient,
        auth_failed: last_auth,
        message,
        credit: None,
        host: None,
        at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_official_shape() {
        assert_eq!(claim_body(), r#"{"req_source":2}"#);
    }

    #[test]
    fn status_fields_reads_official_payload() {
        let v: Value = serde_json::from_str(
            r#"{"checked_in":false,"code":0,"credits":150,"did_checked_in":false,"enable":true,"extra_credits":50,"message":"success"}"#,
        )
        .unwrap();
        let (checked_in, enable, credits) = status_fields(&v);
        assert!(!checked_in);
        assert!(enable);
        assert_eq!(credits, Some(150), "取 credits 原值，不合并 extra_credits");
    }

    #[test]
    fn transient_and_auth_classification() {
        assert!(is_transient(Some(9074), "当前参与用户太多，请稍后再试"));
        assert!(!is_transient(Some(0), "success"));
        assert!(is_auth_error(Some(1001), "not able to authenticate you"));
        assert!(!is_auth_error(Some(9074), "too many"));
    }

    #[test]
    fn device_id_prefers_user_id() {
        let acc = Account {
            id: "local".into(),
            name: String::new(),
            phone: None,
            region: None,
            user_id: Some("u123".into()),
            token: "t".into(),
            refresh_token: None,
            host: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: Some("dev".into()),
            machine_id: Some("mach".into()),
            created_at: String::new(),
            credit_snapshot: None,
        };
        assert_eq!(device_id(&acc), "u123");
    }

    fn acc_with(user_id: Option<&str>, token: &str, device_id: Option<&str>) -> Account {
        Account {
            id: "local".into(),
            name: String::new(),
            phone: None,
            region: None,
            user_id: user_id.map(str::to_string),
            token: token.to_string(),
            refresh_token: None,
            host: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: device_id.map(str::to_string),
            machine_id: Some("mach".into()),
            created_at: String::new(),
            credit_snapshot: None,
        }
    }

    /// 回归（2026-09-14「新加的号签不上」）：uid 缺失时从 **JWT 载荷**取 uid
    /// （`GetUserInfo` 对授权码换来的 token 会 401，不能依赖它）。
    #[test]
    fn device_id_falls_back_to_uid_inside_jwt() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let payload = URL_SAFE_NO_PAD.encode(br#"{"data":{"id":"3225324630062683"}}"#);
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.sig");
        let acc = acc_with(None, &token, Some("e6fb29121995483988b3f6cff4b9ff1c"));
        assert_eq!(device_id(&acc), "3225324630062683");
    }

    /// 回归：浏览器登录流程落库的**随机 device_id** 绝不能再被当成签到头
    /// ——服务端不认识它，`claim` 会一直回 9074。
    #[test]
    fn device_id_never_uses_the_random_stored_device_id() {
        let random = "e6fb29121995483988b3f6cff4b9ff1c";
        // user_id 缺失 + token 不是 JWT：应当退到本机真实设备号 / machine_id / id
        let acc = acc_with(None, "opaque-token", Some(random));
        assert_ne!(device_id(&acc), random);
    }

    #[test]
    fn to_ms_accepts_seconds_millis_and_strings() {
        // 秒级时间戳 → 放大到毫秒
        assert_eq!(to_ms(&Value::from(1_700_000_000i64)), Some(1_700_000_000_000));
        // 毫秒级原样保留
        assert_eq!(to_ms(&Value::from(1_700_000_000_000i64)), Some(1_700_000_000_000));
        // 字符串数字
        assert_eq!(to_ms(&Value::from("1700000000")), Some(1_700_000_000_000));
        // 日期时间串 / ISO
        assert!(to_ms(&Value::from("2026-12-31 23:59:59")).is_some());
        assert!(to_ms(&Value::from("not a time")).is_none());
    }

    /// 用 2026-09-14 实机抓到的真实响应（6 个额度包）核对汇总口径。
    #[test]
    fn ent_usage_matches_official_aggregation() {
        let real = r#"{
          "is_credits_billing": true,
          "user_entitlement_pack_list": [
            {"display_desc":"老用户福利","expire_time":1791979006,
             "entitlement_base_info":{"quota":{"credits_limit":2000},"end_time":1791979006},"usage":{}},
            {"display_desc":"老用户福利","expire_time":1791979006,
             "entitlement_base_info":{"quota":{"credits_limit":2000},"end_time":1791979006},
             "usage":{"credits_amount":579.3308}},
            {"display_desc":"免费","expire_time":1790783999,
             "entitlement_base_info":{"quota":{"solo_agent_parallel_limit":2},"end_time":1790783999},
             "usage":{}},
            {"display_desc":"每月登录赠送","expire_time":1790783999,
             "entitlement_base_info":{"quota":{"credits_limit":500},"end_time":1790783999},
             "usage":{"credits_amount":500}},
            {"display_desc":"签到奖励","expire_time":1791979013,
             "entitlement_base_info":{"quota":{"credits_limit":150},"end_time":1791979013},"usage":{}},
            {"display_desc":"签到奖励","expire_time":1792057191,
             "entitlement_base_info":{"quota":{"credits_limit":150},"end_time":1792057191},"usage":{}}
          ]
        }"#;
        let u = parse_ent_usage(&serde_json::from_str::<Value>(real).unwrap());
        // 2000 + (2000 − 579.3308) + 150 + 150 = 3720.6692 → 3721
        assert_eq!(u.remaining, Some(3721), "按官方 hHe() 汇总剩余额度并四舍五入到整数");
        assert!(!u.unlimited);
        assert_eq!(
            u.earliest_expiry_ms,
            Some(1_791_979_006_000),
            "秒级 expire_time 应放大到毫秒；已用尽的包不参与「到期最早」"
        );
        // 账号已有积分 ≠ 签到奖励（签到给 150，这里是 3721）
        assert_ne!(u.remaining, Some(150));
    }

    #[test]
    fn ent_usage_handles_unlimited_and_malformed() {
        let unlimited = serde_json::from_str::<Value>(
            r#"{"user_entitlement_pack_list":[{"entitlement_base_info":{"quota":{"credits_limit":-1}}}]}"#,
        )
        .unwrap();
        let u = parse_ent_usage(&unlimited);
        assert!(u.unlimited, "credits_limit = -1 视为不限量");
        assert_eq!(u.remaining, None, "不限量时不给数字（由 unlimited 表达）");

        // 异常响应不 panic，也不编造数字
        assert_eq!(parse_ent_usage(&Value::from(0)).remaining, None);
        let no_list = serde_json::from_str::<Value>(r#"{"code":0}"#).unwrap();
        assert_eq!(parse_ent_usage(&no_list).remaining, None);
        let empty = serde_json::from_str::<Value>(r#"{"user_entitlement_pack_list":[]}"#).unwrap();
        assert_eq!(parse_ent_usage(&empty).remaining, None);
    }

    /// 用 2026-09-14 / 09-21 实机抓到的真实响应核对**逐包解析**（与上面汇总测试同一份 body）。
    #[test]
    fn packages_parse_real_ent_usage_shape() {
        let real = r#"{
          "is_credits_billing": true,
          "user_entitlement_pack_list": [
            {"display_desc":"老用户福利","expire_time":1791979006,"source_id":"357880625410",
             "entitlement_base_info":{"entitlement_id":"357880625410","quota":{"credits_limit":2000},"end_time":1791979006},"usage":{}},
            {"display_desc":"老用户福利","expire_time":1791979006,"source_id":"357880625666",
             "entitlement_base_info":{"entitlement_id":"357880625666","quota":{"credits_limit":2000},"end_time":1791979006},
             "usage":{"credits_amount":2000}},
            {"display_desc":"免费","expire_time":1790783999,"source_id":"free_utc20269_1",
             "entitlement_base_info":{"quota":{"solo_agent_parallel_limit":2},"end_time":1790783999},
             "usage":{}},
            {"display_desc":"每月登录赠送","expire_time":1790783999,"source_id":"monthly_bonus_20269_1",
             "entitlement_base_info":{"quota":{"credits_limit":500},"end_time":1790783999},
             "usage":{"credits_amount":500}},
            {"display_desc":"签到奖励","expire_time":1791979013,"source_id":"checkin_20260913_1",
             "entitlement_base_info":{"quota":{"credits_limit":150},"end_time":1791979013},"usage":{}},
            {"display_desc":"签到奖励","expire_time":1792057191,"source_id":"checkin_20260914_1",
             "entitlement_base_info":{"quota":{"credits_limit":150},"end_time":1792057191},"usage":{}}
          ]
        }"#;
        let pkgs = parse_packages(&serde_json::from_str::<Value>(real).unwrap());

        // 无 credits_limit 的「免费」包应被跳过
        assert_eq!(pkgs.len(), 5, "「免费」包没有 credits_limit，不进台账");
        let old: Vec<_> = pkgs.iter().filter(|p| p.name == "老用户福利").collect();
        assert_eq!(old.len(), 2, "描述与到期都相同的两个包必须各自成条");
        assert_eq!(
            old.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            vec!["357880625410", "357880625666"],
            "键 = entitlement_id：退化成「描述@到期」这两个包会并成一条，\
             台账取最大值后被那个用满的钉死，另一个还在消耗的包再也记不进来"
        );
        assert!(old.iter().any(|p| p.size == 2000.0 && p.used == 0.0));
        assert!(old.iter().any(|p| p.size == 2000.0 && p.used == 2000.0), "used 取已用量 credits_amount");
        assert!(old.iter().all(|p| p.cycle_start.is_empty()), "本项目无周期概念，cycle_start 恒空");
        assert!(
            old.iter().all(|p| p.expiry_ms == Some(1791979006000)),
            "到期时间随包带出（秒级 expire_time 应放大成毫秒），展示侧不再从键里反解"
        );
        // 两个「签到奖励」到期时间不同 → 两个独立条目
        let ck: Vec<_> = pkgs.iter().filter(|p| p.name == "签到奖励").collect();
        assert_eq!(ck.len(), 2);
        // 无 credits_limit 的包不进台账
        assert!(!pkgs.iter().any(|p| p.name == "免费"));
        // 既没有 entitlement_id 也没有 source_id 的包无法构成唯一键 ⇒ 不记账（宁可漏，不要错账）
        assert!(parse_packages(&serde_json::from_str::<Value>(
            r#"{"user_entitlement_pack_list":[{"display_desc":"老用户福利","expire_time":1791979006,
                 "entitlement_base_info":{"quota":{"credits_limit":2000}},"usage":{}}]}"#
        ).unwrap())
        .is_empty());
        // 异常响应不 panic
        assert!(parse_packages(&Value::from(0)).is_empty());
        assert!(parse_packages(&serde_json::from_str::<Value>(r#"{"code":0}"#).unwrap()).is_empty());
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;
    use crate::accounts::Account;
    use crate::trae_auth;

    /// 排障用（只读 + 顺手规范化）：打印账号池里每个账号**实际会用的签到头 `x-device-id`**。
    ///
    /// 下次「某个号又签不上」时先跑它 —— 若打印出的值不是 uid、也不是本机真实设备号，
    /// 那就是设备号问题（服务端会对它回 9074）：
    /// `cargo test --lib -- --ignored --nocapture dump_checkin_device_ids`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn dump_checkin_device_ids() {
        let dir = dirs::home_dir()
            .expect("home")
            .join("Library/Application Support/cn.traework.assistant");
        let list = crate::accounts::load_accounts(&dir);
        let (dev, mach) = trae_auth::local_device_identity();
        println!("本机 telemetry: devDeviceId={dev:?} machineId={mach:?}");
        for a in &list {
            println!(
                "账号 id={:?} uid={:?} 落库device_id={:?} → x-device-id={:?}",
                a.id,
                a.user_id,
                a.device_id,
                device_id(a)
            );
        }
    }

    fn to_account(a: &trae_auth::TraeLocalAccount) -> Account {
        Account {
            id: "t".into(),
            name: a.nickname.clone().unwrap_or_default(),
            phone: a.phone.clone(),
            region: a.region.clone(),
            user_id: a.user_id.clone(),
            token: a.token.clone(),
            refresh_token: a.refresh_token.clone(),
            host: a.host.clone(),
            expires_at: a.expires_at,
            refresh_expires_at: a.refresh_expires_at,
            device_id: a.device_id.clone(),
            machine_id: a.machine_id.clone(),
            created_at: String::new(),
            credit_snapshot: None,
        }
    }

    #[test]
    #[ignore]
    fn live_status_and_checkin() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            println!("app_version={}", app_version());
            let list = trae_auth::discover_local_accounts();
            println!("accounts={}", list.len());
            for a in list {
                let acc = to_account(&a);
                let _status = query_status(&acc).await;
                let r = do_checkin(&acc).await;
                println!(
                    "CHECKIN success={} already={} inactive={} transient={} auth={} credit={:?} msg={:?}",
                    r.success, r.already, r.inactive, r.transient, r.auth_failed, r.credit, r.message
                );
            }
        });
    }
}
