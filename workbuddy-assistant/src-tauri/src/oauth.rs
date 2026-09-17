//! 「无感登录」：纯 HTTP 的 OAuth state 轮询，用来把**新账号**加进列表。
//!
//! 流程与 WorkDaddy 的 `/api/oauth/start` + `/api/oauth/poll` 一致：
//!   1. `POST {host}/v2/plugin/auth/state?platform=workbuddy`（body `{}`，无需鉴权）
//!      → `{code:0, data:{state, authUrl}}`
//!   2. 系统浏览器打开 `authUrl`，用户扫码/登录（**应用全程不退出**）
//!   3. 轮询 `GET {host}/v2/plugin/auth/token?state=…`
//!      未授权 → `{code:11217, msg:"…login ing…"}`（**这是正常态，不是错误**）
//!      已授权 → `{code:0, data:{accessToken, refreshToken, expiresAt, domain, …}}`
//!   4. `GET {host}/v2/plugin/login/account?state=…`（`Authorization: Bearer` + `X-Domain`）
//!      → `{uid, nickname, uin, phoneNumber, type}`
//!
//! 相比「导入本机账号」（[`crate::auth_file`]）：那条通道只能拿到本机**已经登录过**的账号，
//! 而这条能主动签发**任意新账号**的凭证；代价是需要用户在浏览器里完成一次授权。
//!
//! host 必须打到账号自己所属的域（国内 `www.workbuddy.cn`，国际 `www.workbuddy.ai`），
//! 不能混用——这也是 WorkDaddy 用 `auth.domain` 区分 apiHost 的原因。

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const API_PREFIX: &str = "/v2/plugin";
/// 授权有效期：超过这个时间未完成就判定超时（WorkDaddy 用 600s）
const OAUTH_TIMEOUT_SECS: u64 = 600;
/// 出结果后再保留一段，避免前端收尾时的重复轮询直接报「请求不存在」
const RESULT_RETENTION_SECS: u64 = 300;
const DEFAULT_HOST: &str = "https://www.workbuddy.cn";

#[derive(Serialize, Clone, Debug)]
pub struct OAuthStart {
    pub login_id: String,
    pub verification_uri: String,
    pub host: String,
    pub expires_in: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct OAuthPoll {
    /// false = 还在等用户授权，前端应继续轮询
    pub done: bool,
    pub token: Option<String>,
    /// 续签用的 refresh token（授权接口一并返回，落库后才能自动续期）
    pub refresh_token: Option<String>,
    pub host: Option<String>,
    pub uid: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    /// access token 过期时间（毫秒时间戳）
    pub expires_at: Option<i64>,
    /// refresh token 过期时间（毫秒时间戳）；授权响应给了才有
    pub rt_expires_at: Option<i64>,
    pub error: Option<String>,
}

impl OAuthPoll {
    /// 还在等用户授权：**不是错误**，前端应继续轮询
    fn waiting() -> Self {
        Self {
            done: false,
            token: None,
            refresh_token: None,
            host: None,
            uid: None,
            nickname: None,
            phone: None,
            expires_at: None,
            rt_expires_at: None,
            error: None,
        }
    }

    fn failed(msg: &str) -> Self {
        Self {
            done: true,
            error: Some(msg.to_string()),
            ..Self::waiting()
        }
    }
}

struct Pending {
    state: String,
    host: String,
    expires_at: Instant,
    result: Option<OAuthPoll>,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn locks() -> std::sync::MutexGuard<'static, HashMap<String, Pending>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// 清掉早就过期的条目（含结果保留期），避免内存里无限堆积
fn sweep(map: &mut HashMap<String, Pending>) {
    let now = Instant::now();
    map.retain(|_, p| {
        now < p.expires_at + Duration::from_secs(RESULT_RETENTION_SECS)
    });
}

/// 把用户输入/账号 base_url 归一化成官方 API host。
///
/// 只认四个官方域；无法识别时退回国内版 `www.workbuddy.cn`。
pub fn normalize_host(input: Option<&str>) -> String {
    let raw = input.unwrap_or("").trim().trim_end_matches('/').to_lowercase();
    if raw.is_empty() {
        return DEFAULT_HOST.to_string();
    }
    let bare = raw
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.");
    let domain = match bare {
        "workbuddy.ai" => "workbuddy.ai",
        "codebuddy.cn" => "codebuddy.cn",
        "codebuddy.ai" => "codebuddy.ai",
        _ => "workbuddy.cn",
    };
    format!("https://www.{domain}")
}

fn biz_code(v: &Value) -> Option<i64> {
    v.get("code").and_then(Value::as_i64)
}

/// 业务成功码：官方用 0，个别网关回 200
fn biz_ok(v: &Value) -> bool {
    matches!(biz_code(v), Some(0) | Some(200))
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

/// 时间戳归一化：秒 / 毫秒 / 字符串 → 毫秒；无效返回 None
pub(crate) fn norm_ts(v: Option<&Value>) -> Option<i64> {
    let raw = match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let ms = if raw < 1e10 { raw * 1000.0 } else { raw };
    Some(ms.round() as i64)
}

/// 从一段 token 载荷里取有效期：**绝对时间戳优先，只有相对秒数时按 `now_ms` 折算**。
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

/// access token 的有效期（`expiresAt` / `expiresIn`）
pub(crate) fn access_expiry(data: &Value, now_ms: i64) -> Option<i64> {
    token_expiry(data, &["expiresAt", "expires_at"], &["expiresIn", "expires_in"], now_ms)
}

/// refresh token 的有效期（`refreshExpiresAt` / `refreshExpiresIn`）。
///
/// **必须与 access token 那个分开**：实测两者相差很远（60 天 vs 30 天），
/// 而只有 refresh token 能换来新的 access token —— 它一归零，这个账号就彻底没救了。
pub(crate) fn refresh_expiry(data: &Value, now_ms: i64) -> Option<i64> {
    token_expiry(
        data,
        &["refreshExpiresAt", "refresh_expires_at"],
        &["refreshExpiresIn", "refresh_expires_in"],
        now_ms,
    )
}

fn excerpt(s: &str) -> String {
    let t = s.trim();
    let cut: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 解析 `auth/state` 响应 → `(state, authUrl)`
pub(crate) fn parse_state_response(v: &Value, host: &str) -> Result<(String, String), String> {
    if !biz_ok(v) {
        let msg = str_of(v, &["msg", "message", "error"]);
        let msg = if msg.is_empty() { "auth/state 调用失败".to_string() } else { msg };
        return Err(format!("{msg}（code={:?}）", biz_code(v)));
    }
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let state = str_of(&data, &["state"]);
    if state.is_empty() {
        return Err("auth/state 响应缺少 state".to_string());
    }
    let mut auth_url = str_of(&data, &["authUrl", "auth_url", "url"]);
    if auth_url.is_empty() {
        auth_url = format!("{host}/login?platform=workbuddy&state={state}");
    }
    Ok((state, auth_url))
}

pub(crate) struct TokenReady {
    pub token: String,
    /// 授权接口返回的 refresh token（可能没有）
    pub refresh_token: Option<String>,
    pub domain: String,
    /// access token 过期时间（毫秒）
    pub expires_at: Option<i64>,
    /// refresh token 过期时间（毫秒）；授权响应给了才有
    pub rt_expires_at: Option<i64>,
}

pub(crate) enum TokenOutcome {
    /// 用户还没完成授权（如 code=11217），继续轮询
    Waiting,
    Ready(TokenReady),
}

/// 解析 `auth/token` 响应。
///
/// **判定只看 `code`**：非 0/200 一律视为「还没授权」而不是错误——
/// 官方在等待期返回的就是 `{"code":11217,"msg":"11217:login ing..."}`。
pub(crate) fn parse_token_response(v: &Value) -> TokenOutcome {
    if !biz_ok(v) {
        return TokenOutcome::Waiting;
    }
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let token = str_of(&data, &["accessToken", "access_token"]);
    if token.is_empty() {
        return TokenOutcome::Waiting;
    }
    let domain = str_of(&data, &["domain"]);
    let refresh_token = {
        let r = str_of(&data, &["refreshToken", "refresh_token"]);
        if r.is_empty() { None } else { Some(r) }
    };
    // expiresAt / expiresIn：两个 token 各取各的（折算规则共用 `token_expiry`）
    let now = chrono::Utc::now().timestamp_millis();
    TokenOutcome::Ready(TokenReady {
        token,
        refresh_token,
        domain,
        expires_at: access_expiry(&data, now),
        rt_expires_at: refresh_expiry(&data, now),
    })
}

pub(crate) struct AccountInfo {
    pub uid: String,
    pub nickname: Option<String>,
    pub phone: Option<String>,
}

/// 解析 `login/account` 响应（字段缺失不致命，uid 才关键）
pub(crate) fn parse_account_response(v: &Value) -> AccountInfo {
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let uid = str_of(&data, &["uid", "userId", "user_id"]);
    let nickname = {
        let n = str_of(&data, &["nickname", "nickName", "name", "uin"]);
        if n.is_empty() { None } else { Some(n) }
    };
    let phone = {
        let p = str_of(&data, &["phoneNumber", "phone_number", "phone", "mobile"]);
        if p.is_empty() { None } else { Some(p) }
    };
    AccountInfo { uid, nickname, phone }
}

/// 登录授权用的客户端。
///
/// 统一 UA 与 `http::client_headers()`（客户端自身的头——语言偏好描述的是这个应用本身，
/// 与走哪个接口无关，所以这条路径同样该有），**不加 `x-client-platform`**：
/// 这条路径走的是插件授权接口（`/v2/plugin/auth/*`），不是计费接口，
/// 声明 `web` 属于无根据的说法，宁可不写。
fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(crate::http::TIMEOUT)
        .user_agent(crate::http::UA)
        .default_headers(crate::http::client_headers())
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))
}

/// 第一步：申请 state + 授权链接。
pub async fn start(host: Option<String>) -> Result<OAuthStart, String> {
    let host = normalize_host(host.as_deref());
    let url = format!("{host}{API_PREFIX}/auth/state?platform=workbuddy");
    let resp = http()?
        .post(&url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("请求 auth/state 失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| format!("auth/state 返回非 JSON（HTTP {status}）：{}", excerpt(&text)))?;
    let (state, verification_uri) = parse_state_response(&v, &host)?;

    let login_id = format!("wba_{}", uuid::Uuid::new_v4().simple());
    {
        let mut map = locks();
        sweep(&mut map);
        map.insert(
            login_id.clone(),
            Pending {
                state,
                host: host.clone(),
                expires_at: Instant::now() + Duration::from_secs(OAUTH_TIMEOUT_SECS),
                result: None,
            },
        );
    }
    Ok(OAuthStart {
        login_id,
        verification_uri,
        host,
        expires_in: OAUTH_TIMEOUT_SECS,
    })
}

fn cache_result(login_id: &str, r: &OAuthPoll) {
    let mut map = locks();
    if let Some(p) = map.get_mut(login_id) {
        p.result = Some(r.clone());
    }
}

/// 第二步：轮询一次授权结果；完成后顺带拉账号信息（昵称/手机号）。
pub async fn poll(login_id: &str) -> Result<OAuthPoll, String> {
    let snapshot = {
        let map = locks();
        map.get(login_id).map(|p| {
            (p.state.clone(), p.host.clone(), p.expires_at, p.result.clone())
        })
    };
    let Some((state, host, expires_at, cached)) = snapshot else {
        return Ok(OAuthPoll::failed("登录请求不存在或已过期，请重新发起"));
    };
    if let Some(r) = cached {
        return Ok(r);
    }
    if Instant::now() > expires_at {
        let r = OAuthPoll::failed("登录超时，请重新发起");
        cache_result(login_id, &r);
        return Ok(r);
    }

    let client = http()?;

    // 1) 拿 token
    let url = format!("{host}{API_PREFIX}/auth/token?state={state}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("轮询 auth/token 失败：{e}"))?;
    let text = resp.text().await.unwrap_or_default();
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        // 网关偶发返回非 JSON：当作「还在等待」，不要打断轮询
        Err(_) => return Ok(OAuthPoll::waiting()),
    };
    let ready = match parse_token_response(&v) {
        TokenOutcome::Waiting => {
            return Ok(OAuthPoll::waiting())
        }
        TokenOutcome::Ready(r) => r,
    };

    // 2) 拿账号信息（失败不致命：token 已经拿到了）
    let mut req = client
        .get(format!("{host}{API_PREFIX}/login/account?state={state}"))
        .header("Authorization", format!("Bearer {}", ready.token));
    if !ready.domain.is_empty() {
        req = req.header("X-Domain", ready.domain.clone());
    }
    let info = match req.send().await {
        Ok(r) => {
            let t = r.text().await.unwrap_or_default();
            serde_json::from_str::<Value>(&t)
                .map(|v| parse_account_response(&v))
                .unwrap_or(AccountInfo { uid: String::new(), nickname: None, phone: None })
        }
        Err(_) => AccountInfo { uid: String::new(), nickname: None, phone: None },
    };

    let result = OAuthPoll {
        done: true,
        token: Some(ready.token),
        refresh_token: ready.refresh_token,
        host: Some(host),
        uid: if info.uid.is_empty() { None } else { Some(info.uid) },
        nickname: info.nickname,
        phone: info.phone,
        expires_at: ready.expires_at,
        rt_expires_at: ready.rt_expires_at,
        error: None,
    };
    cache_result(login_id, &result);
    Ok(result)
}

/// 在系统默认浏览器打开链接（授权页）。
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err("仅支持 http(s) 链接".to_string());
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(u);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(u);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(u);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| format!("打开浏览器失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_hosts_to_official_domains() {
        assert_eq!(normalize_host(None), "https://www.workbuddy.cn");
        assert_eq!(normalize_host(Some("")), "https://www.workbuddy.cn");
        assert_eq!(normalize_host(Some("workbuddy.cn")), "https://www.workbuddy.cn");
        assert_eq!(normalize_host(Some("www.workbuddy.ai")), "https://www.workbuddy.ai");
        assert_eq!(
            normalize_host(Some("https://www.workbuddy.ai/")),
            "https://www.workbuddy.ai"
        );
        assert_eq!(normalize_host(Some("codebuddy.cn")), "https://www.codebuddy.cn");
        // 未知域一律退回国内版，避免把请求打到任意 host
        assert_eq!(normalize_host(Some("evil.example.com")), "https://www.workbuddy.cn");
    }

    #[test]
    fn parses_state_response_and_falls_back_to_login_url() {
        let v = json!({"code": 0, "data": {"state": "abc-123"}});
        let (state, url) = parse_state_response(&v, "https://www.workbuddy.cn").unwrap();
        assert_eq!(state, "abc-123");
        assert_eq!(url, "https://www.workbuddy.cn/login?platform=workbuddy&state=abc-123");

        let v = json!({"code": 0, "data": {"state": "s1", "authUrl": "https://x/y"}});
        assert_eq!(parse_state_response(&v, "https://h").unwrap().1, "https://x/y");
    }

    #[test]
    fn state_response_errors_are_reported() {
        let v = json!({"code": 500, "msg": "boom"});
        let e = parse_state_response(&v, "https://h").unwrap_err();
        assert!(e.contains("boom"), "{e}");
        let v = json!({"code": 0, "data": {}});
        assert!(parse_state_response(&v, "https://h").unwrap_err().contains("state"));
    }

    #[test]
    fn polling_code_11217_means_still_waiting_not_error() {
        // 官方等待态
        let v = json!({"code": 11217, "msg": "11217:login ing..."});
        assert!(matches!(parse_token_response(&v), TokenOutcome::Waiting));
        // 已授权但 body 里还没 token
        let v = json!({"code": 0, "data": {}});
        assert!(matches!(parse_token_response(&v), TokenOutcome::Waiting));
    }

    #[test]
    fn parses_ready_token_with_camel_and_snake_case() {
        let v = json!({"code": 0, "data": {
            "accessToken": "eyJhbGciOi.x.y", "domain": "https://www.workbuddy.cn",
            "expiresAt": 1_760_000_000
        }});
        match parse_token_response(&v) {
            TokenOutcome::Ready(r) => {
                assert_eq!(r.token, "eyJhbGciOi.x.y");
                assert_eq!(r.domain, "https://www.workbuddy.cn");
                // 秒 → 毫秒
                assert_eq!(r.expires_at, Some(1_760_000_000_000));
            }
            _ => panic!("应当解析出 token"),
        }

        let v = json!({"code": 200, "data": {"access_token": "t", "expires_in": 7200}});
        match parse_token_response(&v) {
            TokenOutcome::Ready(r) => assert!(r.expires_at.unwrap() > chrono::Utc::now().timestamp_millis()),
            _ => panic!("snake_case 也应识别"),
        }
    }

    #[test]
    fn authorization_response_carries_both_token_expiries() {
        // 授权接口与续签接口给的是同一套字段。**两个都要收** ——
        // 只收 access token 那个，账号落库后 refresh token 的有效期就永远是空的，
        // 上传给管家时也就少了「这条续签链还能活多久」这个唯一答案。
        let v = json!({"code": 0, "data": {
            "accessToken": "at", "refreshToken": "rt", "domain": "www.workbuddy.cn",
            "expiresIn": 5_184_000, "refreshExpiresIn": 2_592_000
        }});
        let now = chrono::Utc::now().timestamp_millis();
        match parse_token_response(&v) {
            TokenOutcome::Ready(r) => {
                let at = r.expires_at.expect("应折算出 access token 有效期");
                let rt = r.rt_expires_at.expect("应折算出 refresh token 有效期");
                assert!((at - (now + 5_184_000_000)).abs() < 5_000, "AT 约 60 天：{at}");
                assert!((rt - (now + 2_592_000_000)).abs() < 5_000, "RT 约 30 天：{rt}");
                assert!(rt < at, "两者是两条独立的命，不能混成一个字段");
            }
            _ => panic!("应解析出 token"),
        }
    }

    #[test]
    fn token_expiry_prefers_absolute_and_never_invents_a_value() {
        let now = 1_700_000_000_000;
        // 绝对时间戳优先（秒 / 毫秒 / 字符串三种写法都认）
        let abs = json!({"expiresAt": 1_800_000_000i64, "expiresIn": 60});
        assert_eq!(access_expiry(&abs, now), Some(1_800_000_000_000));
        // 只有相对秒数时按 now 折算
        assert_eq!(access_expiry(&json!({"expires_in": 60}), now), Some(now + 60_000));
        // 两套字段名各自独立，读 access 的那套不会误取 refresh 的
        let mixed = json!({"refreshExpiresIn": 60});
        assert_eq!(access_expiry(&mixed, now), None, "AT 的字段不该被 RT 的值填上");
        assert_eq!(refresh_expiry(&mixed, now), Some(now + 60_000));
        // 坏值一律 None，不产生「0 = 已过期」这种假结论
        assert_eq!(access_expiry(&json!({"expiresIn": 0}), now), None);
        assert_eq!(access_expiry(&json!({"expiresIn": -1}), now), None);
        assert_eq!(access_expiry(&json!({"expiresAt": "abc"}), now), None);
        assert_eq!(refresh_expiry(&json!({}), now), None);
    }

    #[test]
    fn parses_account_info_with_field_fallbacks() {
        let v = json!({"code": 0, "data": {
            "uid": "u-1", "nickname": "waxiloao", "phoneNumber": "190****9775", "uin": "123"
        }});
        let a = parse_account_response(&v);
        assert_eq!(a.uid, "u-1");
        assert_eq!(a.nickname.as_deref(), Some("waxiloao"));
        assert_eq!(a.phone.as_deref(), Some("190****9775"));

        // uid 缺失时不 panic，字段留空
        let a = parse_account_response(&json!({"code": 0, "data": {"phone": "1"}}));
        assert_eq!(a.uid, "");
        assert_eq!(a.phone.as_deref(), Some("1"));
    }

    #[test]
    fn norm_ts_handles_seconds_millis_strings_and_junk() {
        assert_eq!(norm_ts(Some(&json!(1_760_000_000))), Some(1_760_000_000_000));
        assert_eq!(norm_ts(Some(&json!(1_760_000_000_000i64))), Some(1_760_000_000_000));
        assert_eq!(norm_ts(Some(&json!("1760000000"))), Some(1_760_000_000_000));
        assert_eq!(norm_ts(Some(&json!(0))), None);
        assert_eq!(norm_ts(Some(&json!("abc"))), None);
        assert_eq!(norm_ts(None), None);
    }

    /// 本机冒烟：真实调用一次 `auth/state`，并确认未授权时的轮询返回「继续等待」。
    ///
    /// 不完成授权（不需要人扫码），只验证「申请 state → 轮询得到等待态」这条链路通。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_oauth_state_and_waiting_poll() {
        for host in ["https://www.workbuddy.cn", "https://www.workbuddy.ai"] {
            let s = start(Some(host.to_string()))
                .await
                .unwrap_or_else(|e| panic!("[{host}] auth/state 失败: {e}"));
            assert!(
                s.verification_uri.starts_with(host),
                "[{host}] 授权链接应落在同一域：{}",
                s.verification_uri
            );
            println!("[{host}] start ok  uri={}", s.verification_uri);

            let p = poll(&s.login_id).await.unwrap();
            assert!(!p.done, "[{host}] 刚申请 state 不可能已完成授权");
            assert!(p.error.is_none(), "[{host}] 等待态不该带错误：{:?}", p.error);
            println!("[{host}] poll ok   done=false（等待授权）");
        }
    }

    #[test]
    fn sweep_drops_finished_entries_after_retention() {
        let mut map = HashMap::new();
        map.insert(
            "old".to_string(),
            Pending {
                state: "s".into(),
                host: "h".into(),
                expires_at: Instant::now() - Duration::from_secs(RESULT_RETENTION_SECS + 60),
                result: None,
            },
        );
        map.insert(
            "fresh".to_string(),
            Pending {
                state: "s".into(),
                host: "h".into(),
                expires_at: Instant::now() - Duration::from_secs(1),
                result: None,
            },
        );
        sweep(&mut map);
        assert!(!map.contains_key("old"));
        assert!(map.contains_key("fresh"));
    }
}
