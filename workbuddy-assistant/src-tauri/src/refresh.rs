//! Access token 续签：用 refresh token 换新 access token。
//!
//! 官方接口（与 WorkDaddy 的 `token-refresh.js` 同源）：
//!
//! ```text
//! POST {host}/v2/plugin/auth/token/refresh
//! X-Refresh-Token: <refreshToken>
//! Authorization:  Bearer <accessToken>   （可选，带上更稳）
//! body: {}
//! ```
//!
//! 响应 `data` 形如：
//! ```json
//! { "accessToken": "…", "refreshToken": "…", "expiresIn": 5184000,
//!   "refreshExpiresIn": 2592000, "tokenType": "Bearer", "scope": "…" }
//! ```
//!
//! 实测**只给相对秒数**（`expiresIn` / `refreshExpiresIn`），没有绝对时间戳，
//! 所以过期时间要由调用时刻折算，不能把 `expiresIn` 当时间戳用。
//!
//! ⚠️ **两个 token 的有效期都要收下来**。`expiresIn` 决定「什么时候该续」，
//! `refreshExpiresIn` 决定「这条续签链还能活多久」—— 后者一旦归零，本账号就再也换不出
//! 新 token 了，而那两个时间点相差很远（实测 60 天 vs 30 天）。只留前者的话，
//! 上传给管家的凭证里就缺了最关键的那个刻度。
//!
//! 设计取舍：续签是「尽力而为」的旁路——失败不影响签到主流程，
//! 只是这次仍用旧 token 去试（过期了自然会得到明确的失败提示）。
//!
//! ⚠️ 但「失败」有两种，只有一种该重试：接口打不通是暂时的，而**服务端明确拒了这份
//! refresh token**（`code=40001「refresh token 已失效」`）是怎么重试都同一结论的死链。
//! 整池同步必须把它们分开，见 [`RefreshError::dead`] 与 `crate::broker::sync`。

use crate::oauth::{access_expiry, refresh_expiry};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

/// 续签阈值：剩余有效期不足 48 小时就自动续一次。
///
/// 取 48h 而不是 24h 的理由：access token 实际有效期 60 天，定时扫描是 12 小时一跳，
/// 取两天余量能保证「即使连续几天没开应用、扫描又恰好错过」，token 也不会中途失效。
pub const REFRESH_THRESHOLD_MS: i64 = 48 * 60 * 60 * 1000;

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
        Self { text: text.into(), dead: false }
    }
    pub(crate) fn dead(text: impl Into<String>) -> Self {
        Self { text: text.into(), dead: true }
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Refreshed {
    pub token: String,
    pub refresh_token: Option<String>,
    /// 折算后的 access token 绝对过期时间（毫秒）；拿不到任何有效期信息时为 None
    pub expires_at: Option<i64>,
    /// 折算后的 refresh token 绝对过期时间（毫秒）；同样可能是 None
    pub rt_expires_at: Option<i64>,
}

/// 是否「值得」续签：未知有效期不动（无从判断），已过期或剩余不足 48 小时则续。
///
/// ⚠️ 别直接拿 `account.expires_at` 调它 —— 那个字段会因为「导入时没带」「云端那条
/// 存的就是 null」而空着，空值在这里等于**永远不续**，票就默默到期了。
/// 调用方要用 [`refresh_due`]，它会先退回票里自己声明的到期时间。
pub fn should_refresh(expires_at: Option<i64>, now_ms: i64) -> bool {
    matches!(expires_at, Some(e) if e - now_ms < REFRESH_THRESHOLD_MS)
}

/// 时间戳归一：各处混发秒与毫秒，小于 1e12 一律当秒。
pub fn norm_ms(v: i64) -> i64 {
    if v < 1_000_000_000_000 {
        v * 1000
    } else {
        v
    }
}

/// access token **自己声明**的到期时间（JWT `exp`，毫秒）；不是 JWT 就读不出来。
///
/// 本项目的票是 Keycloak 签发的 JWT（`checkin::issuer_host` 靠同一份载荷取 `iss`），
/// 所以这份事实一直就在票里。签到链路读的是 `iss`，续签判据读的却是**账号上的字段**，
/// 于是「字段空 → 永不续签」这条死路一直没被发现。
pub fn token_expiry_ms(token: &str) -> Option<i64> {
    let part = token.split('.').nth(1)?;
    let mut padded = part.replace('-', "+").replace('_', "/");
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = STANDARD.decode(padded).ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    payload.get("exp").and_then(Value::as_i64).map(norm_ms)
}

/// 这个凭证什么时候到期：账号上的字段与票里自己写的 `exp`，**取更早的那一个**。
///
/// 两个来源都认、并且往早里取，是因为两边的代价不对称：
/// 提前一次续签只多打一趟请求（而且整池有闸，不会两台机器同时换），
/// 而晚一次续签就是「票已经死了、这个账号只能重登」。
/// 本机实测两个来源本来就不重合（waxiloao：字段 11-11、票 11-12）。
pub fn expiry_ms(expires_at: Option<i64>, token: &str) -> Option<i64> {
    match (expires_at, token_expiry_ms(token)) {
        (Some(f), Some(e)) => Some(f.min(e)),
        (Some(f), None) => Some(f),
        (None, Some(e)) => Some(e),
        (None, None) => None,
    }
}

/// [`should_refresh`] 该有的样子：把「字段空」这个坑先补上再判。
pub fn refresh_due(expires_at: Option<i64>, token: &str, now_ms: i64) -> bool {
    should_refresh(expiry_ms(expires_at, token), now_ms)
}

/// 这条链的**续签能力**是否已经过期（`rt_expires_at` 归一到毫秒后与 now 比）。
///
/// 不知道就返回 `false` —— 没有证据就别放弃这条链。而已经知道它过期了还去打接口，
/// 只会换回一次明确的拒绝，并把「本机在签」的闸白白占一轮。
pub fn rt_is_expired(rt_expires_at: Option<i64>, now_ms: i64) -> bool {
    rt_expires_at.map(|e| norm_ms(e) <= now_ms).unwrap_or(false)
}

fn biz_ok(v: &Value) -> bool {
    matches!(v.get("code").and_then(Value::as_i64), Some(0) | Some(200))
}

/// 服务端说「这张票已经没了」的几种写法。命中任一才判死链。
///
/// 为什么不按 code 区间判（比如「4xxxx 都算」）：本接口实测到的只有 `40001 refresh token
/// 已失效` 一个码，把猜测写成判据的代价是**一台好机器从此不再替这个账号续签** ——
/// 而那本来只差一次成功的请求。判据宁可漏判（多试一次）不可误判（放弃一条活链）。
const DEAD_TOKEN_MARKS: &[&str] = &[
    "失效",
    "过期",
    "作废",
    "无效",
    "重新登录",
    "invalid_grant",
    "invalid refresh token",
    "refresh token expired",
    "token has expired",
];

/// 业务拒绝是否等于「这条链死透了」。
fn refusal_says_dead_token(msg: &str, code: Option<i64>) -> bool {
    if code == Some(40001) {
        return true;
    }
    let m = msg.to_lowercase();
    DEAD_TOKEN_MARKS.iter().any(|k| m.contains(k))
}

fn str_of(data: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|k| data.get(*k))
        .find_map(|x| match x {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// 解析续签响应：`data.accessToken` 必填，缺失即视为失败。
pub(crate) fn parse_refresh_response(v: &Value, now_ms: i64) -> Result<Refreshed, RefreshError> {
    if !biz_ok(v) {
        let msg = str_of(v, &["msg", "message", "error"]);
        let code = v.get("code").and_then(Value::as_i64);
        // 服务端**听懂了**我们并给了个业务拒绝。只有它明确说这张票没了才算死链。
        let text = if msg.is_empty() {
            format!("续签失败（code={code:?}）")
        } else {
            format!("{msg}（code={code:?}）")
        };
        return Err(if refusal_says_dead_token(&msg, code) {
            RefreshError::dead(text)
        } else {
            RefreshError::transient(text)
        });
    }
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let token = str_of(&data, &["accessToken", "access_token"]);
    if token.is_empty() {
        return Err(RefreshError::transient("续签响应缺少 accessToken"));
    }
    // 两个 token 各取各的有效期。规则本体在 `oauth::token_expiry` ——
    // 授权接口与续签接口用的是同一套字段约定，所以定义只有那一处。
    let expires_at = access_expiry(&data, now_ms);
    let rt_expires_at = refresh_expiry(&data, now_ms);
    let refresh_token = {
        let r = str_of(&data, &["refreshToken", "refresh_token"]);
        if r.is_empty() { None } else { Some(r) }
    };
    Ok(Refreshed {
        token,
        refresh_token,
        expires_at,
        rt_expires_at,
    })
}

/// 发起一次续签。`host` 为账号所属域（如 `https://www.workbuddy.cn`）。
///
/// 返回 [`RefreshError`]：只有服务端明确拒票那一种算死链，其余都值得再试。
pub async fn refresh(host: &str, token: &str, refresh_token: &str) -> Result<Refreshed, RefreshError> {
    let url = format!("{}/v2/plugin/auth/token/refresh", host.trim_end_matches('/'));
    // 走插件授权族那一套头（`http::client_headers()`：Accept + Accept-Language），
    // **不声明 `x-client-platform`**——续签端点是 `/v2/plugin/auth/*`，与 oauth 的
    // state / token 同族，不是计费接口。这条路历史上本来就不带它、续签一直正常；
    // 上一轮「统一身份」时顺手给加上了，反倒造出「同族两套头」的不一致。
    let client = reqwest::Client::builder()
        .timeout(crate::http::TIMEOUT)
        .user_agent(crate::http::UA)
        .default_headers(crate::http::client_headers())
        .build()
        .map_err(|e| RefreshError::transient(format!("初始化 HTTP 客户端失败：{e}")))?;
    let resp = client
        .post(&url)
        .header("X-Refresh-Token", refresh_token)
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| RefreshError::transient(format!("请求续签接口失败：{e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        RefreshError::transient(format!("续签接口返回非 JSON（HTTP {status}）"))
    })?;
    parse_refresh_response(&v, chrono::Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refreshes_only_when_expiry_is_known_and_near() {
        let now = 1_700_000_000_000;
        // 已知有效期：已过期 / 剩余不足 48h → 续
        assert!(should_refresh(Some(now - 1), now));
        assert!(should_refresh(Some(now + REFRESH_THRESHOLD_MS - 1), now));
        // 剩余超过 48h → 不折腾
        assert!(!should_refresh(Some(now + REFRESH_THRESHOLD_MS + 1), now));
        // 刚好 48h 整：边界上不续（下一跳扫描还会再判一次，宁可少打一次请求）
        assert!(!should_refresh(Some(now + REFRESH_THRESHOLD_MS), now));
        // 未知有效期 → 不折腾（无从判断，避免每次签到都白跑一次请求）
        assert!(!should_refresh(None, now));
    }

    #[test]
    fn threshold_is_forty_eight_hours() {
        assert_eq!(REFRESH_THRESHOLD_MS, 48 * 60 * 60 * 1000);
    }

    #[test]
    fn parses_refresh_response_with_relative_expiry() {
        let now = 1_700_000_000_000;
        let v = json!({"code": 0, "data": {
            "accessToken": "new-at", "refreshToken": "new-rt", "expiresIn": 5_184_000
        }});
        let r = parse_refresh_response(&v, now).unwrap();
        assert_eq!(r.token, "new-at");
        assert_eq!(r.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(r.expires_at, Some(now + 5_184_000_000));
    }

    #[test]
    fn parses_both_token_expiries_from_the_same_response() {
        // 真实响应给的是两个**时长不同**的相对秒数：AT 60 天、RT 30 天。
        // 只认 expiresIn 会让 refresh token 的有效期永远为空，
        // 而那个刻度正是「这条续签链还能活多久」的唯一答案。
        let now = 1_700_000_000_000;
        let v = json!({"code": 0, "data": {
            "accessToken": "at", "refreshToken": "rt",
            "expiresIn": 5_184_000, "refreshExpiresIn": 2_592_000
        }});
        let r = parse_refresh_response(&v, now).unwrap();
        assert_eq!(r.expires_at, Some(now + 5_184_000_000));
        assert_eq!(r.rt_expires_at, Some(now + 2_592_000_000));
        assert!(r.rt_expires_at < r.expires_at, "RT 短于 AT，这正是要分开存的原因");
    }

    #[test]
    fn prefers_absolute_expiry_over_relative() {
        let v = json!({"code": 200, "data": {
            "access_token": "at", "expires_at": 1_800_000_000_000i64, "expires_in": 60
        }});
        let r = parse_refresh_response(&v, 1_700_000_000_000).unwrap();
        assert_eq!(r.expires_at, Some(1_800_000_000_000));
    }

    #[test]
    fn refresh_token_expiry_supports_snake_case_and_absolute_forms() {
        let now = 1_700_000_000_000;
        let snake = json!({"code": 0, "data": {
            "accessToken": "at", "refresh_expires_in": 3_600
        }});
        assert_eq!(parse_refresh_response(&snake, now).unwrap().rt_expires_at, Some(now + 3_600_000));

        let abs = json!({"code": 0, "data": {
            "accessToken": "at", "refreshExpiresAt": 1_900_000_000_000i64
        }});
        assert_eq!(
            parse_refresh_response(&abs, now).unwrap().rt_expires_at,
            Some(1_900_000_000_000)
        );
    }

    #[test]
    fn missing_refresh_token_expiry_stays_none_instead_of_being_invented() {
        // 拿不到就是拿不到。编一个（比如复用 AT 的有效期）会让管家看到一份假事实，
        // 而它恰恰是用来判断「这一池还能撑多久」的。
        let v = json!({"code": 0, "data": {"accessToken": "at", "expiresIn": 60}});
        let r = parse_refresh_response(&v, 1_700_000_000_000).unwrap();
        assert_eq!(r.rt_expires_at, None);
        assert!(r.expires_at.is_some());
    }

    #[test]
    fn reports_business_errors_and_missing_token() {
        let v = json!({"code": 40001, "msg": "refresh token 已失效"});
        let e = parse_refresh_response(&v, 0).unwrap_err();
        assert!(e.text.contains("refresh token 已失效"), "{}", e.text);
        // 服务端明确拒票 = 死链：重试多少次都是这一句
        assert!(e.dead, "拒票必须标成死链，否则整池会被一个坏账号反复拖停");
        // code=0 但没给 accessToken：响应看不懂，**不是**死链
        assert!(!parse_refresh_response(&json!({"code": 0, "data": {}}), 0)
            .unwrap_err()
            .dead);
    }

    /// 死链分级：只有服务端明确说这张票没了才放弃；看不懂的答复一律留着重试的机会。
    #[test]
    fn only_an_explicit_refusal_counts_as_a_dead_chain() {
        for body in [
            json!({"code": 40001, "msg": "refresh token 已失效"}),
            json!({"code": 410, "message": "invalid_grant"}),
            json!({"code": 40003, "msg": "登录已过期，请重新登录"}),
        ] {
            assert!(
                parse_refresh_response(&body, 0).unwrap_err().dead,
                "服务端明说票没了 = 死链：{body}"
            );
        }
        for body in [
            // 服务端自己的问题：下次也许就好了
            json!({"code": 500, "msg": "服务开小差了"}),
            // 没有 msg 的业务拒绝：判不出对象，不敢当成死链
            json!({"code": 40009}),
            json!({"code": -1}),
        ] {
            assert!(
                !parse_refresh_response(&body, 0).unwrap_err().dead,
                "这不是「票没了」的证据，不能放弃这条链：{body}"
            );
        }
        // 网络层失败根本到不了 parse，由 `refresh()` 给 transient
        assert!(!RefreshError::transient("请求续签接口失败：timeout").dead);
    }

    /// 「字段空 = 永不续签」这条死路的回归测试：到期时间要能从票里自己读出来。
    #[test]
    fn reads_expiry_out_of_the_ticket_when_the_field_is_empty() {
        let jwt = jwt_with_exp(1_800_000_000);
        assert_eq!(token_expiry_ms(&jwt), Some(1_800_000_000_000), "秒要归一成毫秒");
        let now = 1_800_000_000_000 - 3_600_000; // 票还剩 1 小时
        // 字段空着：只看 `should_refresh(None)` 会永远判成「不用续」
        assert!(!should_refresh(None, now));
        assert!(refresh_due(None, &jwt, now), "票里写着 exp，就必须去续");
        // 两边都知道时取**更早**的那个：字段说还有 30 天，票说自己 1 小时后死 —— 信票
        assert!(
            refresh_due(Some(now + 30 * 86_400_000), &jwt, now),
            "两个来源冲突时往早里取，晚一步就是重登"
        );
        // 反过来：字段说自己 1 小时后死、票说还有 30 天，也一样要早续
        let far = jwt_with_exp(2_000_000_000);
        assert!(refresh_due(Some(1_800_000_000_000 - 3_600_000), &far, now));
        // 两边都很宽裕 → 不折腾
        assert!(!refresh_due(Some(now + 30 * 86_400_000), &far, now));
        // 不透明串读不出到期：保持「不知道就不动」
        assert_eq!(token_expiry_ms("not-a-jwt"), None);
        assert!(!refresh_due(None, "not-a-jwt", now));
    }

    /// 拼一张本项目真实形态的票（三段式 JWT，载荷里 `exp` 是**秒**）。
    fn jwt_with_exp(exp: i64) -> String {
        let payload = format!(r#"{{"iss":"https://www.workbuddy.cn/auth/realms/copilot","exp":{exp}}}"#);
        let b64 = STANDARD
            .encode(payload)
            .replace('+', "-")
            .replace('/', "_")
            .trim_end_matches('=')
            .to_string();
        format!("eyJhbGciOiJSUzI1NiJ9.{b64}.sig")
    }

    #[test]
    fn a_known_dead_refresh_token_is_not_even_tried() {
        let now = 1_800_000_000_000;
        assert!(rt_is_expired(Some(now - 1), now));
        assert!(rt_is_expired(Some(now), now), "刚好到点算过期");
        assert!(!rt_is_expired(Some(now + 1), now));
        // 秒级写法也要认（老数据混发两种单位）
        assert!(rt_is_expired(Some((now - 1) / 1000), now));
        assert!(!rt_is_expired(None, now), "不知道就别放弃这条链");
    }

    /// 真实接口冒烟：用本机登录文件里的 refresh token 换一次新凭证。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_refresh_endpoint() {
        let list = crate::auth_file::discover_local_accounts();
        let a = list.first().expect("本机应存在 WorkBuddy 登录信息");
        let rt = a
            .refresh_token
            .clone()
            .expect("登录文件里应带 refresh token");
        let host = crate::oauth::normalize_host(a.host.as_deref());
        let r = refresh(&host, &a.token, &rt)
            .await
            .unwrap_or_else(|e| panic!("[{host}] 续签失败: {e}"));
        println!(
            "[{host}] 续签成功：新 token 长度={}，expires_at={:?}，rt_expires_at={:?}",
            r.token.len(),
            r.expires_at,
            r.rt_expires_at
        );
        assert!(!r.token.is_empty());
        assert!(r.expires_at.unwrap_or(0) > chrono::Utc::now().timestamp_millis());
    }
}
