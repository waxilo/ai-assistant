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

/// 官方固定文件名：只有这两份才是 WorkBuddy 真正读取的「当前登录」
const CANONICAL_FILES: &[&str] = &["workbuddy-desktop.info", "workbuddy-desktop-ai.info"];

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

fn str_at(root: &Value, path: &[&str]) -> Option<String> {
    let mut cur = root;
    for key in path {
        cur = cur.get(*key)?;
    }
    let s = cur.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
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

/// 把 auth.domain（如 `www.workbuddy.cn`）规整成 origin
fn origin_from_domain(domain: &str) -> Option<String> {
    let d = domain.trim().trim_end_matches('/');
    if d.is_empty() {
        return None;
    }
    if d.starts_with("http://") || d.starts_with("https://") {
        Some(d.to_string())
    } else {
        Some(format!("https://{d}"))
    }
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

fn parse_json(name: &str, path: &Path, json: &Value) -> Option<LocalAccount> {
    if !json.is_object() {
        return None;
    }

    let token = pick(json, "auth", &["accessToken", "access_token", "token"])?;
    if token.len() < MIN_TOKEN_LEN {
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

fn parse_file(path: &Path) -> Option<LocalAccount> {
    let name = path.file_name()?.to_str()?;
    let text = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    parse_json(name, path, &json)
}

/// 扫描本机 WorkBuddy 登录文件，返回可直接导入的账号列表。
///
/// 同一账号可能同时存在 CN 与 AI 两份文件，按 uid 去重并保留「当前登录」优先的那份；
/// 未过期的排在前面。
pub fn discover_local_accounts() -> Vec<LocalAccount> {
    let mut found: Vec<LocalAccount> = Vec::new();
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
            if let Some(acc) = parse_file(&path) {
                found.push(acc);
            }
        }
    }

    found.sort_by(|a, b| b.is_current.cmp(&a.is_current));
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
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(access_token: &str) -> Value {
        json!({
            "account": {
                "uid": "bf8a3f24-6415",
                "nickname": "waxiloao",
                "phoneNumber": "19098779775",
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

        assert_eq!(acc.uid.as_deref(), Some("bf8a3f24-6415"));
        assert_eq!(acc.nickname.as_deref(), Some("waxiloao"));
        assert_eq!(acc.phone.as_deref(), Some("19098779775"));
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
                "domain": "www.workbuddy.ai"
            }
        });
        let acc = parse_json("workbuddy-desktop-ai.info", Path::new("x.info"), &json).unwrap();

        assert_eq!(acc.phone.as_deref(), Some("13800000000"));
        assert_eq!(acc.expires_at, Some(1794410008517));
        assert_eq!(acc.host.as_deref(), Some("https://www.workbuddy.ai"));
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
        assert_eq!(
            origin_from_domain("https://www.workbuddy.ai/").as_deref(),
            Some("https://www.workbuddy.ai")
        );
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
