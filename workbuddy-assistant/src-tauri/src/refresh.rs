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

use crate::oauth::{access_expiry, refresh_expiry};
use serde::Serialize;
use serde_json::Value;

/// 续签阈值：剩余有效期不足 48 小时就自动续一次。
///
/// 取 48h 而不是 24h 的理由：access token 实际有效期 60 天，定时扫描是 12 小时一跳，
/// 取两天余量能保证「即使连续几天没开应用、扫描又恰好错过」，token 也不会中途失效。
pub const REFRESH_THRESHOLD_MS: i64 = 48 * 60 * 60 * 1000;

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
pub fn should_refresh(expires_at: Option<i64>, now_ms: i64) -> bool {
    matches!(expires_at, Some(e) if e - now_ms < REFRESH_THRESHOLD_MS)
}

fn biz_ok(v: &Value) -> bool {
    matches!(v.get("code").and_then(Value::as_i64), Some(0) | Some(200))
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
pub(crate) fn parse_refresh_response(v: &Value, now_ms: i64) -> Result<Refreshed, String> {
    if !biz_ok(v) {
        let msg = str_of(v, &["msg", "message", "error"]);
        let code = v.get("code").and_then(Value::as_i64);
        return Err(if msg.is_empty() {
            format!("续签失败（code={code:?}）")
        } else {
            format!("{msg}（code={code:?}）")
        });
    }
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let token = str_of(&data, &["accessToken", "access_token"]);
    if token.is_empty() {
        return Err("续签响应缺少 accessToken".to_string());
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
pub async fn refresh(host: &str, token: &str, refresh_token: &str) -> Result<Refreshed, String> {
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
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))?;
    let resp = client
        .post(&url)
        .header("X-Refresh-Token", refresh_token)
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("请求续签接口失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| format!("续签接口返回非 JSON（HTTP {status}）"))?;
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
        assert!(e.contains("refresh token 已失效"), "{e}");
        // code=0 但没给 accessToken：不能当成成功
        assert!(parse_refresh_response(&json!({"code": 0, "data": {}}), 0).is_err());
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
