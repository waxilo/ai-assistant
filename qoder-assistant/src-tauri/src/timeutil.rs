//! 时间戳归一化：把各家接口五花八门的时间写法统一折算成**毫秒绝对值**。
//!
//! 这套规则原先长在 `oauth.rs` 里，但它的实际使用者有三个（`oauth` 本身、`refresh`、
//! `auth_file`），继续留在 oauth 里会逼着后两者反向依赖一个业务模块 —— 于是独立成模块。
//!
//! 现实中遇到的写法（**都由实测响应驱动，不是猜的**）：
//!
//! | 写法 | 出处 |
//! |---|---|
//! | `1790927528327` | Qoder `usage` 的 `qoderUsage.expiresAt` / `plan` 的 `start_date` |
//! | `"1760000000"` | 数字被序列化成字符串 |
//! | `"2026-10-18T07:51:53Z"` | **Qoder `auth.v1.dat` 的 `expiresAt`** —— 最容易漏的一种 |
//! | `expires_in: 3600` | 相对秒数（`token_expiry` 用 `now_ms` 折算） |
//!
//! 漏掉 ISO 字符串的代价很隐蔽：`"2026-10-18T07:51:53Z".parse::<i64>()` 会**失败**，
//! 于是有效期静静地变成 `None` —— 界面不显示到期、续签判定直接不触发，
//! 但没有任何一处报错。所以这里把「不认识就返回 `None`」和「认识但值非法也返回 `None`」
//! 分开处理：前者是覆盖不全，后者是数据坏了，都不该编出一个假时间。

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

/// 秒 / 毫秒的分界：早于 2001-09-09（`1e10` 毫秒）的一律当秒看待。
///
/// Qoder 的毫秒时间戳都在 `1.7e12` 量级、秒级在 `1.7e9`，两者相差三个数量级，
/// 用这个分界不会误判。
const SECONDS_MAX: f64 = 1e10;

/// 当前时刻的毫秒时间戳。
///
/// 单独抽出来是为了让「取现在」只有一处定义 —— 相对秒数（`expires_in`）折算、
/// 采样打点、续签阈值判定都要用它，而这几处一旦各写各的 `SystemTime::now()`，
/// 就会出现「同一轮里两个不同的现在」，让差值计算莫名其妙地差几毫秒。
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 时间戳归一化：秒 / 毫秒 / 数字字符串 / ISO-8601 字符串 → 毫秒；无效返回 `None`。
pub(crate) fn norm_ts(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => from_number(n.as_f64()?),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // 先按纯数字试（「秒数被序列化成字符串」很常见），再退回 ISO 8601
            match s.parse::<f64>() {
                Ok(f) => from_number(f),
                Err(_) => iso_to_ms(s),
            }
        }
        _ => None,
    }
}

/// 数字 → 毫秒；非有限数或非正数一律 `None`（**不做「0 = 已过期」这种推断**）。
fn from_number(raw: f64) -> Option<i64> {
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let ms = if raw < SECONDS_MAX { raw * 1000.0 } else { raw };
    Some(ms.round() as i64)
}

/// ISO-8601 → 毫秒。认两种：带时区的 RFC3339（`2026-10-18T07:51:53Z`，Qoder 实际写法）
/// 与不带时区的 `2026-10-18T07:51:53`（按 UTC 解读 —— 宁可当 UTC，也不要当本地时区，
/// 后者会让同一份数据在不同机器上算出不同的时间）。
fn iso_to_ms(s: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp_millis())
}

/// 毫秒绝对值 → **官方文件里那个写法**：UTC 的 ISO-8601、秒精度（`2026-10-19T03:38:47Z`）。
///
/// 这是 [`norm_ts`] 的逆运算，存在的唯一理由是**写回**：`auth.v1.dat` 里两个有效期字段是
/// 字符串，若续签后写成毫秒数字，官方客户端读到的类型就变了。精度按秒——原文就是秒，
/// 多写三位小数会让同一份数据在两次写回之间"看起来变了"。
pub(crate) fn iso_utc(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// 从一段响应里取有效期：**绝对时间戳优先，只有相对秒数时按 `now_ms` 折算**。
///
/// access token 与 refresh token 用的是同一套字段约定（`expiresAt`/`expiresIn`
/// 与 `refreshExpiresAt`/`refreshExpiresIn`），所以规则只有这一处 —— 抄两遍迟早有一遍
/// 会漏掉 snake_case 别名，而漏掉的表现是「有效期莫名其妙为空」，很难看出来。
pub(crate) fn token_expiry(data: &Value, abs: &[&str], rel: &[&str], now_ms: i64) -> Option<i64> {
    abs.iter()
        .find_map(|k| norm_ts(data.get(*k)))
        .or_else(|| {
            rel.iter()
                .find_map(|k| data.get(*k))
                .and_then(Value::as_i64)
                .filter(|s| *s > 0)
                .map(|s| now_ms + s * 1000)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn norm_ts_handles_seconds_millis_strings_and_junk() {
        assert_eq!(norm_ts(Some(&json!(1_760_000_000))), Some(1_760_000_000_000));
        assert_eq!(norm_ts(Some(&json!(1_760_000_000_000i64))), Some(1_760_000_000_000));
        assert_eq!(norm_ts(Some(&json!("1760000000"))), Some(1_760_000_000_000));
        assert_eq!(norm_ts(Some(&json!(0))), None);
        assert_eq!(norm_ts(Some(&json!(-1))), None);
        assert_eq!(norm_ts(Some(&json!("abc"))), None);
        assert_eq!(norm_ts(Some(&json!(""))), None);
        assert_eq!(norm_ts(Some(&json!(null))), None);
        assert_eq!(norm_ts(None), None);
    }

    /// 实测值：Qoder `auth.v1.dat` 的 `expiresAt` / `refreshTokenExpiresAt` 是 ISO 字符串。
    /// 这三个期望值由独立换算得出（不是拿实现自己算出来又断言自己）。
    #[test]
    fn norm_ts_parses_iso8601_strings() {
        assert_eq!(
            norm_ts(Some(&json!("2026-10-18T07:51:53Z"))),
            Some(1_792_309_913_000)
        );
        assert_eq!(
            norm_ts(Some(&json!("2027-09-13T07:51:53Z"))),
            Some(1_820_821_913_000)
        );
        // 带毫秒、带偏移量的写法同样认
        assert_eq!(
            norm_ts(Some(&json!("2026-10-18T07:51:53.000Z"))),
            Some(1_792_309_913_000)
        );
        assert_eq!(
            norm_ts(Some(&json!("2026-10-18T15:51:53+08:00"))),
            Some(1_792_309_913_000)
        );
        // 不带时区 → 按 UTC 解读（跨机器同一结果）
        assert_eq!(
            norm_ts(Some(&json!("2026-10-18T07:51:53"))),
            Some(1_792_309_913_000)
        );
        // 认不出的写法宁可为空，也不要瞎猜
        assert_eq!(norm_ts(Some(&json!("2026-10-18"))), None);
        assert_eq!(norm_ts(Some(&json!("not-a-time"))), None);
    }

    #[test]
    fn token_expiry_prefers_absolute_over_relative() {
        let now = 1_700_000_000_000;
        // 绝对优先：两套都给时用绝对的
        let both = json!({ "expiresAt": "2026-10-18T07:51:53Z", "expires_in": 60 });
        assert_eq!(token_expiry(&both, &["expiresAt"], &["expires_in"], now), Some(1_792_309_913_000));
        // 只有相对秒数时按 now 折算
        assert_eq!(
            token_expiry(&json!({ "expires_in": 3600 }), &["expiresAt"], &["expires_in"], now),
            Some(now + 3_600_000)
        );
        // 相对值非法（0 / 负数 / 非整数）一律不折算
        assert_eq!(token_expiry(&json!({ "expires_in": 0 }), &[], &["expires_in"], now), None);
        assert_eq!(token_expiry(&json!({ "expires_in": -5 }), &[], &["expires_in"], now), None);
        assert_eq!(token_expiry(&json!({ "expires_in": "60" }), &[], &["expires_in"], now), None);
        assert_eq!(token_expiry(&json!({}), &["expiresAt"], &["expires_in"], now), None);
        // 绝对字段是垃圾 → 不产生「0 = 已过期」这种假结论
        assert_eq!(token_expiry(&json!({ "expiresAt": "abc" }), &["expiresAt"], &[], now), None);
    }

    /// `iso_utc` 与 `norm_ts` 必须互为逆运算 —— 写回用它，读回用前者，
    /// 一旦两者精度/时区口径不一致，就会出现「续签一次，有效期漂移几小时」。
    #[test]
    fn iso_utc_is_the_inverse_of_norm_ts() {
        // 实测值：本机国内版 auth.v1.dat 的 expiresAt
        assert_eq!(
            iso_utc(1_792_381_127_000).as_deref(),
            Some("2026-10-19T03:38:47Z")
        );
        for ms in [1_792_381_127_000i64, 1_820_893_127_000, 1_700_000_000_000] {
            let text = iso_utc(ms).expect("合法毫秒应可格式化");
            assert_eq!(norm_ts(Some(&json!(text.clone()))), Some(ms), "{text}");
        }
        // 超出 chrono 表示范围时宁可为 None，也不编一个时间
        assert_eq!(iso_utc(i64::MAX), None);
    }

    /// 绝对字段缺失（`null`）时**不能**回退到相对字段：`null` 是明确的「没有」，
    /// 而相对字段若存在，说明服务端本意就是给相对值 —— 但两者同时出现且绝对为 null
    /// 属于异常响应，此时宁可空着。
    #[test]
    fn token_expiry_treats_explicit_null_as_missing() {
        let now = 1_700_000_000_000;
        let v = json!({ "expiresAt": null, "expires_in": 60 });
        // 绝对字段读不出 → 走相对字段（这是现有语义，保持一致）
        assert_eq!(
            token_expiry(&v, &["expiresAt"], &["expires_in"], now),
            Some(now + 60_000)
        );
    }
}
