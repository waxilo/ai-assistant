//! Qoder 登录：**设备授权流（device flow）**。
//!
//! 本项目从 workbuddy(clone) 改名而来，登录原本走 CodeBuddy 的 `/v2/plugin/auth/*`
//! （`auth/state` → `auth/token` 轮询 → `login/account`）。Qoder 是另一套，
//! 官方桌面端自己做的就是下面这四步（证据见 `basedata/20260918_Qoder缺失接口逆向.md` 第 2 节）：
//!
//! ```text
//! 1. verifier = 64 字符随机串（charset: A-Za-z0-9-._~）
//!    challenge = base64url(sha256(verifier))              ← PKCE S256
//!    nonce     = uuid v4
//!    machine_id = 读 Qoder 的 auth.machine-id（无则自生成 uuid v4）
//! 2. 交给系统浏览器打开：
//!    {authBaseUrl}/device/selectAccounts?challenge=&challenge_method=S256
//!        &nonce=&machine_id=&client_id=
//!    —— 服务端自己会 302 到 {authBaseUrl}/users/sign-in?biz_variant=qoder&oauth_callback=…
//!    所以**不需要**我们拼 sign-in 地址。
//!    ⚠️ 官方那条链接末尾还有 `&redirect_uri=qoder-app://`，那是它给自己注册的回调，
//!    我们**不能照抄** —— 见下节。
//! 3. 轮询（间隔 1s、总超时 300s）：
//!    GET {openApiBaseUrl}/api/v1/deviceToken/poll?nonce=&verifier=&challenge_method=S256
//!    404 = 用户还没点完（**正常等待态，不是错误**）；200 且带 token+refresh_token 即完成
//! 4. GET {openApiBaseUrl}/api/v1/userinfo 取昵称/邮箱/头像（失败不致命，token 已经到手）
//! ```
//!
//! # 为什么授权链接里**故意不带** `redirect_uri`
//!
//! 官方桌面端填的是 `qoder-app://`（asar 里 `authRedirectUris.stable`）。但那个 scheme
//! 不是公共设施，是**官方应用自己注册占用的**：
//!
//! - `/Applications/Qoder.app/Contents/Info.plist` → `CFBundleURLSchemes = [qoder, qoder-app]`；
//! - `lsregister -dump` → `Qoder` 这个 bundle 的 `claimed schemes: qoder-app:, qoder:`。
//!
//! 于是浏览器走完登录、服务端把用户送到设备流最后一步时，**系统会按 scheme 把 Qoder
//! 桌面应用拉起来**。这不是巧合 —— Qoder 主进程里 `Ke.on("open-url")` 明确把 `qoder-app://`
//! 当成它自己的登录回调（`o8 = () => authService.notifyLoginCallback()`），
//! 「浏览器授权完 → 唤起客户端」本来就是它的设计。
//!
//! 结论：照抄官方的 `redirect_uri` = **借用了别人的回调地址**，代价是用户每在我们这里
//! 登录一次，Qoder 桌面端就被拽到前台一次（2026-09-18 用户实报的就是这个现象）。
//! 我们靠轮询取凭证、**不需要任何浏览器回调**，所以这个参数直接留空；官方实现同样
//! 允许它为空（`...e.redirectUri ? { redirect_uri } : {}`），且实测「带 / 不带」在
//! 授权入口表现完全一致（都是 302 到 sign-in，oauth_callback 里也不塞该参数）。
//!
//! 实测（2026-09-18）：`poll` 用假 nonce 打过去返回 `HTTP 404 {"errorCode":"NotFound"}`，
//! `selectAccounts` 返回 `302` 到 sign-in 页 —— 端点与流程都对得上。
//!
//! # 区域（两套部署），所以「区域」是真的参数
//!
//! 旧版（CodeBuddy 时代）有国内版 / 国际版多套域，[`OAuthStart`] / [`OAuthPoll`] 都带一个
//! `host` 字段、前端配了个下拉框。Qoder 当时只有一套 Global 域，于是那个参数**恒被忽略**、
//! 字段**恒是同一个常量** —— 2026-09-18 整体删除。判断依据是：**不能改的域不该以「参数」
//! 的形态出现在签名里**，否则下一个人会以为这里可以选。
//!
//! 2026-09-19 Qoder 真的有两套部署了（国际版 `qoder.com` / 国内版 `qoder.cn`，
//! 域、CLI 目录、官方客户端全不同，实测见 [`crate::region`]），于是「区域」**回来了** ——
//! 但它不是当年那个 `host` 的复活：`host` 是「猜一个恒被忽略的值」，
//! 而 [`Region`] 是**用户在选择登录入口时明确指定的**那一个。同一条判据仍然成立：
//! 现在这个参数**真的能改**，所以它才配出现在签名里。
//!
//! 一次登录只属于一个区域：区域存进 [`Pending`] 会话，轮询与取用户信息都在它的
//! OpenAPI 上做，前端拿到 [`OAuthStart::region`] 后原样带进导入项。
//!
//! 其余字段与语义：
//! - `phone` 取 `/api/v1/userinfo` 的 `security_mobile`（**2026-09-19 更正**：旧版写的
//!   「该接口不含手机号」是看漏了字段，代价是助手内登录的账号手机号恒空）；
//! - `verification_uri` 是 `/device/selectAccounts` 那条地址（不是旧的 `login?state=`）。

use crate::qoder_api;
use crate::region::Region;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// PKCE verifier 的字符集与长度（官方 `Eve()` 逐字一致）。
const PKCE_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
const PKCE_LEN: usize = 64;

/// 授权有效期：超过这个时间未完成就判定超时（官方 `ave` 也是 300s）。
const OAUTH_TIMEOUT_SECS: u64 = 300;
/// 出结果后再保留一段，避免前端收尾时的重复轮询直接报「请求不存在」
const RESULT_RETENTION_SECS: u64 = 300;

/// Qoder 机器身份文件名（位于其 userData 目录下）
const MACHINE_ID_FILE: &str = "auth.machine-id";

/// 登录成功响应的有效期字段候选（官方用 `new Date(...).toISOString()` 生成 ⇒ ISO 字符串）
const ACCESS_ABS: &[&str] = &[
    "expiresAt",
    "expires_at",
    "accessTokenExpiresAt",
    "access_token_expires_at",
];
const ACCESS_REL: &[&str] = &["expiresIn", "expires_in", "access_token_expires_in"];
const REFRESH_ABS: &[&str] = &[
    "refreshTokenExpiresAt",
    "refresh_token_expires_at",
    "refreshExpiresAt",
    "refresh_expires_at",
];
const REFRESH_REL: &[&str] = &[
    "refreshExpiresIn",
    "refresh_expires_in",
    "refresh_token_expires_in",
];

#[derive(Serialize, Clone, Debug)]
pub struct OAuthStart {
    pub login_id: String,
    /// 交给系统浏览器打开的授权地址
    pub verification_uri: String,
    pub expires_in: u64,
    /// 这一轮登录打的是哪套部署 —— 前端导入时原样带回（见 `ImportItem::region`）。
    /// 授权链接本身不含区域信息，**只有发起方知道**用户点的是哪个入口。
    pub region: Region,
}

#[derive(Serialize, Clone, Debug)]
pub struct OAuthPoll {
    /// false = 还在等用户授权，前端应继续轮询
    pub done: bool,
    pub token: Option<String>,
    /// 续签用的 refresh token（授权接口一并返回，落库后才能自动续期）
    pub refresh_token: Option<String>,
    pub uid: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    /// 邮箱（`/api/v1/userinfo` 的 `email`）。国际版账号认的就是它
    /// —— 国内版的那套按手机号登录，邮箱常为空。
    pub email: Option<String>,
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
            uid: None,
            nickname: None,
            phone: None,
            email: None,
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

/// 一次进行中的登录：保存 PKCE 材料，等用户授权完成后换凭证。
struct Pending {
    /// PKCE verifier（轮询时必须原样回传）
    verifier: String,
    /// 本轮 nonce（既是会话标识也是轮询参数）
    nonce: String,
    /// 这一轮登录所属的区域 —— 轮询与取用户信息都打它的 OpenAPI。
    /// 存在会话里而不是让调用方每次传：前端手里只有 `login_id`，
    /// 「这个 login_id 属于哪套部署」只有发起时才知道。
    region: Region,
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
    map.retain(|_, p| now < p.expires_at + Duration::from_secs(RESULT_RETENTION_SECS));
}

/// 是不是一个格式合法的 uuid（官方 `jVe` 同款）。
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        };
        if !ok {
            return false;
        }
    }
    // 版本位与变体位：`[1-8]` 与 `[89ab]`
    matches!(b[14], b'1'..=b'8') && matches!(b[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

/// 本应用自己那份机器身份文件（Qoder 没装时才会用到）。
fn own_machine_id_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::data_dir()?
            .join("com.waxilo.qoder-assistant")
            .join(MACHINE_ID_FILE),
    )
}

/// 机器身份。
///
/// 优先用 **Qoder 自己那份** `auth.machine-id` —— 与桌面端同一个身份，服务端不会把本应用
/// 的登录当成一台陌生机器。**只读，绝不写 Qoder 的文件**（那是它的地盘）。
/// Qoder 没装或文件缺失时，退化成我们自己持久化的一份，保证反复登录时身份稳定。
///
/// 与官方 `MachineIdentity` 的差别只有一处：官方读不到时会**写回 Qoder 的目录**，
/// 我们改写自己的 —— 「不改别的应用的私有数据」比「少写一个文件」重要。
fn machine_id(region: Region) -> String {
    for dir in region.profile_dirs() {
        if let Ok(raw) = std::fs::read_to_string(dir.join(MACHINE_ID_FILE)) {
            let t = raw.trim();
            if is_uuid(t) {
                return t.to_string();
            }
        }
    }
    let path = own_machine_id_path();
    if let Some(existing) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
        let t = existing.trim();
        if is_uuid(t) {
            return t.to_string();
        }
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    if let Some(p) = path {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, &fresh);
    }
    fresh
}

/// 生成 PKCE verifier（64 字符，官方字符集）。
fn pkce_verifier() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..PKCE_LEN)
        .map(|_| PKCE_CHARS[rng.gen_range(0..PKCE_CHARS.len())] as char)
        .collect()
}

/// `base64url(sha256(verifier))`，无 padding（官方 `digest("base64url")`）。
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// 拼设备授权地址（官方 `Ive()`）。
///
/// 注意**不要自己再加 `/users/sign-in` 包装**：实测直接请求这个地址，服务端会自己
/// `302` 到 `{AUTH_BASE}/users/sign-in?oauth_callback=…&directLogin=true`。
/// 自己再包一层只会多一层转义、还可能与服务端的 `directLogin` 语义打架。
///
/// 也**不要**把官方那个 `redirect_uri` 补回来（理由见模块头那节）：它的值是
/// `qoder-app://`，而系统按这个 scheme 拉起的是**官方桌面端**，不是我们。
/// 顺带一提：国内版客户端的 `authRedirectUris.stable` 直接是 `null`，
/// 官方自己在这条路上都不用自定义 scheme。
///
/// `region` 决定 `{authBase}`（`qoder.com` / `qoder.cn`）；`client_id` 两个区域相同。
fn authorization_url(region: Region, challenge: &str, nonce: &str, machine_id: &str) -> String {
    format!(
        "{}/device/selectAccounts?challenge={}&challenge_method=S256&nonce={}&machine_id={}&client_id={}",
        region.auth_base(),
        urlencode(challenge),
        urlencode(nonce),
        urlencode(machine_id),
        urlencode(Region::AUTH_CLIENT_ID),
    )
}

/// 最小化的 URL 查询参数编码（只编码授权链接里会出现的字符集）。
///
/// 手写而不是引第三方：参与编码的只有 base64url 字符（challenge）与 uuid
/// （nonce / machine_id / client id），**全部是 URL 非保留字符** —— 也就是说当前
/// 每一次调用都应当逐字原样返回。留着它，是因为「输入今天恰好安全」不是可以长期
/// 依赖的前提；而单测会把这条前提钉住（授权链接里不该出现任何 `%`）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 登录成功响应解析出来的 token 对。
#[derive(Debug, Clone)]
pub(crate) struct TokenReady {
    pub token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub rt_expires_at: Option<i64>,
}

/// 解析 `deviceToken/poll` 的响应。
///
/// 只有同时拿到非空 `token` 才算完成（官方判定：`typeof d.token === "string" &&
/// typeof d.refresh_token === "string"`）；其余一律当作「还没授权」，
/// 这样 404、空体、网关塞进来的说明性 JSON 都会被正确归类成「继续等」。
pub(crate) fn parse_poll_response(v: &Value, now_ms: i64) -> Option<TokenReady> {
    let token = v.get("token").and_then(Value::as_str)?.trim();
    if token.is_empty() {
        return None;
    }
    let refresh_token = v
        .get("refresh_token")
        .or_else(|| v.get("refreshToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let expires_at = crate::timeutil::token_expiry(v, ACCESS_ABS, ACCESS_REL, now_ms);
    let rt_expires_at = crate::timeutil::token_expiry(v, REFRESH_ABS, REFRESH_REL, now_ms);

    Some(TokenReady {
        token: token.to_string(),
        refresh_token,
        expires_at,
        rt_expires_at,
    })
}

/// 从 `/api/v1/userinfo` 响应里取 uid / 昵称 / 手机号 / 邮箱（失败不致命）。
///
/// # 手机号就在这个响应里，字段叫 `security_mobile`
///
/// 「userinfo 不含手机号」曾经是本模块的结论 —— 那是**看漏了字段**：响应里本来就有
/// `security_mobile`（官方桌面端 `AuthService.fetchUser()` 正是这么取的：
/// `phone: fc(i,["security_mobile"])?.trim() || null`，见 `app.asar` 主进程 bundle）。
/// 代价是助手内登录出来的账号手机号恒为空，而账号列表、跨机账号合并（`broker` 的
/// 身份锚点是「手机号 → 昵称 → 本地 id」）都拿它当第一顺位，于是一个本该最稳的锚点
/// 直接退化成昵称。
///
/// # 邮箱也在同一个响应里，字段叫 `email`
///
/// 两套部署的登录身份不同：国内版手机号、国际版邮箱（实测响应见
/// `basedata/20260918_Qoder缺失接口逆向.md`，两个字段同框）。这里一次取全，
/// 免得再为邮箱单开一趟请求 —— 界面按区域选一个展示（见前端 `accountIdent`）。
///
/// 只认官方那**一个**键：这里不是「猜服务端可能叫什么」，而是抄官方实现，
/// 多列几个近义词反而会让「字段真的改名了」这件事被静默掩盖。
pub(crate) struct UserInfo {
    pub uid: Option<String>,
    pub name: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
}

pub(crate) fn parse_userinfo(v: &Value) -> UserInfo {
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| v.get(*k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    UserInfo {
        uid: pick(&["id", "user_id", "uid"]),
        name: pick(&["name", "username", "user_name"]),
        phone: pick(&["security_mobile"]),
        email: pick(&["email"]),
    }
}

/// 第一步：生成 PKCE 材料并给出授权地址。
///
/// `region` 是用户选的那个登录入口（国际版 / 国内版），它决定授权页的域、
/// 机器身份的来源目录，以及后续轮询打哪套 OpenAPI。
pub async fn start(region: Region) -> Result<OAuthStart, String> {
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let nonce = uuid::Uuid::new_v4().to_string();
    let machine = machine_id(region);
    let verification_uri = authorization_url(region, &challenge, &nonce, &machine);

    let login_id = format!("qoder_{}", uuid::Uuid::new_v4().simple());
    {
        let mut map = locks();
        sweep(&mut map);
        map.insert(
            login_id.clone(),
            Pending {
                verifier,
                nonce,
                region,
                expires_at: Instant::now() + Duration::from_secs(OAUTH_TIMEOUT_SECS),
                result: None,
            },
        );
    }
    Ok(OAuthStart {
        login_id,
        verification_uri,
        expires_in: OAUTH_TIMEOUT_SECS,
        region,
    })
}

fn cache_result(login_id: &str, r: &OAuthPoll) {
    let mut map = locks();
    if let Some(p) = map.get_mut(login_id) {
        p.result = Some(r.clone());
    }
}

/// 第二步：轮询一次授权结果；完成后顺带拉账号信息（昵称/uid）。
pub async fn poll(login_id: &str) -> Result<OAuthPoll, String> {
    let snapshot = {
        let map = locks();
        map.get(login_id).map(|p| {
            (
                p.verifier.clone(),
                p.nonce.clone(),
                p.region,
                p.expires_at,
                p.result.clone(),
            )
        })
    };
    let Some((verifier, nonce, region, expires_at, cached)) = snapshot else {
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

    let url = format!(
        "{}/api/v1/deviceToken/poll?nonce={}&verifier={}&challenge_method=S256",
        region.openapi_base(),
        urlencode(&nonce),
        urlencode(&verifier),
    );
    let resp = match qoder_api::client().get(&url).send().await {
        Ok(r) => r,
        // 网络抖动：当作「还在等待」，让前端继续轮询（与官方重试语义一致）
        Err(_) => return Ok(OAuthPoll::waiting()),
    };
    // 404 = 用户还没完成授权（实测就是这个码），**不是错误**
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(OAuthPoll::waiting());
    }
    if !resp.status().is_success() {
        return Ok(OAuthPoll::waiting());
    }
    let Ok(body) = resp.json::<Value>().await else {
        return Ok(OAuthPoll::waiting());
    };

    let now = crate::timeutil::now_ms();
    let Some(ready) = parse_poll_response(&body, now) else {
        return Ok(OAuthPoll::waiting());
    };

    // 拿到 token 后取账号信息：失败不致命（token 已经到手，只是少了几样标识）
    let info = match qoder_api::get_json(region, &ready.token, "/api/v1/userinfo", &[]).await {
        Some(v) => parse_userinfo(&v),
        None => UserInfo {
            uid: None,
            name: None,
            phone: None,
            email: None,
        },
    };

    let result = OAuthPoll {
        done: true,
        token: Some(ready.token),
        refresh_token: ready.refresh_token,
        uid: info.uid,
        nickname: info.name,
        // 手机号 / 邮箱来自同一个响应（`security_mobile` 与 `email`，见 [`parse_userinfo`]）。
        // 取不到就是取不到：不拿 uid / 昵称去凑一个「看起来像」的值。
        phone: info.phone,
        email: info.email,
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
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("打开浏览器失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_verifier_uses_official_charset_and_length() {
        let v = pkce_verifier();
        assert_eq!(v.len(), PKCE_LEN);
        assert!(
            v.bytes().all(|b| PKCE_CHARS.contains(&b)),
            "verifier 只该用官方字符集，实际 {v}"
        );
        // 两次调用必须不同（随机性）
        assert_ne!(v, pkce_verifier());
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 附录 B 的官方示例向量，用来确认编码方式（base64url 无 padding）没写错
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorization_url_carries_every_required_param() {
        let url = authorization_url(
            Region::Global,
            "abc-DEF_123",
            "9ecfb156-86c9-49b8-8563-a6cef3987f5c",
            "m1",
        );
        assert!(url.starts_with("https://qoder.com/device/selectAccounts?"), "{url}");
        assert!(url.contains("challenge=abc-DEF_123"), "base64url 字符不该被转义：{url}");
        assert!(url.contains("challenge_method=S256"));
        assert!(url.contains("nonce=9ecfb156-86c9-49b8-8563-a6cef3987f5c"));
        assert!(url.contains("machine_id=m1"));
        assert!(url.contains(Region::AUTH_CLIENT_ID));
    }

    /// 回归：两个区域的授权链接里都**绝不能**出现 `redirect_uri`。
    ///
    /// 官方那条链接的末尾是 `&redirect_uri=qoder-app://`，而系统按这个 scheme 拉起的是
    /// **官方桌面端**（`lsregister` 里 `qoder-app:` 归 `/Applications/Qoder.app`）。
    /// 一旦有人「照着官方补回来」，用户每在我们这里登录一次，Qoder 应用就被拽到前台一次。
    #[test]
    fn authorization_url_never_carries_a_redirect_uri() {
        let url = authorization_url(
            Region::Global,
            "c",
            "9ecfb156-86c9-49b8-8563-a6cef3987f5c",
            "m",
        );
        assert!(
            !url.contains("redirect_uri") && !url.contains("qoder-app"),
            "不许把官方桌面端的回调地址借过来：{url}"
        );
        // 顺带钉住「所有取值都是 URL 非保留字符」这个前提：真出现转义，说明有新输入混进来了
        assert!(!url.contains('%'), "四个取值都该无需转义：{url}");
    }

    #[test]
    fn urlencode_leaves_unreserved_chars_alone() {
        assert_eq!(urlencode("aZ0-._~"), "aZ0-._~");
        assert_eq!(urlencode("a/b:c"), "a%2Fb%3Ac");
    }

    #[test]
    fn uuid_check_accepts_real_ids_and_rejects_junk() {
        assert!(is_uuid("9ecfb156-86c9-49b8-8563-a6cef3987f5c"));
        assert!(!is_uuid(""));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("9ecfb15686c949b88563a6cef3987f5c"));
        // 版本位非法（第 15 位必须是 1-8）
        assert!(!is_uuid("9ecfb156-86c9-09b8-8563-a6cef3987f5c"));
    }

    #[test]
    fn poll_404_and_empty_body_mean_still_waiting() {
        // 实测 404 的响应体（这就是「用户还没点」的正常态）
        let v = json!({"errorCode": "NotFound", "errorMessage": "Not found",
                       "requestId": "f64ab0f8-22f2-4ee3-8615-d7b8b84e37cc"});
        assert!(parse_poll_response(&v, 0).is_none());
        // 非 JSON 或空对象同理
        assert!(parse_poll_response(&json!({}), 0).is_none());
        // token 为空串也不算完成
        assert!(parse_poll_response(&json!({"token": "  ", "refresh_token": "r"}), 0).is_none());
    }

    #[test]
    fn parses_ready_token_pair() {
        let v = json!({"token": "at-123", "refresh_token": "rt-456"});
        let r = parse_poll_response(&v, 1_000).expect("有 token 就该判定完成");
        assert_eq!(r.token, "at-123");
        assert_eq!(r.refresh_token.as_deref(), Some("rt-456"));
        // 响应没给有效期就不该发明一个
        assert_eq!(r.expires_at, None);
        assert_eq!(r.rt_expires_at, None);
    }

    #[test]
    fn poll_parses_iso_expiries() {
        // 官方用 `new Date(...).toISOString()` 生成，所以实际是 ISO 字符串
        let v = json!({
            "token": "at", "refresh_token": "rt",
            "expiresAt": "2026-10-18T07:51:53Z",
            "refreshTokenExpiresAt": "2027-09-13T07:51:53Z",
        });
        let r = parse_poll_response(&v, 0).unwrap();
        assert_eq!(r.expires_at, Some(1792309913000));
        assert_eq!(r.rt_expires_at, Some(1820821913000));
        // 两条命互不串用
        assert_ne!(r.expires_at, r.rt_expires_at);
    }

    #[test]
    fn poll_accepts_camel_and_snake_aliases() {
        let v = json!({
            "token": "at", "refreshToken": "rt-camel",
            "expires_in": 3600,
            "refresh_token_expires_in": 7200,
        });
        let r = parse_poll_response(&v, 1_000_000).unwrap();
        assert_eq!(r.refresh_token.as_deref(), Some("rt-camel"));
        // 相对秒数按 now 折算
        assert_eq!(r.expires_at, Some(1_000_000 + 3_600_000));
        assert_eq!(r.rt_expires_at, Some(1_000_000 + 7_200_000));
    }

    #[test]
    fn parses_userinfo_with_field_fallbacks() {
        // 这段是从实测响应抄来的，原始的 id / name / email / 手机号是**真实账号数据**，
        // 一律替换成占位值（uuid 尾部清零，与 `usage.rs` 的脱敏写法保持一致）。
        let info = parse_userinfo(&json!({
            "id": "01a0b380-0000-0000-0000-000000000000",
            "name": "Example User",
            "email": "user@example.com",
            "security_mobile": "13800000000"
        }));
        assert_eq!(info.uid.as_deref(), Some("01a0b380-0000-0000-0000-000000000000"));
        assert_eq!(info.name.as_deref(), Some("Example User"));
        assert_eq!(info.phone.as_deref(), Some("13800000000"));
        assert_eq!(info.email.as_deref(), Some("user@example.com"));
        // 只有 username 时也要能取到
        let info = parse_userinfo(&json!({"user_id": "u1", "username": "nick"}));
        assert_eq!(info.uid.as_deref(), Some("u1"));
        assert_eq!(info.name.as_deref(), Some("nick"));
        assert!(info.email.is_none());
        // 空对象不 panic，也不编造
        let info = parse_userinfo(&json!({}));
        assert!(info.uid.is_none() && info.name.is_none());
        assert!(info.phone.is_none() && info.email.is_none());
    }

    /// 邮箱字段的边界：只认 `email`，空白值不算数（与手机号同一条规矩）。
    ///
    /// 国际版账号的展示身份就是它 —— 落成 `Some("")` 会在界面上显示一个空格子，
    /// 也会让「有身份标识」的判分（去重时用）把一条没用的记录算成完整的。
    #[test]
    fn email_comes_from_the_email_key_only_and_blank_is_none() {
        let info = parse_userinfo(&json!({"email": " user@example.com "}));
        assert_eq!(info.email.as_deref(), Some("user@example.com"), "两侧空白要剪掉");
        for v in [
            json!({"email": ""}),
            json!({"email": "   "}),
            json!({"mail": "user@example.com"}),
            json!({"user_email": "user@example.com"}),
            json!({"email": null}),
        ] {
            let info = parse_userinfo(&v);
            assert!(info.email.is_none(), "不该把 {v} 认成邮箱");
        }
    }

    /// 手机号字段的边界：只认官方那个键，且空白值不算数。
    ///
    /// 这钉住两件事 —— ① 字段名是 `security_mobile`（抄 `AuthService.fetchUser()`），
    /// 不是 `mobile` / `phone`；② `"  "` 与 `""` 都是「没有」，不能落成 `Some("")`
    /// （界面上会显示一个空格子，而合并键会把它当成一个真实身份）。
    #[test]
    fn phone_comes_from_security_mobile_only_and_blank_is_none() {
        let info = parse_userinfo(&json!({"security_mobile": " 13800000000 "}));
        assert_eq!(info.phone.as_deref(), Some("13800000000"), "两侧空白要剪掉");
        for v in [
            json!({"security_mobile": ""}),
            json!({"security_mobile": "   "}),
            json!({"mobile": "13800000000"}),
            json!({"phone": "13800000000"}),
            json!({"security_mobile": null}),
            json!({"security_mobile": 13800000000i64}),
        ] {
            let info = parse_userinfo(&v);
            assert!(info.phone.is_none(), "不该把 {v} 认成手机号");
        }
    }

    #[test]
    fn sweep_drops_finished_entries_after_retention() {
        let mut map = HashMap::new();
        map.insert(
            "old".to_string(),
            Pending {
                region: Region::Global,
                verifier: "v".into(),
                nonce: "n".into(),
                expires_at: Instant::now() - Duration::from_secs(RESULT_RETENTION_SECS + 10),
                result: None,
            },
        );
        map.insert(
            "fresh".to_string(),
            Pending {
                region: Region::Global,
                verifier: "v".into(),
                nonce: "n".into(),
                expires_at: Instant::now() + Duration::from_secs(60),
                result: None,
            },
        );
        sweep(&mut map);
        assert!(!map.contains_key("old"), "过期太久的条目要清掉");
        assert!(map.contains_key("fresh"));
    }

    /// 本机冒烟：真实发一次 device flow 的 start + 一次 poll。
    ///
    /// 不做真实登录（那要人扫码），只确认：授权地址拼得对、poll 打到真端点且
    /// 在未授权时稳定返回 `waiting`（**不是 error**）。
    /// `cargo test --lib -- --ignored --nocapture smoke_real_device_flow`
    #[tokio::test]
    #[ignore = "真实网络调用"]
    async fn smoke_real_device_flow() {
        // 两个区域各起一轮：域不同（`qoder.com` / `qoder.cn`），其余拼法一致
        let s = start(Region::Global).await.expect("start 不该失败（纯本地生成）");
        println!("login_id={}", s.login_id);
        println!("verification_uri={}", s.verification_uri);
        assert!(s.verification_uri.starts_with("https://qoder.com/device/selectAccounts?"));
        assert_eq!(s.region, Region::Global);
        let cn = start(Region::Cn).await.expect("start 不该失败（纯本地生成）");
        assert!(
            cn.verification_uri
                .starts_with("https://qoder.cn/device/selectAccounts?"),
            "国内版的授权页必须打在 qoder.cn 上：{}",
            cn.verification_uri
        );
        assert_eq!(cn.region, Region::Cn);
        assert!(s.verification_uri.contains("directLogin") || s.verification_uri.contains("client_id"));

        let r = poll(&s.login_id).await.expect("poll 不该返回 Err");
        println!("done={} error={:?}", r.done, r.error);
        assert!(!r.done, "没人扫码时必须是 waiting（done=false），而不是失败");
        assert!(r.error.is_none(), "等待态不该被当成错误：{:?}", r.error);
    }
}
