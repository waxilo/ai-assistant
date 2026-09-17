//! 「浏览器登录」：**本地回环 OAuth（RFC 8252）** + 授权码换 token。
//!
//! 逆向自 TraeWork（TRAE Solo CN）桌面端编译产物，与 WorkBuddy 的 `auth/state`
//! 服务器轮询**不是**同一套。真实流程：
//!
//!   1. 本进程在 `127.0.0.1:{port}` 起一个一次性回环 HTTP 服务器，回调路由 `GET /authorize`
//!   2. 生成 PKCE（`codeVerifier` = 48B base64url，`codeChallenge` = base64url(sha256)，S256）
//!   3. 构造官方授权 URL 并交给系统浏览器打开
//!       `{ssoHost}/authorization?login_version=1&auth_from=solo&login_channel=native_ide
//!         &plugin_version=…&auth_type=local&client_id=…&redirect=0&login_trace_id=…
//!         &auth_callback_url=http://127.0.0.1:{port}/authorize&machine_id&device_id
//!         &code_challenge=…&code_challenge_method=S256`
//!   4. 用户在页面登录后，浏览器重定向到本地回调并携带 `authCodeInfo`(JSON，含 AuthCode)
//!   5. 用 AuthCode + CodeVerifier 换到 `Cloud-IDE-JWT`：
//!       `POST {apiHost}/trae/api/v3/oauth/ExchangeToken`
//!   6. 拉账号信息：`POST {apiHost}/cloudide/api/v3/trae/GetUserInfo`
//!      —— **鉴权头是 `x-cloudide-token: <JWT>`，不是 `Authorization: Cloud-IDE-JWT`**
//!      （后者对这个接口一律 401 `20310 The user is not logged in`，见 [`fetch_user_info`]）
//!
//! 相比「扫描本机 / 导入文件」：这两条只能拿到本机**已登录过**的账号，而这里能主动
//! 签发**任意新账号**的凭证；代价是需要用户在浏览器里完成一次授权。
//!
//! ## 确认的消费端常量（逆向自 main.js `Fb()`）
//! - SOLO 精简版 client_id：`en1oxy7wnw8j9n`
//! - TRAE client_id：`ono9krqynydwx5`
//! - 默认 ssoHost：国内 `https://www.trae.cn`，国际 `https://www.trae.ai`
//! - 默认 apiHost：国内 `https://api.trae.cn`，国际 `https://api.trae.ai`

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::devicekey;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// SOLO 精简版消费端 client_id（逆向自 main.js `Fb()`）。
/// 续签（[`crate::renew`]）也用同一个 client_id —— 服务端按 client 校验。
pub(crate) const CLIENT_ID_SOLO: &str = "en1oxy7wnw8j9n";
/// 授权码换 token（**auth-code 与 refresh-token 两种授权共用同一端点**）
pub(crate) const EXCHANGE_TOKEN_PATH: &str = "/trae/api/v3/oauth/ExchangeToken";
/// 拉账号信息
const GET_USER_INFO_PATH: &str = "/cloudide/api/v3/trae/GetUserInfo";
/// 本地回调路由（前端轮询期间浏览器只回打这里一次）
const CALLBACK_PATH: &str = "/authorize";

/// 授权有效期：超过这个时间未完成就判定超时（官方登录页 5 分钟有效）
const OAUTH_TIMEOUT_SECS: u64 = 600;
/// 出结果后再保留一段，避免前端收尾时的重复轮询直接报「请求不存在」
const RESULT_RETENTION_SECS: u64 = 300;

const DEFAULT_HOST: &str = "https://api.trae.cn";

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
    /// 续签用 refresh token（换 token 接口一并返回，落库后才能自动续期）
    pub refresh_token: Option<String>,
    pub host: Option<String>,
    pub region: Option<String>,
    pub uid: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    pub expires_at: Option<i64>,
    /// 登录时绑定的设备标识（签到必填头 `X-Device-Id` 来源）
    pub device_id: Option<String>,
    /// 登录时绑定的机器标识（签到必填头 `X-Machine-Id` 来源）
    pub machine_id: Option<String>,
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
            region: None,
            uid: None,
            nickname: None,
            phone: None,
            expires_at: None,
            device_id: None,
            machine_id: None,
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
    /// 本地回环服务器收到的 `/authorize?…` 查询串（None = 用户还没登录完）
    callback: Arc<Mutex<Option<String>>>,
    code_verifier: String,
    client_id: String,
    api_host: String,
    /// 授权与换 token 共用的设备身份（必须一致，否则服务端报 device not match）
    device_id: String,
    machine_id: String,
    expires_at: Instant,
    result: Option<OAuthPoll>,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn locks() -> std::sync::MutexGuard<'static, HashMap<String, Pending>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

fn sweep(map: &mut HashMap<String, Pending>) {
    let now = Instant::now();
    map.retain(|_, p| now < p.expires_at + Duration::from_secs(RESULT_RETENTION_SECS));
}

/// host → 国内/国际，返回 `(api_host, sso_host, region)`
fn hosts(input: Option<&str>) -> (String, String, String) {
    let raw = input.unwrap_or("").trim().to_lowercase();
    let intl = raw.contains("trae.ai") || raw.contains("trae.com");
    if intl {
        (
            "https://api.trae.ai".to_string(),
            "https://www.trae.ai".to_string(),
            "us".to_string(),
        )
    } else {
        (
            "https://api.trae.cn".to_string(),
            "https://www.trae.cn".to_string(),
            "cn".to_string(),
        )
    }
}

fn region_for_host(host: &str) -> Option<String> {
    if host.contains("trae.ai") || host.contains("trae.com") {
        Some("us".into())
    } else if host.contains("trae.cn") {
        Some("cn".into())
    } else {
        None
    }
}

/// 账号上存的 `host`（可能是空串 / 裸域名 / 完整 URL）→ 规范化后的 api host。
/// 缺省国内版。续签要用它拼 `ExchangeToken` 地址。
pub fn normalize_api_host(host: &str) -> String {
    let raw = host.trim().to_lowercase();
    if raw.contains("trae.ai") || raw.contains("trae.com") {
        "https://api.trae.ai".to_string()
    } else {
        "https://api.trae.cn".to_string()
    }
}

// ---------------------------------------------------------------------------
// 设备身份
// ---------------------------------------------------------------------------

/// 应用数据目录。设备密钥对与设备号要落盘（见 [`crate::devicekey`]），
/// 所以这两个函数需要知道数据目录；由 `lib.rs` 在 setup 时注入。
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

fn data_dir() -> PathBuf {
    DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("traework-assistant"))
}

/// 设备身份 (device_id, machine_id)：**落盘持久化**，一次进程内也不会变。
///
/// 必须持久化，否则续签必失败：token 绑定「签发时的 DeviceID + 私钥」，
/// 换一次身份就回 `20403 Token device not match`（详见 [`crate::renew`] 顶部对照表）。
/// 首次生成时优先用本机 TraeWork 的真实设备号（官方语义：一台机器一个设备号）。
fn device_identity() -> (String, String) {
    let (dev, mach) = crate::trae_auth::local_device_identity();
    let id = crate::devicekey::load_or_create(&data_dir(), dev, mach);
    (id.device_id, id.machine_id)
}

/// 设备名：best-effort 取 hostname，失败给占位（仅提示用，不影响校验）。
fn device_name() -> String {
    crate::proc::cmd("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok().map(|x| x.trim().to_string()))
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| "TraeWorkAssistant-Device".into())
}

/// 设备公钥（SPKI PEM）：取自**落盘的**设备密钥对，供 `DeviceInfo.DevicePublicKey`
/// 完成设备绑定 —— auth-code 换 token 只需公钥；refresh 流程还要用同一把**私钥**签
/// `DeviceProof`（见 [`crate::renew`]），所以这里绝不能每次现生成一把。
fn device_public_key() -> String {
    devicekey::load_or_create(&data_dir(), None, None).public_key_pem
}

// ---------------------------------------------------------------------------
// URL 工具
// ---------------------------------------------------------------------------


/// percent-decode `%XX` 与 `+` → 空格
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() + 1 && i + 2 <= bytes.len() - 1 + 1 && i + 2 < bytes.len() => {
                if i + 2 < bytes.len() {
                    if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                }
                out.push(b'%');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把 `k=v&k2=v2…` 解析成 map（已 url-decode）
fn parse_query(q: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some(eq) = pair.find('=') {
            let k = url_decode(&pair[..eq]);
            let v = url_decode(&pair[eq + 1..]);
            m.insert(k, v);
        }
    }
    m
}

/// base64url 随机串，用于 code_verifier / 各类 id
fn rand_b64url(nbytes: usize) -> String {
    let mut bytes = Vec::with_capacity(nbytes);
    // 用多份 uuid::Uuid（v4 自带随机）拼足长度，避免引入 rand 依赖
    while bytes.len() < nbytes {
        let u = uuid::Uuid::new_v4();
        bytes.extend_from_slice(u.as_bytes());
    }
    bytes.truncate(nbytes);
    URL_SAFE_NO_PAD.encode(&bytes)
}

/// PKCE：返回 (codeVerifier, codeChallenge)
fn pkce() -> (String, String) {
    let verifier = rand_b64url(48);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

// ---------------------------------------------------------------------------
// JSON 解析（防御式，字段缺失不致命）
// ---------------------------------------------------------------------------

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

fn norm_ts(v: Option<&Value>) -> Option<i64> {
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

fn excerpt(s: &str) -> String {
    let t = s.trim();
    let cut: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 在嵌套 `Result` 或平铺里找字符串键
fn dig_str(v: &Value, keys: &[&str]) -> String {
    let nested = v.get("Result").or_else(|| v.get("result")).or_else(|| v.get("data"));
    let direct = str_of(v, keys);
    if !direct.is_empty() {
        direct
    } else if let Some(nv) = nested {
        str_of(nv, keys)
    } else {
        String::new()
    }
}

fn dig_ts(v: &Value, keys: &[&str]) -> Option<i64> {
    let direct = norm_ts(keys.iter().find_map(|k| v.get(*k)));
    if direct.is_some() {
        return direct;
    }
    let nested = v.get("Result").or_else(|| v.get("result")).or_else(|| v.get("data"))?;
    norm_ts(keys.iter().find_map(|k| nested.get(*k)))
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("TraeWorkAssistant/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))
}

struct ExchangeResult {
    token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    apihost: String,
}

fn parse_exchange_resp(v: &Value) -> Option<ExchangeResult> {
    let token = dig_str(v, &["Token", "token", "accessToken"]);
    if token.is_empty() {
        return None;
    }
    Some(ExchangeResult {
        token,
        refresh_token: {
            let r = dig_str(v, &["RefreshToken", "refreshToken", "refresh_token"]);
            if r.is_empty() { None } else { Some(r) }
        },
        expires_at: dig_ts(v, &["TokenExpireAt", "tokenExpireAt", "expiresAt", "expires_at"]),
        apihost: {
            let a = dig_str(v, &["apiHost", "api_host"]);
            if a.is_empty() { DEFAULT_HOST.to_string() } else { a }
        },
    })
}

/// 账号资料（`GetUserInfo` 的 `Result` 子树）
#[derive(Clone, Debug, Default)]
pub struct UserInfo {
    pub uid: String,
    /// 服务端昵称（`ScreenName`，形如 `用户0044120650`）——**账号名称的唯一真实来源**
    pub nickname: Option<String>,
    /// 脱敏手机号（`NonPlainTextMobile`，形如 `191******52`）
    pub phone: Option<String>,
}

fn parse_user_info(v: &Value) -> UserInfo {
    let uid = dig_str(v, &["UserID", "userId", "uid", "user_id"]);
    let nickname = {
        let n = dig_str(v, &["ScreenName", "Nickname", "nickname", "name", "uin"]);
        if n.is_empty() { None } else { Some(n) }
    };
    let phone = {
        let p = dig_str(
            v,
            &["NonPlainTextMobile", "PhoneNumber", "Phone", "phone", "mobile"],
        );
        if p.is_empty() { None } else { Some(p) }
    };
    UserInfo { uid, nickname, phone }
}

// ---------------------------------------------------------------------------
// 本地回环回调服务器
// ---------------------------------------------------------------------------

/// 起一个一次性回环 HTTP 服务器，等 `/authorize` 回调，把查询串写进 `callback`。
/// 拿到授权信息或 60s 没等到就结束线程（后续轮询读 `callback` 即可）。
fn spawn_callback_server() -> Result<(u16, Arc<Mutex<Option<String>>>), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("绑定回环端口失败：{e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let callback: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cb = callback.clone();

    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        for socket in listener.incoming() {
            if Instant::now() > deadline {
                break;
            }
            let Ok(mut s) = socket else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf = Vec::with_capacity(2048);
            let mut chunk = [0u8; 2048];
            // 读到请求头结束（\r\n\r\n）为止
            while buf.len() < 64 * 1024 {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&buf);
            if let Some(target) = req.split_whitespace().nth(1) {
                if target.starts_with(CALLBACK_PATH) {
                    if let Some(qi) = target.find('?') {
                        let q = &target[qi + 1..];
                        if q.contains("authCodeInfo") || q.contains("userTag") {
                            let mut g = cb.lock().unwrap_or_else(|e| e.into_inner());
                            if g.is_none() {
                                *g = Some(q.to_string());
                            }
                        }
                    }
                }
            }
            // 回一个可关闭的提示页
            let body =
                "<html><body style=\"font-family:sans-serif;text-align:center;padding-top:70px\">\
                 <h2>登录成功</h2><p>可以关闭此窗口，回到 TraeWorkAssistant。</p></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
            if cb.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                break; // 已拿到授权信息，收工
            }
        }
    });

    Ok((port, callback))
}

fn build_authorization_url(
    login_host: &str,
    port: u16,
    client_id: &str,
    code_challenge: &str,
    device_id: &str,
    machine_id: &str,
) -> String {
    let login_trace_id = uuid::Uuid::new_v4().simple().to_string();
    let callback_url = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    format!(
        "{login_host}/authorization?login_version=1&auth_from=solo&login_channel=native_ide\
         &plugin_version={}&auth_type=local&client_id={}&redirect=0&login_trace_id={}\
         &auth_callback_url={}&machine_id={}&device_id={}\
         &x_device_id={}&x_machine_id={}&code_challenge={}&code_challenge_method=S256\
         &hide_saas_login=true",
        env!("CARGO_PKG_VERSION"),
        client_id,
        login_trace_id,
        callback_url,
        machine_id,
        device_id,
        device_id,
        machine_id,
        code_challenge,
    )
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 第一步：绑定回环回调、生成 PKCE、构造授权 URL。
pub async fn start(host: Option<String>) -> Result<OAuthStart, String> {
    let (api_host, login_host, _region) = hosts(host.as_deref());
    let (port, callback) = spawn_callback_server()?;
    let (code_verifier, code_challenge) = pkce();
    let client_id = CLIENT_ID_SOLO.to_string();
    let (device_id, machine_id) = device_identity();

    let login_id = format!("trae_{}", uuid::Uuid::new_v4().simple());
    let verification_uri = build_authorization_url(
        &login_host, port, &client_id, &code_challenge, &device_id, &machine_id,
    );

    {
        let mut map = locks();
        sweep(&mut map);
        map.insert(
            login_id.clone(),
            Pending {
                callback,
                code_verifier,
                client_id,
                api_host: api_host.clone(),
                device_id,
                machine_id,
                expires_at: Instant::now() + Duration::from_secs(OAUTH_TIMEOUT_SECS),
                result: None,
            },
        );
    }

    Ok(OAuthStart {
        login_id,
        verification_uri,
        host: api_host,
        expires_in: OAUTH_TIMEOUT_SECS,
    })
}

fn cache_result(login_id: &str, r: &OAuthPoll) {
    let mut map = locks();
    if let Some(p) = map.get_mut(login_id) {
        p.result = Some(r.clone());
    }
}

/// 授权码 → Cloud-IDE-JWT
async fn exchange_token(
    api_host: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    device_id: &str,
    machine_id: &str,
) -> Result<ExchangeResult, String> {
    let url = format!("{api_host}{EXCHANGE_TOKEN_PATH}");
    let body = serde_json::json!({
        "ClientID": client_id,
        "AuthCode": code,
        "CodeVerifier": code_verifier,
        "DeviceInfo": {
            "DeviceID": device_id,
            "MachineID": machine_id,
            "PlatformCode": "SOLO_PC",
            "DeviceType": "PC",
            "DevicePublicKey": device_public_key(),
            "DeviceName": device_name(),
            "DeviceModel": "",
            "DeviceBrand": "",
            "DeviceCPU": "",
            "OSInfo": "",
            "OSVersion": "",
            "ClientVersion": env!("CARGO_PKG_VERSION"),
        },
        "IDEVersion": env!("CARGO_PKG_VERSION"),
    });
    let resp = http()?
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("ExchangeToken 请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| format!("ExchangeToken 返回非 JSON（HTTP {status}）：{}", excerpt(&text)))?;
    parse_exchange_resp(&v).ok_or_else(|| {
        format!(
            "ExchangeToken 未返回 Token（HTTP {status} body={}）",
            excerpt(&text)
        )
    })
}

/// 用 Cloud-IDE-JWT 拉账号资料（昵称 / 脱敏手机号 / uid）。失败返回 `None`（不致命）。
///
/// ## 鉴权头是 `x-cloudide-token`，不是 `Authorization`（2026-09-14 实测）
///
/// 同一个 token、同一个 body，只换鉴权头，结果天差地别：
///
/// | 鉴权头 | 结果 |
/// |---|---|
/// | `Authorization: Cloud-IDE-JWT <JWT>` | HTTP 401 `20310 The user is not logged in,` |
/// | `x-cloudide-token: <JWT>` | HTTP 200，`Result.ScreenName` = 真实昵称 |
///
/// 7 种组合（`ReqSource` 取 IDE/Lite、带不带 `X-User-Region`、官方 UA、空 body）用
/// `Authorization` 全部 401，换成 `x-cloudide-token` 全部 200 —— 与 body、区域头、UA
/// 都无关，**只由鉴权头决定**。官方客户端里也是这个写法：
/// `headers: { "x-cloudide-token": token }`。
pub async fn fetch_user_info(api_host: &str, token: &str) -> Option<UserInfo> {
    let url = format!("{api_host}{GET_USER_INFO_PATH}");
    let resp = http()
        .ok()?
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-cloudide-token", token)
        .json(&serde_json::json!({ "ReqSource": "Lite", "IDEVersion": env!("CARGO_PKG_VERSION") }))
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;
    serde_json::from_str::<Value>(&text).ok().map(|v| parse_user_info(&v))
}

/// 第二步：轮询一次授权结果。
///
/// 本地回调服务器一收到 `/authorize` 就取到 `authCodeInfo`，这里随即换 token、拉账号。
/// 返回 `done=false` 表示用户还没登录完（**不是错误**，前端继续轮询即可）。
pub async fn poll(login_id: &str) -> Result<OAuthPoll, String> {
    let snapshot = {
        let map = locks();
        map.get(login_id).map(|p| {
            (
                p.callback.clone(),
                p.code_verifier.clone(),
                p.client_id.clone(),
                p.api_host.clone(),
                p.device_id.clone(),
                p.machine_id.clone(),
                p.expires_at,
                p.result.clone(),
            )
        })
    };
    let Some((callback, code_verifier, client_id, api_host, device_id, machine_id, expires_at, cached)) = snapshot else {
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

    // 还没收到回调 → 继续等（不是错误）
    let q = callback.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(q) = q else {
        return Ok(OAuthPoll::waiting());
    };
    let params = parse_query(&q);
    let auth_code = params
        .get("authCodeInfo")
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| Some(str_of(&v, &["AuthCode", "authCode"])))
        .filter(|s| !s.is_empty())
        .or_else(|| params.get("code").cloned());
    let Some(auth_code) = auth_code else {
        let r = OAuthPoll::failed("浏览器回调缺少授权码（authCodeInfo/AuthCode）");
        cache_result(login_id, &r);
        return Ok(r);
    };

    let ex = match exchange_token(&api_host, &client_id, &auth_code, &code_verifier, &device_id, &machine_id).await {
        Ok(ex) => ex,
        Err(e) => {
            let r = OAuthPoll::failed(&e);
            cache_result(login_id, &r);
            return Ok(r);
        }
    };

    let info = if ex.apihost.is_empty() {
        UserInfo::default()
    } else {
        fetch_user_info(&ex.apihost, &ex.token).await.unwrap_or_default()
    };

    // uid 优先用 GetUserInfo 的结果；该接口拿不到时（离线、限流）退回 JWT 载荷里的 `data.id`。
    // 这不是「锦上添花」——uid 同时是账号身份与签到头 `x-device-id` 的首选值
    // （见 `checkin::device_id`），丢了它新号就签不上。
    let uid = if info.uid.is_empty() {
        crate::token::user_id(&ex.token)
    } else {
        Some(info.uid)
    };

    let result = OAuthPoll {
        done: true,
        token: Some(ex.token),
        refresh_token: ex.refresh_token,
        host: Some(ex.apihost.clone()),
        region: region_for_host(&ex.apihost),
        uid,
        nickname: info.nickname,
        phone: info.phone,
        expires_at: ex.expires_at,
        device_id: Some(device_id.clone()),
        machine_id: Some(machine_id.clone()),
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
        let mut c = crate::proc::cmd("open");
        c.arg(u);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = crate::proc::cmd("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(u);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = crate::proc::cmd("xdg-open");
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
    fn host_selection_cn_vs_intl() {
        let (api, sso, region) = hosts(Some("https://api.trae.ai"));
        assert_eq!(api, "https://api.trae.ai");
        assert_eq!(sso, "https://www.trae.ai");
        assert_eq!(region, "us");
        let (_, sso, region) = hosts(Some("https://www.trae.cn"));
        assert_eq!(sso, "https://www.trae.cn");
        assert_eq!(region, "cn");
    }

    #[test]
    fn url_decode_handles_pct_and_plus() {
        assert_eq!(url_decode("a%2Bb%20c"), "a+b c");
        assert_eq!(url_decode("a%3D1"), "a=1");
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("plain"), "plain");
    }

    #[test]
    fn parse_query_decodes_values() {
        let m = parse_query("a=1&b=hello%20world&authCodeInfo=%7B%7D");
        assert_eq!(m["a"], "1");
        assert_eq!(m["b"], "hello world");
        assert_eq!(m["authCodeInfo"], "{}");
    }

    #[test]
    fn pkce_generates_valid_pairs() {
        let (v, c) = pkce();
        assert!(v.len() >= 43, "code_verifier 太短: {}", v.len());
        // code_challenge 必须是 base64url(sha256(code_verifier))
        let digest = Sha256::digest(v.as_bytes());
        let expect = URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(c, expect);
    }

    #[test]
    fn builds_authorization_url_with_callback_and_pkce() {
        let url = build_authorization_url("https://www.trae.cn", 51234, CLIENT_ID_SOLO, "abc", "DEV", "MAC");
        assert!(url.starts_with("https://www.trae.cn/authorization"));
        assert!(url.contains("client_id=en1oxy7wnw8j9n"));
        assert!(url.contains("auth_callback_url=http://127.0.0.1:51234/authorize"));
        assert!(url.contains("code_challenge=abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("login_version=1"));
        assert!(url.contains("auth_type=local"));
        assert!(url.contains("device_id=DEV"));
        assert!(url.contains("machine_id=MAC"));
        assert!(url.contains("hide_saas_login=true"));
    }

    #[test]
    fn parses_exchange_response_flat_and_nested() {
        let flat = json!({"Token": "t1", "TokenExpireAt": 1_760_000_000, "RefreshToken": "rt"});
        let e = parse_exchange_resp(&flat).unwrap();
        assert_eq!(e.token, "t1");
        assert_eq!(e.refresh_token.as_deref(), Some("rt"));
        assert_eq!(e.expires_at, Some(1_760_000_000_000));

        let nested = json!({"Result": {"Token": "t2", "apiHost": "https://api.trae.cn"}});
        let e = parse_exchange_resp(&nested).unwrap();
        assert_eq!(e.token, "t2");
        assert_eq!(e.apihost, "https://api.trae.cn");

        assert!(parse_exchange_resp(&json!({"Code": 1})).is_none());
    }

    #[test]
    fn parses_user_info_flat_and_nested() {
        let u = parse_user_info(&json!({"Result": {
            "UserID": "u-1", "Nickname": "waxiloao", "PhoneNumber": "190****9775"
        }}));
        assert_eq!(u.uid, "u-1");
        assert_eq!(u.nickname.as_deref(), Some("waxiloao"));
        assert_eq!(u.phone.as_deref(), Some("190****9775"));
    }

    /// 真实响应形状（2026-09-14 实测 `GetUserInfo` 200 的 `Result`）：昵称在 `ScreenName`，
    /// 手机号在 `NonPlainTextMobile` —— 这两个键名与原实现猜的 `Nickname`/`Phone` 不同。
    #[test]
    fn parses_user_info_real_screen_name_shape() {
        let u = parse_user_info(&json!({"Result": {
            "AIRegion": "CN",
            "NonPlainTextEmail": "",
            "NonPlainTextMobile": "191******52",
            "Region": "CN",
            "ScreenName": "用户0044120650",
            "TenantID": "7o2d894p7dr0o4",
            "UserID": "3225324630062683"
        }}));
        assert_eq!(u.uid, "3225324630062683");
        assert_eq!(u.nickname.as_deref(), Some("用户0044120650"));
        assert_eq!(u.phone.as_deref(), Some("191******52"));
    }

    #[test]
    fn spawn_callback_server_captures_query() {
        let (port, callback) = spawn_callback_server().unwrap();
        use std::io::Write as _;
        use std::net::TcpStream;
        let target = format!(
            "GET /authorize?authCodeInfo=%7B%22AuthCode%22%3A%22C%22%7D&userTag=x HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = stream.write_all(target.as_bytes());
        let _ = stream.flush();
        // 等后台线程读到并写回调
        for _ in 0..40 {
            if callback.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let q = callback.lock().unwrap().clone().expect("未收到回调");
        let params = parse_query(&q);
        assert!(params.contains_key("authCodeInfo"));
        assert_eq!(params["userTag"], "x");
        let info = serde_json::from_str::<Value>(params["authCodeInfo"].as_str()).unwrap();
        assert_eq!(str_of(&info, &["AuthCode"]), "C");
    }

#[test]
    fn timeout_produces_failed_not_error() {
        // 直接构造一个已过期的 pending（不真正发请求）
        let login_id = format!("trae_{}", uuid::Uuid::new_v4().simple());
        {
            let mut map = locks();
            map.insert(
                login_id.clone(),
                Pending {
                    callback: Arc::new(Mutex::new(None)),
                    code_verifier: "v".into(),
                    client_id: CLIENT_ID_SOLO.into(),
                    api_host: "https://api.trae.cn".into(),
                    device_id: "d".into(),
                    machine_id: "m".into(),
                    expires_at: Instant::now() - Duration::from_secs(1),
                    result: None,
                },
            );
        }
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(poll(&login_id))
            .unwrap();
        assert!(r.done);
        assert!(r.error.unwrap().contains("超时"));
    }
}
