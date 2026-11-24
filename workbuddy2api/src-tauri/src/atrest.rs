//! WorkBuddy 桌面端（Electron 版，0.9.x 之后）**静态加密字段**的解密。
//!
//! # 为什么需要这个模块
//!
//! 官方客户端把 `auth/*.info` 里真正值钱的那几个字段（`accessToken`、
//! `refreshToken`、`nickname`、`phoneNumber`）换成了密文壳子：
//!
//! ```json
//! { "$wbEncrypted": 1,
//!   "envelope": "<base64 of {suite,keyId,nonce,authTag,ciphertext}>" }
//! ```
//!
//! 于是 [`crate::auth_file`] 原来那套「读 JSON、取字符串」直接失效 —— `as_str()`
//! 拿到的是 `None`，表现为「本机明明登录着，却一个账号都导不出来」。这里把壳子
//! 打开，让上层重新拿到明文。
//!
//! # 算法（逆向自客户端，`app-lifetime-job.js` 与 CLI 的 `codebuddy-lite-wb.mjs` 同源）
//!
//! - **套件**：`suite: 1` ⇒ AES-256-GCM（12 字节 nonce、16 字节 tag），
//!   且 ciphertext 与 tag **分开存放**（送进 AEAD 时要按 `ciphertext || authTag` 拼）。
//! - **AAD**：不是空值，而是一段带长度前缀的结构化字节（差一个字节都验不过）：
//!
//!   ```text
//!   "WB-AAD\0"                        // 7 字节魔数
//!   || 0x01                           // 版本
//!   || u32be(5) "WBEV1"               // framing 名（field）
//!   || u32be(6) "sym-v1"              // 方案名
//!   || u32be(1)                       // suite
//!   || u32be(16) "<keyId>"            // 16 个十六进制字符
//!   || 0x02                           // framing 码（file=1, field=2, record=3, stream=4）
//!   || 0x00                           // sequence = undefined
//!   || 0x00                           // final    = undefined
//!   ```
//!
//! - **密钥派生**（最容易看错的一步，照抄客户端写法）：
//!
//!   ```text
//!   key   = sha256(atRestSecretKey 的 UTF-8 字节)   // ← 对**这串 base64 文本**哈希
//!   keyId = sha256(key).hex().slice(0, 16)         // ← 再对密钥本身哈希取前缀
//!   ```
//!
//! # 密钥从哪来（本模块唯一的平台相关工作）
//!
//! 客户端把 `{"version":1,"atRestSecretKey":"…"}` 这份 capability JSON 交给定制
//! Electron 的 native 层（`process._linkedBinding("electron_browser_workbuddy_storage")`
//! 的 `loggerGet()`）保管，**磁盘和用户级凭据库里都没有它**：
//!
//! - 不在 `app.asar` / `codebuddy-lite-wb.mjs` 里（ASCII、UTF-16LE 都翻过）；
//! - 不在 `~/.workbuddy` 的任何 json/db 里；
//! - 不在 Windows 凭据管理器里（`CredEnumerateW` 全量枚举过）。
//!
//! 但它**跨版本稳定**（`~/.workbuddy/keyblob` 的 `protectorKeyId` 与 auth 文件里
//! envelope 的 `keyId` 始终同值，跨越了两次客户端升级），说明进程里一定有一份每次
//! 启动都能重新得到的副本。所以这里走**只读扫描客户端进程内存**：锚定
//! `"atRestSecretKey":"` 这个精确前缀，取紧随其后的 44 字节 base64，并且要求它算出的
//! `keyId` 与目标 envelope 声明的一致才算数 —— 宁可拿不到，也不拿一份可能错的。
//!
//! 拿到后用 DPAPI（`CryptProtectData`，当前用户作用域）缓存到本助手的数据目录，
//! 之后启动不再扫内存；缓存里是 DPAPI 密文，安全边界与「只有本用户能解」等价。
//!
//! # 边界
//!
//! - 只处理 `standard`（`sym-v1`）字段；`scheme: "asym-v1"` 的高级模式需要
//!   `keyblob` 里的 userKey + RSA-OAEP 包裹，当前部署的 policy 是 `fields`，走不到，
//!   遇到就返回 [`AtRestError::UnsupportedScheme`]，不猜也不刷日志。
//! - 非 Windows 没有内存扫描通道，[`key_for`] 返回 `None`；上层据此给出可操作提示，
//!   而不是把「解不开」当成「本机没有账号」。

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(target_os = "windows")]
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// 协议常量（与客户端逐字节对齐）
// ---------------------------------------------------------------------------

const AAD_MAGIC: &[u8] = b"WB-AAD\0";
/// 只支持 suite 1（客户端的支持集合当前也只有这一个元素）。
const SUITE: u32 = 1;
/// 字段级 framing 名（写进 AAD 的那一份）。
const FRAMING_NAME_FIELD: &str = "WBEV1";
/// 字段级 framing 码（写进 AAD 的那一份）。
const FRAMING_CODE_FIELD: u8 = 2;
const SCHEME: &str = "sym-v1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// 扫描内存时用的锚点：紧跟着就是 44 字节密钥。
const CAP_ANCHOR: &[u8] = b"\"atRestSecretKey\":\"";
/// DPAPI 缓存的落盘文件名（内容是 DPAPI 密文）。
const CACHE_FILE: &str = "atrest-key.bin";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtRestError {
    /// 不是加密壳子（普通值），调用方按原样处理即可。
    NotEncrypted,
    /// 壳子结构不合法（字段缺失 / 非规范 base64 / 长度不对 / suite 不认识）。
    Malformed(&'static str),
    /// 需要高级（asym-v1）通道，本模块不处理。
    UnsupportedScheme,
    /// AES-GCM 认证失败：密钥不对，或 AAD 与写入时不一致。
    AuthFailed,
    /// 拿不到 at-rest 密钥（客户端没运行 / 内存里没找到 / 非 Windows）。
    NoKey,
    /// 明文不是合法 UTF-8。
    NotUtf8,
}

// ---------------------------------------------------------------------------
// 密钥
// ---------------------------------------------------------------------------

/// 一把可直接用于 AES-256-GCM 的密钥，附带它自己声明的 keyId。
#[derive(Clone, PartialEq, Eq)]
pub struct Key {
    pub key_id: String,
    key: [u8; 32],
}

impl std::fmt::Debug for Key {
    /// 别把密钥本身打进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("key_id", &self.key_id).finish()
    }
}

/// `keyId` 的派生规则：对**密钥字节**做 sha256，取前 16 个十六进制字符。
pub fn derive_key_id(key: &[u8; 32]) -> String {
    let digest = Sha256::digest(key);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    hex.truncate(16);
    hex
}

/// 由 capability 里的 `atRestSecretKey`（base64 文本）算出真正的密钥。
///
/// 客户端写的是 `sha256(secretString, "utf8")` —— 对**字符串**哈希，不是对 base64
/// 解码后的 32 字节哈希。搞反的话 keyId 会完全不同，表现成「密钥明明对，却一直
/// 认证失败」。
pub fn normalize_secret(secret: &str) -> Option<Key> {
    let raw = secret.trim();
    if raw.is_empty() {
        return None;
    }
    let digest = Sha256::digest(raw.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    Some(Key {
        key_id: derive_key_id(&key),
        key,
    })
}

/// 本进程缓存目录（DPAPI 缓存文件放这里）。由 [`init`] 设置一次。
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();
/// 进程内已解析出的密钥，避免每次导入都扫一遍客户端内存。
static KEY: OnceLock<Key> = OnceLock::new();

/// 注册缓存目录；不调用也能工作，只是每次都去扫内存。
pub fn init(dir: impl Into<PathBuf>) {
    let _ = CACHE_DIR.set(dir.into());
}

/// 取一把满足 `want_key_id` 的密钥。
///
/// 顺序：进程内缓存 → DPAPI 缓存文件 → 扫描客户端进程内存。
/// 缓存里的 keyId 与目标不符会被丢掉重扫 —— 客户端升级可能换密钥，缓存不能当真理。
pub fn key_for(want_key_id: &str) -> Option<Key> {
    if let Some(k) = KEY.get() {
        if k.key_id == want_key_id {
            return Some(k.clone());
        }
    }
    if let Some(k) = load_cached_key(want_key_id) {
        return adopt(k);
    }
    let found = discover_key(want_key_id)?;
    store_cached_key(&found);
    adopt(found)
}

/// 把密钥装进进程缓存；`OnceLock` 只允许装一次，装不进去也要把手上的这份交出去。
fn adopt(k: Key) -> Option<Key> {
    match KEY.set(k.clone()) {
        Ok(()) => Some(k),
        // 并发的另一条路径抢先装了：只要 keyId 一致就是同一把，直接用缓存里那份。
        Err(_) => KEY.get().filter(|e| e.key_id == k.key_id).cloned(),
    }
}

/// 测试里禁止联网/扫内存：只有显式开关才允许走真实发现流程。
#[cfg(not(test))]
fn discovery_allowed() -> bool {
    true
}

#[cfg(test)]
fn discovery_allowed() -> bool {
    std::env::var_os("WB_ATREST_LIVE").is_some()
}

// ---------------------------------------------------------------------------
// 解密
// ---------------------------------------------------------------------------

/// 从任意 JSON 值里取明文：字符串原样返回、加密壳子解出明文、其它 `None`。
///
/// 上层只关心「这个字段最终是不是一段非空字符串」，加密与否是本模块的事。
pub fn reveal(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    decrypt_wrapper(value).ok()
}

/// 解开 `{$wbEncrypted:1, envelope:"…"}` 这一层壳子。
pub fn decrypt_wrapper(wrapper: &Value) -> Result<String, AtRestError> {
    if wrapper.get("$wbEncrypted").and_then(Value::as_u64) != Some(1) {
        return Err(AtRestError::NotEncrypted);
    }
    if wrapper.get("scheme").and_then(Value::as_str) == Some("asym-v1") {
        return Err(AtRestError::UnsupportedScheme);
    }
    let envelope_b64 = wrapper
        .get("envelope")
        .and_then(Value::as_str)
        .ok_or(AtRestError::Malformed("envelope 不是字符串"))?;
    let inner_bytes = STANDARD
        .decode(envelope_b64)
        .map_err(|_| AtRestError::Malformed("envelope 不是规范 base64"))?;
    let inner: Value = serde_json::from_slice(&inner_bytes)
        .map_err(|_| AtRestError::Malformed("envelope 不是 JSON"))?;

    let suite = inner
        .get("suite")
        .and_then(Value::as_u64)
        .ok_or(AtRestError::Malformed("suite 缺失"))?;
    if suite != SUITE as u64 {
        return Err(AtRestError::Malformed("不认识的 suite"));
    }
    let key_id = inner
        .get("keyId")
        .and_then(Value::as_str)
        .ok_or(AtRestError::Malformed("keyId 缺失"))?;
    if !is_lower_hex16(key_id) {
        return Err(AtRestError::Malformed("keyId 必须是 16 个小写十六进制字符"));
    }
    let nonce = field_bytes(&inner, "nonce", NONCE_LEN)?;
    let tag = field_bytes(&inner, "authTag", TAG_LEN)?;
    let ciphertext = field_bytes(&inner, "ciphertext", 0)?;

    let key = key_for(key_id).ok_or(AtRestError::NoKey)?;
    decrypt(&key, &nonce, &ciphertext, &tag, key_id)
}

fn is_lower_hex16(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn field_bytes(inner: &Value, name: &'static str, want_len: usize) -> Result<Vec<u8>, AtRestError> {
    let b64 = inner
        .get(name)
        .and_then(Value::as_str)
        .ok_or(AtRestError::Malformed("envelope 字段缺失"))?;
    let bytes = STANDARD
        .decode(b64)
        .map_err(|_| AtRestError::Malformed("envelope 字段不是规范 base64"))?;
    if want_len > 0 && bytes.len() != want_len {
        return Err(AtRestError::Malformed("envelope 字段长度不对"));
    }
    Ok(bytes)
}

fn decrypt(
    key: &Key,
    nonce: &[u8],
    ciphertext: &[u8],
    tag: &[u8],
    declared_key_id: &str,
) -> Result<String, AtRestError> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};

    if key.key_id != declared_key_id {
        return Err(AtRestError::NoKey);
    }
    let aad = field_aad(declared_key_id);
    // 客户端的 authTag 是独立字段；AEAD 接口要的是 ct||tag。
    let mut msg = Vec::with_capacity(ciphertext.len() + tag.len());
    msg.extend_from_slice(ciphertext);
    msg.extend_from_slice(tag);

    let cipher = Aes256Gcm::new_from_slice(&key.key).map_err(|_| AtRestError::NoKey)?;
    let plain = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: &msg,
                aad: &aad,
            },
        )
        .map_err(|_| AtRestError::AuthFailed)?;
    String::from_utf8(plain).map_err(|_| AtRestError::NotUtf8)
}

/// 字段级（`field` framing、`sym-v1`）的 AAD。
pub fn field_aad(key_id: &str) -> Vec<u8> {
    aad(key_id, FRAMING_NAME_FIELD, FRAMING_CODE_FIELD)
}

fn aad(key_id: &str, framing_name: &str, framing_code: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(AAD_MAGIC);
    out.push(1); // 版本
    push_lp(&mut out, framing_name.as_bytes());
    push_lp(&mut out, SCHEME.as_bytes());
    out.extend_from_slice(&SUITE.to_be_bytes());
    push_lp(&mut out, key_id.as_bytes());
    out.push(framing_code);
    out.extend_from_slice(&[0, 0]); // sequence = undefined, final = undefined
    out
}

fn push_lp(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

// ---------------------------------------------------------------------------
// 密钥发现：扫描客户端进程内存
// ---------------------------------------------------------------------------

fn discover_key(want_key_id: &str) -> Option<Key> {
    if !discovery_allowed() {
        return None;
    }
    for secret in scan_client_memory() {
        if let Some(k) = normalize_secret(&secret) {
            if k.key_id == want_key_id {
                return Some(k);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn scan_client_memory() -> Vec<String> {
    win_mem::find_capabilities(&["WorkBuddy.exe", "WorkBuddyAI.exe"])
}

#[cfg(not(target_os = "windows"))]
fn scan_client_memory() -> Vec<String> {
    // macOS / Linux 暂无对应通道：返回空，上层据此提示「拿不到密钥」。
    Vec::new()
}

/// Windows 只读内存扫描。
///
/// 只做三件事：`OpenProcess(PROCESS_VM_READ|QUERY_LIMITED_INFORMATION)`、
/// `VirtualQueryEx` 遍历可读区域、`ReadProcessMemory` 读出来找锚点。
/// **不注入、不修改、不挂起**进程；读不到（权限不足 / 进程刚退出）就跳过这个进程。
///
/// 手写 FFI 而不是引 `windows` crate：这里只用到 8 个 API，而那个 crate 会显著拖慢
/// 冷编译（本仓依赖刻意保持很薄）。结构体定义与 `winnt.h` 对齐，注释里标了来源。
#[cfg(target_os = "windows")]
mod win_mem {
    use super::{CAP_ANCHOR, STANDARD};
    use base64::Engine;
    use std::ffi::c_void;

    type Handle = isize;
    type Bool = i32;

    const PROCESS_VM_READ: u32 = 0x0010;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const MAX_PATH: usize = 260;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_GUARD: u32 = 0x100;
    const PAGE_READONLY: u32 = 0x02;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;

    /// 单次读取上限：大区域分段读，避免一次申请上百 MB。
    const CHUNK: usize = 16 << 20;
    const MAX_HITS: usize = 8;

    /// `MEMORY_BASIC_INFORMATION`（winnt.h）。64 位下 `PartitionId` 之后编译器插入
    /// 2 字节对齐，`Type` 之后补到 8 字节，`repr(C)` 会按字段类型自动做，别手动加填充。
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition_id: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        typ: u32,
    }

    /// `PROCESSENTRY32W`（tlhelp32.h）。
    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; MAX_PATH],
    }

    impl Default for ProcessEntry32W {
        fn default() -> Self {
            // SAFETY: 全零是 Win32 结构体的合法初值（随后会补 dw_size）。
            unsafe { std::mem::zeroed() }
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: Bool, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> Bool;
        fn VirtualQueryEx(
            process: Handle,
            address: *const c_void,
            buffer: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
        fn ReadProcessMemory(
            process: Handle,
            address: *const c_void,
            buffer: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> Bool;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    }

    pub fn find_capabilities(exe_names: &[&str]) -> Vec<String> {
        let procs = processes_by_name(exe_names);
        if procs.is_empty() {
            return Vec::new();
        }
        // 只有**主进程**持有 native 的 capability：它的父进程不在同名集合里
        // （renderer / utility 的父进程是主进程）。不筛的话要把十几个进程、十几个 G
        // 全读一遍，实测要三分钟以上 —— 而这只是「点一下从本机导入」。
        let names: Vec<u32> = procs.iter().map(|(pid, _)| *pid).collect();
        let mut roots: Vec<u32> = procs
            .iter()
            .filter(|(_, ppid)| !names.contains(ppid))
            .map(|(pid, _)| *pid)
            .collect();
        if roots.is_empty() {
            roots = names;
        }

        let mut out = Vec::new();
        for pid in roots {
            collect_from_pid(pid, &mut out);
            if !out.is_empty() {
                break; // 同一台机器上各进程拿到的 secret 是同一份，有一个就够了
            }
        }
        out
    }

    /// 目标进程的 `(pid, 父进程 pid)`。
    fn processes_by_name(names: &[&str]) -> Vec<(u32, u32)> {
        let mut procs = Vec::new();
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == 0 || snapshot == -1 {
            return procs;
        }
        let mut entry = ProcessEntry32W {
            dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
            ..Default::default()
        };
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while ok {
            let name_end = entry
                .sz_exe_file
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(MAX_PATH);
            if let Ok(name) = String::from_utf16(&entry.sz_exe_file[..name_end]) {
                if names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                    procs.push((entry.th32_process_id, entry.th32_parent_process_id));
                }
            }
            ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe {
            CloseHandle(snapshot);
        }
        procs
    }

    fn collect_from_pid(pid: u32, out: &mut Vec<String>) {
        let handle = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            return; // 权限不足 / 进程已退出：静默跳过
        }
        unsafe {
            scan_handle(handle, out);
            CloseHandle(handle);
        }
    }

    unsafe fn scan_handle(handle: Handle, out: &mut Vec<String>) {
        let mut addr: usize = 0;
        let limit: usize = if cfg!(target_pointer_width = "64") {
            0x7fff_ffff_ffff
        } else {
            0x7fff_ffff
        };
        while addr < limit {
            let mut mbi = std::mem::MaybeUninit::<MemoryBasicInformation>::zeroed();
            let got = VirtualQueryEx(
                handle,
                addr as *const c_void,
                mbi.as_mut_ptr(),
                std::mem::size_of::<MemoryBasicInformation>(),
            );
            if got == 0 {
                break;
            }
            let mbi = mbi.assume_init();
            let base = mbi.base_address as usize;
            let size = mbi.region_size;
            if size == 0 {
                break;
            }
            if mbi.state == MEM_COMMIT
                && (mbi.protect & PAGE_GUARD) == 0
                && matches!(
                    mbi.protect,
                    PAGE_READONLY | PAGE_READWRITE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE
                )
            {
                let mut off = 0usize;
                while off < size {
                    let want = CHUNK.min(size - off);
                    let mut buf = vec![0u8; want];
                    let mut read = 0usize;
                    let ok = ReadProcessMemory(
                        handle,
                        (base + off) as *const c_void,
                        buf.as_mut_ptr() as *mut c_void,
                        want,
                        &mut read,
                    ) != 0;
                    if ok && read > 0 {
                        buf.truncate(read);
                        extract(&buf, out);
                        if out.len() >= MAX_HITS {
                            return;
                        }
                    }
                    off += want;
                }
            }
            addr = base.saturating_add(size);
        }
    }

    /// 在内存块里找所有 `"atRestSecretKey":"<44 字符 base64>"`。
    fn extract(buf: &[u8], out: &mut Vec<String>) {
        let mut from = 0usize;
        while let Some(pos) = find(&buf[from..], CAP_ANCHOR) {
            let start = from + pos + CAP_ANCHOR.len();
            // base64(32 字节) 固定 44 字符，末位是 '='。
            if start + 44 <= buf.len() && buf[start + 43] == b'=' {
                if let Ok(text) = std::str::from_utf8(&buf[start..start + 44]) {
                    // 只收「能解码成 32 字节」的，避免把代码里的字面量当成密钥。
                    if STANDARD.decode(text).map(|v| v.len() == 32).unwrap_or(false)
                        && !out.iter().any(|s| s == text)
                    {
                        out.push(text.to_string());
                    }
                }
            }
            from = start;
            if from >= buf.len() {
                break;
            }
        }
    }

    /// 朴素搜索，但**先用首字节过滤**再比整段。
    ///
    /// 之前写成 `haystack.windows(n).position(|w| w == needle)`，对每个起点都做一次
    /// 20 字节比较 —— 扫一个 800MB 的进程要十几秒。首字节命中率极低，几百万次里
    /// 只有个位数，所以这个改动的收益是数量级的。
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        let n = needle.len();
        if n == 0 || haystack.len() < n {
            return None;
        }
        let first = needle[0];
        let limit = haystack.len() - n;
        for i in 0..=limit {
            if haystack[i] == first && &haystack[i..i + n] == needle {
                return Some(i);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// DPAPI 缓存
// ---------------------------------------------------------------------------

fn cache_path() -> Option<PathBuf> {
    CACHE_DIR.get().map(|d| d.join(CACHE_FILE))
}

fn load_cached_key(want_key_id: &str) -> Option<Key> {
    let path = cache_path()?;
    let blob = std::fs::read(path).ok()?;
    let plain = dpapi_unprotect(&blob)?;
    let json: Value = serde_json::from_slice(&plain).ok()?;
    if json.get("version").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let key_id = json.get("keyId")?.as_str()?;
    if key_id != want_key_id {
        return None;
    }
    let raw = STANDARD.decode(json.get("key")?.as_str()?).ok()?;
    if raw.len() != 32 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw);
    // 缓存里的 keyId 也可能是被改过的：以重新派生的结果为准。
    (derive_key_id(&key) == want_key_id).then_some(Key {
        key_id: key_id.to_string(),
        key,
    })
}

fn store_cached_key(key: &Key) {
    let Some(path) = cache_path() else {
        return;
    };
    let payload = serde_json::json!({
        "version": 1,
        "keyId": key.key_id,
        "key": STANDARD.encode(key.key),
    });
    let Ok(bytes) = serde_json::to_vec(&payload) else {
        return;
    };
    let Some(enc) = dpapi_protect(&bytes) else {
        return; // DPAPI 不可用就静默降级：下次再扫内存
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, enc);
}

/// `DATA_BLOB`（dpapi.h）。
#[cfg(target_os = "windows")]
#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[cfg(target_os = "windows")]
#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        data_in: *const DataBlob,
        description: *const u16,
        entropy: *const DataBlob,
        reserved: *const c_void,
        prompt: *const c_void,
        flags: u32,
        data_out: *mut DataBlob,
    ) -> i32;
    fn CryptUnprotectData(
        data_in: *const DataBlob,
        description: *mut *mut u16,
        entropy: *const DataBlob,
        reserved: *const c_void,
        prompt: *const c_void,
        flags: u32,
        data_out: *mut DataBlob,
    ) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

/// DPAPI 加密（当前用户作用域）。失败就静默降级成「不缓存」。
#[cfg(target_os = "windows")]
fn dpapi_protect(plain: &[u8]) -> Option<Vec<u8>> {
    use std::ffi::c_void;

    let input = DataBlob {
        cb_data: plain.len() as u32,
        pb_data: plain.as_ptr() as *mut u8,
    };
    let mut output = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null::<c_void>(),
            std::ptr::null::<c_void>(),
            0,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return None;
    }
    let owned = unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize) }.to_vec();
    unsafe {
        LocalFree(output.pb_data as *mut c_void);
    }
    Some(owned)
}

#[cfg(target_os = "windows")]
fn dpapi_unprotect(blob: &[u8]) -> Option<Vec<u8>> {
    use std::ffi::c_void;

    let input = DataBlob {
        cb_data: blob.len() as u32,
        pb_data: blob.as_ptr() as *mut u8,
    };
    let mut output = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null::<c_void>(),
            std::ptr::null::<c_void>(),
            0,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return None;
    }
    let owned = unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize) }.to_vec();
    unsafe {
        LocalFree(output.pb_data as *mut c_void);
    }
    Some(owned)
}

#[cfg(not(target_os = "windows"))]
fn dpapi_protect(_plain: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(not(target_os = "windows"))]
fn dpapi_unprotect(_blob: &[u8]) -> Option<Vec<u8>> {
    None
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};

    /// 测试用假密钥（形状与真实 capability 一致：44 字符 base64，解码后 32 字节）。
    ///
    /// **不要把它换成真机的 `atRestSecretKey`**：本仓库是公开的，而这个字符串就是
    /// 打开客户端落盘凭证的钥匙。真实密钥只从进程内存取，不进任何源码/测试。
    const SECRET: &str = "CvFQoSw3fK5chmmK8sxZmnSOblCLYyd3Q3tUJvFJq5c=";

    fn install_key() -> Key {
        let key = normalize_secret(SECRET).unwrap();
        let _ = KEY.set(key.clone());
        key
    }

    /// 按客户端的方式造壳子，用于验证「算法自洽」。
    fn seal(secret: &str, plain: &str) -> Value {
        let key = normalize_secret(secret).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key.key).unwrap();
        let nonce = [7u8; NONCE_LEN];
        let aad = field_aad(&key.key_id);
        let out = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain.as_bytes(),
                    aad: &aad,
                },
            )
            .unwrap();
        let (ct, tag) = out.split_at(out.len() - TAG_LEN);
        let inner = serde_json::json!({
            "suite": 1,
            "keyId": key.key_id,
            "nonce": STANDARD.encode(nonce),
            "authTag": STANDARD.encode(tag),
            "ciphertext": STANDARD.encode(ct),
        });
        serde_json::json!({
            "$wbEncrypted": 1,
            "envelope": STANDARD.encode(serde_json::to_vec(&inner).unwrap()),
        })
    }

    #[test]
    fn key_id_is_the_sha256_prefix_of_the_derived_key() {
        // 钉住派生规则：key = sha256(base64 文本)，keyId = sha256(key).hex[..16]。
        // 写反（对解码后的 32 字节做哈希）会让真机上的认证永远失败，所以这里必须有断言。
        assert_eq!(normalize_secret(SECRET).unwrap().key_id, "82abe577af5a8dc9");
    }

    #[test]
    fn aad_matches_the_client_layout() {
        let aad = field_aad("82abe577af5a8dc9");
        let mut want = Vec::new();
        want.extend_from_slice(b"WB-AAD\0\x01");
        push_lp(&mut want, b"WBEV1");
        push_lp(&mut want, b"sym-v1");
        want.extend_from_slice(&1u32.to_be_bytes());
        push_lp(&mut want, b"82abe577af5a8dc9");
        want.push(2);
        want.extend_from_slice(&[0, 0]);
        assert_eq!(aad, want);
        assert_eq!(aad.len(), 7 + 1 + (4 + 5) + (4 + 6) + 4 + (4 + 16) + 1 + 2);
    }

    #[test]
    fn plain_strings_pass_through() {
        assert_eq!(reveal(&serde_json::json!("  hello  ")).as_deref(), Some("hello"));
        assert_eq!(reveal(&serde_json::json!(12)), None);
        assert_eq!(reveal(&serde_json::json!(null)), None);
        assert_eq!(reveal(&serde_json::json!("")), None);
    }

    #[test]
    fn malformed_wrappers_are_rejected() {
        assert_eq!(
            decrypt_wrapper(&serde_json::json!("abc")),
            Err(AtRestError::NotEncrypted)
        );
        assert_eq!(
            decrypt_wrapper(&serde_json::json!({"$wbEncrypted": 1, "scheme": "asym-v1", "envelope": "x"})),
            Err(AtRestError::UnsupportedScheme)
        );
        assert!(matches!(
            decrypt_wrapper(&serde_json::json!({"$wbEncrypted": 1, "envelope": "!!!"})),
            Err(AtRestError::Malformed(_))
        ));
        // suite 不是 1
        let inner = serde_json::json!({"suite": 2, "keyId": "82abe577af5a8dc9"});
        let wrapper = serde_json::json!({
            "$wbEncrypted": 1,
            "envelope": STANDARD.encode(serde_json::to_vec(&inner).unwrap()),
        });
        assert!(matches!(
            decrypt_wrapper(&wrapper),
            Err(AtRestError::Malformed(_))
        ));
    }

    #[test]
    fn roundtrip_with_injected_key() {
        install_key();
        let wrapper = seal(SECRET, "eyJhbGciOiJSUzI1NiJ9.payload.sig");
        assert_eq!(
            reveal(&wrapper).as_deref(),
            Some("eyJhbGciOiJSUzI1NiJ9.payload.sig")
        );
    }

    #[test]
    fn foreign_key_id_cannot_be_opened() {
        install_key();
        // 另一个 secret 造的壳子：keyId 不同，进程缓存也帮不上忙（测试环境禁扫内存）
        let other = seal("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "x");
        assert_eq!(reveal(&other), None);
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        install_key();
        let mut wrapper = seal(SECRET, "hello");
        let envelope = wrapper["envelope"].as_str().unwrap().to_string();
        let mut inner: Value = serde_json::from_slice(&STANDARD.decode(&envelope).unwrap()).unwrap();
        let mut raw = STANDARD.decode(inner["ciphertext"].as_str().unwrap()).unwrap();
        raw[0] ^= 0xff;
        inner["ciphertext"] = Value::String(STANDARD.encode(raw));
        wrapper["envelope"] = Value::String(STANDARD.encode(serde_json::to_vec(&inner).unwrap()));
        assert_eq!(reveal(&wrapper), None);
    }

    #[test]
    fn discovery_is_off_in_tests() {
        // 避免单测无意间去扫真实客户端内存（慢、且结果依赖本机状态）
        assert!(!discovery_allowed());
    }

    /// 端到端：读**本机真实**的 auth/*.info，解出 accessToken。
    ///
    /// 需要 Windows + 客户端已登录 + 客户端进程在跑（密钥从它的内存里来），
    /// 所以默认忽略；改动算法后这样验一次：
    ///
    /// ```text
    /// WB_ATREST_LIVE=1 cargo test --lib --offline atrest::tests::decrypts_real_auth_file \
    ///     -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "读本机真实凭证，需要客户端已登录且在运行"]
    fn decrypts_real_auth_file() {
        let Ok(local) = std::env::var("LOCALAPPDATA") else {
            return;
        };
        let dir = std::path::Path::new(&local)
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("info") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(at) = json.pointer("/auth/accessToken") else {
                continue;
            };
            if at.get("$wbEncrypted").is_none() {
                continue;
            }
            let token = reveal(at).unwrap_or_else(|| panic!("解密失败：{}", path.display()));
            println!("{} → token len {}", path.display(), token.len());
            assert!(token.starts_with("eyJ"), "解出来的不是 JWT：{token:.20}");
            assert_eq!(token.matches('.').count(), 2, "JWT 应该是三段");
            checked += 1;
        }
        println!("checked {checked} encrypted auth file(s)");
    }
}
