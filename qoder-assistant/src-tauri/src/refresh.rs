//! Access token 续签：用 refresh token 换新一对 token。
//!
//! Qoder 与 CodeBuddy 是两套完全不同的协议。Qoder 的续签走设备令牌族：
//!
//! ```text
//! POST https://openapi.qoder.sh/api/v1/deviceToken/refresh
//! body: { "refresh_token": "<refreshToken>" }
//! 头（Bx）: Accept: application/json
//!           Authorization: Bearer <token>
//!           Cosy-ClientType: "10"
//!           User-Agent: Qoder
//! ```
//!
//! 响应直接平铺 token 对及其有效期：
//! ```json
//! { "token": "…", "refresh_token": "…",
//!   "expires_at": 1_800_000_000_000, "refresh_token_expires_at": … }
//! ```
//! 有效期可能是绝对时间戳（秒/毫秒）或相对秒数（`expires_in` / `refresh_token_expires_in`）。
//! 失败会在顶层给 `reason`（`expired` / `rejected` 等），并让会话失效。
//!
//! ⚠️ **两个 token 的有效期都要收下来**。`expires_at` 决定「什么时候该续」，
//! `refresh_token_expires_at` 决定「这条续签链还能活多久」—— 后者归零，本账号就再也
//! 换不出新 token 了。
//!
//! 设计取舍：续签是「尽力而为」的旁路——失败仅影响本次续签（大概率是 refresh token 已过期），
//! 由调用方决定如何呈现；成功后由调用方决定是否原子写回 `auth.v1.dat`（见 `auth_file::apply_refresh`）。
//!
//! ⚠️ 但「失败」有两种，只有一种该重试：接口打不通是暂时的，而**服务端明确拒了这份
//! refresh token**（顶层 `reason: "expired"` / `"rejected"`）是怎么重试都同一结论的死链。
//! 整池同步必须把它们分开，见 [`RefreshError::dead`] 与 `crate::broker::sync`。

use crate::timeutil::token_expiry;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

/// 一次续签失败：文字给人看，[`dead`](RefreshError::dead) 给程序做决定。
#[derive(Debug, Clone)]
pub struct RefreshError {
    pub text: String,
    /// 这条链是否已经**死透** —— 见模块头那条分隔。
    ///
    /// `true` 只在「请求发出去了、服务端认得我们并明确拒了这份 refresh token」时成立。
    /// 网络失败、返回体看不懂都算 `false`：那些确实值得再来一次。
    pub dead: bool,
}

impl RefreshError {
    pub(crate) fn transient(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dead: false,
        }
    }
    pub(crate) fn dead(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dead: true,
        }
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// 一次成功续签的产物。
#[derive(Serialize, Clone, Debug)]
pub struct Refreshed {
    pub token: String,
    pub refresh_token: Option<String>,
    /// 折算后的 access token 绝对过期时间（毫秒）；拿不到任何有效期信息时为 None
    pub expires_at: Option<i64>,
    /// 折算后的 refresh token 绝对过期时间（毫秒）；同样可能是 None
    pub rt_expires_at: Option<i64>,
}

/// 续签阈值：剩余有效期不足 48 小时就自动续一次。
///
/// 取 48h 而不是 24h 的理由：access token 实际有效期长，定时扫描小时级一跳，
/// 取两天余量能保证「即使连续几天没开应用、扫描又恰好错过」，token 也不会中途失效。
pub const REFRESH_THRESHOLD_MS: i64 = 48 * 60 * 60 * 1000;

// 这里曾有 `const OPENAPI_HOST = crate::auth_file::OPENAPI_HOST`（固定指向国际版的
// openapi 域）。续签基址现在由 `region::Region::openapi_base` 按**账号自己的区域**
// 给出 —— 见 `refresh` 的 `region` 参数。国内版的续签同样是 `/api/v1/deviceToken/refresh`，
// 只是域换成 `openapi.qoder.com.cn`（路径同构，实测两域都存在该路由）。

/// Qoder 客户端类型：desktop app（`clientType:10, businessProduct:app, sessionType:app`）
const COSY_CLIENT_TYPE: &str = "10";

/// 是否「值得」续签：未知有效期不动（无从判断），已过期或剩余不足 48 小时则续。
///
/// ⚠️ 本项目的 access token 是**不透明串**（`dt-…`，不是 JWT），到期时间只能来自
/// `expires_at` 这一个字段 —— 没有「从票里读出来」这条路（traework 那边有，它的票是 JWT）。
/// 所以这里的兜底办法是**换一个事实来源**：续签响应每次都会带回新的 `expires_at`，
/// 只要有一条路径把它收下（[`crate::commands::refresh_account_in_place`]、授权、导入、
/// 采纳登录文件的 [`crate::commands::adopt_file_sessions`]），这个字段就不会空。
/// 而**空着的账号等于「永远不会被自动续」**：`should_refresh(None)` 恒 `false`，
/// 界面上不会有任何报错 —— 它只是安静地过期。
pub fn should_refresh(expires_at: Option<i64>, now_ms: i64) -> bool {
    matches!(expires_at, Some(e) if e - now_ms < REFRESH_THRESHOLD_MS)
}

/// **是否该尝试续签**：进了续签窗口，**或已被上游判死**（`invalidated_at` 有值）。
///
/// 「已被判死」必须单列：被吊销的票 `expires_at` 可能还在未来，按窗口判断它永远轮不到续，
/// 而续签是它唯一的自救通道（2026-10-08 traework「本地死票锁死」事故的另一半）。
pub fn refresh_due(acct: &crate::accounts::Account, now_ms: i64) -> bool {
    should_refresh(acct.expires_at, now_ms) || acct.invalidated_at.is_some()
}

/// 死链（`RefreshError::dead`）后的冷却时长：重试一百次也是同一个结论。
pub const DEAD_RENEW_BLOCK_MS: i64 = 6 * 3600 * 1000;

/// 该账号是否仍在续签冷却期内。
///
/// 冷却写在账号上并**随 accounts.json 落盘**（[`Account::renew_blocked_until`]）——
/// 不落盘的话应用一重启就失忆，死链每轮都去打一个必然被拒的接口。
pub fn renew_blocked(acct: &crate::accounts::Account, now_ms: i64) -> bool {
    acct.renew_blocked_until.is_some_and(|until| until > now_ms)
}

/// 这条链的**续签能力**是否已经过期（`rt_expires_at` 归一到毫秒后与 now 比）。
///
/// 不知道就返回 `false` —— 没有证据就别放弃这条链。而已经知道它过期了还去打接口，
/// 只会换回一次明确的拒绝，并把「本机在签」的闸白白占一轮。
pub fn rt_is_expired(rt_expires_at: Option<i64>, now_ms: i64) -> bool {
    norm_rt_ms(rt_expires_at)
        .map(|ms| ms <= now_ms)
        .unwrap_or(false)
}

/// `rt_expires_at` 归一成毫秒（历史数据里秒级写法也存过）；不知道就 `None`。
///
/// 单独给出来是因为调用点除了「过没过期」还要**把那个时间说给人听**，
/// 而重新解析一遍会出现两套归一规则。
pub fn norm_rt_ms(rt_expires_at: Option<i64>) -> Option<i64> {
    let v = rt_expires_at?;
    crate::timeutil::norm_ts(Some(&serde_json::Value::from(v)))
}

/// 顶层 `reason` 里「这张票没了」的几种说法。命中任一才判死链。
///
/// 为什么不只认 `expired`：`rejected` 同样是服务端对这张票的表态，而漏判的代价是
/// **一个坏账号反复把整池拖进冷却**（见 `crate::broker::sync`）。
/// 反过来，把 `transient` 这类也算成死链，代价是一条还能救的链被我们放弃 ——
/// 所以判据宁窄勿宽：认不出来的 reason 一律当「值得再试」。
const DEAD_REASONS: &[&str] = &["expired", "rejected", "invalid_grant", "revoked"];

/// 业务失败分级：`reason` 命中 [`DEAD_REASONS`] 才算死链。
fn dead_by_reason(reason: &str) -> bool {
    let r = reason.trim().to_lowercase();
    DEAD_REASONS.iter().any(|k| r == *k)
}

/// 续签响应解析：Qoder 返回**平铺**的 token 对（不套 `code/data`）。
fn parse_response(v: &Value, now_ms: i64) -> Result<Refreshed, RefreshError> {
    // 顶层 reason，业务失败统一在这（expired / rejected / transient…）
    if let Some(reason) = v.get("reason").and_then(Value::as_str) {
        let text = format!("续签失败：{reason}");
        return Err(if dead_by_reason(reason) {
            RefreshError::dead(text)
        } else {
            RefreshError::transient(text)
        });
    }
    let token = str_of(v, &["token", "device_token", "accessToken", "access_token"]);
    if token.is_empty() {
        // 答复看不懂 ≠ 票没了：留着下一轮再试
        return Err(RefreshError::transient("续签响应缺少 token"));
    }
    let expires_at = token_expiry(
        v,
        &["expires_at", "expiresAt", "expire_time"],
        &["expires_in", "expiresIn"],
        now_ms,
    );
    let rt_expires_at = token_expiry(
        v,
        &["refresh_token_expires_at", "refreshTokenExpiresAt"],
        &["refresh_token_expires_in", "refreshTokenExpiresIn"],
        now_ms,
    );
    let rt = str_of(v, &["refresh_token", "refreshToken"]);
    Ok(Refreshed {
        token,
        refresh_token: if rt.is_empty() { None } else { Some(rt) },
        expires_at,
        rt_expires_at,
    })
}

fn str_of(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|k| v.get(*k))
        .find_map(|x| match x {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// 发起一次续签。`region` 取**账号自己的**区域 —— 续签打的是该区域的 openapi 域：
/// 用另一套部署换回来的 token 在账号所属的网关上无效，而续签失败会表现成
/// 「这个账号突然签到全失败」，与网络问题的症状一模一样。
pub async fn refresh(
    region: crate::region::Region,
    token: &str,
    refresh_token: &str,
) -> Result<Refreshed, RefreshError> {
    let url = format!("{}/api/v1/deviceToken/refresh", region.openapi_base());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| RefreshError::transient(format!("初始化 HTTP 客户端失败：{e}")))?;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        "cosy-clienttype",
        reqwest::header::HeaderValue::from_static(COSY_CLIENT_TYPE),
    );
    let resp = client
        .post(&url)
        .headers(headers)
        .bearer_auth(token)
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .map_err(|e| RefreshError::transient(format!("请求续签接口失败：{e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        RefreshError::transient(format!("续签接口返回非 JSON（HTTP {status}）"))
    })?;
    match parse_response(&v, chrono::Utc::now().timestamp_millis()) {
        Ok(r) => Ok(r),
        // HTTP 非 2xx 且无 reason → 包一层状态码，便于调用方判断；死链标记要保住
        Err(e) if !status.is_success() => Err(RefreshError {
            text: format!("{}（HTTP {status}）", e.text),
            dead: e.dead,
        }),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refreshes_only_when_expiry_is_known_and_near() {
        let now = 1_700_000_000_000;
        assert!(should_refresh(Some(now - 1), now));
        assert!(should_refresh(Some(now + REFRESH_THRESHOLD_MS - 1), now));
        assert!(!should_refresh(Some(now + REFRESH_THRESHOLD_MS + 1), now));
        assert!(!should_refresh(Some(now + REFRESH_THRESHOLD_MS), now));
        assert!(!should_refresh(None, now));
    }

    #[test]
    fn parses_flat_qoder_response() {
        let now = 1_700_000_000_000;
        let v = serde_json::json!({
            "token": "new-at",
            "refresh_token": "new-rt",
            "expires_at": 1_800_000_000_000i64,
            "refresh_token_expires_at": 1_750_000_000_000i64
        });
        let r = parse_response(&v, now).unwrap();
        assert_eq!(r.token, "new-at");
        assert_eq!(r.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(r.expires_at, Some(1_800_000_000_000));
        assert_eq!(r.rt_expires_at, Some(1_750_000_000_000));
        assert!(r.rt_expires_at < r.expires_at);
    }

    #[test]
    fn parses_relative_expiry_and_seconds_timestamps() {
        let now = 1_700_000_000_000;
        // 相对秒数 + 秒级绝对时间戳都要能折算/归一
        let v = serde_json::json!({
            "token": "at",
            "expires_in": 3600,
            "refresh_token_expires_in": 86400
        });
        let r = parse_response(&v, now).unwrap();
        assert_eq!(r.expires_at, Some(now + 3_600_000));

        let v2 = serde_json::json!({ "token": "at", "expire_time": 1_800_000_000 });
        assert_eq!(parse_response(&v2, now).unwrap().expires_at, Some(1_800_000_000_000));
    }

    #[test]
    fn reports_business_reason_and_missing_token() {
        let e = parse_response(&serde_json::json!({"reason": "expired"}), 0).unwrap_err();
        assert!(e.text.contains("expired"), "{e}");
        assert!(parse_response(&serde_json::json!({}), 0).is_err());
        assert!(parse_response(&serde_json::json!({"code": 0}), 0).is_err());
    }

    #[test]
    fn only_an_explicit_refusal_counts_as_a_dead_chain() {
        // 服务端点名这张票没了 → 死链；其余失败（哪怕带 reason）都还值得再试一次
        for reason in ["expired", "rejected", "invalid_grant", "REVOKED"] {
            let e = parse_response(&serde_json::json!({"reason": reason}), 0).unwrap_err();
            assert!(e.dead, "{reason} 应判死链");
        }
        for reason in ["transient", "rate_limited", "maintenance"] {
            let e = parse_response(&serde_json::json!({"reason": reason}), 0).unwrap_err();
            assert!(!e.dead, "{reason} 不该判死链");
        }
        // 答复看不懂（缺 token）同样是「再来一次」，不是死刑
        assert!(!parse_response(&serde_json::json!({}), 0)
            .unwrap_err()
            .dead);
    }

    #[test]
    fn an_expired_refresh_token_is_never_worth_a_request() {
        let now = 1_700_000_000_000;
        assert!(rt_is_expired(Some(now - 1), now));
        assert!(rt_is_expired(Some(now), now));
        assert!(rt_is_expired(Some(1_700_000_000 - 1), now)); // 秒级也要归一后比较
        assert!(!rt_is_expired(Some(now + 1), now));
        // 不知道就不放弃这条链 —— 没有证据就别替服务端做决定
        assert!(!rt_is_expired(None, now));
    }

    /// 真实接口冒烟：用本机 auth.v1.dat 里的 refresh token 换一次新凭证。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_refresh_endpoint() {
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        let rt = a
            .refresh_token
            .clone()
            .expect("登录文件里应带 refresh token");
        let r = refresh(a.region, &a.token, &rt)
            .await
            .unwrap_or_else(|e| panic!("续签失败: {e}"));
        println!(
            "续签成功：新 token 长度={}，expires_at={:?}，rt_expires_at={:?}",
            r.token.len(),
            r.expires_at,
            r.rt_expires_at
        );
        assert!(!r.token.is_empty());
    }
}