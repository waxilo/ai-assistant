//! 选号 + 上游错误处置：把「此刻该用哪个账号」和「上游这个错该怎么算」从网关里独立出来。
//!
//! # 选号规则（刻意简单，与助手的接管反代一致）
//!
//! 1. **每条请求都纯排序**，不用历史记忆干扰。排序键
//!    `(还有余量的资源包最早到期自然日, −最早到期日当天待过期积分)`——
//!    先把快作废的积分烧掉；同一天到期时先清小余额。
//! 2. 剩余积分为 0 的账号跳过（除非全员为 0，让上游自己报错更有信息量）。
//! 3. 限流冷却分两档：**账号级**（账号整体不可用）与 **账号 × 模型**（某个模型
//!    被 429 不影响同账号的其它模型）。实测（2026-09-14）：同一账号 `hy3` 吃 429
//!    的同一时刻，主模型 `deepseek-v4.1-flash` 依然 200——限流本来就是按模型算的。
//!    但账号级故障（余额耗尽、会话失效、被上游限制）换模型也救不回来，必须按账号记。
//!
//! # 错误处置规则
//!
//! 网关以前只认 429，其余状态码一律原样透传——那是错的：某号余额耗尽（402）或
//! 对该模型无权限（11102）时冷却表没记，下一轮排序照样选中它，于是每次请求都撞
//! 同一个号同一个错，直到人手关掉。所以这里按 [`ErrKind`] 分类处置：
//!
//! | 分类 | 处置 |
//! | --- | --- |
//! | [`ErrKind::CreditExhausted`] | 账号级硬冷却 6h（积分恢复时刻不可知，6h 足够跨过大多数恢复点） |
//! | [`ErrKind::RateModel`] | 有重置时刻就精确冷却到该墙钟；没有则**有界退避**（10m 起 ×2 封顶 2h） |
//! | [`ErrKind::RateAccount`] | 同上，但记账号级——换模型不该绕过账号级限流 |
//! | [`ErrKind::Waf`] | 账号级 60s（IP/指纹维频控，信号比 429 轻） |
//! | [`ErrKind::NotFound`] | 账号级 60s，**不按限流退避**（偶发路径缺失不是限流信号） |
//! | [`ErrKind::SessionDead`] | 账号级硬冷却 24h |
//! | [`ErrKind::Restricted`] | 账号级硬冷却 24h（解除只能由用户去官方申诉，重登无效） |
//! | [`ErrKind::ModelBlocked`] | (账号, 模型) 负缓存 6h 起 ×2 封顶 24h，该模型成功即清 |
//! | [`ErrKind::Server`] | 熔断计数：连败 3 次才罚，30m × 2ⁿ 封顶 6h；成功清零 |
//! | [`ErrKind::ChannelRefused`] | **不罚号**（账号本身健康，换号也改不了通道指纹），但可以换号试 |
//! | [`ErrKind::BadRequest`] | **不罚号也不换号**（请求的问题，换号得到同样的错） |
//! | [`ErrKind::Other`] | 不罚号，直接透传 |
//!
//! # 两条容易写错的细节
//!
//! - **有上游重置时刻时绝不做指数堆加**。上游给的是权威墙钟，堆加只会让号更久不可用。
//! - **已经在冷却中再撞一次 429，不推进退避、不延长冷却**。用户重试或并发兜底探测
//!   每次都把 streak +1 指数翻倍，是「全池被推到 2h 封顶」的元凶。

use crate::accounts;
use crate::checkin::fetch_resource_view;
use crate::commands;
use crate::ledger;
use chrono;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

/// 积分快照缓存时长：路由决策不必每次都打资源接口
const SNAPSHOT_TTL: Duration = Duration::from_secs(600);

/// 选号时"顺手补拉积分快照"用的客户端：与签到 / 刷新**同一套身份**，并强制直连。
///
/// 不能用网关的转发客户端：那个只统一 UA、绝不注入身份头（客户端的头要原样过去）。
/// 而这里是**我们自己主动**打腾讯的计费接口，需要完整的账号接口头条。
static API_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(crate::http::api_client_direct);

/// 同一次客户端请求里，最多换几个账号重试（首次 + 2 次切换）
pub const FAILOVER_MAX_TRIES: usize = 3;

// ---------------------------------------------------------------------------
// 冷却时长
// ---------------------------------------------------------------------------

/// 有界退避的基数：上游 429 没给重置时刻时从这里起翻倍
pub const SOFT_BASE_MS: i64 = 10 * 60 * 1000;
/// 有界退避的封顶
const SOFT_CAP_MS: i64 = 2 * 60 * 60 * 1000;

/// 冷却时长下限：解析出的时刻若已过去（时钟偏差、文案写的是历史时间）也要真冷却一小会儿
const COOLDOWN_MIN_MS: i64 = 30 * 1000;
/// 冷却时长上限：再离谱也不能把账号锁死超过一天
const COOLDOWN_MAX_MS: i64 = 24 * 60 * 60 * 1000;

/// 5xx 熔断：连续失败达该次数才罚号（单次 5xx 是上游抖动，不是账号的问题）
const BREAKER_THRESHOLD: u32 = 3;
const BREAKER_BASE_MS: i64 = 30 * 60 * 1000;
const BREAKER_CAP_MS: i64 = 6 * 60 * 60 * 1000;

/// 各类错误的固定冷却
const WAF_MS: i64 = 60 * 1000;
const NOT_FOUND_MS: i64 = 60 * 1000;
const RESTRICTED_MS: i64 = 24 * 60 * 60 * 1000;
const SESSION_DEAD_MS: i64 = 24 * 60 * 60 * 1000;
/// 余额耗尽：积分恢复时刻不可知（包按自然日到期、签到补发时刻由用户配置），
/// 6h 足够跨过大多数恢复点，又不至于把号锁死一天。
const CREDIT_EXHAUSTED_MS: i64 = 6 * 60 * 60 * 1000;

/// (账号, 模型) 负缓存：官方明确该后端无此模型，重试无意义，只能换模型/换号
const MODEL_BLOCK_BASE_MS: i64 = 6 * 60 * 60 * 1000;
const MODEL_BLOCK_CAP_MS: i64 = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/// 上游错误的处置分类。
///
/// 判据来自实测（2026-11-24）：上游把语义都写在响应体的 `code` 里，HTTP 状态可能
/// 统一是 400。所以**业务码优先，HTTP 状态兜底**。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrKind {
    /// 余额/积分耗尽（402，或文案里的余额关键词）
    CreditExhausted,
    /// 模型级限流（6004）：换模型即可用
    RateModel,
    /// 账号级限流（非 6004 的 429/11140 rate-limiting）：换模型也没用
    RateAccount,
    /// WAF / 403：IP 或指纹维频控，账号本身健康
    Waf,
    /// 会话失效（12153）：refresh token 已废，只能重新登录
    SessionDead,
    /// 路径不存在（404）
    NotFound,
    /// 账号被上游限制（11140 + request illegal）：解除只能走官方申诉
    Restricted,
    /// 通道校验失败（11128）：账号本身健康，客户端可正常用同一账号
    ChannelRefused,
    /// 该账号对该模型无权限 / 无此模型（11102、11158）
    ModelBlocked,
    /// 上游 5xx
    Server,
    /// 请求本身的问题（参数错 11101、prompt 超长 11115、内容拦截、图片非法）
    BadRequest,
    /// 判不出来
    Other,
}

impl ErrKind {
    /// 日志里的中文短标签
    pub fn label(self) -> &'static str {
        match self {
            ErrKind::CreditExhausted => "积分耗尽",
            ErrKind::RateModel => "模型级限流",
            ErrKind::RateAccount => "账号级限流",
            ErrKind::Waf => "WAF 拦截",
            ErrKind::SessionDead => "会话失效",
            ErrKind::NotFound => "上游 404",
            ErrKind::Restricted => "账号被限制",
            ErrKind::ChannelRefused => "通道校验失败",
            ErrKind::ModelBlocked => "该号无此模型",
            ErrKind::Server => "上游 5xx",
            ErrKind::BadRequest => "请求有问题",
            ErrKind::Other => "未分类错误",
        }
    }

    /// 值不值得换号重试。
    ///
    /// `BadRequest` / `Other` 不换：同一个请求体换任何号都会得到同样的错，
    /// 换号只是把一次失败放大成 N 次（还会烧 N 个号的自尊）。
    pub fn worth_retry(self) -> bool {
        !matches!(self, ErrKind::BadRequest | ErrKind::Other)
    }

    /// 这个错是不是账号的问题（会被记进冷却/熔断）。
    ///
    /// 目前只有单元测试用它把「该罚 / 不该罚」钉成表——运行时处置由
    /// `apply_policy` 的 match 直接决定。留着是因为它是这份分类的**规格说明**，
    /// 改分类时先改这里，比读 12 个 match 分支快。
    #[allow(dead_code)]
    pub fn penalizes(self) -> bool {
        matches!(
            self,
            ErrKind::CreditExhausted
                | ErrKind::RateModel
                | ErrKind::RateAccount
                | ErrKind::Waf
                | ErrKind::SessionDead
                | ErrKind::NotFound
                | ErrKind::Restricted
                | ErrKind::ModelBlocked
                | ErrKind::Server
        )
    }
}

/// 取上游错误体里的业务码（数字或数字字符串都认）
fn error_code(body: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let c = v.get("code")?;
    c.as_i64()
        .or_else(|| c.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 上游错误分类（纯函数）。
///
/// 业务码优先：上游实测把语义写进 `code`，HTTP 状态可能统一是 400
/// （`{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}` 是 400）。
pub fn classify(status: u16, body: &[u8]) -> ErrKind {
    let text = String::from_utf8_lossy(body);
    match error_code(body) {
        // 11101：「Non-stream chat request is currently not supported」、
        //        「Unmarshal chat params failed」。都是请求形态的问题
        Some(11101) | Some(11115) => return ErrKind::BadRequest,
        Some(11102) | Some(11158) => return ErrKind::ModelBlocked,
        Some(11128) => return ErrKind::ChannelRefused,
        // 11140 有两种：rate-limiting 是限流，request illegal 是账号被限制。
        // 两者都以 11140 下发，只能靠文案分
        Some(11140) => {
            return if text.contains("request illegal") {
                ErrKind::Restricted
            } else {
                ErrKind::RateAccount
            };
        }
        Some(12153) => return ErrKind::SessionDead,
        Some(6004) => return ErrKind::RateModel,
        _ => {}
    }
    match status {
        402 => ErrKind::CreditExhausted,
        429 => ErrKind::RateAccount,
        403 => ErrKind::Waf,
        404 => ErrKind::NotFound,
        500..=599 => ErrKind::Server,
        400..=499 => {
            // 没有业务码时只能看文案：余额类关键词归到积分耗尽，
            // 其余 4xx 一律当请求问题（不罚号）
            if text.contains("余额")
                || text.contains("积分")
                || text.contains("insufficient")
                || text.contains("quota")
            {
                ErrKind::CreditExhausted
            } else {
                ErrKind::BadRequest
            }
        }
        _ => ErrKind::Other,
    }
}

// ---------------------------------------------------------------------------
// 冷却表
// ---------------------------------------------------------------------------

/// 冷却的存储键。两档语义必须分开存：
///
/// - [`CoolKey::Account`]：账号整体不可用（余额耗尽 / 会话失效 / 账号级限流 / WAF）
/// - [`CoolKey::Model`]：只有这一个模型不可用（6004 模型级限流 / 11102 无此模型）
///
/// 合成一档会给两种行为都带上错误后果：按账号整体记会把本该还能服务的模型一起
/// 赶走；一律按模型记则账号级限流会被「换个模型」绕过，反而继续打同一个受限账号。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum CoolKey {
    Account(String),
    Model(String, String),
}

/// 时间节点的来源：决定日志怎么写、要不要提示「上游没给」
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitSource {
    /// 响应体文案里的「将在 … 重置」（实测网关走这条）
    Body,
    /// `Retry-After` 响应头
    RetryAfter,
    /// `X-RateLimit-Reset` 一类响应头
    ResetHeader,
    /// 上游没给 → 有界指数退避
    Backoff,
}

impl LimitSource {
    /// 是上游给的权威时刻吗（决定日志说「至 HH:MM」还是「按兜底 N 分钟」）
    fn from_upstream(self) -> bool {
        self != LimitSource::Backoff
    }
}

/// 一条冷却记录
#[derive(Clone, Copy, Debug)]
pub struct CooldownEntry {
    /// 解禁时刻（unix 毫秒）
    pub until_ms: i64,
    source: LimitSource,
    /// 这条记录是怎么来的（日志用）
    reason: &'static str,
    /// 硬冷却：调了必定失败（余额耗尽 / 会话失效 / 被限制）。
    /// 全冷却兜底时**跳过**硬冷却号——否则每次请求都拿它撞一遍 402。
    hard: bool,
}

fn cooldown() -> &'static Mutex<HashMap<CoolKey, CooldownEntry>> {
    static COOLDOWN: OnceLock<Mutex<HashMap<CoolKey, CooldownEntry>>> = OnceLock::new();
    COOLDOWN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 账号的健康计数（熔断 / 退避用）。与冷却表分开存：
/// 冷却表按「键」记结果，健康计数按「账号」记过程，两者的清零点不同。
#[derive(Default, Clone, Copy)]
struct AccountHealth {
    /// 连续 5xx 次数
    failures: u32,
    /// 连续软冷却次数（无重置时刻的 429）
    soft_streak: u32,
}

fn health() -> &'static Mutex<HashMap<String, AccountHealth>> {
    static HEALTH: OnceLock<Mutex<HashMap<String, AccountHealth>>> = OnceLock::new();
    HEALTH.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 当前墙钟毫秒。冷却用**墙钟**而非 `Instant`：上游给的是绝对时刻，
/// 只有存绝对时刻才能把「几点解禁 / 还剩多久」原样展示出来。
pub fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// 该账号此刻是否不可用（账号级冷却，或该模型的模型级冷却）。
///
/// 顺手清掉已过期的记录（表很小，全表 retain 比逐键判便宜）。
fn cooling(account_id: &str, model: Option<&str>) -> bool {
    let Ok(mut m) = cooldown().lock() else {
        return false;
    };
    let now = now_ms();
    m.retain(|_, e| e.until_ms > now);
    if m.contains_key(&CoolKey::Account(account_id.to_string())) {
        return true;
    }
    model.is_some_and(|mo| m.contains_key(&CoolKey::Model(account_id.to_string(), mo.to_string())))
}

/// 写入一条冷却记录（内部统一入口，负责夹取时长上下界）
fn write_cool(
    key: CoolKey,
    until_ms: i64,
    source: LimitSource,
    reason: &'static str,
    hard: bool,
) -> CooldownEntry {
    let now = now_ms();
    let entry = CooldownEntry {
        until_ms: until_ms.clamp(now + COOLDOWN_MIN_MS, now + COOLDOWN_MAX_MS),
        source,
        reason,
        hard,
    };
    if let Ok(mut m) = cooldown().lock() {
        m.insert(key, entry);
    }
    entry
}

/// 记一条**账号级**冷却
pub fn cool_account(
    account_id: &str,
    until_ms: i64,
    source: LimitSource,
    reason: &'static str,
) -> CooldownEntry {
    write_cool(CoolKey::Account(account_id.to_string()), until_ms, source, reason, false)
}

/// 记一条**模型级**冷却（只挡住这一个模型）
pub fn cool_model(
    account_id: &str,
    model: &str,
    until_ms: i64,
    source: LimitSource,
    reason: &'static str,
) -> CooldownEntry {
    write_cool(
        CoolKey::Model(account_id.to_string(), model.to_string()),
        until_ms,
        source,
        reason,
        false,
    )
}

/// 记一条**硬冷却**（调了必定失败，全冷却兜底会跳过它）
fn cool_hard(account_id: &str, dur_ms: i64, reason: &'static str) -> CooldownEntry {
    write_cool(
        CoolKey::Account(account_id.to_string()),
        now_ms() + dur_ms,
        LimitSource::Backoff,
        reason,
        true,
    )
}

/// 余额耗尽：账号级硬冷却
pub fn cool_credit_exhausted(account_id: &str) -> CooldownEntry {
    cool_hard(account_id, CREDIT_EXHAUSTED_MS, "余额耗尽")
}

/// 会话失效：账号级硬冷却（只能重新登录）
pub fn cool_session_dead(account_id: &str) -> CooldownEntry {
    cool_hard(account_id, SESSION_DEAD_MS, "会话失效")
}

/// 被上游限制：账号级硬冷却（只能走官方申诉）
pub fn cool_restricted(account_id: &str) -> CooldownEntry {
    cool_hard(account_id, RESTRICTED_MS, "账号被限制")
}

/// WAF 403：账号级短冷却
pub fn cool_waf(account_id: &str) -> CooldownEntry {
    cool_account(account_id, now_ms() + WAF_MS, LimitSource::Backoff, "WAF 拦截")
}

/// 上游 404：账号级短冷却，**不按限流退避**（偶发路径缺失不是限流信号）
pub fn cool_not_found(account_id: &str) -> CooldownEntry {
    cool_account(account_id, now_ms() + NOT_FOUND_MS, LimitSource::Backoff, "上游 404")
}

/// 有界指数退避：`base × 2^(streak-1)`，封顶 `cap`。
///
/// **已经在冷却中就直接返回 `None`，不推进 streak、不延长冷却**。
/// 这是「全池被推到 2h 封顶」的根治：用户重试或并发兜底探测每撞一次 429 就翻一倍，
/// 号会越重试越久不可用。
fn backoff(scope: CoolKey, account_id: &str, base_ms: i64, cap_ms: i64, reason: &'static str) -> Option<CooldownEntry> {
    if cooling(account_id, match &scope {
        CoolKey::Model(_, m) => Some(m.as_str()),
        CoolKey::Account(_) => None,
    }) {
        return None; // 冷却中：不延长
    }
    let streak = {
        let Ok(mut h) = health().lock() else { return None };
        let e = h.entry(account_id.to_string()).or_default();
        e.soft_streak += 1;
        e.soft_streak
    };
    let mut dur = base_ms;
    for _ in 1..streak {
        dur = dur.saturating_mul(2);
        if dur >= cap_ms {
            break;
        }
    }
    Some(write_cool(scope, now_ms() + dur.min(cap_ms), LimitSource::Backoff, reason, false))
}

/// 账号级有界退避
pub fn backoff_account(account_id: &str, base_ms: i64, reason: &'static str) -> Option<CooldownEntry> {
    backoff(CoolKey::Account(account_id.to_string()), account_id, base_ms, SOFT_CAP_MS, reason)
}

/// 模型级有界退避
pub fn backoff_model(
    account_id: &str,
    model: &str,
    base_ms: i64,
    reason: &'static str,
) -> Option<CooldownEntry> {
    backoff(
        CoolKey::Model(account_id.to_string(), model.to_string()),
        account_id,
        base_ms,
        SOFT_CAP_MS,
        reason,
    )
}

/// 5xx 熔断：连败达 [`BREAKER_THRESHOLD`] 才罚，`30m × 2ⁿ` 封顶 6h。
///
/// 未达阈值返回 `None`（只记数不冷却）——单次 5xx 是上游抖动，罚号纯属自伤。
pub fn note_failure(account_id: &str) -> Option<CooldownEntry> {
    let (failures, overflow) = {
        let Ok(mut h) = health().lock() else { return None };
        let e = h.entry(account_id.to_string()).or_default();
        e.failures += 1;
        let over = e.failures.saturating_sub(BREAKER_THRESHOLD);
        (e.failures, over)
    };
    if failures < BREAKER_THRESHOLD {
        return None;
    }
    let mut dur = BREAKER_BASE_MS;
    for _ in 0..overflow {
        dur = dur.saturating_mul(2).min(BREAKER_CAP_MS);
    }
    Some(write_cool(
        CoolKey::Account(account_id.to_string()),
        now_ms() + dur,
        LimitSource::Backoff,
        "连续 5xx 熔断",
        false,
    ))
}

/// 一次成功：清掉熔断计数与退避 streak，并解开该模型的负缓存。
///
/// 负缓存必须在这里清（而不是等 TTL 到期）：11102 是「官方说这个后端没有这个模型」，
/// 但模型上下线是动态的，一旦这个模型在该号上真的通了，就不该再避让 24h。
pub fn note_success(account_id: &str, model: Option<&str>) {
    if let Ok(mut h) = health().lock() {
        if let Some(e) = h.get_mut(account_id) {
            e.failures = 0;
            e.soft_streak = 0;
        }
    }
    if let Some(model) = model {
        if let Ok(mut m) = cooldown().lock() {
            m.remove(&CoolKey::Model(account_id.to_string(), model.to_string()));
        }
    }
}

/// 11102 负缓存：`(账号, 模型)` 避让，`6h` 起按命中次数 `×2` 封顶 `24h`。
///
/// 命中次数直接读该键的历史 `until_ms` 无法推出，所以借 `health` 的 `soft_streak`
/// 之外单独记在冷却条目里——这里用「同一键连续命中」的近似：只要该键还在表里
/// 就翻倍，被 `note_success` 或过期清掉后重新从 6h 起。
pub fn block_model(account_id: &str, model: &str, reason: &'static str) -> CooldownEntry {
    let key = CoolKey::Model(account_id.to_string(), model.to_string());
    let prev = cooldown().lock().ok().and_then(|m| m.get(&key).copied());
    let dur = match prev {
        Some(e) => {
            // 还在避让期内再次命中 → TTL 翻倍（半开到期后再撞才走到这里）
            let span = (e.until_ms - now_ms()).max(MODEL_BLOCK_BASE_MS);
            (span.saturating_mul(2)).min(MODEL_BLOCK_CAP_MS)
        }
        None => MODEL_BLOCK_BASE_MS,
    };
    write_cool(key, now_ms() + dur, LimitSource::Backoff, reason, false)
}

/// 日志里那半句「冷却 …」：带来源说明与剩余时长。
///
/// 上游给了权威墙钟才展示具体时刻——我们自己的退避时长展示成时刻只是噪音。
pub fn cooldown_note(entry: CooldownEntry) -> String {
    let mins = ((entry.until_ms - now_ms()).max(0) as f64 / 60_000.0).ceil() as i64;
    if entry.source.from_upstream() {
        format!("{}，至 {}（约 {mins} 分钟）", entry.reason, until_text(entry.until_ms))
    } else {
        format!("{}，{mins} 分钟", entry.reason)
    }
}

/// 解禁时刻的展示串：当天只给 `HH:MM`，跨天才补日期
pub fn until_text(until_ms: i64) -> String {
    let Some(t) = chrono::DateTime::from_timestamp_millis(until_ms) else {
        return "未知".into();
    };
    let local = t.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m-%d %H:%M").to_string()
    }
}

// ---------------------------------------------------------------------------
// 重置时刻解析
// ---------------------------------------------------------------------------

/// 解析 429 里的「重置时刻」，返回 (unix 毫秒, 来源)。
///
/// 上游实测（2026-09-14，`Server: APISIX/3.9.1`）：429 **不带 `Retry-After`**，
/// 时间节点只写在响应体中文文案里：
/// `"您的使用量已超出频率限制，将在 2026-09-14 19:35:25 UTC+8 重置…"`
/// 同一分钟内连发两次，返回的重置时刻逐字相同 —— 是绝对时刻，可直接当解禁时间。
///
/// 解析顺序：`Retry-After` → `X-RateLimit-Reset(-After)` → 响应体文案的时间戳。
pub fn limit_until_ms(
    headers: &[(String, String)],
    body: &[u8],
    now: i64,
) -> Option<(i64, LimitSource)> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    if let Some(ms) = header("retry-after").and_then(|v| parse_retry_after(v, now)) {
        return Some((ms, LimitSource::RetryAfter));
    }
    for name in ["x-ratelimit-reset-after", "x-ratelimit-reset"] {
        if let Some(ms) = header(name).and_then(|v| parse_reset_header(v, now)) {
            return Some((ms, LimitSource::ResetHeader));
        }
    }
    parse_reset_text(&String::from_utf8_lossy(body)).map(|ms| (ms, LimitSource::Body))
}

/// `Retry-After`：RFC 允许「秒数」或「HTTP-date」两种写法
fn parse_retry_after(v: &str, now: i64) -> Option<i64> {
    if let Ok(secs) = v.parse::<i64>() {
        return (secs >= 0).then(|| now + secs * 1000);
    }
    chrono::DateTime::parse_from_rfc2822(v)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// `X-RateLimit-Reset` 系：各网关语义不统一（秒差 / unix 秒 / unix 毫秒），
/// 按数量级判——> 1e12 当毫秒、> 1e9 当 unix 秒、否则当「还剩多少秒」。
fn parse_reset_header(v: &str, now: i64) -> Option<i64> {
    let n: f64 = v.trim().parse().ok()?;
    if !(n > 0.0) {
        return None;
    }
    Some(if n > 1e12 {
        n as i64
    } else if n > 1e9 {
        (n * 1000.0) as i64
    } else {
        now + (n * 1000.0) as i64
    })
}

/// 文案里的时间戳本体 `YYYY-MM-DD HH:MM:SS`
static RESET_AT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2}):(\d{2})").unwrap()
});
/// 紧跟在时间戳之后的时区偏移：`UTC+8` / `GMT-05:00` / `+08:00`
static RESET_TZ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:UTC|GMT)?\s*([+-])(\d{1,2})(?::?(\d{2}))?").unwrap());

/// 解析响应体文案里的重置时刻
fn parse_reset_text(text: &str) -> Option<i64> {
    let caps = RESET_AT_RE.captures(text)?;
    let num = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    let naive = chrono::NaiveDate::from_ymd_opt(num(1)? as i32, num(2)?, num(3)?)?
        .and_hms_opt(num(4)?, num(5)?, num(6)?)?;
    let offset_secs = caps
        .get(0)
        .and_then(|m| RESET_TZ_RE.captures(&text[m.end()..]))
        .and_then(|tz| {
            let sign = if &tz[1] == "-" { -1 } else { 1 };
            let h: i64 = tz[2].parse().ok()?;
            let m: i64 = tz.get(3).map_or(Some(0), |x| x.as_str().parse().ok())?;
            Some(sign * (h * 3600 + m * 60))
        });
    match offset_secs {
        Some(off) => Some(naive.and_utc().timestamp_millis() - off * 1000),
        None => naive
            .and_local_timezone(chrono::Local)
            .single()
            .map(|d| d.timestamp_millis()),
    }
}

// ---------------------------------------------------------------------------
// 候选筛选
// ---------------------------------------------------------------------------

/// 该账号在该模型上的解禁时刻 `(until_ms, 是否硬冷却)`。
fn release_of(account_id: &str, model: Option<&str>) -> Option<(i64, bool)> {
    let m = cooldown().lock().ok()?;
    let acc = m.get(&CoolKey::Account(account_id.to_string()));
    let mdl = model.and_then(|mo| m.get(&CoolKey::Model(account_id.to_string(), mo.to_string())));
    match (acc, mdl) {
        (Some(a), Some(b)) => Some((a.until_ms.max(b.until_ms), a.hard || b.hard)),
        (Some(a), None) => Some((a.until_ms, a.hard)),
        (None, Some(b)) => Some((b.until_ms, b.hard)),
        (None, None) => None,
    }
}

/// 全冷却兜底：在仍有解禁时刻的账号里挑最早的一个。
///
/// 硬冷却（余额耗尽 / 会话失效 / 被限制）**绝不参与**——调了必定失败，
/// 只会把「没有可用账号」变成一条误导性的 402。
fn pick_earliest_release<T>(
    items: &[T],
    release_of: impl Fn(&T) -> Option<(i64, bool)>,
) -> Option<T>
where
    T: Clone,
{
    items
        .iter()
        .filter_map(|a| release_of(a).filter(|(_, hard)| !hard).map(|(ms, _)| (ms, a)))
        .min_by_key(|(ms, _)| *ms)
        .map(|(_, a)| a.clone())
}

/// 路由排序的第一键：把「到期时刻」压到**本地自然日**（同一天 = 同一档）。
///
/// 界面上的到期时间本来就是日粒度（「X 天后过期」），按毫秒严格比会让
/// 「两个都显示 2 天后过期」的账号随机定序。
fn expiry_day(expiry_ms: Option<i64>) -> i64 {
    use chrono::{Datelike, TimeZone};
    expiry_ms
        .and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single())
        .map_or(i64::MAX, |t| t.date_naive().num_days_from_ce() as i64)
}

/// 「待过期积分」：**最早到期自然日**当天到期的资源包剩余合计。
///
/// 用它而不是总剩余：总剩余多但大半是远期包的账号，不该压住「快作废的多」的。
/// 退路：逐包明细缺失或没有到期时间时，退回总剩余。
fn expiring_credits(info: &ledger::CreditFact) -> f64 {
    let day = expiry_day(info.earliest_expiry_ms);
    if day == i64::MAX || info.packages.is_empty() {
        return info.credits.unwrap_or(0.0);
    }
    info.packages
        .iter()
        .filter(|p| expiry_day(p.expiry_ms) == day)
        .map(|p| p.remaining)
        .sum()
}

/// 路由排序键：到期自然日早者优先 → 同日内待过期积分少者优先
fn score(info: &ledger::CreditFact) -> (i64, i64) {
    (
        expiry_day(info.earliest_expiry_ms),
        (expiring_credits(info) * 100.0) as i64,
    )
}

/// 从候选里选出该用的账号下标。`infos` 与 `ids` 一一对应。
fn pick_index(ids: &[String], infos: &[ledger::CreditFact]) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score: Option<(i64, i64)> = None;
    for (i, info) in infos.iter().enumerate() {
        if info.credits == Some(0.0) {
            continue;
        }
        let s = score(info);
        if best_score.map_or(true, |cur| s < cur) {
            best_score = Some(s);
            best = Some(i);
        }
    }
    // 全员为 0 时退化为取第一个（让上游自己报错，比网关直接 503 更有信息量）
    best.or(if ids.is_empty() { None } else { Some(0) })
}

/// 台账里的积分读数是否过期（决定要不要重新打资源接口）。
fn snapshot_stale(at: Option<&str>) -> bool {
    let Some(at) = at else { return true };
    match chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|t| t.and_local_timezone(chrono::Local).single())
    {
        Some(t) => {
            chrono::Local::now().timestamp() - t.timestamp() >= SNAPSHOT_TTL.as_secs() as i64
        }
        None => true,
    }
}

/// 扣费候选集过滤（纯函数）：设置里勾了谁，谁才有资格被扣费。
///
/// - 勾选列表为空 = 不限制；
/// - 勾选的 id 一个都找不到（账号已删光）→ 退回全部，
///   宁可多扣也不能让网关直接瘫掉。
fn billing_candidates(
    accounts: &[accounts::Account],
    selected: &[String],
) -> Vec<accounts::Account> {
    if selected.is_empty() {
        return accounts.to_vec();
    }
    let picked: Vec<_> = accounts
        .iter()
        .filter(|a| selected.iter().any(|s| s == &a.id))
        .cloned()
        .collect();
    if picked.is_empty() {
        accounts.to_vec()
    } else {
        picked
    }
}

// ---------------------------------------------------------------------------
// 选号入口
// ---------------------------------------------------------------------------

/// 选出一个该用的账号。
///
/// `model` 是本轮请求要用的模型：冷却按 **账号 × 模型** 记账，选号必须知道模型是谁。
/// `ban` 是本轮已试败的账号（限流切换用），严格剔除；剔完为空返回 None。
pub async fn choose_account(
    dir: &Path,
    ban: &[String],
    model: Option<&str>,
) -> Option<accounts::Account> {
    let settings = accounts::load_settings(dir);
    // ⚠️ 整池同步必须在**读账号之前**：它会把云端那一份并进 accounts.json，
    // 而闸带回来的才是最新凭证。顺序颠倒 = 整轮请求都在用已作废的 token。
    crate::commands::sync_pool_if_bound(dir).await;
    let all = accounts::load_accounts(dir);
    if all.is_empty() {
        return None;
    }
    let candidates = billing_candidates(&all, &settings.billing_account_ids);
    let usable_accounts: Vec<_> = candidates
        .into_iter()
        .filter(|a| !ban.iter().any(|b| b == &a.id))
        .collect();
    if usable_accounts.is_empty() {
        return None;
    }

    // 先挑不在冷却里的；全都冷却时才兜底选「最早解禁」的那一个——
    // 直接 503 会让客户端以为网关挂了，而实际上只是暂时没号。
    let healthy: Vec<_> = usable_accounts
        .iter()
        .filter(|a| !cooling(&a.id, model))
        .cloned()
        .collect();
    let usable = if healthy.is_empty() {
        match pick_earliest_release(&usable_accounts, |a| release_of(&a.id, model)) {
            Some(a) => vec![a],
            None => {
                // 全员硬冷却（余额耗尽/会话失效）或从未有过健康记录 → 真的没号
                return None;
            }
        }
    } else {
        healthy
    };

    // 积分事实只看台账（账户管理页显示的就是它）；读数过期就顺手补拉一次
    let ids: Vec<String> = usable.iter().map(|a| a.id.clone()).collect();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let store = ledger::store(dir);
    let snaps: Vec<Option<ledger::CreditFact>> =
        usable.iter().map(|a| store.fact_of(&a.id)).collect();
    let mut readings: Vec<ledger::Reading> = Vec::new();
    for (i, acct) in usable.iter().enumerate() {
        if !snapshot_stale(snaps[i].as_ref().map(|f| f.at.as_str())) {
            continue;
        }
        let host = commands::account_host(acct);
        let view = fetch_resource_view(&API_CLIENT, &host, &acct.token).await;
        readings.push(ledger::Reading {
            id: acct.id.clone(),
            packages: view.packages,
            credits: view.credits,
            expiry_ms: view.earliest_expiry_ms,
        });
    }
    if !readings.is_empty() {
        if let Err(e) = store.apply(&readings, &now, ledger::Mode::Normal) {
            eprintln!("积分台账落盘失败：{e}");
        }
    }
    let infos: Vec<ledger::CreditFact> = usable
        .iter()
        .map(|a| store.fact_of(&a.id).unwrap_or_default())
        .collect();

    let picked = pick_index(&ids, &infos)?;
    let mut account = usable[picked].clone();
    // 单机模式下选中的账号临期就先续签（绑了池的在上面整池同步时已处理好）；失败不阻断
    if commands::ensure_fresh_token(&mut account).await.unwrap_or(false) {
        let mut merged = all;
        if let Some(a) = merged.iter_mut().find(|a| a.id == account.id) {
            *a = account.clone();
        }
        // refresh token 是一次性的：这里落盘失败必须响亮，否则重启后读回的是废票
        if let Err(e) = accounts::save_accounts(dir, &merged) {
            crate::logs::push(
                dir,
                crate::logs::CAT_RENEW,
                &account.name,
                false,
                format!("网关续签结果落盘失败：{e}"),
            );
            // 日志只有翻文件的人才看得见，而这里丢的是一条**不可逆的凭证轮换**：
            // 现在不吭声，过几天这个账号变成「需要重新登录」，没人会联想到这次写盘失败。
            crate::alerts::notify(
                dir,
                &format!("renew-persist:{}", account.id),
                crate::alerts::CRITICAL_COOLDOWN_MS,
                &format!(
                    "账号 {}：网关续签成功，但没能写进 accounts.json（{e}）。\n\
                     那枚旧 refresh token 已在服务端作废：本进程还在用新票，重启后读回来的是废票。\n\
                     现在能做的：确认磁盘没满、数据目录可写；绑了凭证池的话，下一轮整池同步会把新票传上去。",
                    account.name
                ),
            );
        }
    }
    Some(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{CreditFact, CreditPackage};

    fn fact(expiry: Option<i64>, credits: f64, pkgs: Vec<(Option<i64>, f64)>) -> CreditFact {
        CreditFact {
            credits: Some(credits),
            at: "2026-01-01 00:00:00".into(),
            earliest_expiry_ms: expiry,
            packages: pkgs
                .into_iter()
                .map(|(expiry_ms, remaining)| CreditPackage {
                    name: "p".into(),
                    remaining,
                    expiry_ms,
                })
                .collect(),
        }
    }

    fn acct(id: &str) -> accounts::Account {
        serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"{id}","token":"t"}}"#
        ))
        .expect("最小账号 JSON 应可反序列化")
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("a{i}")).collect()
    }

    #[test]
    fn earliest_expiry_wins() {
        let infos = vec![fact(Some(2_000_000_000_000), 100.0, vec![]), fact(Some(1_000_000_000_000), 10.0, vec![])];
        assert_eq!(pick_index(&ids(2), &infos), Some(1));
    }

    #[test]
    fn same_day_prefers_smaller_expiring_balance() {
        // 同一天到期，待过期积分少者优先
        let day = 1_800_000_000_000i64;
        let infos = vec![
            fact(Some(day), 500.0, vec![(Some(day), 500.0)]),
            fact(Some(day + 3_600_000), 20.0, vec![(Some(day + 3_600_000), 20.0)]),
        ];
        assert_eq!(pick_index(&ids(2), &infos), Some(1));
    }

    #[test]
    fn unknown_expiry_sorts_last() {
        let infos = vec![fact(None, 1000.0, vec![]), fact(Some(1_000_000_000_000), 1.0, vec![])];
        assert_eq!(pick_index(&ids(2), &infos), Some(1), "有到期日的必须排在前");
    }

    #[test]
    fn zero_balance_is_skipped_but_falls_back_when_all_zero() {
        let infos = vec![fact(Some(1_000_000_000_000), 0.0, vec![]), fact(Some(2_000_000_000_000), 5.0, vec![])];
        assert_eq!(pick_index(&ids(2), &infos), Some(1));
        let all_zero = vec![fact(Some(1_000_000_000_000), 0.0, vec![]), fact(Some(2_000_000_000_000), 0.0, vec![])];
        assert_eq!(pick_index(&ids(2), &all_zero), Some(0), "全员为 0 时退化取第一个");
    }

    // ---- 冷却档位 ----

    #[test]
    fn account_cooldown_hides_every_model() {
        let id = "acc-tier-account";
        cool_account(id, now_ms() + 60_000, LimitSource::Body, "账号级限流");
        assert!(cooling(id, Some("model-x")), "账号级冷却对任何模型都生效");
        assert!(cooling(id, Some("model-y")));
        assert!(cooling(id, None), "模型未知时账号级冷却也必须生效");
        assert!(!cooling("other-account", Some("model-x")));
    }

    #[test]
    fn model_cooldown_only_hides_that_model() {
        let id = "acc-tier-model";
        cool_model(id, "model-x", now_ms() + 60_000, LimitSource::Body, "模型级限流");
        assert!(cooling(id, Some("model-x")));
        assert!(!cooling(id, Some("model-y")), "换模型必须立刻回到候选集");
        assert!(!cooling(id, None), "模型级冷却不该因为「没带模型」而挡住整号");
        assert!(!cooling("other-account", Some("model-x")));
    }

    #[test]
    fn cooldown_clamps_both_ends() {
        // 上游给了一个过去的时刻 → 仍要冷却一小会儿
        let past = cool_account("acc-past", 0, LimitSource::Body, "测试");
        assert!(past.until_ms > now_ms());
        // 上游给了一个离谱的远期时刻 → 最多锁一天
        let far = cool_account("acc-far", now_ms() + 1_000 * 24 * 3600 * 1000, LimitSource::Body, "测试");
        assert!(far.until_ms <= now_ms() + COOLDOWN_MAX_MS + 5);
    }

    #[test]
    fn backoff_doubles_then_caps_and_never_extends_while_cooling() {
        let id = "acc-backoff";
        let first = backoff_account(id, SOFT_BASE_MS, "测试").expect("首次应写入");
        let span1 = first.until_ms - now_ms();
        // 冷却中再撞一次：不推进、不延长
        assert!(backoff_account(id, SOFT_BASE_MS, "测试").is_none(), "冷却中不得延长");
        assert_eq!(cooling(id, None), true);
        assert!((span1 - SOFT_BASE_MS).abs() < 2_000, "首次就是基数，不翻倍");
        // 解锁后再撞：翻倍
        note_success(id, None);
        // 手动把冷却清过期，模拟解禁
        if let Ok(mut m) = cooldown().lock() {
            m.remove(&CoolKey::Account(id.to_string()));
        }
        let second = backoff_account(id, SOFT_BASE_MS, "测试").expect("解禁后应可再写入");
        let span2 = second.until_ms - now_ms();
        assert!(span2 >= span1, "streak 未清零，第二次应不短于第一次");
    }

    #[test]
    fn hard_cooldown_is_excluded_from_fallback() {
        let a = acct("hard-a");
        let b = acct("hard-b");
        cool_credit_exhausted(&a.id); // 硬冷却，且解禁更早
        cool_account(&b.id, now_ms() + 3_600_000, LimitSource::Body, "软冷却");
        let picked = pick_earliest_release(&[a.clone(), b.clone()], |x| release_of(&x.id, None))
            .expect("至少有一个软冷却号可兜底");
        assert_eq!(picked.id, b.id, "余额耗尽的号即使解禁更早也不能兜底选中");
    }

    #[test]
    fn fallback_picks_earliest_release_when_all_cooling() {
        let a = acct("rel-a");
        let b = acct("rel-b");
        cool_account(&a.id, now_ms() + 3_600_000, LimitSource::Body, "测试");
        cool_account(&b.id, now_ms() + 60_000, LimitSource::Body, "测试");
        let picked = pick_earliest_release(&[a, b], |x| release_of(&x.id, None)).unwrap();
        assert_eq!(picked.id, "rel-b", "应选最早解禁的那个");
    }

    #[test]
    fn note_success_clears_model_block() {
        let id = "acc-block";
        block_model(id, "m1", "该号无此模型");
        assert!(cooling(id, Some("m1")));
        note_success(id, Some("m1"));
        assert!(!cooling(id, Some("m1")), "该模型真的通了就该立刻解避让");
    }

    #[test]
    fn breaker_only_trips_after_threshold() {
        let id = "acc-breaker";
        assert!(note_failure(id).is_none(), "第 1 次 5xx 不罚");
        assert!(note_failure(id).is_none(), "第 2 次 5xx 不罚");
        assert!(note_failure(id).is_some(), "第 3 次才熔断");
        assert!(cooling(id, None));
        note_success(id, None);
        assert!(note_failure(id).is_none(), "成功之后计数清零，重新从 1 数");
    }

    // ---- 错误分类 ----

    #[test]
    fn classifies_by_business_code_first() {
        // 上游把语义写在 code 里，HTTP 状态可能统一 400
        let cases: [(&str, u16, ErrKind); 5] = [
            (
                r#"{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}"#,
                400,
                ErrKind::ChannelRefused,
            ),
            (
                r#"{"code":11102,"msg":"model [x] service info not found"}"#,
                400,
                ErrKind::ModelBlocked,
            ),
            (
                r#"{"code":11101,"msg":"Non-stream chat request is currently not supported"}"#,
                400,
                ErrKind::BadRequest,
            ),
            (r#"{"code":12153,"msg":"session dead"}"#, 400, ErrKind::SessionDead),
            (
                r#"{"code":6004,"msg":"您的使用量已超出频率限制"}"#,
                429,
                ErrKind::RateModel,
            ),
        ];
        for (body, status, want) in cases {
            assert_eq!(classify(status, body.as_bytes()), want, "body={body}");
        }
    }

    #[test]
    fn distinguishes_restricted_from_rate_limit_on_same_code() {
        // 11140 两种语义只能靠文案分
        let restricted = r#"{"code":11140,"msg":"request illegal"}"#;
        let rate = r#"{"code":11140,"msg":"rate-limiting"}"#;
        assert_eq!(classify(403, restricted.as_bytes()), ErrKind::Restricted);
        assert_eq!(classify(429, rate.as_bytes()), ErrKind::RateAccount);
    }

    #[test]
    fn falls_back_to_status_when_code_is_unknown() {
        assert_eq!(classify(402, b"payment required"), ErrKind::CreditExhausted);
        assert_eq!(classify(429, b""), ErrKind::RateAccount);
        assert_eq!(classify(403, b"blocked"), ErrKind::Waf);
        assert_eq!(classify(404, b""), ErrKind::NotFound);
        assert_eq!(classify(502, b"bad gateway"), ErrKind::Server);
    }

    #[test]
    fn request_side_errors_are_not_penalised_and_not_retried() {
        for kind in [ErrKind::BadRequest, ErrKind::Other] {
            assert!(!kind.penalizes(), "{:?} 不该罚号", kind);
            assert!(!kind.worth_retry(), "{:?} 换号也是一样的错", kind);
        }
        // 通道校验：不罚号，但值得换个号试
        assert!(!ErrKind::ChannelRefused.penalizes());
        assert!(ErrKind::ChannelRefused.worth_retry());
        // 余额/限流/5xx 都要罚号
        for kind in [
            ErrKind::CreditExhausted,
            ErrKind::RateModel,
            ErrKind::RateAccount,
            ErrKind::Server,
        ] {
            assert!(kind.penalizes(), "{:?} 必须罚号", kind);
            assert!(kind.worth_retry());
        }
    }

    // ---- 重置时刻解析 ----

    #[test]
    fn parses_reset_time_from_gateway_copywriting() {
        let body = r#"{"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-14 19:35:25 UTC+8 重置，您也可以切换其他模型继续使用。"}"#;
        let (ms, src) = limit_until_ms(&[], body.as_bytes(), now_ms()).expect("文案里有时间戳");
        assert_eq!(src, LimitSource::Body);
        let text = until_text(ms);
        assert!(text.ends_with("19:35") || text.ends_with("11:35"), "实际解析出：{text}");
    }

    #[test]
    fn retry_after_header_wins_over_body() {
        let headers = vec![("Retry-After".to_string(), "120".to_string())];
        let body = "将在 2099-01-01 00:00:00 UTC+8 重置";
        let (ms, src) = limit_until_ms(&headers, body.as_bytes(), now_ms()).unwrap();
        assert_eq!(src, LimitSource::RetryAfter);
        assert!(ms <= now_ms() + 121_000);
    }

    #[test]
    fn billing_filter_falls_back_when_selection_matches_nothing() {
        let accts = vec![acct("id0"), acct("id1")];
        assert_eq!(billing_candidates(&accts, &[]).len(), 2, "空 = 全部");
        assert_eq!(billing_candidates(&accts, &["id1".into()]).len(), 1);
        assert_eq!(
            billing_candidates(&accts, &["gone".into()]).len(),
            2,
            "勾了但一个都找不到 → 退回全部，不能把网关搞瘫"
        );
    }
}
