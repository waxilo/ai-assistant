//! Cloud-IDE-JWT 载荷解析（**纯离线**，不发任何网络请求）。
//!
//! ## 为什么需要它
//!
//! 账号的 uid 有两个用途：
//! 1. 签到请求头 `x-device-id` 的来源（见 [`crate::checkin`]）；
//! 2. 账号去重与展示。
//!
//! 而唯一的在线来源 `POST /cloudide/api/v3/trae/GetUserInfo` 对「浏览器登录」换来的
//! token 会直接拒绝（2026-09-14 实测 HTTP 401
//! `{"Error":{"Code":"20310","Message":"The user is not logged in"}}`），
//! 于是 uid 落空、后续逻辑退化到不可信的兜底值。
//!
//! 但 JWT 载荷里本来就有 uid（`data.id`，与账号 uid 完全一致且**不需要鉴权**），
//! 所以这里直接解 token，别再依赖那次网络请求。
//!
//! ```text
//! {"data":{"id":"3225324630062683","source":"refresh_token","type":"user"},"exp":...,"iat":...}
//! ```

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;

/// 解出 JWT 的载荷段。不是 JWT / 解不开 / 不是 JSON 对象时返回 `None`。
pub fn payload(token: &str) -> Option<Value> {
    let mut parts = token.trim().split('.');
    parts.next()?; // header
    let body = parts.next()?; // payload
    if body.is_empty() {
        return None;
    }
    let raw = URL_SAFE_NO_PAD.decode(body.as_bytes()).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    v.is_object().then_some(v)
}

/// 用户 id（uid）：优先 `data.id`（TraeWork 的真实位置），再退到顶层常见键名。
pub fn user_id(token: &str) -> Option<String> {
    let v = payload(token)?;
    let null = Value::Null;
    for node in [v.get("data").unwrap_or(&null), &v] {
        for key in ["id", "uid", "user_id", "userId", "sub"] {
            let picked = match node.get(key) {
                Some(Value::String(s)) => {
                    let t = s.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                }
                // uid 在服务端是 snowflake 数字，JWT 里可能不带引号
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            if picked.is_some() {
                return picked;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一个只有载荷有意义的假 JWT（签名段不参与解析）。
    fn jwt(payload_json: &str) -> String {
        format!(
            "eyJhbGciOiJSUzI1NiJ9.{}.sig",
            URL_SAFE_NO_PAD.encode(payload_json.as_bytes())
        )
    }

    #[test]
    fn reads_uid_from_data_id() {
        let t = jwt(r#"{"data":{"id":"3225324630062683","type":"user"},"exp":1790601750}"#);
        assert_eq!(user_id(&t).as_deref(), Some("3225324630062683"));
    }

    #[test]
    fn accepts_numeric_and_top_level_keys() {
        assert_eq!(user_id(&jwt(r#"{"data":{"id":123456}}"#)).as_deref(), Some("123456"));
        assert_eq!(user_id(&jwt(r#"{"sub":"u-9"}"#)).as_deref(), Some("u-9"));
        assert_eq!(user_id(&jwt(r#"{"uid":"  42  "}"#)).as_deref(), Some("42"));
    }

    #[test]
    fn rejects_non_jwt_and_garbage() {
        assert!(payload("not-a-jwt").is_none());
        assert!(payload("a..c").is_none());
        assert!(user_id("").is_none());
        // 载荷是 JSON 但不是对象 / 没有 uid 键
        assert!(payload(&jwt("[1,2]")).is_none());
        assert!(user_id(&jwt(r#"{"data":{"id":""}}"#)).is_none());
        assert!(user_id(&jwt(r#"{"foo":"bar"}"#)).is_none());
    }
}
