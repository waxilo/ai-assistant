//! 直接读取 WorkBuddy 桌面端写在磁盘上的登录信息文件（对齐 WorkDaddy 的做法）。
//!
//! WorkBuddy 登录成功后会把账号与凭证落盘到：
//!
//! - macOS: `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/*.info`
//! - Windows: `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\*.info`
//!
//! 文件是 JSON，结构形如：
//! ```json
//! { "account": { "uid": "...", "nickname": "...", "phoneNumber": "...", "lastLogin": true },
//!   "auth":    { "accessToken": "...", "refreshToken": "...", "expiresAt": 1794410008517,
//!                "domain": "www.workbuddy.cn" } }
//! ```
//!
//! 相比「登录新账号」OAuth 与已移除的 CDP 抓包，这条通道的优势：
//! 1. 不要求应用处于运行状态、也不要求带 `--remote-debugging-port` 启动；
//! 2. 一次就能拿到 token **加上**昵称与手机号，用户无需手填；
//! 3. token 由应用自己续期，读到的就是最新的那份。
//!
//! 局限：它只能拿到**已经在本机登录过**的账号——要收新账号得走 [`crate::oauth`]。

use crate::checkin;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 国内版官方固定文件名：只有它才是 WorkBuddy 真正读取的「当前登录」。
/// 国际版的 `workbuddy-desktop-ai.info` 不在此列 —— 它整个会被丢弃（见 [`is_international`]）。
const CANONICAL_FILES: &[&str] = &["workbuddy-desktop.info"];

/// 判定 token 下限长度，避免把占位串当作凭证
const MIN_TOKEN_LEN: usize = 20;

/// 直接读本机 WorkBuddy 登录文件（auth/*.info）得到的账号。
/// 一次就能拿到 token + 昵称 + 手机号，是首选通道。
#[derive(Serialize, Clone, Debug)]
pub struct LocalAccount {
    pub token: String,
    /// 续签用的 refresh token（有它才能自动续期）
    pub refresh_token: Option<String>,
    pub source: String,
    pub host: Option<String>,
    /// 账号唯一 ID（官方文件里的 account.uid）
    pub uid: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    /// access token 过期时间（毫秒时间戳）
    pub expires_at: Option<i64>,
    /// **refresh token** 过期时间（毫秒时间戳）；文件里没写就是 None。
    /// 与 `expires_at` 分开，因为两者是两条独立的命，而只有后者能换来新 token。
    pub rt_expires_at: Option<i64>,
    /// 来自官方固定文件或带 lastLogin 标记，即当前实际登录的账号
    pub is_current: bool,
    pub file: String,
}

// ---------------------------------------------------------------------------
// 平台路径
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn auth_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![auth_dir(home.join("Library").join("Application Support"))]
}

#[cfg(target_os = "windows")]
fn auth_dirs() -> Vec<PathBuf> {
    let local = dirs::data_local_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("AppData").join("Local")))
        .unwrap_or_else(|| PathBuf::from("."));
    vec![auth_dir(local)]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn auth_dirs() -> Vec<PathBuf> {
    Vec::new()
}

fn auth_dir(root: PathBuf) -> PathBuf {
    root.join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
}

// ---------------------------------------------------------------------------
// JSON 取值（外部数据，全部按宽松方式读取）
// ---------------------------------------------------------------------------

/// 取字符串。
///
/// 新版客户端把 `accessToken` / `refreshToken` / `nickname` / `phoneNumber` 换成了
/// `{$wbEncrypted:1, envelope:"…"}` 密文壳子，所以这里不能只认 `as_str()`——
/// 统一交给 [`crate::atrest::reveal`]：明文原样返回，壳子就地解开。
/// 解不开（拿不到密钥 / 结构不认识）仍然返回 `None`，由调用方决定怎么报。
fn str_at(root: &Value, path: &[&str]) -> Option<String> {
    let mut cur = root;
    for key in path {
        cur = cur.get(*key)?;
    }
    crate::atrest::reveal(cur)
}

fn bool_at(root: &Value, path: &[&str]) -> bool {
    let mut cur = root;
    for key in path {
        match cur.get(*key) {
            Some(v) => cur = v,
            None => return false,
        }
    }
    cur.as_bool().unwrap_or(false)
}

fn num_at(root: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = root;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_i64()
        .or_else(|| cur.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// 把 auth.domain（如 `www.workbuddy.cn`）规整成 origin。
///
/// 只支持 WorkBuddy：历史文件里按 CodeBuddy 国内域登录的会归一到 `workbuddy.cn`
/// （两域同一套后端）；国际版 `*.ai` 返回 None，调用方退回国内默认域。
fn origin_from_domain(domain: &str) -> Option<String> {
    let d = domain.trim().trim_end_matches('/');
    if d.is_empty() {
        return None;
    }
    let lower = d.to_ascii_lowercase();
    if lower.contains("workbuddy.ai") || lower.contains("codebuddy.ai") {
        return None;
    }
    let origin = if d.starts_with("http://") || d.starts_with("https://") {
        d.to_string()
    } else {
        format!("https://{d}")
    };
    Some(crate::oauth::to_workbuddy_host(&origin))
}

/// 先按 camelCase 找，再退回 snake_case（官方字段两种风格都出现过）
fn pick(root: &Value, dir: &str, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| str_at(root, &[dir, *k]))
}

/// 数字版 [`pick`]：同一个键有好几种写法时，按给定顺序取第一个能解析出来的。
fn num_pick(root: &Value, dir: &str, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| num_at(root, &[dir, *k]))
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 国际版识别：命中任意一条即整条丢弃 —— 本助手只保留国内版一套部署。
/// 三个互不依赖的信号：官方 `-ai` 文件名、auth.domain 非国内域、token iss 非国内网关。
///
/// 国内域见 [`crate::oauth::is_domestic_host`]：`.cn` 与 `copilot.tencent.com` 都算
/// （后者是桌面端自己的网关）。**这一步判错会静默丢账号**，所以两个分支共用同一份判定。
fn is_international(name: &str, json: &Value, token: &str) -> bool {
    if name.starts_with("workbuddy-desktop-ai") {
        return true;
    }
    if let Some(d) = pick(json, "auth", &["domain", "issuer"]) {
        if !crate::oauth::is_domestic_host(&d) {
            return true;
        }
    }
    if let Some(iss) = checkin::token_issuer_origin(token) {
        // 只保留国内部署签发的票。历史账号可能由 CodeBuddy 国内域签发 ——
        // 那仍是国内版、同一套后端，不算国际版；它的 host 归一会落到 workbuddy.cn。
        if !crate::oauth::is_domestic_host(&iss) {
            return true;
        }
    }
    false
}

fn parse_json(name: &str, path: &Path, json: &Value) -> Option<LocalAccount> {
    if !json.is_object() {
        return None;
    }

    let token = pick(json, "auth", &["accessToken", "access_token", "token"])?;
    if token.len() < MIN_TOKEN_LEN {
        return None;
    }
    if is_international(name, json, &token) {
        return None;
    }

    // 优先用 token 的 iss（与签到链路一致，可直接当 API host），
    // 拿不到再退回文件里的 domain
    let host = checkin::issuer_host(&token).or_else(|| {
        pick(json, "auth", &["domain", "issuer"]).and_then(|d| origin_from_domain(&d))
    });

    Some(LocalAccount {
        token,
        refresh_token: pick(json, "auth", &["refreshToken", "refresh_token"]),
        source: format!("本机 WorkBuddy 登录信息（{name}）"),
        host,
        uid: str_at(json, &["account", "uid"]),
        nickname: str_at(json, &["account", "nickname"]),
        phone: pick(json, "account", &["phoneNumber", "phone_number", "phone"]),
        expires_at: num_pick(json, "auth", &["expiresAt", "expires_at"]),
        // 登录文件是**快照**，只会写绝对时间戳（没有 `expiresIn` 那种相对秒数的余地）
        rt_expires_at: num_pick(
            json,
            "auth",
            &["refreshExpiresAt", "refresh_expires_at", "refreshTokenExpiresAt"],
        ),
        is_current: CANONICAL_FILES.contains(&name) || bool_at(json, &["account", "lastLogin"]),
        file: path.display().to_string(),
    })
}

/// 单个登录文件的解析结果。
///
/// 之所以要区分 `Locked` 和 `Skip`：新版客户端把凭证字段加密后，**「解不开」与
/// 「本来就没有账号」在界面上长得一模一样**（都是「本机未发现账号」），而这两件事
/// 的处理方式完全不同——前者要提示用户去启动一次客户端或改用无感登录，后者只能
/// 让用户自己去登录。所以这个区别必须在类型上保留下来，不能在这里悄悄吞掉。
enum Parsed {
    Account(LocalAccount),
    /// 文件里确实有凭证字段，但它是 `$wbEncrypted` 壳子且这次解不开。
    Locked,
    /// 不是登录文件 / 国际版 / 字段不全，本来就不该导入。
    Skip,
}

fn parse_file(path: &Path) -> Parsed {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Parsed::Skip;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Parsed::Skip;
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return Parsed::Skip;
    };
    match parse_json(name, path, &json) {
        Some(acc) => Parsed::Account(acc),
        None if auth_field_is_envelope(&json) => Parsed::Locked,
        None => Parsed::Skip,
    }
}

/// `auth.accessToken` 是不是一个没被解开的加密壳子。
fn auth_field_is_envelope(json: &Value) -> bool {
    json.pointer("/auth")
        .and_then(|a| a.get("accessToken"))
        .map(|v| v.get("$wbEncrypted").and_then(Value::as_u64) == Some(1))
        .unwrap_or(false)
}

/// 一次扫描的完整结果。
pub struct LocalScan {
    /// 可以直接导入的账号（已去重、按可用性排序）。
    pub accounts: Vec<LocalAccount>,
    /// 「有凭证但解不开」的文件名 —— 非空即为**需要告诉用户**的状态。
    pub locked: Vec<String>,
    /// 当前登录文件（`workbuddy-desktop.info`）在盘上，但这次没解析出账号。
    ///
    /// 这时 `accounts` 里的东西**全部来自历史快照**，而快照里的票早被客户端顶掉了。
    /// 上层必须据此报错：让旧快照悄悄顶上去，用户看到的是「导入成功」，实际导进来的
    /// 是一张死票，签到必定 401 —— 比直接说「取不到当前登录信息」难查得多。
    pub stale_only: bool,
}

/// 扫描本机 WorkBuddy 登录文件。
///
/// 国际版账号（`-ai` 文件 / `*.ai` 域 / 非国内网关 iss）整条丢弃，只回国内版；
/// 同一账号的多份快照（官方落盘的带时间戳备份）按 uid 去重，保留「当前登录」优先的那份；
/// 未过期的排在前面。
pub fn scan_local_accounts() -> LocalScan {
    let mut found: Vec<LocalAccount> = Vec::new();
    let mut locked: Vec<String> = Vec::new();
    let mut on_disk: Vec<String> = Vec::new();
    for dir in auth_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // 只认 .info，跳过 WorkDaddy 的 `.logged-out` 与写入中的临时文件
            if !name.ends_with(".info") || name.ends_with(".tmp") {
                continue;
            }
            on_disk.push(name.to_string());
            match parse_file(&path) {
                Parsed::Account(acc) => found.push(acc),
                Parsed::Locked => locked.push(name.to_string()),
                Parsed::Skip => {}
            }
        }
    }

    let is_canonical = |n: &str| CANONICAL_FILES.contains(&n);
    dedup_and_sort(&mut found);
    // 判定放在去重之后：万一 canonical 被去重挤掉（它才是唯一可信的那份），
    // 结果就只剩快照了，这时必须报错。
    let stale_only = on_disk.iter().any(|n| is_canonical(n))
        && !found
            .iter()
            .filter_map(|a| Path::new(&a.file).file_name().and_then(|n| n.to_str()))
            .any(is_canonical);

    LocalScan {
        accounts: found,
        locked,
        stale_only,
    }
}

/// 只要账号列表（调用方不关心「为什么少了一个」时用这个）。
pub fn discover_local_accounts() -> Vec<LocalAccount> {
    scan_local_accounts().accounts
}

/// 去重 + 排序。
///
/// **优先级必须是「canonical 文件 > lastLogin 标记 > 目录顺序」**：官方落盘的
/// 历史快照（`workbuddy-desktop.2026-09-11T….info`）里也写着 `lastLogin: true`，
/// 如果只按 `is_current` 排、再按目录顺序取第一条，读到哪份快照就导哪份 ——
/// 结果是「导入成功，票却是两个月前的死票」。目录顺序在 NTFS 上完全不可控。
fn dedup_and_sort(found: &mut Vec<LocalAccount>) {
    found.sort_by(|a, b| {
        canonical_rank(a).cmp(&canonical_rank(b)).reverse().then(b.is_current.cmp(&a.is_current))
    });
    let mut seen: HashSet<String> = HashSet::new();
    found.retain(|a| {
        let key = a.uid.clone().unwrap_or_else(|| a.token.clone());
        seen.insert(key)
    });

    let now = chrono::Utc::now().timestamp_millis();
    found.sort_by(|a, b| {
        let live = |x: &LocalAccount| x.expires_at.map(|e| e > now).unwrap_or(true);
        live(b).cmp(&live(a)).then(b.expires_at.cmp(&a.expires_at))
    });
}

/// 文件是不是「当前登录」那份（`workbuddy-desktop.info`）。
fn is_canonical_file(field: &str) -> bool {
    Path::new(field)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| CANONICAL_FILES.contains(&n))
        .unwrap_or(false)
}

/// 排序用：canonical 记 1，历史快照记 0。
fn canonical_rank(a: &LocalAccount) -> u8 {
    u8::from(is_canonical_file(&a.file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    fn sample(access_token: &str) -> Value {
        json!({
            "account": {
                "uid": "aaaaaaaa-0000",
                "nickname": "demo-user",
                "phoneNumber": "13800000000",
                "lastLogin": true
            },
            "auth": {
                "accessToken": access_token,
                "refreshToken": "refresh-value",
                "expiresAt": 1794410008517i64,
                "domain": "www.workbuddy.cn"
            }
        })
    }

    #[test]
    fn parses_camel_case_offsets() {
        let json = sample(&"a".repeat(64));
        let acc = parse_json(
            "workbuddy-desktop.info",
            Path::new("/tmp/workbuddy-desktop.info"),
            &json,
        )
        .expect("应能解析");

        assert_eq!(acc.uid.as_deref(), Some("aaaaaaaa-0000"));
        assert_eq!(acc.nickname.as_deref(), Some("demo-user"));
        assert_eq!(acc.phone.as_deref(), Some("13800000000"));
        assert_eq!(acc.expires_at, Some(1794410008517));
        assert_eq!(acc.host.as_deref(), Some("https://www.workbuddy.cn"));
        assert!(acc.is_current);
    }

    #[test]
    fn accepts_snake_case_fields() {
        let json = json!({
            "account": { "uid": "u-1", "phone_number": "13800000000" },
            "auth": {
                "access_token": "b".repeat(48),
                "expires_at": "1794410008517",
                "domain": "www.codebuddy.cn"
            }
        });
        let acc = parse_json("workbuddy-desktop-cn.info", Path::new("x.info"), &json).unwrap();

        assert_eq!(acc.phone.as_deref(), Some("13800000000"));
        assert_eq!(acc.expires_at, Some(1794410008517));
        // 老文件里按 CodeBuddy 国内域登录的账号，host 归一到 WorkBuddy
        assert_eq!(acc.host.as_deref(), Some("https://www.workbuddy.cn"));
    }

    #[test]
    fn rejects_placeholder_and_junk() {
        // 过短的占位串不算凭证
        assert!(parse_json("a.info", Path::new("a.info"), &sample("short")).is_none());
        // 缺 auth.accessToken
        let no_token = json!({ "account": { "uid": "u" }, "auth": {} });
        assert!(parse_json("a.info", Path::new("a.info"), &no_token).is_none());
        // 非对象
        assert!(parse_json("a.info", Path::new("a.info"), &json!([1, 2, 3])).is_none());
    }

    #[test]
    fn drops_international_accounts() {
        let token = "a".repeat(64);
        // 官方 -ai 文件名（含带时间戳的快照变体）
        let json = sample(&token);
        assert!(parse_json("workbuddy-desktop-ai.info", Path::new("x"), &json).is_none());
        assert!(parse_json(
            "workbuddy-desktop-ai.2026-09-17T10-05-04-510Z.50672.x.info",
            Path::new("x"),
            &json
        )
        .is_none());
        // auth.domain 指向国际域
        let mut ai_domain = sample(&token);
        ai_domain["auth"]["domain"] = json!("www.workbuddy.ai");
        assert!(parse_json("someone.info", Path::new("x"), &ai_domain).is_none());
        // token iss 指向非 CN 网关（domain 字段缺失也得拦住）
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"iss":"https://gateway.workbuddy.ai/v1"}"#);
        let jwt = format!("{}.{}.{}", "x", payload, "y");
        let iss_ai = json!({
            "account": { "uid": "u-ai", "nickname": "overseas" },
            "auth": { "accessToken": jwt }
        });
        assert!(iss_ai["auth"]["accessToken"].as_str().unwrap().len() >= MIN_TOKEN_LEN);
        assert!(parse_json("someone.info", Path::new("x"), &iss_ai).is_none());
        // 对照：国内域照常解析
        assert!(parse_json("workbuddy-desktop.info", Path::new("x"), &sample(&token)).is_some());

        // 对照：CodeBuddy 国内域签发（历史账号）仍属国内版 —— 不能当国际版丢掉，
        // 它的 host 要归一成 WorkBuddy 后继续可用
        let cn_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"iss":"https://www.codebuddy.cn/auth/realms/copilot"}"#);
        let cn_jwt = format!("{}.{}.{}", "x", cn_payload, "y");
        let iss_cn = json!({
            "account": { "uid": "u-cn", "nickname": "old-cn" },
            "auth": { "accessToken": cn_jwt }
        });
        let acc = parse_json("someone.info", Path::new("x"), &iss_cn)
            .expect("CodeBuddy 国内域不算国际版");
        assert_eq!(acc.host.as_deref(), Some("https://www.workbuddy.cn"));
    }

    #[test]
    fn keeps_accounts_signed_by_the_desktop_gateway() {
        // 回归：桌面端签发的 token `iss` 与 auth.domain 都是 `copilot.tencent.com`。
        // 曾经把它当「国际版」整条丢掉，于是导入只剩历史快照 → 导进来的是死票，签到一直 401。
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"iss":"https://copilot.tencent.com/auth/realms/copilot"}"#);
        let jwt = format!("{}.{}.{}", "x", payload, "y");
        let json = json!({
            "account": {
                "uid": "aaaaaaaa-0000-4000-8000-000000000000",
                "nickname": "demo-user",
                "phoneNumber": "13800000000",
                "lastLogin": true
            },
            "auth": {
                "accessToken": jwt,
                "refreshToken": "refresh-value",
                "expiresAt": 1795245624261i64,
                "domain": "copilot.tencent.com"
            }
        });
        let acc = parse_json("workbuddy-desktop.info", Path::new("x"), &json)
            .expect("桌面端网关签发的票属于国内版，不能丢");
        assert_eq!(acc.uid.as_deref(), Some("aaaaaaaa-0000-4000-8000-000000000000"));
        assert_eq!(acc.host.as_deref(), Some("https://copilot.tencent.com"));
        assert!(acc.is_current);
    }

    /// 端到端自检：跑一遍真实的「从本机凭证导入」，打印结果。
    ///
    /// 需要客户端已登录且在运行（密钥从它的内存里来），所以默认忽略：
    ///
    /// ```text
    /// WB_ATREST_LIVE=1 cargo test --lib --offline auth_file::tests::scans_real_auth_dir \
    ///     -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "读本机真实登录文件，需要客户端已登录且在运行"]
    fn scans_real_auth_dir() {
        // 用临时缓存目录，强制走「扫内存拿密钥」这条真实路径
        let tmp = std::env::temp_dir().join("wb-import-selfcheck");
        let _ = std::fs::remove_file(tmp.join("atrest-key.bin"));
        crate::atrest::init(&tmp);

        let scan = scan_local_accounts();
        for a in &scan.accounts {
            println!(
                "account nickname={:?} uid={:?} host={:?} current={} token_len={} file={}",
                a.nickname,
                a.uid,
                a.host,
                a.is_current,
                a.token.len(),
                a.file
            );
        }
        println!("locked={:?} stale_only={}", scan.locked, scan.stale_only);
        assert!(
            !scan.stale_only,
            "当前登录文件没解析出来 —— 只会导进历史快照的死票"
        );
        assert!(
            scan.accounts.iter().any(|a| a.is_current),
            "结果里必须包含「当前登录」那个账号"
        );
    }

    #[test]
    fn canonical_login_file_wins_over_stale_snapshots() {
        // 回归：历史快照里也写着 lastLogin: true，曾经靠目录顺序决定谁胜出 ——
        // 结果导进来的是两个月前的死票（签到必 401）。canonical 必须无条件优先。
        let mk = |file: &str, token: &str, exp: i64| LocalAccount {
            token: token.to_string(),
            refresh_token: None,
            source: "test".into(),
            host: None,
            uid: Some("same-uid".into()),
            nickname: None,
            phone: None,
            expires_at: Some(exp),
            rt_expires_at: None,
            is_current: true,
            file: file.to_string(),
        };
        let mut found = vec![
            mk(
                "/a/workbuddy-desktop.2026-09-11T09-10-52-457Z.44696.x.info",
                "stale-token",
                1794410008517,
            ),
            mk("/a/workbuddy-desktop.info", "live-token", 1795245624261),
        ];
        dedup_and_sort(&mut found);

        assert_eq!(found.len(), 1, "同 uid 只留一条");
        assert_eq!(found[0].token, "live-token", "必须留当前登录那份");
    }

    #[test]
    fn non_canonical_file_without_last_login_is_not_current() {
        let json = sample(&"c".repeat(64));
        let acc = parse_json("someone-else.info", Path::new("/tmp/someone-else.info"), &json).unwrap();
        // sample 里 lastLogin = true，因此这里仍算当前登录
        assert!(acc.is_current);

        let mut other = sample(&"d".repeat(64));
        other["account"]["lastLogin"] = json!(false);
        let acc = parse_json("someone-else.info", Path::new("/tmp/someone-else.info"), &other).unwrap();
        assert!(!acc.is_current);
    }

    #[test]
    fn normalizes_domain_origins() {
        assert_eq!(
            origin_from_domain("www.workbuddy.cn").as_deref(),
            Some("https://www.workbuddy.cn")
        );
        // CodeBuddy 国内域归一到 WorkBuddy（同一套后端，只换域名）
        assert_eq!(
            origin_from_domain("https://www.codebuddy.cn/").as_deref(),
            Some("https://www.workbuddy.cn")
        );
        // 国际版已移除：*.ai 不再产出 host
        assert_eq!(origin_from_domain("https://www.workbuddy.ai/"), None);
        assert_eq!(origin_from_domain("www.codebuddy.ai"), None);
        assert_eq!(origin_from_domain("   "), None);
    }

    /// 本机冒烟：`cargo test -- --ignored --nocapture`。
    /// 只打印非敏感字段（token 仅打印长度），用于确认真实环境能读到并解析。
    #[test]
    #[ignore]
    fn smoke_reads_real_auth_dir() {
        let list = discover_local_accounts();
        for a in &list {
            println!(
                "uid={:?} nickname={:?} phone={:?} host={:?} current={} expired={} token_len={}",
                a.uid,
                a.nickname,
                a.phone,
                a.host,
                a.is_current,
                a.expires_at
                    .map(|e| e <= chrono::Utc::now().timestamp_millis())
                    .unwrap_or(false),
                a.token.len(),
            );
        }
        println!("total={}", list.len());
    }
}
