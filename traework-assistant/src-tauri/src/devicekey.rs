//! 设备身份：EC P-256 密钥对 + 设备号，**持久化在应用数据目录**。
//!
//! ## 为什么必须持久化
//!
//! `ExchangeToken` 换来的 token 会**绑定签发时的设备身份**。续签（`RefreshToken` 授权）
//! 时服务端要求：
//!
//! 1. `DeviceInfo.DeviceID` 与签发时一致（否则 `20403 Token device not match`，
//!    连 `DeviceProof` 都不要求你提供）；
//! 2. `DeviceProof.Signature` 用**签发时那对密钥的私钥**签名 —— 换一把钥匙同样回 `20403`
//!    （2026-09-14 实测：拿着官方客户端自己落盘的私钥去续签它自己的 token 也是 20403）。
//!
//! 早期实现每次进程启动**现生成**一把公钥（`oauth.rs` 里那个 `OnceLock`），只用于换 token、
//! 用完即弃 —— 于是签发的 token **永远无法续签**。这里把密钥对和设备号落到磁盘，
//! 「登录一次，之后一直能续」。
//!
//! 签名格式对齐官方 `main.js` 的 `bTe()`：
//! `crypto.sign("sha256", Buffer.from(msg), privateKeyPEM)` → **DER 编码**的 ECDSA 签名，
//! 再 base64。RustCrypto 默认给固定 64 字节的 `r‖s`，所以这里自己转 DER。

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 设备身份文件（含私钥，权限同 `accounts.json`，都在应用数据目录内）
const IDENTITY_FILE: &str = "device.json";

/// 设备身份：密钥对 + 设备号。`device_id` / `machine_id` 就是 `DeviceInfo` 里那两个字段，
/// 也是账号记录里 `device_id` / `machine_id` 的来源。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DeviceIdentity {
    #[serde(default)]
    pub private_key_pem: String,
    #[serde(default)]
    pub public_key_pem: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub machine_id: String,
}

fn identity_path(dir: &Path) -> PathBuf {
    dir.join(IDENTITY_FILE)
}

/// 进程内缓存：`load_or_create` 会被登录、续签、签到等多处调用，不必每次都读盘。
fn cache() -> &'static Mutex<HashMap<PathBuf, DeviceIdentity>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, DeviceIdentity>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 读取设备身份；**首次调用时生成并落盘**（之后固定不变，token 才续得上）。
///
/// `prefer_device` / `prefer_machine`：若给了本机 TraeWork 的真实设备号（
/// [`crate::trae_auth::local_device_identity`]），首次生成时优先采用 —— 与官方
/// 「一台机器一个设备号」的语义一致；此后即使 TraeWork 的设备号变了也**不改**
/// （改了等于换设备，已签发的 token 会立刻失效）。
pub fn load_or_create(
    dir: &Path,
    prefer_device: Option<String>,
    prefer_machine: Option<String>,
) -> DeviceIdentity {
    let key = dir.to_path_buf();
    if let Some(hit) = cache()
        .lock()
        .ok()
        .and_then(|g| g.get(&key).cloned())
    {
        return hit;
    }

    let mut id = read_file(dir).unwrap_or_default();
    let mut dirty = false;
    if id.private_key_pem.trim().is_empty() || id.public_key_pem.trim().is_empty() {
        let (priv_pem, pub_pem) = gen_keypair();
        id.private_key_pem = priv_pem;
        id.public_key_pem = pub_pem;
        dirty = true;
    }
    if id.device_id.trim().is_empty() {
        id.device_id = prefer_device
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        dirty = true;
    }
    if id.machine_id.trim().is_empty() {
        id.machine_id = prefer_machine
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        dirty = true;
    }
    if dirty {
        if let Err(e) = write_file(dir, &id) {
            crate::logs::push("设备", false, format!("设备身份落盘失败：{e}"));
        }
    }
    if let Ok(mut g) = cache().lock() {
        g.insert(key, id.clone());
    }
    id
}

fn read_file(dir: &Path) -> Option<DeviceIdentity> {
    let text = std::fs::read_to_string(identity_path(dir)).ok()?;
    serde_json::from_str::<DeviceIdentity>(&text).ok()
}

fn write_file(dir: &Path, id: &DeviceIdentity) -> Result<(), String> {
    let json = serde_json::to_string_pretty(id).map_err(|e| e.to_string())?;
    std::fs::write(identity_path(dir), json).map_err(|e| e.to_string())
}

fn gen_keypair() -> (String, String) {
    let sk = SigningKey::random(&mut rand::rngs::OsRng);
    let private = sk
        .to_pkcs8_pem(LineEnding::LF)
        .map(|s| s.to_string())
        .unwrap_or_default();
    let public = sk
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .unwrap_or_default();
    (private, public)
}

/// ECDSA-SHA256 签名，**DER 编码后 base64**（与官方 `bTe()` 的 `crypto.sign` 同格式）。
pub fn sign_der_base64(private_key_pem: &str, message: &[u8]) -> Result<String, String> {
    let sk = SigningKey::from_pkcs8_pem(private_key_pem.trim())
        .map_err(|e| format!("设备私钥无法解析：{e}"))?;
    let sig: Signature = sk.sign(message);
    Ok(STANDARD.encode(der_encode(sig.to_bytes().as_slice())))
}

/// 把固定长度的 `r‖s` 编成 DER `SEQUENCE { INTEGER r, INTEGER s }`。
///
/// INTEGER 规则：去掉多余前导零；若首字节最高位为 1（会被当成负数）则补一个 `0x00`。
fn der_encode(raw: &[u8]) -> Vec<u8> {
    let half = raw.len() / 2;
    let mut body: Vec<u8> = Vec::with_capacity(raw.len() + 8);
    for part in [&raw[..half], &raw[half..]] {
        let mut p = part;
        while p.len() > 1 && p[0] == 0 {
            p = &p[1..];
        }
        let mut int = Vec::with_capacity(p.len() + 1);
        if p.first().map(|b| b & 0x80 != 0).unwrap_or(false) {
            int.push(0);
        }
        int.extend_from_slice(p);
        body.push(0x02);
        body.push(int.len() as u8);
        body.extend_from_slice(&int);
    }
    let mut out = Vec::with_capacity(body.len() + 3);
    out.push(0x30);
    // P-256 的签名长度固定落在短格式（< 128 字节），但按规则写全更稳。
    if body.len() < 128 {
        out.push(body.len() as u8);
    } else {
        out.push(0x81);
        out.push(body.len() as u8);
    }
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(identity_path(&dir));
        dir
    }

    #[test]
    fn persists_and_reuses_identity() {
        let dir = tmp("twa_devicekey_persist");
        let a = load_or_create(&dir, None, None);
        assert!(!a.private_key_pem.is_empty());
        assert!(a.public_key_pem.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(identity_path(&dir).exists(), "必须落盘，否则重启后无法续签");

        // 换一个「新进程」（清缓存）重读：必须是同一份，否则续签必失败
        cache().lock().unwrap().remove(&dir);
        let b = load_or_create(&dir, Some("ignored".into()), None);
        assert_eq!(a.private_key_pem, b.private_key_pem);
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.machine_id, b.machine_id);
    }

    #[test]
    fn prefers_real_device_id_on_first_creation() {
        let dir = tmp("twa_devicekey_prefer");
        let id = load_or_create(
            &dir,
            Some("384e4787-b85e-4f59-b23c-bc3a0f27cd2e".into()),
            Some("73d062c3fe30fb08".into()),
        );
        assert_eq!(id.device_id, "384e4787-b85e-4f59-b23c-bc3a0f27cd2e");
        assert_eq!(id.machine_id, "73d062c3fe30fb08");
        // 已有记录时不被覆盖：改设备号 = 换设备，已签发 token 会立刻失配
        cache().lock().unwrap().remove(&dir);
        let again = load_or_create(&dir, Some("other-device".into()), None);
        assert_eq!(again.device_id, "384e4787-b85e-4f59-b23c-bc3a0f27cd2e");
    }

    /// DER 结构：`SEQUENCE{INTEGER,INTEGER}`，整数取最小长度、正数按符号位补 `0x00`。
    #[test]
    fn der_encoding_shape() {
        let mut raw = vec![0x11u8; 64];
        // r：两个前导零必须去掉（32 → 30 字节）
        raw[0] = 0x00;
        raw[1] = 0x00;
        // s：首位 >= 0x80 会被当成负数，必须补一个 0x00（32 → 33 字节）
        raw[32] = 0x80;
        let der = der_encode(&raw);

        assert_eq!(der[0], 0x30, "SEQUENCE");
        assert_eq!(der[1] as usize, der.len() - 2, "长度字段 = 剩余字节数");
        assert_eq!(der[2], 0x02, "INTEGER r");
        assert_eq!(der[3], 30, "r：32 字节去掉 2 个前导零");
        assert_eq!(der[4], 0x11, "r 首字节应是被剥掉前导零后的 0x11");
        let s_at = 4 + 30;
        assert_eq!(der[s_at], 0x02, "INTEGER s");
        assert_eq!(der[s_at + 1], 33, "s：高位为 1 需补 0x00");
        assert_eq!(&der[s_at + 2..s_at + 4], &[0x00, 0x80]);
    }

    /// 真实签名的 DER 总长落在 P-256 的常见区间（70/71/72），不是固定长度。
    #[test]
    fn real_signature_der_length_is_typical() {
        let dir = tmp("twa_devicekey_derlen");
        let id = load_or_create(&dir, None, None);
        let der = STANDARD
            .decode(sign_der_base64(&id.private_key_pem, b"x").unwrap())
            .unwrap();
        assert_eq!(der[0], 0x30);
        assert!(
            (68..=72).contains(&der.len()),
            "P-256 DER 签名长度异常：{}",
            der.len()
        );
        assert_eq!(der[1] as usize, der.len() - 2);
    }

    /// 签名可被独立实现验证：自签的 DER 签名必须能被公钥验过。
    ///
    /// 顺带固定一个事实：RustCrypto 的 ECDSA 默认走 **RFC 6979 确定性随机数**，
    /// 同一消息 + 同一密钥签名结果**完全一致**（Node 的 `crypto.sign` 是随机 k，会不同）。
    /// 服务端只验签，不影响续签。
    #[test]
    fn signature_is_unique_and_verifiable() {
        use p256::ecdsa::signature::Verifier;
        use p256::ecdsa::VerifyingKey;
        use p256::pkcs8::DecodePublicKey;

        let dir = tmp("twa_devicekey_sign");
        let id = load_or_create(&dir, None, None);
        let msg = b"POST /trae/api/v3/oauth/ExchangeToken en1oxy7wnw8j9n rt 1 nonce";
        let sig1 = sign_der_base64(&id.private_key_pem, msg).unwrap();
        let sig2 = sign_der_base64(&id.private_key_pem, msg).unwrap();
        assert_eq!(sig1, sig2, "确定性签名：同消息同密钥结果一致");

        // 把 DER 解回 r‖s（自写解析，避免依赖 ecdsa 的 der feature）
        let der = STANDARD.decode(sig1).unwrap();
        assert_eq!(der[0], 0x30);
        let (r, s) = split_der_ints(&der);
        assert_eq!(r.len(), 32);
        assert_eq!(s.len(), 32);
        let mut raw = Vec::with_capacity(64);
        raw.extend_from_slice(&r);
        raw.extend_from_slice(&s);

        let vk = VerifyingKey::from_public_key_pem(&id.public_key_pem).unwrap();
        let sig = Signature::from_slice(&raw).unwrap();
        assert!(vk.verify(msg, &sig).is_ok(), "自签的 DER 签名必须能被公钥验过");
        assert!(
            vk.verify(b"another-message", &sig).is_err(),
            "换个消息必须验不过（确认验签真的在起作用）"
        );
    }

    /// 极简 DER 解析：取 SEQUENCE 里两个 INTEGER，去掉符号补位、左补零到 32 字节。
    fn split_der_ints(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut i = 2usize;
        let mut out: Vec<Vec<u8>> = Vec::new();
        while out.len() < 2 {
            assert_eq!(der[i], 0x02, "期望 INTEGER 标签");
            let len = der[i + 1] as usize;
            let mut v = der[i + 2..i + 2 + len].to_vec();
            while v.first() == Some(&0) {
                v.remove(0);
            }
            while v.len() < 32 {
                v.insert(0, 0);
            }
            out.push(v);
            i += 2 + len;
        }
        (out[0].clone(), out[1].clone())
    }

    /// 打一条固定消息的签名，供外部（openssl）独立验签：
    /// `cargo test --lib -- --ignored --nocapture print_signature_for_openssl`
    #[test]
    #[ignore]
    fn print_signature_for_openssl() {
        let dir = std::env::temp_dir().join("twa_devicekey_dump");
        let _ = std::fs::create_dir_all(&dir);
        let id = load_or_create(&dir, None, None);
        let msg = b"hello-traework";
        println!("PUB\n{}", id.public_key_pem);
        println!("MSG\n{}", String::from_utf8_lossy(msg));
        println!("SIG\n{}", sign_der_base64(&id.private_key_pem, msg).unwrap());
    }
}
