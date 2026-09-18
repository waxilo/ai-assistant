//! 直接读取 Qoder 桌面端写给本机的凭据文件 `auth.v1.dat`。
//!
//! ## 与 workbuddy 的根本差异
//!
//! CodeBuddy/WorkBuddy 的登录文件是**明文 JSON**（`auth/*.info`），Qoder 则用
//! Electron `safeStorage`（Chromium **OSCrypt**）加密，文件为二进制：
//!
//! - Windows: `%APPDATA%\com.qoder.app.stable\auth.v1.dat`
//! - macOS:   `~/Library/Application Support/com.qoder.app.stable`（safeStorage 走 Keychain，端外解不出 → 走 OAuth）
//!
//! `auth.v1.dat` 布局（实测 426B）：`"v10"` 魔数(3B) + nonce(12B) + AES-256-GCM 密文 + tag(16B)。
//! AES 数据密钥不落盘在此文件，而是由 DPAPI 包裹后存在**同目录 `Local State`** 的
//! `os_crypt.encrypted_key`（base64，前缀 `"DPAPI"`）。
//!
//! 端外读取两步（已实测验证）：
//! 1. `Local State` 的 `encrypted_key` 去 `"DPAPI"` 前缀 → `CryptUnprotectData`(当前用户) → 32B AES 密钥；
//! 2. `auth.v1.dat` 去 `"v10"` → 切 nonce(12)/密文/tag(16) → AES-256-GCM 解密 → UTF-8 JSON。
//!
//! JSON 结构：`{schemaVersion:1, token, refreshToken, expiresAt, refreshTokenExpiresAt,
//!  user:{id,name,email,phone,avatarUrl}, profileOverlay:{...}, firstLoginOnboardingSeen}`。
//!
//! 实测两个有效期字段都是 **ISO-8601 字符串**（`"2026-10-18T07:51:53Z"` 与
//! `"2027-09-13T07:51:53Z"`），不是毫秒数 —— 解析必须走 [`crate::timeutil`]，
//! 直接 `parse::<i64>()` 会静默失败并把有效期变成 `None`。
//!
//! ## 唯一账号
//!
//! `auth.v1.dat` 只保存**当前登录的这一个账号**，切号/重登都会覆盖它 —— 因此没有 workbuddy
//! 那种「多账号列表」，Qoder 是单账号跟随模型。这里始终只返回 0 或 1 条。
//!
//! ## 写回策略
//!
//! 续签后允许**原子写回**回 `auth.v1.dat`（用户选择"允许原子写回"）。AES 密钥不变（仍读
//! `Local State`），所以写回只需重新生成 nonce + AES-GCM 加密即可，不触碰 `Local State`。
//! 写回走"临时文件 + rename"保证原子性，避免与 Qoder 主进程读到半个文件。

use crate::timeutil::token_expiry;
use aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Qoder 当前账号 API 基准域名（续签 `/api/v1/deviceToken/refresh` 等 api/v1 族打到 openapi）
pub const OPENAPI_HOST: &str = crate::qoder_api::OPENAPI_BASE;

/// auth.v1.dat 的名字（固定）
const AUTH_FILE: &str = "auth.v1.dat";

/// 记录 AES 密钥的 Chromium 状态文件名（带空格，勿改）
const LOCAL_STATE_FILE: &str = "Local State";

/// DPAPI blob 前缀（encrypted_key base64 解码后前 5 字节）
const DPAPI_PREFIX: &[u8] = b"DPAPI";

/// 判定 token 下限长度，避免把占位串当作凭证
const MIN_TOKEN_LEN: usize = 20;

/// 直读本机 Qoder 凭据得到的「当前账号」（Qoder 单账号模型，始终 0 或 1 条）。
/// 一次就能拿到 token + refresh_token + 昵称 + 手机号，是首选通道。
#[derive(Serialize, Clone, Debug)]
pub struct LocalAccount {
    pub token: String,
    /// 续签用的 refresh token
    pub refresh_token: Option<String>,
    pub source: String,
    // 这里曾经有 `pub host: Option<String>`，恒等于 `OPENAPI_HOST`。一个永远取同一个值的
    // 字段不携带任何信息，只会让「这里可以选域」的错觉活下去，2026-09-18 删除。
    pub uid: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    /// access token 过期时间（毫秒时间戳）
    pub expires_at: Option<i64>,
    /// refresh token 过期时间（毫秒时间戳）
    pub rt_expires_at: Option<i64>,
    /// auth.v1.dat 只有当前账号，恒为 true
    pub is_current: bool,
    pub file: String,
}

// ---------------------------------------------------------------------------
// 平台路径
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub(crate) fn profile_dirs() -> Vec<PathBuf> {
    let Some(roaming) = dirs::data_dir() else {
        return Vec::new();
    };
    vec![roaming.join("com.qoder.app.stable")]
}

#[cfg(target_os = "macos")]
pub(crate) fn profile_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![home
        .join("Library")
        .join("Application Support")
        .join("com.qoder.app.stable")]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn profile_dirs() -> Vec<PathBuf> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// DPAPI（仅 Windows）：解出 Chromium OSCrypt 的 32B AES 数据密钥
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{
        CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB, CryptUnprotectData,
    };

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // 解密仅用当前用户凭据：CRYPTPROTECT_UI_FORBIDDEN 禁止弹 UI，熵为空
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(), // ppszDataDescr
            std::ptr::null(),     // pOptionalEntropy
            std::ptr::null(),     // pvReserved
            std::ptr::null_mut(), // pPromptStruct
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        let code = std::io::Error::last_os_error();
        return Err(format!("DPAPI 解密 encrypted_key 失败：{code}"));
    }
    let len = out.cbData as usize;
    let mut result = vec![0u8; len];
    unsafe {
        std::ptr::copy_nonoverlapping(out.pbData, result.as_mut_ptr(), len);
        // CryptUnprotectData 通过 LocalAlloc 分配，理想应 LocalFree 归还；此处单次调用、
        // 块很小，且当前 windows-sys 暴露面下 LocalFree 不可用，故不复原（进程退出即回收）。
    }
    Ok(result)
}

#[cfg(not(target_os = "windows"))]
fn dpapi_unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
    Err("当前平台(Qoder safeStorage 走 Keychain)不支持端外解密，请走 OAuth 登录".to_string())
}

/// 从 profile 目录的 `Local State` 解出 32B AES-256 数据密钥。
fn derive_aead_key(profile_dir: &Path) -> Result<[u8; 32], String> {
    let state_path = profile_dir.join(LOCAL_STATE_FILE);
    let text = std::fs::read_to_string(&state_path)
        .map_err(|e| format!("读取 {} 失败：{e}", state_path.display()))?;
    let state: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 {} 失败：{e}", state_path.display()))?;
    let b64 = state
        .get("os_crypt")
        .and_then(|o| o.get("encrypted_key"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} 缺少 os_crypt.encrypted_key", state_path.display()))?;
    let decoded = B64
        .decode(b64)
        .map_err(|e| format!("加密密钥不是合法 base64：{e}"))?;
    if !decoded.starts_with(DPAPI_PREFIX) {
        return Err("encrypted_key 缺少 DPAPI 前缀（非 Windows 加密层？）".to_string());
    }
    let blob = &decoded[DPAPI_PREFIX.len()..];
    let unwrapped = dpapi_unprotect(blob)?;
    if unwrapped.len() < 32 {
        return Err(format!("AES 密钥长度异常：{}（应 ≥32B）", unwrapped.len()));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&unwrapped[..32]);
    Ok(key)
}

// ---------------------------------------------------------------------------
// OSCrypt 密文：打包 / 拆包
// ---------------------------------------------------------------------------

/// auth.v1.dat 拆包：`v10` + nonce(12) + 密文(+tag)
fn unpack_v10(data: &[u8]) -> Option<(&[u8], &[u8])> {
    // 魔数 "v10"
    if data.get(..3) != Some(b"v10".as_slice()) {
        return None;
    }
    let nonce = data.get(3..3 + 12)?;
    let body = &data[3 + 12..];
    if body.is_empty() {
        return None;
    }
    Some((nonce, body))
}

/// AES-256-GCM 解密（OSCrypt 数据层）。
fn gcm_decrypt(key: &[u8; 32], nonce: &[u8], body: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("AES 密钥初始化失败：{e}"))?;
    cipher
        .decrypt(aes_gcm::Nonce::from_slice(nonce), body)
        .map_err(|_| "AES-GCM 数据校验失败（AuthTag 不匹配，密钥或密文被改动）".to_string())
}

/// AES-256-GCM 加密：随机 nonce，返回可直接写盘的 `v10 + nonce + 密文(+tag)` 封装。
/// 块2（续签写回）启用后即被调用。
#[allow(dead_code)]
fn gcm_encrypt_pack(key: &[u8; 32], plain: &[u8], rng: &mut impl RngCore) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("AES 密钥初始化失败：{e}"))?;
    let mut nonce = [0u8; 12];
    rng.fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), plain)
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    let mut packed = Vec::with_capacity(3 + 12 + sealed.len());
    packed.extend_from_slice(b"v10");
    packed.extend_from_slice(&nonce);
    packed.extend_from_slice(&sealed);
    Ok(packed)
}

// ---------------------------------------------------------------------------
// 解析 auth.v1.dat 的 JSON → LocalAccount
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

/// 把解密出的 Qoder 凭据 JSON 映射成 [`LocalAccount`]。
fn parse_credentials(path: &Path, json: &Value) -> Option<LocalAccount> {
    if !json.is_object() {
        return None;
    }
    let token = str_at(json, &["token"])
        .or_else(|| str_at(json, &["accessToken"]))
        .or_else(|| str_at(json, &["access_token"]))?;
    if token.len() < MIN_TOKEN_LEN {
        return None;
    }
    let source = "本机 Qoder 凭据（auth.v1.dat，OSCrypt 解密）".to_string();
    Some(LocalAccount {
        token,
        refresh_token: str_at(json, &["refreshToken"]).or_else(|| str_at(json, &["refresh_token"])),
        source,
        uid: str_at(json, &["user", "id"]),
        nickname: str_at(json, &["user", "name"]).or_else(|| str_at(json, &["user", "username"])),
        phone: str_at(json, &["user", "phone"]).or_else(|| str_at(json, &["phone"])),
        // 有效期是 **ISO-8601 字符串**（实测 `"2026-10-18T07:51:53Z"`），不是毫秒数 ——
        // 这里必须走统一的时间戳归一化，否则 `parse::<i64>()` 静默失败、有效期变成 None。
        // `now_ms` 传 0：这两个字段是绝对时间，不涉及相对秒数折算。
        expires_at: token_expiry(json, &["expiresAt", "expires_at"], &[], 0),
        rt_expires_at: token_expiry(
            json,
            &["refreshTokenExpiresAt", "refresh_token_expires_at"],
            &[],
            0,
        ),
        is_current: true,
        file: path.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// 公开入口
// ---------------------------------------------------------------------------

/// 扫描 Qoder 本机凭据，返回当前登录账号（Qoder 单账号模型 → 恒 0 或 1 条）。
///
/// macOS/其它平台 safeStorage 走 Keychain，端外解不出，返回空（前端据此走 OAuth 登录）。
pub fn discover_local_accounts() -> Vec<LocalAccount> {
    let Some((profile_dir, file, json)) = read_credentials_if_possible() else {
        return Vec::new();
    };
    let _ = &profile_dir;
    parse_credentials(&file, &json).into_iter().collect()
}

/// 读 + 解密成功后返回 (profile 目录, 文件路径, 解密后的 JSON)。
/// 任何一环缺失/失败都静默返回 None（走 OAuth 兜底），不 panic。
pub(crate) fn read_credentials_if_possible() -> Option<(PathBuf, PathBuf, Value)> {
    for profile_dir in profile_dirs() {
        let file = profile_dir.join(AUTH_FILE);
        let locked = std::fs::read(&file).ok()?;
        let key = derive_aead_key(&profile_dir).ok()?;
        let (nonce, body) = unpack_v10(&locked)?;
        let plain = gcm_decrypt(&key, nonce, body).ok()?;
        let json: Value = serde_json::from_slice(&plain).ok()?;
        if json.is_object() {
            return Some((profile_dir, file, json));
        }
    }
    None
}

/// 重新加密并**原子写回** `auth.v1.dat`（续签后用）。
///
/// AES 密钥不变，只重加密数据；写盘走「临时文件 + rename」，保证 Qoder 主进程不会读到半个文件。
/// 仅 Windows 可成功写回；其它平台无密钥 → Err，调用方应吞掉（只读模式）。
pub(crate) fn write_back(json: &Value) -> Result<(), String> {
    let Some((profile_dir, file, _)) = read_credentials_if_possible() else {
        // 连读都读不到时没有密钥，无法写回（安全起见也绝不新建文件覆盖原状态）
        return Err("未读到 Qoder 凭据，无法写回".to_string());
    };
    let key = derive_aead_key(&profile_dir)?;
    let plain = serde_json::to_vec(json).map_err(|e| format!("序列化凭据失败：{e}"))?;
    let packed = gcm_encrypt_pack(&key, &plain, &mut rand::rngs::OsRng)?;
    let tmp = file.with_extension("dat.tmp");
    std::fs::write(&tmp, &packed).map_err(|e| format!("写临时凭据文件失败：{e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("原子写回凭据失败：{e}"))?;
    Ok(())
}

/// 续签成功后，把新 token 对与有效期**原子写回** `auth.v1.dat`。
///
/// 只改 token / refreshToken 及两者有效期四项，其余字段（`user` 等）原样保留。
/// 读不到凭据（macOS、未登录）时静默返回 Ok —— 只读模式，写回是尽力而为。
pub(crate) fn apply_refresh(
    token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<i64>,
    rt_expires_at: Option<i64>,
) -> Result<(), String> {
    let Some((_, _, mut json)) = read_credentials_if_possible() else {
        return Ok(());
    };
    json["token"] = Value::String(token.to_string());
    match refresh_token {
        Some(rt) => json["refreshToken"] = Value::String(rt.to_string()),
        None => _ = json["refreshToken"].take(),
    }
    match expires_at {
        Some(ms) => json["expiresAt"] = Value::from(ms),
        None => _ = json["expiresAt"].take(),
    }
    match rt_expires_at {
        Some(ms) => json["refreshTokenExpiresAt"] = Value::from(ms),
        None => _ = json["refreshTokenExpiresAt"].take(),
    }
    write_back(&json)
}

/// 加密层可直接测试的独立函数：encrypt-pack → unpack → decrypt 需还原原文。
#[cfg_attr(not(test), allow(dead_code))]
fn roundtrip_encrypt_decrypt(key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut rng = rand::rngs::OsRng;
    let packed = gcm_encrypt_pack(key, plain, &mut rng)?;
    let (nonce, body) = unpack_v10(&packed).ok_or("拆包失败")?;
    gcm_decrypt(key, nonce, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_key() -> [u8; 32] {
        *b"0123456789abcdef0123456789abcdef"
    }

    #[test]
    fn unpack_rejects_bad_magic_and_short_data() {
        assert!(unpack_v10(b"nope").is_none());
        // 合法的 v10 + 12B nonce 但无密文(body 为空) → 拒绝
        assert!(unpack_v10(b"v10------------").is_none());
        // 合法 v10 但长度不足 15B
        assert!(unpack_v10(b"v10").is_none());
    }

    #[test]
    fn roundtrip_preserves_plaintext() {
        let plain = br#"{"token":"t","user":{"id":"u","name":"n"}}"#;
        let out = roundtrip_encrypt_decrypt(&dummy_key(), plain).unwrap();
        assert_eq!(out.as_slice(), plain);
    }

    #[test]
    fn tamper_detected_by_gcm_tag() {
        let plain = b"hello".to_vec();
        let key = dummy_key();
        let mut rng = rand::rngs::OsRng;
        let packed = gcm_encrypt_pack(&key, &plain, &mut rng).unwrap();
        let (nonce, body) = unpack_v10(&packed).unwrap();
        // 篡改密文 1 字节 → tag 校验失败
        let mut broken = body.to_vec();
        if let Some(b) = broken.first_mut() {
            *b ^= 0xFF;
        }
        assert!(gcm_decrypt(&key, nonce, &broken).is_err());
    }

    #[test]
    fn parse_requires_token_of_plausible_length() {
        let json = serde_json::json!({ "token": "x".repeat(32) });
        let acc = parse_credentials(Path::new("a"), &json).unwrap();
        assert_eq!(acc.uid, None);
        assert!(acc.is_current);
        // 过短占位不被当作凭证
        let short = serde_json::json!({ "token": "short" });
        assert!(parse_credentials(Path::new("a"), &short).is_none());
        // 非对象
        assert!(parse_credentials(Path::new("a"), &serde_json::json!([1, 2])).is_none());
    }

    #[test]
    fn parse_nested_user_fields() {
        let json = serde_json::json!({
            "schemaVersion": 1,
            "token": "tk".repeat(40),
            "refreshToken": "rt",
            "expiresAt": 1800000000000i64,
            "refreshTokenExpiresAt": 1700000000000i64,
            "user": { "id": "u-1", "name": "waxilo", "email": "a@b.c", "phone": "13800000000" }
        });
        let acc = parse_credentials(Path::new("auth.v1.dat"), &json).unwrap();
        assert_eq!(acc.uid.as_deref(), Some("u-1"));
        assert_eq!(acc.nickname.as_deref(), Some("waxilo"));
        assert_eq!(acc.phone.as_deref(), Some("13800000000"));
        assert_eq!(acc.expires_at, Some(1800000000000));
        assert_eq!(acc.rt_expires_at, Some(1700000000000));
        assert_eq!(acc.refresh_token.as_deref(), Some("rt"));
    }

    /// 实测形态（2026-09-18 解密本机 `auth.v1.dat` 得到，这里只替换掉身份类字段）：
    /// 有效期是 **ISO-8601 字符串**。这条测试就是为它而写 —— 用数字解析会静默得到 `None`。
    #[test]
    fn parses_iso_string_expiry_like_the_real_file() {
        let json = serde_json::json!({
            "schemaVersion": 1,
            "token": "Tk7fQ2mZ8xLpVr4sYbN1dWc6Aq",     // 实测长度 27
            "refreshToken": "rt-something-long-enough",
            "expiresAt": "2026-10-18T07:51:53Z",
            "refreshTokenExpiresAt": "2027-09-13T07:51:53Z",
            "user": { "id": "u-1", "name": "someone", "email": "a@b.c", "phone": "13800000000" },
            "firstLoginOnboardingSeen": true
        });
        let acc = parse_credentials(Path::new("auth.v1.dat"), &json).unwrap();
        assert_eq!(acc.expires_at, Some(1_792_309_913_000));
        assert_eq!(acc.rt_expires_at, Some(1_820_821_913_000));
        // refresh 比 access 长得多（实测一年 vs 一个月），这是「续签链还能活多久」的依据
        assert!(acc.rt_expires_at.unwrap() > acc.expires_at.unwrap());
    }

    /// 本机冒烟：`cargo test -- --ignored --nocapture`。
    /// 仅打印非敏感字段（token 只打长度），确认真实环境能 OSCrypt 解密并解析。
    #[test]
    #[ignore]
    fn smoke_reads_real_qoder_credentials() {
        let list = discover_local_accounts();
        for a in &list {
            println!(
                "uid={:?} nickname={:?} phone={:?} current={} token_len={} at_exp={:?} rt_exp={:?}",
                a.uid,
                a.nickname,
                a.phone,
                a.is_current,
                a.token.len(),
                a.expires_at,
                a.rt_expires_at,
            );
        }
        println!("total={}", list.len());
    }
}