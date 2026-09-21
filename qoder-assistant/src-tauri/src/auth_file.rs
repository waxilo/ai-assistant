//! 直接读取 Qoder 桌面端写给本机的凭据文件 `auth.v1.dat`。
//!
//! # 它是什么
//!
//! 它不是明文 JSON，而是 Chromium `safeStorage`（**OSCrypt**）的产物：
//! `v10` 版本前缀 + 一段对称加密的密文，密钥由**操作系统**保管。
//! 所以端外读取永远是两步 —— 先向系统要钥匙，再解数据，两步都不能省。
//!
//! # 两个平台的「钥匙」与「封装」都不一样（2026-09-19 实测）
//!
//! | | Windows | macOS |
//! |---|---|---|
//! | 钥匙放在哪 | `<profile>/Local State` 的 `os_crypt.encrypted_key` | 钥匙串 `Qoder[ CN] App Safe Storage` / `Qoder[ CN] App Key` |
//! | 钥匙怎么来 | `CryptUnprotectData` → **32 字节原始密钥** | 钥匙串密码（base64 文本）→ PBKDF2-HMAC-SHA1(`saltysalt`, 1003 轮) → **16 字节** |
//! | 数据怎么封 | `v10` + nonce(12) + **AES-256-GCM** 密文 + tag(16) | `v10` + **AES-128-CBC**(IV = 16 个空格, PKCS7) |
//!
//! 两条通道各自都是 Chromium 的既有实现，不是我们发明的格式；也正因为如此**不能混用**：
//! 拿 macOS 的 16 字节密钥去走 GCM 分支，结果只会是「读不出来」，而它与「没登录」长得一模一样。
//! [`Key`] 把「钥匙」与「封装」绑在同一个类型里，`decrypt_blob` / `encrypt_blob` 就不可能配错对。
//!
//! 实测证据：国内版 `~/Library/Application Support/com.qodercn.app.stable/auth.v1.dat`
//! 403 字节 = `v10`(3) + 400 字节密文；解出的明文是 389 字节 JSON，补 11 字节 PKCS7 正好 400 ——
//! 与 CBC 分支的封锁严丝合缝。钥匙串那条：
//! `security find-generic-password -s "Qoder CN App Safe Storage" -a "Qoder CN App Key" -w`
//! 在**没有任何授权弹窗**的情况下直接返回了密码。
//!
//! # 一条被纠正的错误结论
//!
//! 这里曾写着「macOS 的 `safeStorage` 走 Keychain，端外解不出 → 只好走 OAuth」，于是 macOS 上
//! 「导入本机账号」**恒返回空**，界面还会补一句「请先在 Qoder 桌面端登录一次」——
//! 而用户明明已经登录了。解不出的是「不解钥匙串」，不是「解不了」。
//!
//! # 每套部署各一个账号
//!
//! `auth.v1.dat` 只保存**当前登录的那一个账号**，切号/重登都会覆盖它 —— 因此没有 workbuddy
//! 那种「多账号列表」，Qoder 是单账号跟随模型。
//!
//! 但**两套部署各有自己的这一份**：国际版在 `com.qoder.app.stable`、国内版在
//! `com.qodercn.app.stable`（同机可以各登录一个）。所以本模块是「按区域各取一条」，
//! 扫描一遍最多回 2 条，每条都带上它所属的 [`Region`]：登录文件本身不写区域，
//! 区域只能由**它来自哪个目录**决定。
//!
//! # 写回策略
//!
//! 续签后允许**原子写回**回 `auth.v1.dat`（用户选择"允许原子写回"）。钥匙不变，
//! 所以写回只需按**同一个平台分支**重新封一次：GCM 换一个随机 nonce，CBC 沿用固定 IV。
//! 写回走"临时文件 + rename"保证原子性，避免与 Qoder 主进程读到半个文件。

use crate::region::Region;
use crate::timeutil::token_expiry;
use aead::{Aead, KeyInit};
use aes::Aes128;
use aes_gcm::Aes256Gcm;
#[cfg(target_os = "windows")]
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Serialize;
use serde_json::Value;
use sha1::Sha1;
use std::path::{Path, PathBuf};

// 这里曾有 `pub const OPENAPI_HOST`（= 国际版的 openapi 域），给 `refresh.rs` 续签用。
// 域现在由 `region::Region::openapi_base` 按**账号自己的区域**给出 ——
// 两套部署的账号必须各打各的域，否则续签拿到的 token 在对方网关上无效。

/// auth.v1.dat 的名字（固定）
const AUTH_FILE: &str = "auth.v1.dat";

/// OSCrypt 的版本前缀（两个平台都是它）
const V10: &[u8] = b"v10";

/// 记录 AES 密钥的 Chromium 状态文件名（带空格，勿改）。仅 Windows 通道读它。
#[cfg(target_os = "windows")]
const LOCAL_STATE_FILE: &str = "Local State";

/// DPAPI blob 前缀（encrypted_key base64 解码后前 5 字节）。仅 Windows 通道用。
#[cfg(target_os = "windows")]
const DPAPI_PREFIX: &[u8] = b"DPAPI";

/// macOS 钥匙串的读取工具（Apple 签名、路径固定 —— 见 [`keychain_password`]）
#[cfg(target_os = "macos")]
const KEYCHAIN_TOOL: &str = "/usr/bin/security";

/// macOS 上派生密钥用的 salt（Chromium 的固定值，勿改）
const MAC_SALT: &[u8] = b"saltysalt";

/// macOS 上的 PBKDF2 迭代轮数（Chromium 的固定值，勿改）
const MAC_ROUNDS: u32 = 1003;

/// CBC 段的 IV：**16 个空格**。
///
/// 这是 Chromium 的固定值，不是随机 nonce —— 所以 CBC 这段没有完整性保护
/// （改一个字节不会立刻被检出，可能解出乱码或填充错误）。与 Chromium 自身行为一致，
/// 不要"改进"成随机 IV：那样官方客户端就读不回来了。
const CBC_IV: [u8; 16] = *b"                ";

/// 判定 token 下限长度，避免把占位串当作凭证
const MIN_TOKEN_LEN: usize = 20;

/// 直读本机 Qoder 凭据得到的「当前账号」（Qoder 单账号模型，始终 0 或 1 条）。
/// 一次就能拿到 token + refresh_token + 昵称 + 手机号，是首选通道。
#[derive(Serialize, Clone, Debug)]
pub struct LocalAccount {
    pub token: String,
    /// 续签用的 refresh token
    pub refresh_token: Option<String>,
    /// **这条凭据属于哪套部署** —— 由它来自哪个 profile 目录决定，不是猜出来的。
    ///
    /// 两套部署的登录文件分别在 `com.qoder.app.stable` 与 `com.qodercn.app.stable` 下，
    /// 同一台机器上可以各有一个「当前账号」。导入时必须把它一起带上：
    /// 登录文件本身不写区域，而下游每一个请求都要靠它选域。
    pub region: Region,
    pub source: String,
    // 这里曾经有 `pub host: Option<String>`，恒等于 `OPENAPI_HOST`。一个永远取同一个值的
    // 字段不携带任何信息，只会让「这里可以选域」的错觉活下去，2026-09-18 删除。
    pub uid: Option<String>,
    pub nickname: Option<String>,
    pub phone: Option<String>,
    /// 邮箱（展示用标识）。**国际版账号认的就是它** —— 那套部署按邮箱登录，
    /// 登录文件里的 `user.phone` 常为空，没了邮箱就只剩昵称可认。
    pub email: Option<String>,
    /// access token 过期时间（毫秒时间戳）
    pub expires_at: Option<i64>,
    /// refresh token 过期时间（毫秒时间戳）
    pub rt_expires_at: Option<i64>,
    /// auth.v1.dat 只有当前账号，恒为 true
    pub is_current: bool,
    pub file: String,
}

/// 单个区域的读取结果。
///
/// 存在的理由：[`LocalScan::accounts`] 只能表达「有没有」，而「没有」至少有三种**互不相同**
/// 的原因 —— 那个版本从未登录过 / 钥匙串里没条目（或用户拒绝授权）/ 解密失败。
/// 上一版把三种都渲染成「请先在 Qoder 桌面端登录一次」，于是已经登录的用户被告知去登录。
#[derive(Serialize, Clone, Debug)]
pub struct LocalProbe {
    pub region: Region,
    /// 该区域是否读到了账号
    pub found: bool,
    /// 一句话说明：读到了什么，或者**为什么没读到**
    pub detail: String,
}

/// 一次「导入本机账号」扫描的完整结果：凭据 + 每个区域各自的读取情况。
#[derive(Serialize, Clone, Debug)]
pub struct LocalScan {
    pub accounts: Vec<LocalAccount>,
    /// 每个区域一条，顺序同 [`Region::ALL`]（界面直接按它渲染，不要再自己按区域分组）
    pub probes: Vec<LocalProbe>,
}

// ---------------------------------------------------------------------------
// 钥匙：把「钥匙来源」与「数据封装」绑在一起
// ---------------------------------------------------------------------------

/// 打开 `auth.v1.dat` 的那把钥匙 —— 它**同时**决定了数据怎么封。
///
/// 两个变体不是"两种实现"，而是 Chromium OSCrypt 在 Windows / macOS 上的两个平台分支。
/// 做成枚举而不是两个自由函数，是为了让「拿 32 字节 GCM 密钥去走 CBC」在类型上就不成立。
#[derive(Clone)]
pub(crate) enum Key {
    /// Windows：DPAPI 解出的 32 字节原始密钥，数据是 AES-256-GCM。
    ///
    /// 非 Windows 构建里不会有实例（构造它的 `open_key` 在那个平台不存在），
    /// 但 `decrypt_blob` / `encrypt_blob` 的**两个 match 分支都必须是它** ——
    /// 这不是冗余，而是「两个平台的封锁协议都写在一个地方」的代价。
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Gcm([u8; 32]),
    /// macOS：钥匙串密码派生的 16 字节密钥，数据是 AES-128-CBC（IV 固定 16 个空格）。
    Cbc([u8; 16]),
}

/// 一次「读到 + 解开」的完整结果。
///
/// 把**钥匙**一起带回来是刻意的：写回必须沿用同一把钥匙（GCM 换 nonce、CBC 用固定 IV），
/// 若写回时再去钥匙串 / DPAPI 取一次，不仅多一次系统调用，
/// 还多出一个「读的时候是一把、写的时候换成了另一把」的窗口。
///
/// 这里**不**存 `region` / `dir`：从 `read_credentials` 的那一端开始它们就不再被用到
/// （钥匙已经拿在手上），留着只会让人误以为写回还要靠它们选域或找文件。
pub(crate) struct Credentials {
    pub file: PathBuf,
    pub key: Key,
    pub json: Value,
}

// ---------------------------------------------------------------------------
// Windows：DPAPI 解出 Chromium OSCrypt 的 32B AES 数据密钥
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

/// 从 profile 目录的 `Local State` 解出 32B AES-256 数据密钥（Windows 通道）。
#[cfg(target_os = "windows")]
fn derive_gcm_key(profile_dir: &Path) -> Result<[u8; 32], String> {
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
        return Err("encrypted_key 缺少 DPAPI 前缀".to_string());
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
// macOS：钥匙串取密码 → PBKDF2-HMAC-SHA1 派生 16B 密钥
// ---------------------------------------------------------------------------

/// 从 macOS 钥匙串取 `safeStorage` 的主密码。
///
/// 走 `/usr/bin/security` 而不是 `security-framework` crate，理由是**谁在请求**这件事：
/// 钥匙串条目的 ACL 绑定「请求者」，而 `security` 是 Apple 签名、路径固定的可执行文件 ——
/// 用户点一次「始终允许」就永久生效。若由本应用直接调 `SecKeychainFindGenericPassword`，
/// 请求者变成我们自己的二进制，而调试构建是 ad-hoc 签名（cdhash 每次重建都会变），
/// 于是**每次重新构建都要重新点一次授权**。实测本机的条目目前连弹窗都不需要。
///
/// 不走 shell（`Command` 直接传 argv），密码只留在内存里；不进日志、不落盘。
#[cfg(target_os = "macos")]
fn keychain_password(region: Region) -> Result<String, String> {
    let service = region.keychain_service();
    let out = std::process::Command::new(KEYCHAIN_TOOL)
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            region.keychain_account(),
            "-w",
        ])
        .output()
        .map_err(|e| format!("调用 {KEYCHAIN_TOOL} 失败：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        let why = if err.is_empty() {
            "条目不存在，或被钥匙串拒绝了授权"
        } else {
            err
        };
        return Err(format!("{service}：{why}"));
    }
    let password = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    if password.is_empty() {
        return Err(format!("{service}：钥匙串里的密码为空"));
    }
    Ok(password)
}

/// PBKDF2-HMAC-SHA1（RFC 2898 / RFC 6070）。
///
/// Chromium 在 macOS 上派生的就是它：`salt = "saltysalt"`、1003 轮、输出 16 字节。
/// 手写而不再引一个 crate：整个算法就是「每块一次 HMAC + 若干次迭代异或」，
/// 而 `hmac` / `sha1` 已经在依赖里；多引一个包的代价是与上游轮数悄悄分叉的风险。
/// 正确性由 RFC 6070 的官方测试向量钉住（见模块内测试）。
///
/// 实现本身刻意**不做平台门控**：它只在 macOS 的生产路径上被调用，但测试要在任何平台
/// 都能跑（否则"改了轮数/盐"这件事只有 macOS 上才被发现）。非 macOS 构建里因此标成
/// 允许未使用，而不是让它消失。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pbkdf2_hmac_sha1(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    /// SHA-1 的输出长度
    const HLEN: usize = 20;
    // rounds 为 0 时下面 `1..rounds` 会一轮都不跑，T = U1 —— 与 RFC 的约定一致
    let blocks = out.len().div_ceil(HLEN);
    for index in 1..=blocks {
        let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(password)
            .expect("HMAC 接受任意长度的密钥");
        mac.update(salt);
        mac.update(&(index as u32).to_be_bytes());
        let mut u = mac.finalize().into_bytes();
        let mut t = u;
        for _ in 1..rounds {
            let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(password)
                .expect("HMAC 接受任意长度的密钥");
            mac.update(&u);
            u = mac.finalize().into_bytes();
            for (acc, byte) in t.iter_mut().zip(u.iter()) {
                *acc ^= *byte;
            }
        }
        let start = (index - 1) * HLEN;
        let take = (out.len() - start).min(HLEN);
        out[start..start + take].copy_from_slice(&t[..take]);
    }
}

// ---------------------------------------------------------------------------
// 按平台取钥匙
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn open_key(_region: Region, profile_dir: &Path) -> Result<Key, String> {
    derive_gcm_key(profile_dir).map(Key::Gcm)
}

#[cfg(target_os = "macos")]
fn open_key(region: Region, _profile_dir: &Path) -> Result<Key, String> {
    let password = keychain_password(region)?;
    let mut key = [0u8; 16];
    pbkdf2_hmac_sha1(password.as_bytes(), MAC_SALT, MAC_ROUNDS, &mut key);
    Ok(Key::Cbc(key))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_key(_region: Region, _profile_dir: &Path) -> Result<Key, String> {
    Err("当前平台没有 OSCrypt 钥匙通道（Windows 走 DPAPI、macOS 走钥匙串），请改用「登录新账号」"
        .to_string())
}

// ---------------------------------------------------------------------------
// 拆包 / 封包
// ---------------------------------------------------------------------------

/// 解 `v10` 封装的密文。钥匙来自哪个平台，就该走哪个分支（由 [`Key`] 保证）。
fn decrypt_blob(key: &Key, data: &[u8]) -> Result<Vec<u8>, String> {
    if data.get(..V10.len()) != Some(V10) {
        return Err("凭据文件缺少 v10 魔数（不是 Chromium safeStorage 的产物？）".to_string());
    }
    let body = &data[V10.len()..];
    match key {
        // Windows：nonce(12) + 密文 + tag(16)；tag 在密文尾部，交给 AEAD 一起校验
        Key::Gcm(k) => {
            if body.len() <= 12 {
                return Err(format!("GCM 段长度异常：{}（应 > 12，含 nonce）", body.len()));
            }
            let (nonce, sealed) = body.split_at(12);
            let cipher = Aes256Gcm::new_from_slice(k).map_err(|e| format!("AES 密钥初始化失败：{e}"))?;
            cipher
                .decrypt(aes_gcm::Nonce::from_slice(nonce), sealed)
                .map_err(|_| "AES-GCM 校验失败（AuthTag 不匹配：密钥不对或密文被改动）".to_string())
        }
        // macOS：整段就是 CBC 密文，IV 固定 16 个空格，填充是 PKCS7
        Key::Cbc(k) => {
            if body.is_empty() || body.len() % 16 != 0 {
                return Err(format!("CBC 段长度异常：{}（应为 16 的整数倍）", body.len()));
            }
            cbc::Decryptor::<Aes128>::new_from_slices(k, &CBC_IV)
                .map_err(|e| format!("AES-CBC 密钥/IV 长度不对：{e}"))?
                .decrypt_padded_vec_mut::<Pkcs7>(body)
                .map_err(|_| "AES-CBC 解填充失败（密钥不对或密文被改动）".to_string())
        }
    }
}

/// 按钥匙所属的平台重新封一次。写回时用（本地凭据"允许原子写回"）。
fn encrypt_blob(key: &Key, plain: &[u8], rng: &mut impl RngCore) -> Result<Vec<u8>, String> {
    let mut packed = Vec::with_capacity(V10.len() + plain.len() + 32);
    packed.extend_from_slice(V10);
    match key {
        Key::Gcm(k) => {
            let cipher = Aes256Gcm::new_from_slice(k).map_err(|e| format!("AES 密钥初始化失败：{e}"))?;
            let mut nonce = [0u8; 12];
            rng.fill_bytes(&mut nonce);
            let sealed = cipher
                .encrypt(aes_gcm::Nonce::from_slice(&nonce), plain)
                .map_err(|_| "AES-GCM 加密失败".to_string())?;
            packed.extend_from_slice(&nonce);
            packed.extend_from_slice(&sealed);
        }
        Key::Cbc(k) => {
            let sealed = cbc::Encryptor::<Aes128>::new_from_slices(k, &CBC_IV)
                .map_err(|e| format!("AES-CBC 密钥/IV 长度不对：{e}"))?
                .encrypt_padded_vec_mut::<Pkcs7>(plain);
            packed.extend_from_slice(&sealed);
        }
    }
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
fn parse_credentials(region: Region, path: &Path, json: &Value) -> Option<LocalAccount> {
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
        region,
        source,
        uid: str_at(json, &["user", "id"]),
        nickname: str_at(json, &["user", "name"]).or_else(|| str_at(json, &["user", "username"])),
        phone: str_at(json, &["user", "phone"]).or_else(|| str_at(json, &["phone"])),
        email: str_at(json, &["user", "email"]).or_else(|| str_at(json, &["email"])),
        // 有效期是 **ISO-8601 字符串**（实测 `"2026-10-18T07:51:53Z"`），不是毫秒数 ——
        // 这里必须走统一的时间戳归一化，否则 `parse::<i64>()` 静默失败、有效期变成 None。
        // `now_ms` 传 0：这两个字段是绝对时间，不涉及相对秒数换算。
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

/// 扫描本机凭据：**两套部署各扫一遍**，每套最多回 1 条（Qoder 是单账号跟随模型）。
///
/// 顺序就是 [`Region::ALL`] 的顺序（国际版在前），界面与冒烟测试都按这个顺序取第一条。
///
/// 与「返回 `Vec<LocalAccount>`」的上一版不同，这里把**每个区域为什么没有**也一起带回来：
/// 「没读到」至少有三种互不相同的原因，界面必须能区分（见 [`LocalProbe`]）。
pub fn discover_local_accounts() -> LocalScan {
    let mut accounts = Vec::new();
    let mut probes = Vec::new();
    for region in Region::ALL {
        match read_credentials(region) {
            Ok(creds) => match parse_credentials(region, &creds.file, &creds.json) {
                Some(account) => {
                    probes.push(LocalProbe {
                        region,
                        found: true,
                        detail: format!("已读到本机登录文件（{}）", creds.file.display()),
                    });
                    accounts.push(account);
                }
                None => probes.push(LocalProbe {
                    region,
                    found: false,
                    detail: "登录文件里没有可用的 token（内容是占位串或已登出）".to_string(),
                }),
            },
            Err(detail) => probes.push(LocalProbe {
                region,
                found: false,
                detail,
            }),
        }
    }
    LocalScan { accounts, probes }
}

/// 读 + 解密出「凭据 + 钥匙 + 文件路径」；任何一环失败都给**人话**原因（不 panic）。
///
/// `region` 必填：两套部署的凭据文件在**不同目录**、用**各自的钥匙**加密，
/// 「扫一遍全机」这种模糊语义在这里是有害的 —— 拿 A 区域的钥匙去解 B 区域的密文
/// 只会得到「读不出来」，与「没装」长得一模一样。
///
/// 多候选目录时才用 `continue`：第一个目录里没有文件**不代表**别的候选也没有
/// （上一版在这里写的是 `?`，等于把「多路径」变成了摆设）。
pub(crate) fn read_credentials(region: Region) -> Result<Credentials, String> {
    let dirs = region.profile_dirs();
    if dirs.is_empty() {
        return Err(format!("{}：当前平台没有已知的数据目录", region.label()));
    }
    let mut miss = String::new();
    for dir in dirs {
        let file = dir.join(AUTH_FILE);
        if !file.exists() {
            // 措辞**不带区域名**：界面那一条的左端已经有区域徽章了，
            // 再来一句「国际版…」会读成「国际版 国际版还没有登录过」。
            miss = format!("还没有登录过（{} 不存在）", file.display());
            continue;
        }
        let data = std::fs::read(&file).map_err(|e| format!("读取 {} 失败：{e}", file.display()))?;
        let key = open_key(region, &dir)?;
        let plain =
            decrypt_blob(&key, &data).map_err(|e| format!("{} 解密失败：{e}", file.display()))?;
        let json: Value = serde_json::from_slice(&plain)
            .map_err(|e| format!("{} 解密后不是合法 JSON：{e}", file.display()))?;
        if !json.is_object() {
            return Err(format!("{} 解密后不是 JSON 对象", file.display()));
        }
        return Ok(Credentials { file, key, json });
    }
    Err(miss)
}

/// 用**同一把钥匙**重新加密并**原子写回** `auth.v1.dat`（续签后用）。
///
/// 写盘走「临时文件 + rename」，保证 Qoder 主进程不会读到半个文件。
///
/// 落盘前先**自检**：把刚封好的包再解一次，确认解出来正是准备写入的那份内容。
/// 这几微秒买到的是「写回逻辑哪天写错格式时，坏的是临时文件，而不是用户唯一一份登录凭据」——
/// 特别是 macOS 那条 CBC 通道**没有完整性保护**（IV 固定、无 MAC），
/// 它是唯一能在"写进去就再也读不回来"之前拦住我们的东西。
pub(crate) fn write_back(creds: &Credentials, json: &Value) -> Result<(), String> {
    let plain = serde_json::to_vec(json).map_err(|e| format!("序列化凭据失败：{e}"))?;
    let packed = encrypt_blob(&creds.key, &plain, &mut rand::rngs::OsRng)?;
    let reopened =
        decrypt_blob(&creds.key, &packed).map_err(|e| format!("写回自检失败（原文件未改动）：{e}"))?;
    if reopened != plain {
        return Err("写回自检失败：重解出的内容与待写内容不一致（原文件未改动）".to_string());
    }
    let tmp = creds.file.with_extension("dat.tmp");
    std::fs::write(&tmp, &packed).map_err(|e| format!("写临时凭据文件失败：{e}"))?;
    std::fs::rename(&tmp, &creds.file).map_err(|e| format!("原子写回凭据失败：{e}"))?;
    Ok(())
}

/// 续签成功后，把新 token 对与有效期**原子写回** `auth.v1.dat`。
///
/// 只改 token / refreshToken 及两者有效期四项，其余字段（`user` 等）原样保留。
/// 读不到凭据（那个版本没登录）时静默返回 Ok —— 写回是尽力而为，失败不该影响续签本身。
pub(crate) fn apply_refresh(
    region: Region,
    token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<i64>,
    rt_expires_at: Option<i64>,
) -> Result<(), String> {
    let Ok(creds) = read_credentials(region) else {
        return Ok(());
    };
    let mut json = creds.json.clone();
    json["token"] = Value::String(token.to_string());
    match refresh_token {
        Some(rt) => json["refreshToken"] = Value::String(rt.to_string()),
        None => _ = json["refreshToken"].take(),
    }
    set_expiry(&mut json, "expiresAt", expires_at);
    set_expiry(&mut json, "refreshTokenExpiresAt", rt_expires_at);
    write_back(&creds, &json)
}

/// 回写一个有效期字段，**保持原文的写法**。
///
/// 官方文件里 `expiresAt` / `refreshTokenExpiresAt` 是 ISO-8601 **字符串**
/// （实测 `"2026-10-19T03:38:47Z"`）。上一版这里直接写毫秒数字：一次"成功"的续签之后，
/// 官方客户端读到的这两个字段就从字符串变成了数字 —— 那是把**别人的登录文件**改坏，
/// 而且发生在一条我们自己认为成功的路径上，最难发现。macOS 原先根本写不回（读都读不出），
/// 所以这个坑此前只在 Windows 上可达。
///
/// 两条规则都不是随手定的：
///
/// 1. **写法跟着原文**：原文是数字（老版本若有）就写数字，否则一律写 ISO 字符串。
/// 2. **拿不到新值就什么都不改**：与 `refresh_account_in_place` 里
///    `r.expires_at.or(account.expires_at)` 是同一条约定 —— 内存里的账号和磁盘上的文件
///    若各按各的规则处理，就会出现「界面显示没过期、文件里已经过期」这种自相矛盾的状态。
///    （也**不**写 `null` / 删字段：`json[field].take()` 实际只会留下一个 `null`，
///    字段还在，客户端可能因此把它当成"显式的空有效期"。）
///
/// 越界时间戳（chrono 表示不出）同样走"什么都不改"：绝不写一个类型不对的值进去。
fn set_expiry(json: &mut Value, field: &str, ms: Option<i64>) {
    let Some(ms) = ms else {
        return;
    };
    if matches!(json.get(field), Some(Value::Number(_))) {
        json[field] = Value::from(ms);
        return;
    }
    if let Some(text) = crate::timeutil::iso_utc(ms) {
        json[field] = Value::String(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_gcm_key() -> Key {
        Key::Gcm(*b"0123456789abcdef0123456789abcdef")
    }

    fn dummy_cbc_key() -> Key {
        Key::Cbc(*b"0123456789abcdef")
    }

    // ------------------------------------------------------------ 封锁（两种平台各一条）

    #[test]
    fn gcm_roundtrip_preserves_plaintext() {
        let plain = br#"{"token":"t","user":{"id":"u","name":"n"}}"#;
        let key = dummy_gcm_key();
        let mut rng = rand::rngs::OsRng;
        let packed = encrypt_blob(&key, plain, &mut rng).unwrap();
        assert_eq!(&packed[..3], V10);
        assert_eq!(decrypt_blob(&key, &packed).unwrap(), plain);
    }

    /// macOS 那条：`v10` + PKCS7 填充的 CBC。这条测试是「国内版 token 能不能读出来」的地基。
    #[test]
    fn cbc_roundtrip_preserves_plaintext() {
        let plain = br#"{"token":"t","user":{"id":"u","name":"n"}}"#;
        let key = dummy_cbc_key();
        let mut rng = rand::rngs::OsRng;
        let packed = encrypt_blob(&key, plain, &mut rng).unwrap();
        assert_eq!(&packed[..3], V10);
        // 与实测文件同一个形状：3 + 16 的整数倍
        assert_eq!((packed.len() - 3) % 16, 0);
        assert_eq!(decrypt_blob(&key, &packed).unwrap(), plain);
    }

    /// 实测形态复刻：明文 389 字节（本机国内版 `auth.v1.dat` 的真实长度）→ 400 字节密文。
    /// 这条断言证明我们的封锁与 Chromium 侧尺寸一致（PKCS7 补 11 字节）。
    #[test]
    fn cbc_padding_matches_the_real_file_shape() {
        let plain = vec![b'a'; 389];
        let mut rng = rand::rngs::OsRng;
        let packed = encrypt_blob(&dummy_cbc_key(), &plain, &mut rng).unwrap();
        assert_eq!(packed.len(), 3 + 400);
        assert_eq!(decrypt_blob(&dummy_cbc_key(), &packed).unwrap(), plain);
    }

    #[test]
    fn decrypt_rejects_bad_magic_and_truncated_body() {
        let key = dummy_cbc_key();
        assert!(decrypt_blob(&key, b"nope").unwrap_err().contains("v10"));
        assert!(decrypt_blob(&key, b"v10").unwrap_err().contains("长度"));
        // 长度不是 16 的整数倍
        assert!(decrypt_blob(&key, &[b"v10".to_vec(), vec![0u8; 17]].concat()).is_err());
    }

    /// 钥匙与封装必须成对：拿 CBC 的钥匙去解 GCM 的数据不能"碰巧成功"，
    /// 拿 GCM 的钥匙去解 CBC 的数据也一样（这正是上一版把 macOS 判成"解不出"的机制）。
    #[test]
    fn keys_do_not_cross_between_platforms() {
        let plain = b"hello world, this is a long enough payload";
        let mut rng = rand::rngs::OsRng;
        let gcm = encrypt_blob(&dummy_gcm_key(), plain, &mut rng).unwrap();
        let cbc = encrypt_blob(&dummy_cbc_key(), plain, &mut rng).unwrap();
        // GCM 密文丢进 CBC 分支：长度就不对，必须被拒
        assert!(decrypt_blob(&dummy_cbc_key(), &gcm).is_err());
        // CBC 密文丢进 GCM 分支：tag 校验必须失败
        assert!(decrypt_blob(&dummy_gcm_key(), &cbc).is_err());
    }

    #[test]
    fn tamper_detected_by_gcm_tag() {
        let key = dummy_gcm_key();
        let mut rng = rand::rngs::OsRng;
        let mut packed = encrypt_blob(&key, b"hello", &mut rng).unwrap();
        // 篡改密文最后 1 字节（落在 tag 里）→ 校验失败
        let last = packed.len() - 1;
        packed[last] ^= 0xFF;
        assert!(decrypt_blob(&key, &packed).is_err());
    }

    // ------------------------------------------------------------ 密钥派生

    /// RFC 6070 的官方向量 —— 手写 PBKDF2 的唯一安全保障。
    /// 错一个字节，macOS 上就永远解不出凭据，而表现只是「没登录」。
    #[test]
    fn pbkdf2_matches_rfc6070_vectors() {
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        let mut out = [0u8; 20];
        pbkdf2_hmac_sha1(b"password", b"salt", 1, &mut out);
        assert_eq!(hex(&out), "0c60c80f961f0e71f3a9b524af6012062fe037a6");
        pbkdf2_hmac_sha1(b"password", b"salt", 2, &mut out);
        assert_eq!(hex(&out), "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957");
        pbkdf2_hmac_sha1(b"password", b"salt", 4096, &mut out);
        assert_eq!(hex(&out), "4b007901b765489abead49d926f721d065a429c1");
        // 多块（dkLen > 20）时第 2 块也要正确
        let mut long = [0u8; 25];
        pbkdf2_hmac_sha1(b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, &mut long);
        assert_eq!(
            hex(&long),
            "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038"
        );
    }

    /// Chromium 在 macOS 上的派生结果（salt/轮数/长度都是它的固定值）。
    /// 这里钉住的是「常量没被改错」——比如把 1003 写成 1000 不会有任何编译期提示。
    #[test]
    fn mac_key_derivation_uses_chromiums_constants() {
        assert_eq!(MAC_SALT, b"saltysalt");
        assert_eq!(MAC_ROUNDS, 1003);
        assert_eq!(CBC_IV, [0x20u8; 16]);
        let mut key = [0u8; 16];
        pbkdf2_hmac_sha1(b"iFnknoEF3iL0uSUdhoSl+A==", MAC_SALT, MAC_ROUNDS, &mut key);
        // 只断言长度与"不是全零"：具体值属于本机钥匙串密码的派生结果，不进测试
        assert_eq!(key.len(), 16);
        assert_ne!(key, [0u8; 16]);
    }

    // ------------------------------------------------------------ 解析

    #[test]
    fn parse_requires_token_of_plausible_length() {
        let json = serde_json::json!({ "token": "x".repeat(32) });
        let acc = parse_credentials(Region::Global, Path::new("a"), &json).unwrap();
        assert_eq!(acc.uid, None);
        assert!(acc.is_current);
        // 过短占位不被当作凭证
        let short = serde_json::json!({ "token": "short" });
        assert!(parse_credentials(Region::Global, Path::new("a"), &short).is_none());
        // 非对象
        assert!(parse_credentials(Region::Global, Path::new("a"), &serde_json::json!([1, 2])).is_none());
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
        let acc = parse_credentials(Region::Global, Path::new("auth.v1.dat"), &json).unwrap();
        assert_eq!(acc.uid.as_deref(), Some("u-1"));
        assert_eq!(acc.nickname.as_deref(), Some("waxilo"));
        assert_eq!(acc.phone.as_deref(), Some("13800000000"));
        assert_eq!(acc.email.as_deref(), Some("a@b.c"));
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
        let acc = parse_credentials(Region::Global, Path::new("auth.v1.dat"), &json).unwrap();
        assert_eq!(acc.expires_at, Some(1_792_309_913_000));
        assert_eq!(acc.rt_expires_at, Some(1_820_821_913_000));
        // refresh 比 access 长得多（实测一年 vs 一个月），这是「续签链还能活多久」的依据
        assert!(acc.rt_expires_at.unwrap() > acc.expires_at.unwrap());
    }

    /// 写回要真的能落到磁盘，并且自检通过；临时文件不能留下。
    /// 两条密钥通道（Windows 的 GCM / macOS 的 CBC）都要走一遍 ——
    /// 这条测试是「写回把用户的登录文件写坏」的最后一道拦截。
    #[test]
    fn write_back_round_trips_through_a_real_file() {
        let dir = std::env::temp_dir().join(format!("qoder-writeback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(AUTH_FILE);
        let json = serde_json::json!({
            "token": "tk".repeat(20),
            "refreshToken": "rt-something-long-enough",
            "expiresAt": "2026-10-19T03:38:47Z",
            "user": { "id": "u-1", "name": "someone", "phone": "13800000000" }
        });
        for (label, key) in [("gcm", dummy_gcm_key()), ("cbc", dummy_cbc_key())] {
            let creds = Credentials {
                file: file.clone(),
                key,
                json: json.clone(),
            };
            write_back(&creds, &json).unwrap_or_else(|e| panic!("{label} 写回失败：{e}"));
            let raw = std::fs::read(&file).unwrap();
            assert_eq!(&raw[..3], V10, "{label}");
            let back: Value = serde_json::from_slice(&decrypt_blob(&creds.key, &raw).unwrap()).unwrap();
            assert_eq!(back, json, "{label}");
            // 临时文件必须已经 rename 掉，不能留在 Qoder 的数据目录里
            assert!(!file.with_extension("dat.tmp").exists(), "{label}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 写回必须**保持原文的写法**：ISO 字符串进 → ISO 字符串出。
    /// 这条测试要是失败，说明续签会把官方登录文件的时间戳类型改掉。
    #[test]
    fn write_back_keeps_the_iso_string_shape() {
        let mut json = serde_json::json!({
            "token": "old",
            "expiresAt": "2026-10-19T03:38:47Z",
            "refreshTokenExpiresAt": "2027-09-14T03:38:47Z"
        });
        set_expiry(&mut json, "expiresAt", Some(1_792_381_127_000));
        assert_eq!(json["expiresAt"], serde_json::json!("2026-10-19T03:38:47Z"));
        assert!(json["expiresAt"].is_string());
        // 数字写法保持数字（万一某个版本就是数字）
        let mut numbered = serde_json::json!({ "expiresAt": 1_700_000_000_000i64 });
        set_expiry(&mut numbered, "expiresAt", Some(1_800_000_000_000i64));
        assert_eq!(numbered["expiresAt"], serde_json::json!(1_800_000_000_000i64));
        // 字段本来不存在 → 也按官方写法补 ISO，而不是补一个数字
        let mut absent = serde_json::json!({ "token": "t" });
        set_expiry(&mut absent, "expiresAt", Some(1_792_381_127_000));
        assert_eq!(absent["expiresAt"], serde_json::json!("2026-10-19T03:38:47Z"));
        // 拿不到新值 → 原值不动（与内存账号的 `.or(旧值)` 同一约定），
        // 既不留 null、也不删字段
        let mut keep = serde_json::json!({ "expiresAt": "2026-10-19T03:38:47Z" });
        set_expiry(&mut keep, "expiresAt", None);
        assert_eq!(keep["expiresAt"], serde_json::json!("2026-10-19T03:38:47Z"));
        // 越界时间戳：同样原值不动，绝不写类型不对的值
        let mut bad = serde_json::json!({ "expiresAt": "2026-10-19T03:38:47Z" });
        set_expiry(&mut bad, "expiresAt", Some(i64::MAX));
        assert_eq!(bad["expiresAt"], serde_json::json!("2026-10-19T03:38:47Z"));
        assert!(bad["expiresAt"].is_string());
    }

    /// 扫描结果必须把「两个区域各一条 probe」带上，且顺序与 [`Region::ALL`] 一致 ——
    /// 界面直接按它渲染，少一条就会让「那个版本为什么没有」重新变成空白。
    #[test]
    fn a_scan_always_probes_every_region() {
        let scan = discover_local_accounts();
        assert_eq!(scan.probes.len(), Region::ALL.len());
        for (probe, region) in scan.probes.iter().zip(Region::ALL) {
            assert_eq!(probe.region, region);
            assert!(!probe.detail.trim().is_empty(), "必须给出人话原因");
        }
        // 读到的账号必须都在 `accounts` 里，且都带区域
        assert_eq!(scan.accounts.len(), scan.probes.iter().filter(|p| p.found).count());
        for account in &scan.accounts {
            assert!(Region::ALL.contains(&account.region));
        }
    }

    /// 本机冒烟：`cargo test -- --ignored --nocapture`。
    /// 仅打印非敏感字段（token 只打长度），确认真实环境能取到钥匙并解密。
    #[test]
    #[ignore]
    fn smoke_reads_real_qoder_credentials() {
        let scan = discover_local_accounts();
        for probe in &scan.probes {
            println!("probe region={} found={} detail={}", probe.region.key(), probe.found, probe.detail);
        }
        for a in &scan.accounts {
            println!(
                "region={} uid={:?} nickname={:?} phone={:?} current={} token_len={} at_exp={:?} rt_exp={:?}",
                a.region.key(),
                a.uid,
                a.nickname,
                a.phone,
                a.is_current,
                a.token.len(),
                a.expires_at,
                a.rt_expires_at,
            );
        }
        println!("total={}", scan.accounts.len());
    }
}
