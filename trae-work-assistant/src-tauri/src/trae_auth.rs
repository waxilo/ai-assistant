//! 读取 TraeWork（TRAE SOLO）桌面端落盘的登录态，解密得到每个账号的凭证。
//!
//! 凭证位置（与 WorkBuddy 的明文 .info 不同，TraeWork 是加密 blob）：
//! - macOS: `~/Library/Application Support/TRAE SOLO CN/User/globalStorage/storage.json`
//! - Windows: `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`
//!
//! 结构：`storage.json` 里 `"iCubeAuthInfo://icube.cloudide"` 字段是一段 base64 的加密
//! 二进制。解密后的 JSON 大致为：
//! ```json
//! { "userId": "...", "token": "<JWT>", "refreshToken": "...",
//!   "expiredAt": 1726..., "refreshExpiredAt": 1738..., "host": "https://api.trae.cn",
//!   "userRegion": { "region": "cn" }, "account": { "nickname": "...", "avatar": "...",
//!   "nonPlainTextMobile": "190******75" } }
//! ```
//!
//! 解密算法（与官方论坛公开的「每日签到」技能包同源，已在本机验证）：
//! 1. 取 blob 偏移 6..38 的 32 字节作为种子 key；
//! 2. `key = sha512(种子key)`，`blobKey = key[0..64] XOR (URE XOR DRE)`（均 64 字节）；
//! 3. `hash = sha512(blobKey)`，取 `aesKey = hash[0..16]`，`iv = hash[16..32]`；
//! 4. `AES-128-CBC` 解密 `blob[38..]`，PKCS7 去填充，再丢弃前 64 字节。
//!
//! 局限：与 WorkBuddy 读文件一个道理——只能拿到**本机已登录过**的账号。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha512};
use std::path::{Path, PathBuf};

/// storage.json 里凭证的键
const AUTH_KEY: &str = "iCubeAuthInfo://icube.cloudide";
/// mac 密钥常量（逆向得到，技能包同源）
const URE: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57,
    130, 155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166,
    194, 35, 61, 238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178,
    118, 91, 162, 73, 109, 139, 209, 37,
];
/// iv 签名常量（逆向得到）
const DRE: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169,
    25, 181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42,
    245, 176, 200, 235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225,
    105, 20, 99, 85, 33, 12, 125,
];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 一个解密出来的本地 TraeWork 账号
#[derive(Serialize, Clone, Debug)]
pub struct TraeLocalAccount {
    pub user_id: Option<String>,
    pub token: String,
    pub refresh_token: Option<String>,
    pub host: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    pub region: Option<String>,
    /// 设备标识：来自 storage.json 顶层 `telemetry.devDeviceId`，签到必填头 `X-Device-Id` 来源
    pub device_id: Option<String>,
    /// 机器标识：来自 storage.json 顶层 `telemetry.machineId`，签到必填头 `X-Machine-Id` 来源
    pub machine_id: Option<String>,
    /// access token 过期（毫秒）
    pub expires_at: Option<i64>,
    /// refresh token 过期（毫秒）
    pub refresh_expires_at: Option<i64>,
}

fn xor_64(a: &[u8; 64], b: &[u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// 解密 storage.json 的某个登录态字段。返回 JSON 字符串。
pub(crate) fn decrypt_auth_blob(b64: &str) -> Result<String, String> {
    let blob = STANDARD
        .decode(b64)
        .map_err(|e| format!("base64 解码失败：{e}"))?;
    // blob 至少要有 Em+RV(=38) 的头 + 一个 16 字节的 AES 块
    if blob.len() < 38 + 16 {
        return Err("登录态 blob 过短".into());
    }
    let seed = &blob[6..38];
    let mut h = Sha512::new();
    h.update(seed);
    let sha: [u8; 64] = h.finalize().into();

    let combo_key = xor_64(&URE, &DRE);
    let mut comb = [0u8; 128];
    comb[..64].copy_from_slice(&sha);
    comb[64..].copy_from_slice(&combo_key);
    let mut h2 = Sha512::new();
    h2.update(comb);
    let hash: [u8; 64] = h2.finalize().into();

    let aes_key: &[u8] = &hash[..16];
    let iv: &[u8] = &hash[16..32];
    let ct = &blob[38..];

    let mut buf = ct.to_vec();
    let pt = Aes128CbcDec::new(aes_key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| format!("AES-CBC 解密失败：{e:?}"))?;
    // 前 64 字节是 hmac 摘要，正文在后
    let body = pt
        .get(64..)
        .ok_or_else(|| "解密结果缺少正文".to_string())?;
    String::from_utf8(body.to_vec()).map_err(|e| format!("解密结果非 UTF-8：{e}"))
}

// ---------------------------------------------------------------------------
// 平台路径
// ---------------------------------------------------------------------------

/// 可能的 TraeWork 桌面端数据根目录（国内版 TRAE SOLO CN 与国内默认 TRAE）
fn storage_paths() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            let base = home.join("Library").join("Application Support");
            for app in ["TRAE SOLO CN", "TRAE", "Trae TRAE", "TRAE CN"] {
                roots.push(base.join(app).join("User").join("globalStorage"));
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // TraeWork 桌面端把 userData 落在 %APPDATA%（Roaming）；少数场景也可能出现在
        // %LOCALAPPDATA%（Local）。两个根都扫，避免漏掉本机登录态。
        let bases: Vec<PathBuf> = [dirs::data_dir(), dirs::data_local_dir()]
            .into_iter()
            .flatten()
            .collect();
        for base in bases {
            for app in ["TRAE SOLO CN", "Trae CN", "TRAE CN", "TRAE", "Trae TRAE"] {
                roots.push(base.join(app).join("User").join("globalStorage"));
            }
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in roots {
        if !seen.contains(&p) {
            seen.push(p.clone());
            out.push(p);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 宽松取值
// ---------------------------------------------------------------------------

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn num_at(v: &Value, key: &str) -> Option<i64> {
    v.get(key)
        .and_then(|x| x.as_i64())
        .or_else(|| str_at(v, key).and_then(|s| s.parse::<i64>().ok()))
}

/// 从某个 storage.json 里解析账号（一个登录态 = 一个账号）。
fn parse_storage(path: &Path) -> Option<TraeLocalAccount> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let blob = json.get(AUTH_KEY)?.as_str()?;
    let dec = decrypt_auth_blob(blob).ok()?;
    let data: Value = serde_json::from_str(&dec).ok()?;

    let token = str_at(&data, "token")?;
    if token.len() < 40 {
        return None; // 占位/失效
    }
    let account = data.get("account");
    let region = data
        .get("userRegion")
        .and_then(|r| str_at(r, "region"))
        .or_else(|| account.and_then(|a| str_at(a, "region")));

    // 设备/机器标识在 storage.json 顶层，以**点号平铺键**存在（不是嵌套对象）：
    // "telemetry.machineId" / "telemetry.devDeviceId"，是签到接口必填头 X-Machine-Id / X-Device-Id 的来源
    let machine_id = str_at(&json, "telemetry.machineId");
    let device_id = str_at(&json, "telemetry.devDeviceId");

    Some(TraeLocalAccount {
        user_id: str_at(&data, "userId"),
        token,
        refresh_token: str_at(&data, "refreshToken"),
        host: str_at(&data, "host"),
        nickname: account.and_then(|a| str_at(a, "nickname")),
        phone: account.and_then(|a| {
            str_at(a, "nonPlainTextMobile")
                .or_else(|| str_at(a, "mobile"))
        }),
        region,
        device_id,
        machine_id,
        expires_at: num_at(&data, "expiredAt"),
        refresh_expires_at: num_at(&data, "refreshExpiredAt"),
    })
}

/// 扫描本机 TraeWork 登录态，返回可导入的账号列表。
/// 一个 storage.json 只有一个当前登录态，但不同安装/不同 user-data 目录可能各自持有一个。
pub fn discover_local_accounts() -> Vec<TraeLocalAccount> {
    let mut out: Vec<TraeLocalAccount> = Vec::new();
    let mut seen_tokens: Vec<String> = Vec::new();
    for dir in storage_paths() {
        let p = dir.join("storage.json");
        if let Some(acc) = parse_storage(&p) {
            if !seen_tokens.contains(&acc.token) {
                let t = acc.token.clone();
                seen_tokens.push(t);
                out.push(acc);
            }
        }
    }
    // 有效的（refresh 未过期）排前面
    let now = chrono::Utc::now().timestamp_millis();
    out.sort_by(|a, b| {
        let live = |x: &TraeLocalAccount| {
            x.refresh_expires_at
                .map(|e| e > now)
                .or(x.expires_at.map(|e| e > now))
                .unwrap_or(true)
        };
        live(b).cmp(&live(a))
    });
    out
}

/// 本机 TraeWork 的**真实设备标识** `(devDeviceId, machineId)`（`storage.json` 顶层的
/// `telemetry.devDeviceId` / `telemetry.machineId`）。
///
/// 语义与官方客户端一致：**一台机器一个设备号**，与当前登录的是哪个账号无关
/// （官方 `fb()` 里就是 `x-device-id = guaranteedDeviceId`）。
///
/// 用途：给「本机没有登录态的账号」（如浏览器登录新签发的号）提供可信的 `x-device-id`。
/// 浏览器登录流程落库的 `device_id` 是**授权时随机生成的 uuid**，服务端从未登记过它，
/// `checkin_credits/claim` 会持续回 9074 —— 见 [`crate::checkin::device_id`]。
pub fn local_device_identity() -> (Option<String>, Option<String>) {
    for dir in storage_paths() {
        let Some(text) = std::fs::read_to_string(dir.join("storage.json")).ok() else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let device = str_at(&json, "telemetry.devDeviceId");
        let machine = str_at(&json, "telemetry.machineId");
        if device.is_some() || machine.is_some() {
            return (device, machine);
        }
    }
    (None, None)
}

/// 本机 TraeWork 登录态里是否有这个 uid 的登录态。
///
/// 用途：**续签的第二条路**（[`crate::renew`]）—— TraeWork 自己会拿 refresh token 续签并把
/// 新 token 写回 `storage.json`，所以对「同时登录在本机 TraeWork 里」的账号，
/// 我们只要在它更新后同步过来即可，不需要任何密钥。
pub fn find_local_session_by_uid(uid: &str) -> Option<TraeLocalAccount> {
    discover_local_accounts()
        .into_iter()
        .find(|a| a.user_id.as_deref() == Some(uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_works() {
        let a = [1u8; 64];
        let b = [1u8; 64];
        assert_eq!(xor_64(&a, &b), [0u8; 64]);
    }

    #[test]
    fn rejects_short_or_invalid_blob() {
        assert!(decrypt_auth_blob("").is_err());
        assert!(decrypt_auth_blob("c2hvcnQ=").is_err());
    }

    /// 真实本机冒烟：`cargo test -- --ignored --nocapture`
    /// 只打印非敏感字段（token 只打长度）。
    #[test]
    #[ignore]
    fn smoke_reads_real_storage() {
        let list = discover_local_accounts();
        for a in &list {
            println!(
                "uid={:?} nickname={:?} phone={:?} region={:?} host={:?} token_len={} refresh={}",
                a.user_id,
                a.nickname,
                a.phone,
                a.region,
                a.host,
                a.token.len(),
                a.refresh_token.is_some(),
            );
        }
        println!("total={}", list.len());
    }
}
