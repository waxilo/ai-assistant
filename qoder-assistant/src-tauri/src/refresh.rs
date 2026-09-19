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

use crate::timeutil::token_expiry;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

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
pub fn should_refresh(expires_at: Option<i64>, now_ms: i64) -> bool {
    matches!(expires_at, Some(e) if e - now_ms < REFRESH_THRESHOLD_MS)
}

/// 续签响应解析：Qoder 返回**平铺**的 token 对（不套 `code/data`）。
fn parse_response(v: &Value, now_ms: i64) -> Result<Refreshed, String> {
    // 顶层 reason，业务失败统一在这（expired / rejected / transient…）
    if let Some(reason) = v.get("reason").and_then(Value::as_str) {
        return Err(format!("续签失败：{reason}"));
    }
    let token = str_of(v, &["token", "device_token", "accessToken", "access_token"]);
    if token.is_empty() {
        return Err("续签响应缺少 token".to_string());
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
) -> Result<Refreshed, String> {
    let url = format!("{}/api/v1/deviceToken/refresh", region.openapi_base());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))?;
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
        .map_err(|e| format!("请求续签接口失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| format!("续签接口返回非 JSON（HTTP {status}）"))?;
    match parse_response(&v, chrono::Utc::now().timestamp_millis()) {
        Ok(r) => Ok(r),
        // HTTP 非 2xx 且无 reason → 包一层状态码，便于调用方判断
        Err(e) if !status.is_success() => Err(format!("{e}（HTTP {status}）")),
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
        assert!(e.contains("expired"), "{e}");
        assert!(parse_response(&serde_json::json!({}), 0).is_err());
        assert!(parse_response(&serde_json::json!({"code": 0}), 0).is_err());
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