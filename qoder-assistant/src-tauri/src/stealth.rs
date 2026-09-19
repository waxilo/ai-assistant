//! 智能接管的**保险丝**：把自定义端点注入官方客户端，并保证它一定能被取下来。
//!
//! # 它到底改了什么
//!
//! 智能接管 = 让官方客户端的所有对话请求自动走本应用的反代。真正的落点是
//! **客户端 `app.asar.unpacked` 里那份被执行的 worker 产物**（见 [`crate::patch`]），
//! 而**不是**任何配置文件：`~/.qoder[-cn]/settings.json` 的 `env` 块**没有消费者**，
//! 写进去只是自我安慰 —— 旧实现写的 `env.CODEBUDDY_BASE_URL` 是 WorkBuddy/CodeBuddy
//! 时代的残留键，Qoder 两个客户端都不读，所以那时接管一直在空转
//! （配置写成功、界面显示已开启、端口在听，对话依旧直连官方）。
//!
//! # 区域（[`Region`]）
//!
//! 两套部署各有自己的客户端与 SDK 目录，所以本模块的**每一个入口都必须带区域**：
//! 装错那一份的后果不是报错，而是「界面显示接管已开启、实际一个请求都没被接管」。
//! 租约也把区域记下来 —— 卸载 / 清扫时手里只有租约，而「该动哪一份产物」正是它决定的。
//! 国际版目前**不支持**端点覆盖（[`Region::endpoint_env_key`] 对它返回 `None`，
//! [`install`] 直接报错而不是静默空转）。
//!
//! # 能装不是本事，「保证一定能卸」才是
//!
//! 用**租约 + 心跳**把这件事钉死：
//!
//! - 装卸都以「产物里有没有我们的注入段」为准，不去猜「这个值是不是我们写的」；
//!   摘除是**精确剥离**，逐字节还原成官方原文（不依赖备份回滚，见 [`crate::patch`]）；
//! - 反代活着就持续心跳；心跳停了（应用崩了 / 被 kill -9 / 端口没了）租约即过期；
//! - 应用启动时先 [`sweep`]：租约过期 = 僵尸注入，直接剥掉，绝不让上次崩溃留下断网残留；
//! - 装的顺序是「先落租约、再改产物」，中途崩了 [`sweep`] 也能收尾。
//!
//! # 与 netfix 的关系
//!
//! `netfix` 的「智能接管」判定直接问产物（[`crate::patch::is_installed`]）：租约新鲜
//! 且指的就是那个区域 → 正常工作中，不算问题；否则是僵尸注入，照常判为会断网。
//! 这样「一键恢复」清掉的是真残留，不会把正在工作的接管误伤掉。
//! 它同样是**逐区域**问的：两个区域各可能留一份，一个是工作中的、另一个是残留。

use crate::region::Region;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// 心跳间隔：反代监督线程按此频率续租
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// 超过这么久没有心跳，就认定接管方已经不在了
pub const LEASE_TTL: Duration = Duration::from_secs(30);

// 「写进去 ≠ 生效」这件事，这块代码栽过两次，都记在这里：
//
// 1. 落点错了。写的是 `~/.qoder[-cn]/settings.json` 的 `env.CODEBUDDY_BASE_URL`
//    （CodeBuddy 时代的残留键）—— Qoder 客户端**根本不读**，它的推理进程 env 来自
//    `buildEnv(){ let e = this.options.env ?? {...process.env} }`，配置文件里的 `env`
//    块没有任何消费者。于是「写成功 / 界面已开启 / 端口在听」而请求全直连官方。
// 2. 配套动作也是假的。当时还跟着一套「退出客户端 → 重启客户端」，理由是「长驻 CLI
//    host 会把旧端点留在 process.env」—— 而 Qoder 的推理进程是每次会话按需 spawn 的
//    一次性 `--print` 进程，跑完即退，没有可重启的东西。
//
// 现在落点是**客户端真正执行的那份产物**（见 [`crate::patch`]），且状态文案只陈述
// 可以当场核验的事实：注入在不在、心跳在不在、日志里到底有没有收到过请求。
//
// 这里曾有 `pub const INERT_NOTE`（一句「客户端目前不读这个键、接管尚未生效」）。
// 它按当时的约定在接线落地后删掉了 —— 那句提示的价值恰恰在于「不会被长期保留」。

const LEASE_FILE: &str = "stealth.json";

/// 接管事件日志（追加式 JSONL）。记录 install / proxy_request / uninstall / 错误，
/// 是排查接管问题的**唯一证据源**：端点写没写进去、反代有没有真的收到请求、
/// 什么时候摘的，全在这里。
///
/// ⚠️ 这里曾有「长驻 CLI host 会把旧端点留在 process.env，所以要按日志判断它有没有
/// 把缓存清掉」的说法 —— 那个前提不成立：Qoder 的推理进程是**每次会话按需 spawn 的
/// 一次性 `--print` 进程**（跑完即退），没有长驻 host，也就不存在跨重启的 env 缓存。
const JOURNAL_FILE: &str = "takeover-journal.jsonl";

/// 接管调试日志（纯文本、一行一条）。**请求级细节全在这里**：反代收到的每个路径、
/// 鉴权形态、完整请求头、上游状态码、连接断在哪一步。
///
/// 为什么要与 [`JOURNAL_FILE`] 分成两份：那份是**对客通知**（界面直接显示），只留
/// 「开关动了 / 这轮对话用了哪个账号 / 真出事了」；而排查要的是「客户端到底发了什么」，
/// 它既不适合端给用户看、又必须一条不漏。两份同锁写入，时间线可以逐条对齐。
const DEBUG_LOG_FILE: &str = "takeover-debug.log";

/// 调试日志的软上限。超了从头部裁掉旧内容（留最近一半）——
/// 一轮长会话里每个请求都要写一行（还带请求头），不设上限它就是无限增长的。
const DEBUG_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 只进调试日志、不进界面时间线的事件名。
///
/// 新代码里这些事件已经改走 [`debug_append`]，压根不会写进 journal —— 这张表是给
/// **升级前留下的那份 journal**兜底的：不清空它，界面也不会再冒出技术细节。
/// （`proxy_request` 是「反代确实收到过请求」的内部证据，历史上就从不展示。）
const DEBUG_ONLY_EVENTS: &[&str] = &[
    "proxy_auth",
    "proxy_request",
    "proxy_bad_request",
    "proxy_head_stalled",
    "proxy_upstream_status",
    "proxy_conn_closed",
    "proxy_conn_setup_failed",
];

/// 一条接管事件
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalEvent {
    pub at_ms: i64,
    /// 本地时间（展示用）
    pub at: String,
    /// install / uninstall / route_start / proxy_request / proxy_upstream_error / …
    /// （历史日志里还可能见到 `restart_qoder` —— 那是「切换拓扑要重启客户端」时代的
    /// 遗留事件，产它的代码已删除，展示层仍能把它读成人话。）
    pub event: String,
    #[serde(default)]
    pub detail: String,
}

pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

/// 追加是串行的纯追加。取锁只为把并发写排成队——代理每个连接一个线程，而这份日志是
/// 排查接管问题**唯一的**证据源，时间顺序乱了它就失去意义。
static JOURNAL_LOCK: Mutex<()> = Mutex::new(());

/// 锁被毒化（某线程持锁时 panic）不能成为丢日志的理由：诊断代码必须比它诊断的
/// 那条路径更能扛。
fn lock_journal() -> std::sync::MutexGuard<'static, ()> {
    JOURNAL_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn journal_read(data_dir: &Path) -> Vec<JournalEvent> {
    read_events(data_dir)
}

/// 不加锁的读。只解析**以换行结束**的完整行：末尾没有换行 = 某次写入还在途中
/// （或崩在半路），跳过它，别把「还没写完」当成「一条坏记录」。
fn read_events(data_dir: &Path) -> Vec<JournalEvent> {
    let Ok(text) = fs::read_to_string(journal_path(data_dir)) else {
        return Vec::new();
    };
    // 纯追加下读者可能正好撞上一次写：只认到最后一个换行为止
    let Some(end) = text.rfind('\n') else {
        return Vec::new();
    };
    text[..=end]
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn make_event(event: &str, detail: &str) -> JournalEvent {
    JournalEvent {
        at_ms: now_ms(),
        at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        event: event.to_string(),
        detail: detail.to_string(),
    }
}

/// 追加一条**对客通知**：界面「接管动态」会显示它，同时抄一份进调试日志
/// （调试时看一个文件就够，不必两份对照）。
///
/// 文案只讲「发生了什么、要不要管」—— 路径、env 键名、鉴权形态、会话 id 这类
/// 排查细节一律走 [`debug_append`]，别混进来：界面上一旦开始讲这些，
/// 用户就再也找不到「这轮对话扣的是哪个账号」那条真正要看的信息了。
///
/// 失败一律静默：日志是诊断辅助，绝不能反过来影响接管本身。
///
/// **不设条数上限**。曾经的「只留最近 200 条」会让高频内部证据把真正要看的事件挤出
/// 窗口，表现成「接管动态自己清空了」。改为把日志与**一次接管会话**绑定：
/// 开启接管时整份重置（见 [`journal_append_reset`]），会话之内一条不丢。
pub fn journal_append(data_dir: &Path, event: &str, detail: &str) {
    let ev = make_event(event, detail);
    let _guard = lock_journal();
    write_events(data_dir, &[ev], false);
    append_debug_raw(data_dir, "user", event, detail);
}

/// 追加一条**仅调试**的细节：只进调试日志文件，界面永远看不到。
///
/// 判定标准很简单：如果这条信息用户看了不知道该做什么、而排障时少了它会卡住，
/// 那它就是调试信息。
pub fn debug_append(data_dir: &Path, event: &str, detail: &str) {
    let _guard = lock_journal();
    append_debug_raw(data_dir, "debug", event, detail);
}

/// 同 [`journal_append`]，但**先丢弃全部历史**。
///
/// 用在「开启接管」这一刻：两份日志描述的都是本轮会话，上一轮的话题已经结束。
pub fn journal_append_reset(data_dir: &Path, event: &str, detail: &str) {
    let ev = make_event(event, detail);
    let _guard = lock_journal();
    write_events(data_dir, &[ev], true);
    reset_debug_log(data_dir);
    append_debug_raw(data_dir, "user", event, detail);
}

/// 落盘。`truncate = true` 先清空历史，否则纯追加。
///
/// 用纯追加而非「读全量 → 改 → 整体重写」：取消上限之后，后者每次追加的代价随文件
/// 长度线性增长（还附带一遍全量 JSON 解析），长会话会把每个反代请求越拖越慢。追加是
/// O(1)，顺带还绕开了 Windows 上 `rename` 会因文件被展示层占用而失败的问题。
///
/// ⚠️ 本函数**不加锁** —— 调用方必须已经持有 `JOURNAL_LOCK`。加锁被提到公开入口，
/// 是因为一次调用要写两份文件（journal + 调试日志），必须整体串行；若各自加锁，
/// 两次拿锁之间会插进别的线程，两份日志的时间线就对不上了。
fn write_events(data_dir: &Path, events: &[JournalEvent], truncate: bool) {
    let body: String = events
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .map(|l| format!("{l}\n"))
        .collect();
    if body.is_empty() {
        return;
    }
    if fs::create_dir_all(data_dir).is_err() {
        return;
    }
    let mut opts = fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    if truncate {
        opts.truncate(true);
    } else {
        opts.append(true);
    }
    let Ok(mut f) = opts.open(journal_path(data_dir)) else {
        return;
    };
    if !truncate && !is_clean_tail(&mut f) {
        // 上次崩在写入途中会留下一条没有换行的残句；先补一个换行把它隔开，否则新记录
        // 会粘在残句后面，一起变成坏行（然后一起被丢掉）。
        let _ = f.write_all(b"\n");
    }
    // 一次 write_all 写完整条：单条记录远小于一个扇区，读者不会看到半条
    let _ = f.write_all(body.as_bytes());
}

pub fn debug_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DEBUG_LOG_FILE)
}

/// 调试日志的一行：
///
/// ```text
/// 2026-09-19 17:47:44.123 [user ] install      | 接管已开启（国内版）…
/// 2026-09-19 17:47:44.456 [debug] proxy_auth   | 反代收到 POST /algo/… | 鉴权 …
/// ```
///
/// 前缀是给 `grep '\[debug\]'` 用的：想知道「界面上这条通知背后到底发生了什么」，
/// 按事件名在同一份文件里往下翻就行 —— 对客通知在调试日志里也有一份。
///
/// ⚠️ 同样不加锁，调用方持 `JOURNAL_LOCK`。
fn append_debug_raw(data_dir: &Path, audience: &str, event: &str, detail: &str) {
    if fs::create_dir_all(data_dir).is_err() {
        return;
    }
    let path = debug_log_path(data_dir);
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    let Ok(mut f) = opts.open(&path) else {
        return;
    };
    let line = format!(
        "{} [{audience:5}] {event:20} | {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        one_line(detail)
    );
    let _ = f.write_all(line.as_bytes());
    drop(f);
    trim_debug_log(&path);
}

/// 调试日志是**一行一条**（`tail` / `grep` 才有意义）。detail 里可能带换行
/// （比如坏请求要附上收到的头部前缀），转成字面量 `\n`，别把一条拆成多行。
fn one_line(s: &str) -> String {
    s.replace("\r\n", "\\n").replace(['\r', '\n'], "\\n")
}

/// 超软上限就从头部裁掉旧内容（留最近一半，按行边界切）。内容本身不丢语义：
/// 每行都自带时间戳与事件名。
fn trim_debug_log(path: &Path) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() <= DEBUG_LOG_MAX_BYTES {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else { return };
    // 半个字节位置可能落在多字节字符中间（中文 3 字节）→ 先挪到字符边界
    let mut at = text.len() / 2;
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    // 再往后找到第一个换行：别把一条记录劈成两半
    let cut = text[at..]
        .find('\n')
        .map(|i| at + i + 1)
        .unwrap_or(text.len());
    let _ = fs::write(path, &text[cut..]);
}

/// 调试日志整份清空（开启接管时用）。
fn reset_debug_log(data_dir: &Path) {
    if fs::create_dir_all(data_dir).is_err() {
        return;
    }
    let _ = fs::write(debug_log_path(data_dir), "");
}

/// 文件是否为空、或以换行结尾（空文件视为「干净」，无需补换行）。
fn is_clean_tail(f: &mut fs::File) -> bool {
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return true;
    };
    if len == 0 {
        return true;
    }
    let mut b = [0u8; 1];
    f.seek(SeekFrom::End(-1)).is_ok() && f.read_exact(&mut b).is_ok() && b[0] == b'\n'
}

/// 接管租约。落在**本应用**的数据目录里，不进 Qoder 的配置。
///
/// 这里曾有一个 `pid` 字段（写着「仅用于展示与自查」）。实测它的下场是**变成谎话**：
/// 应用重启后接管仍开着时，新进程只是给旧租约续心跳，于是盘上留下一个指向已死进程的
/// 编号。既然没有读者、又会在最需要可信的时候骗人，删掉 —— 存活判据从头到尾只有
/// `heartbeat_ms` 一个。老租约文件里多余的 `pid` 会被 serde 忽略，不影响读取。
///
/// 也曾有一个 `previous` 字段（装载前那份配置的原值，卸载时还原回去）。换到
/// **补丁**模型后它没有意义了：注入段是加在文件头的一段可识别文本，摘除就是
/// **精确剥掉那一段**（逐字节还原），不需要记「原来是啥」，也不会把官方更新后的
/// 新版本误还原成旧版。老租约里残留的 `previous` 同样被 serde 忽略。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Lease {
    /// **这个租约装在哪个区域上**。卸载 / 清扫时手里只有租约，而「该动哪个
    /// 客户端的产物」只有它知道，所以区域必须随租约落盘。
    /// 老租约没有这个字段 → `#[serde(default)]` 落成国际版（那个字段出现之前只有国际版）。
    #[serde(default)]
    pub region: Region,
    /// 注入客户端的端点值，如 `https://127.0.0.1:8789`
    pub url: String,
    pub port: u16,
    pub installed_at_ms: i64,
    /// 最近一次心跳（毫秒）。存活判据只看这个。
    pub heartbeat_ms: i64,
    /// 最近一次注入失败的**原文**（成功时清空）。
    ///
    /// 为什么必须落盘：注入发生在反代监督线程里，它的 stderr 用户看不到 —— 于是
    /// 「界面说接管已开启、磁盘上却什么都没有」就成了这个项目栽过最多的那类坑。
    /// 把失败原因存进租约，[`status`] 就能把它原样端到界面上，包括「该去系统设置里
    /// 开哪个开关」这种只有出错当下才知道的下一步。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn lease_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LEASE_FILE)
}

pub fn load_lease(data_dir: &Path) -> Option<Lease> {
    let text = fs::read_to_string(lease_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 上一次注入失败的原因（租约里的 `last_error`）。
///
/// 单独开一个读口，是因为这里有一处**顺序陷阱**：「开启接管」失败时会先
/// `uninstall`（它删掉租约）再往上抛错。调用方必须在摘除之前把原因取走，
/// 否则界面上只剩一句「端口被占用」，而真正的原因 —— 典型是 macOS「App 管理」
/// 没授权，且自签应用**永远不会弹授权框** —— 恰恰是用户唯一能动手解决的那一项。
pub fn last_error(data_dir: &Path) -> Option<String> {
    load_lease(data_dir)?.last_error
}

fn save_lease(data_dir: &Path, lease: &Lease) -> std::io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let target = lease_path(data_dir);
    let tmp = data_dir.join(format!("{LEASE_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(lease)?)?;
    fs::rename(&tmp, &target)?;
    crate::accounts::set_private_permissions(&target);
    Ok(())
}

/// 租约是否还活着
pub fn is_alive(lease: &Lease) -> bool {
    now_ms().saturating_sub(lease.heartbeat_ms) < LEASE_TTL.as_millis() as i64
}

/// 续租。没有租约就什么也不做（避免凭空造出一个租约）。
/// 端口变了说明配置被改过，也不续 —— 让 `sweep` 去收拾。
pub fn heartbeat(data_dir: &Path, port: u16) {
    if let Some(mut lease) = load_lease(data_dir) {
        if lease.port == port {
            lease.heartbeat_ms = now_ms();
            let _ = save_lease(data_dir, &lease);
        }
    }
}

/// 本机反代的端点 URL。
///
/// **必须是 https**：客户端只接受 `https:` 的 origin（`M7a()` 里根本没有 `http:`
/// 分支），所以反代自己终止 TLS —— 证书见 [`crate::certs`]，
/// 客户端侧的信任注入见 [`crate::patch`]。
fn url_for_port(port: u16) -> String {
    format!("https://127.0.0.1:{port}")
}

/// 装卸的落点：**被执行的**那份 worker 产物（asar 之外，见 [`crate::patch`]）。
///
/// 这里曾经写的是 `~/.qoder[-cn]/settings.json` 的 `env.CODEBUDDY_BASE_URL` ——
/// 一个客户端根本不读的键：SDK 起推理进程时 env 取自
/// `buildEnv(){ let e = this.options.env ?? {...process.env} }`，配置文件里的
/// `env` 块**没有任何消费者**。于是端点写成功、界面显示已开启、端口在听，
/// 而对话一直直连官方（用户看到的「接管没生效、扣的是另一个账号」正是这个）。
fn target_file(region: Region) -> Option<PathBuf> {
    crate::patch::worker_path(region)
}

/// 读取**指定区域**当前注入的端点（另一个区域装了什么不影响这个答案）。
pub fn current_endpoint(region: Region) -> Option<String> {
    crate::patch::current_url(region)
}

/// **装**：把端点注入**指定区域**客户端的 worker 产物，并落下租约。
///
/// 幂等：已装着、且区域与端口都一致 → 只续一次心跳，不重复写 33MB 的产物文件。
/// 但**每次都复查一遍注入是否还在**：官方更新会把整个文件换掉，那样注入就没了，
/// 而心跳（每 5s）正是发现这件事最自然的时机 —— 复查只读文件头 4KB。
///
/// 区域不一致时**先按旧租约卸干净**再装新的：换区域等于换一个客户端接管，
/// 旧客户端上的注入必须一起摘掉，否则它会一直指向本机端口，而我们已不为它服务。
///
/// `ca_pem` 是反代那张自签 CA 的证书：端点被客户端强制成 https，客户端必须
/// 认得出它。证书随注入一起写进产物，见 [`crate::patch`]。
pub fn install(region: Region, data_dir: &Path, port: u16, ca_pem: &str) -> Result<(), String> {
    let url = url_for_port(port);
    if region.endpoint_env_key().is_none() {
        return Err(format!(
            "{}的端点覆盖尚未支持（该区域的客户端不读这个键），已跳过。",
            region.label()
        ));
    }
    let target = target_file(region).ok_or_else(|| {
        format!(
            "找不到{}客户端的 worker 产物，无法接管：请确认官方客户端已装在 /Applications。",
            region.label()
        )
    })?;

    if let Some(mut lease) = load_lease(data_dir) {
        if lease.region == region && lease.port == port && lease.url == url {
            lease.heartbeat_ms = now_ms();
            let _ = save_lease(data_dir, &lease);
            // 值可能被官方更新覆盖掉，这里保证它仍是我们期望的那个（已一致则空操作）
            return match crate::patch::install_at(&target, region, &url, ca_pem) {
                Ok(_) => {
                    // 上一次失败过、这次好了 → 把旧错误清掉，别让界面继续报陈年故障
                    if lease.last_error.take().is_some() {
                        let _ = save_lease(data_dir, &lease);
                    }
                    Ok(())
                }
                Err(e) => {
                    lease.last_error = Some(e.clone());
                    let _ = save_lease(data_dir, &lease);
                    Err(e)
                }
            };
        }
        // 区域或端口变了：先按旧租约卸干净，再装新的
        let _ = uninstall(lease.region, data_dir);
    }

    let lease = Lease {
        region,
        url: url.clone(),
        port,
        installed_at_ms: now_ms(),
        heartbeat_ms: now_ms(),
        last_error: None,
    };
    // 先落租约再改产物：中途崩了也留有记录，sweep 能收尾
    save_lease(data_dir, &lease).map_err(|e| format!("写入租约失败：{e}"))?;
    if let Err(e) = crate::patch::install_at(&target, region, &url, ca_pem) {
        // 注入失败：把原文留在租约里，界面会照读 —— 别让它变成一句只有 stderr 知道的秘密
        let mut failed = lease;
        failed.last_error = Some(e.clone());
        let _ = save_lease(data_dir, &failed);
        return Err(e);
    }
    // 事件里带上扣费备选名单，界面时间线能直接回答「开启时当前账号池是什么」
    let settings = crate::accounts::load_settings(data_dir);
    let names = billing_account_names(data_dir, &settings.billing_account_ids);
    // 开启接管 = 新的一轮会话，日志随之重置；顺手把「清掉了上一轮多少条」写进第一条
    // 事件里，界面上那句「以前的动态怎么没了」就地有答案
    let cleared = journal_read(data_dir).len();
    let note = if cleared > 0 {
        format!("；已清空上一轮动态 {cleared} 条")
    } else {
        String::new()
    };
    journal_append_reset(
        data_dir,
        "install",
        &format!(
            "接管已开启（{}）：对话请求已改由本应用转发，扣费备选：{names}{note}",
            region.label()
        ),
    );
    // 落点路径 / env 键名 / 端点值只在排查时有用，端到界面上既占地方又答非所问 ——
    // 它们进调试日志。上面那条 reset 刚清过文件，所以这是新一轮的第一条细节。
    debug_append(
        data_dir,
        "install_target",
        &format!(
            "端点已注入 {}（env.{}={url}），并注入了本机 CA 以信任本地 TLS",
            target.display(),
            region.endpoint_env_key().unwrap_or("")
        ),
    );
    Ok(())
}

/// **卸**：剥掉 worker 产物里的注入段（逐字节还原成官方原样），删掉租约。
///
/// 只认**我们自己的注入标记**：产物里没有那段就什么都不做 —— 不去猜
/// 「文件里这个端点值是不是我们写进去的」，因为那是别人的文件，
/// 而且补丁模型本就不需要「记住原来是什么」（摘除是精确剥离）。
///
/// 这里**不碰任何客户端进程**：Qoder 的推理进程是每次会话按需起的一次性 `--print`
/// 进程，没有可重启的长驻 host。摘掉注入后客户端的**下一次会话**就会读到干净的产物、
/// 恢复直连；正在登录的账号与正在进行的对话都不受影响。
pub fn uninstall(region: Region, data_dir: &Path) -> Result<(), String> {
    let label = region.label();
    let changed = match target_file(region) {
        Some(target) => crate::patch::uninstall_at(&target)?,
        // 客户端已经卸载了：没有可摘的东西，也不算错误
        None => false,
    };
    if changed {
        journal_append(
            data_dir,
            "uninstall",
            &format!("接管已关闭（{label}）：客户端已恢复直连，下一次对话不再经过本应用"),
        );
        // 「还原了哪个文件的哪一段」只有排查时用得上
        debug_append(
            data_dir,
            "uninstall_target",
            &match target_file(region) {
                Some(t) => format!("已从 {} 精确剥离注入段（逐字节还原官方原样）", t.display()),
                None => "客户端产物不存在，没有可剥离的注入段".to_string(),
            },
        );
    }
    let _ = fs::remove_file(lease_path(data_dir));
    Ok(())
}

/// **把两个区域上的注入全部剥掉**（一键网络恢复用）。
///
/// 两个客户端都可能被上一轮接管过，只清一个区域会留下另一个客户端继续指向
/// 已经不存在的本机端口。判定不靠租约，靠**产物里有没有我们的注入段** ——
/// 租约文件丢了也能清干净。
///
/// 返回第一个错误（清理会尽量做完，不会因为一个区域失败就半途而废）。
pub fn uninstall_all(data_dir: &Path) -> Result<(), String> {
    let mut first_err: Option<String> = None;
    for region in Region::ALL {
        if !crate::patch::is_installed(region) {
            continue;
        }
        if let Err(e) = uninstall(region, data_dir) {
            first_err.get_or_insert(e);
        }
    }
    let _ = fs::remove_file(lease_path(data_dir));
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// **清扫僵尸**：启动时调用。
///
/// 上一轮可能是崩溃退出的（`kill -9`、系统重启），此时产物里还留着指向本机的注入，
/// 而反代已经不在 —— 这就是断网现场。判定同样只看「有没有我们的注入段」，
/// 不依赖租约是否还在（租约可能跟进程一起丢了）。
///
/// **两个区域都扫**：上一轮接管的是哪一个不一定还查得到（用户可能中途换过区域），
/// 而残留留在任何一个区域上都会让那个客户端出问题。
pub fn sweep(data_dir: &Path) {
    let lease = load_lease(data_dir);
    for region in Region::ALL {
        if !crate::patch::is_installed(region) {
            continue;
        }
        // 租约指向本区域且心跳还新鲜 → 是本进程装的（或上个进程刚交出去的），留着
        if lease
            .as_ref()
            .is_some_and(|l| l.region == region && is_alive(l))
        {
            continue;
        }
        let _ = uninstall(region, data_dir);
    }
}

/// 给前端展示的接管状态
#[derive(Serialize, Clone, Debug)]
pub struct StealthStatus {
    /// 设置里是否开启
    pub enabled: bool,
    /// 端点是否真的写进 Qoder 配置了
    pub installed: bool,
    /// 租约是否新鲜（心跳还在跳）
    pub alive: bool,
    /// 这份状态描述的是**哪个区域**的接管 —— 界面要据此显示「正在接管：国内版」
    pub region: Region,
    pub port: u16,
    pub url: String,
    /// 人话说明当前状态与下一步该做什么
    pub note: String,
}

/// 综合「设置 + 租约 + 产物里的注入」给出状态。只读，不改动任何东西。
///
/// `region` 由调用方给（正常路径 = `settings.takeover_region`），这样界面在
/// **切换区域但还没保存**时也能预览目标区域的状态，而不是必须先把设置写坏。
pub fn status(region: Region, data_dir: &Path) -> StealthStatus {
    let settings = crate::accounts::load_settings(data_dir);
    let port = settings.proxy_port;
    let url = url_for_port(port);
    let lease = load_lease(data_dir);
    // 租约必须**也属于这个区域**才算「我们的」：另一个区域的旧租约还在心跳期时，
    // 不能让它把当前区域的状态说成「已装载」。
    let alive = lease
        .as_ref()
        .is_some_and(|l| l.region == region && is_alive(l));
    // 「装好了」= 产物里的注入正是我们要的那个端点。官方更新会把注入冲掉，
    // 于是这里立刻变回 false —— 界面如实回落，而不是继续显示「生效中」。
    // 反过来，判据也从不是「我们写过没有」，而是**文件现在是什么样**。
    let installed = current_endpoint(region).as_deref() == Some(url.as_str());
    let target = target_file(region)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（未找到客户端产物）".to_string());

    let label = region.label();
    let note = match (settings.proxy_enabled, installed, alive) {
        (false, _, _) => format!(
            "未开启。开启后 {label} 客户端的对话请求会自动走本机反代（按选定的扣费账号轮换），\
             全程不需要重启或退出 Qoder。"
        ),
        (true, true, true) => format!(
            "接管生效中（{label}）：端点已注入 {target}，本机反代在 {url} 上监听 TLS；\
             客户端的下一次对话就会走这里。"
        ),
        (true, true, false) => {
            format!("注入还在 {target}，但心跳已停 —— 本应用的反代可能已退出，请点「停止接管」清理。")
        }
        (true, false, _) => {
            if region.endpoint_env_key().is_none() {
                format!("{label}的端点覆盖尚未支持：该区域客户端不读这个键。切到国内版再开启接管。")
            } else if let Some(err) = lease
                .as_ref()
                .filter(|l| l.region == region)
                .and_then(|l| l.last_error.as_deref())
            {
                // 注入失败的**真实原因**就在这里，原样端出去。
                // 界面上「接管已开启」而产物没被改，是用户最难自查的一种状态 ——
                // 原因不该只活在监督线程的 stderr 里。
                format!("接管没能生效：注入 {target} 失败。\n{err}")
            } else {
                format!(
                    "正在注入：稍等几秒后刷新；若一直卡在这里，检查 {target} 是否可写。\
                     （官方更新会覆盖这段注入，本应用每次心跳都会自动重打。）"
                )
            }
        }
    };
    // 「接管开着、签到也都正常，只有对话全部 503」只有一个成因：被接管那个区域里
    // 一个账号都没有（路由只在同区域的账号里选）。这句话必须由状态本身说出来，
    // 而不是让用户去翻日志猜 —— 它是**接线之后**第一个会撞上的坑。
    let note = if settings.proxy_enabled
        && !crate::accounts::load_accounts(data_dir)
            .iter()
            .any(|a| a.region == region)
    {
        format!(
            "{note} ⚠️ 当前没有任何{}账号：反代只在同区域账号里选号（一个都没有时对话请求会全部 503），\
             先在「账号」页登录或导入一个该区域的账号。",
            region.label()
        )
    } else {
        note
    };

    StealthStatus {
        enabled: settings.proxy_enabled,
        installed,
        alive,
        region,
        port,
        url,
        note,
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

/// 扣费备选账号的人话名单（用于事件详情）。
/// 全没勾 = 「全部账号（智能轮换）」；有勾 = 逐个列名。
fn billing_account_names(data_dir: &Path, selected: &[String]) -> String {
    if selected.is_empty() {
        return "全部账号（智能轮换）".to_string();
    }
    let accounts = crate::accounts::load_accounts(data_dir);
    let names: Vec<String> = selected
        .iter()
        .map(|id| {
            accounts
                .iter()
                .find(|a| &a.id == id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| id.chars().take(8).collect())
        })
        .collect();
    format!("{}（未选中的不扣费）", names.join("、"))
}

/// 查询接管状态（只读）。区域取自设置里的 `takeover_region` ——
/// 「当前在接管哪一个」本来就是设置的一部分，不该由前端各传一份。
#[tauri::command]
pub fn stealth_status(app: tauri::AppHandle) -> Result<StealthStatus, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    let region = crate::accounts::load_settings(&dir).takeover_region;
    Ok(status(region, &dir))
}

/// 接管通知流（新的在前）：开启 / 关闭 / 这轮对话用了哪个账号 / 限流切换 / 真故障。
///
/// **这里只出对客通知**。请求级细节（收到了哪个路径、鉴权是什么形态、上游回了几多）
/// 走 [`debug_append`] 落 `takeover-debug.log`，界面上一个都不显示 ——
/// 那道过滤在**写入侧**，本函数只对升级前的旧 journal 做一次兜底（见
/// [`DEBUG_ONLY_EVENTS`]）。
///
/// 这里曾有一步 `merge_install_restart`：把「开启接管」与紧随其后的「重启 Qoder」
/// 合并成一条，免得同一个动作在时间线上占两格。切换拓扑不再重启客户端之后，
/// 就再也没有 `restart_qoder` 事件产出了 —— 聚合逻辑随之删除（历史日志里的旧事件
/// 照常单独显示，展示层仍认得它）。
#[tauri::command]
pub fn takeover_events(app: tauri::AppHandle) -> Vec<JournalEvent> {
    let Ok(dir) = crate::commands::try_data_dir(&app) else {
        return Vec::new();
    };
    visible_events(journal_read(&dir))
}

/// 从落盘的 journal 里挑出**给用户看**的那部分（新的在前）。
///
/// 抽成纯函数是为了能在单测里把「技术细节一个都不许漏到界面上」钉死 ——
/// 这道过滤只在**读取侧**对升级前的旧 journal 生效；新写入的调试事件压根不进这个文件。
fn visible_events(events: Vec<JournalEvent>) -> Vec<JournalEvent> {
    let mut out: Vec<JournalEvent> = events
        .into_iter()
        .filter(|e| !DEBUG_ONLY_EVENTS.contains(&e.event.as_str()))
        .collect();
    out.reverse();
    out
}

/// 在文件管理器里定位**接管调试日志**（请求级细节都在那份文件里）。
///
/// 界面上只留对客通知，于是「客户端到底发了什么」必须有地方可查 —— 就是它。
/// 文件不存在时先建一个空的：用户点这个按钮的目的正是「我想看看里面有什么」，
/// 而 `open -R` 对一个不存在的路径会直接报错。
#[tauri::command]
pub fn reveal_debug_log(app: tauri::AppHandle) -> Result<(), String> {
    let dir = crate::commands::try_data_dir(&app)?;
    let path = debug_log_path(&dir);
    if !path.exists() {
        if fs::create_dir_all(&dir).is_err() {
            return Err("无法创建应用数据目录".to_string());
        }
        let _ = fs::write(&path, "");
    }
    reveal_in_file_manager(&path)
}

#[cfg(target_os = "macos")]
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    // `-R` = 在 Finder 里选中该文件（而不是拿某个 App 打开它）
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开日志位置失败：{e}"))
}

#[cfg(target_os = "windows")]
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开日志位置失败：{e}"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    let dir = path.parent().unwrap_or(path);
    std::process::Command::new("xdg-open")
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开日志位置失败：{e}"))
}

/// 清空接管动态（不可恢复）：把**对客通知**那份文件截断为空。
///
/// 调试日志**故意不跟着清** —— 它是排查材料，而界面上这个按钮的本意只是「把看过的
/// 通知划掉」，误点一下就丢掉全部请求级细节是不可接受的代价。它自己在开启接管时重置。
#[tauri::command]
pub fn takeover_events_clear(app: tauri::AppHandle) -> Result<(), String> {
    let dir = crate::commands::try_data_dir(&app)?;
    // 与追加共用一把锁：否则「清空」和并发写入可能交叉，留下半条记录
    let _guard = lock_journal();
    let path = journal_path(&dir);
    if path.exists() {
        fs::write(&path, "").map_err(|e| format!("清空接管动态失败：{e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 注入段里会嵌一份 CA；本测试不握手，用假的即可
    const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIBfakeAAAA\n-----END CERTIFICATE-----\n";
    /// 客户端产物的「官方原版」：注入必须逐字节接在它前面、摘除必须逐字节还原它
    const OFFICIAL: &str =
        "const _$d=(s,k)=>s;\nimport{createRequire as __banner_createRequire}from\"node:module\";\n";

    /// (临时 SDK 根, 数据目录)
    fn sandbox() -> (PathBuf, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "qa-stealth-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let data = base.join("data");
        fs::create_dir_all(&data).unwrap();
        (base.join("sdk"), data)
    }

    fn default_port(data: &Path) -> u16 {
        crate::accounts::load_settings(data).proxy_port
    }

    fn ours(data: &Path) -> String {
        format!("https://127.0.0.1:{}", default_port(data))
    }

    fn official_file(sdk: &Path, region: Region) -> PathBuf {
        sdk.join(region.key())
            .join("dist")
            .join("_worker")
            .join("qoder-worker-runtime.obf.mjs")
    }

    /// 在临时 SDK 根下铺好两个区域的官方产物，并在该根下执行 f。
    ///
    /// 必须用临时根：真去动 `/Applications` 下那份会把用户装好的客户端改坏。
    /// 生产路径永远走 `Region::worker_sdk_root()`，`patch::with_sdk_root` 只在测试里生效。
    fn with_client<T>(sdk: &Path, f: impl FnOnce() -> T) -> T {
        for region in Region::ALL {
            crate::patch::plant_worker(sdk, region, OFFICIAL);
        }
        crate::patch::with_sdk_root(sdk, f)
    }

    #[test]
    fn install_injects_and_uninstall_restores_the_official_bytes() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            assert_eq!(current_endpoint(Region::Cn).as_deref(), Some(ours(&data).as_str()));
            let injected = fs::read_to_string(official_file(&sdk, Region::Cn)).unwrap();
            assert!(injected.ends_with(OFFICIAL), "官方原文必须原样接在注入段之后");
            assert!(injected.contains("QODERCN_SERVER_ENDPOINT"));

            uninstall(Region::Cn, &data).unwrap();
            assert_eq!(current_endpoint(Region::Cn), None, "端点必须被摘掉");
            assert_eq!(
                fs::read_to_string(official_file(&sdk, Region::Cn)).unwrap(),
                OFFICIAL,
                "还原必须逐字节一致"
            );
            assert!(!lease_path(&data).exists(), "租约要一并删除");
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 注入失败的**原因**必须出现在 `status().note` 里。
    ///
    /// 这条守的是最贵的那一课：注入发生在反代监督线程里，失败只进 stderr，用户看不到 ——
    /// 于是「界面说接管已开启、产物却一个字都没改、也没说为什么」就成了一个查无可查的状态
    /// （macOS 的「App 管理」权限就是这样被发现的）。
    #[test]
    fn a_failed_injection_surfaces_its_reason_in_the_status_note() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            // 设置里「接管开着、区域是国内版」，status 才会走「已开启但没装上」那条分支
            let mut s = crate::accounts::load_settings(&data);
            s.proxy_enabled = true;
            s.takeover_region = Region::Cn;
            crate::accounts::save_settings(&data, &s).unwrap();

            // 把产物所在目录设成只读 ⇒ **备份那一步**就写不进去（EACCES = PermissionDenied）
            let dir = official_file(&sdk, Region::Cn).parent().unwrap().to_path_buf();
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(&dir, perms.clone()).unwrap();

            let err = install(Region::Cn, &data, default_port(&data), CA).unwrap_err();
            assert!(err.contains("失败"), "错误得说清是哪一步失败：{err}");

            let st = status(Region::Cn, &data);
            assert!(!st.installed, "产物没被改，状态就不能说装好了");
            assert!(
                st.note.contains("接管没能生效"),
                "失败原因必须端到界面上，不能只活在 stderr：{}",
                st.note
            );

            // 还原，否则临时目录自己都删不干净
            perms.set_readonly(false);
            let _ = fs::set_permissions(&dir, perms);
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 心跳每 5s 一次，参数没变就**不能**重写 33MB 的产物；
    /// 但换了端口要重定向，且不能把旧注入留在文件里。
    #[test]
    fn install_is_idempotent_and_retargets_on_port_change() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            let file = official_file(&sdk, Region::Cn);
            let p = default_port(&data);
            install(Region::Cn, &data, p, CA).unwrap();

            // 探针：参数一字未变时，复查只读文件头，不该整份重写（探针会活下来）
            let before = fs::read_to_string(&file).unwrap();
            fs::write(&file, format!("{before}// heartbeat-probe\n")).unwrap();
            install(Region::Cn, &data, p, CA).unwrap();
            assert!(
                fs::read_to_string(&file).unwrap().contains("heartbeat-probe"),
                "参数没变时不该重写产物"
            );

            install(Region::Cn, &data, 8899, CA).unwrap();
            assert_eq!(current_endpoint(Region::Cn).as_deref(), Some("https://127.0.0.1:8899"));
            assert_eq!(load_lease(&data).unwrap().port, 8899);
            let text = fs::read_to_string(&file).unwrap();
            assert!(!text.contains(&format!("127.0.0.1:{p}")), "旧端点不能残留");
            assert_eq!(text.matches("qoder-assistant-takeover:begin").count(), 1, "注入段不能叠加");
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 官方更新会把整个产物换掉。下一次心跳（install 的早返回路径）必须自动重打。
    #[test]
    fn official_update_is_healed_by_the_next_heartbeat() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            let file = official_file(&sdk, Region::Cn);
            let p = default_port(&data);
            install(Region::Cn, &data, p, CA).unwrap();

            // 模拟官方更新：整份文件被换成新版原版
            let updated = format!("{OFFICIAL}// v1.1.54\n");
            fs::write(&file, &updated).unwrap();
            assert_eq!(current_endpoint(Region::Cn), None, "被覆盖后要立刻如实回落");

            install(Region::Cn, &data, p, CA).unwrap();
            assert_eq!(
                current_endpoint(Region::Cn).as_deref(),
                Some(ours(&data).as_str()),
                "心跳要自动重打注入"
            );
            assert!(fs::read_to_string(&file).unwrap().ends_with(&updated));
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 客户端没装（或布局变了）时必须**明确报错**，而不是静默什么都不做
    #[test]
    fn install_reports_a_missing_client_instead_of_failing_silently() {
        let (sdk, data) = sandbox();
        crate::patch::with_sdk_root(&sdk, || {
            let err = install(Region::Cn, &data, default_port(&data), CA).unwrap_err();
            assert!(err.contains("worker 产物"), "{err}");
            assert!(err.contains("国内版"), "{err}");
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 国际版没有可用的端点键：必须明确拒绝，而不是写一段没人读的注入
    /// （那正是这块历史踩过的坑：写成功、显示已开启、请求全直连官方）
    #[test]
    fn global_region_is_refused_instead_of_silently_injected() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            let err = install(Region::Global, &data, default_port(&data), CA).unwrap_err();
            assert!(err.contains("尚未支持"), "{err}");
            assert_eq!(
                fs::read_to_string(official_file(&sdk, Region::Global)).unwrap(),
                OFFICIAL,
                "失败不能改文件"
            );
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    #[test]
    fn sweep_clears_injections_left_by_a_crash() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            // 模拟崩溃：心跳拨回很久以前，就像应用被 kill -9 后再也没起来
            let mut lease = load_lease(&data).unwrap();
            lease.heartbeat_ms = now_ms() - LEASE_TTL.as_millis() as i64 - 1_000;
            save_lease(&data, &lease).unwrap();
            assert!(!is_alive(&lease), "过期租约必须判定为不存活");

            sweep(&data);
            assert_eq!(current_endpoint(Region::Cn), None, "僵尸注入必须被摘掉");
            assert_eq!(fs::read_to_string(official_file(&sdk, Region::Cn)).unwrap(), OFFICIAL);
            assert!(!lease_path(&data).exists());
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    #[test]
    fn sweep_keeps_a_live_lease() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            sweep(&data);
            assert_eq!(
                current_endpoint(Region::Cn).as_deref(),
                Some(ours(&data).as_str()),
                "心跳新鲜的接管不能被清扫掉"
            );
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 租约丢了（数据目录被清理过）但注入还在 → 孤儿，必须清；
    /// 判定靠「产物里有没有我们的注入段」，不靠租约。
    #[test]
    fn sweep_clears_an_orphan_injection_without_a_lease() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            let _ = fs::remove_file(lease_path(&data));
            assert!(current_endpoint(Region::Cn).is_some());

            sweep(&data);
            assert_eq!(current_endpoint(Region::Cn), None, "无租约的孤儿注入要清掉");
            assert_eq!(fs::read_to_string(official_file(&sdk, Region::Cn)).unwrap(), OFFICIAL);
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 一键恢复要把**两个**客户端都摘干净；同时不能碰没被接管的那个区域的产物。
    #[test]
    fn uninstall_all_detaches_every_injection() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            uninstall_all(&data).unwrap();
            assert_eq!(current_endpoint(Region::Cn), None);
            assert!(!lease_path(&data).exists());
            assert_eq!(
                fs::read_to_string(official_file(&sdk, Region::Global)).unwrap(),
                OFFICIAL,
                "国际版的产物全程不该被动过"
            );
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 装国内版时不能顺手动到国际版的产物 —— 装错文件不会报错，
    /// 只会让界面显示「接管已开启」而实际一个请求都没被接管。
    #[test]
    fn install_never_touches_the_other_regions_file() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            assert_eq!(current_endpoint(Region::Global), None);
            assert_eq!(
                fs::read_to_string(official_file(&sdk, Region::Global)).unwrap(),
                OFFICIAL
            );
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 状态必须**照实**说：注入在不在看文件，而不是看我们「写过没有」。
    #[test]
    fn status_reports_what_the_file_actually_says() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            let mut st = crate::accounts::load_settings(&data);
            st.proxy_enabled = true;
            crate::accounts::save_settings(&data, &st).unwrap();

            let before = status(Region::Cn, &data);
            assert!(!before.installed);

            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            let on = status(Region::Cn, &data);
            assert!(on.installed && on.alive && on.region == Region::Cn);
            assert!(on.note.contains("生效中"), "{}", on.note);

            // 官方更新把注入冲掉 → 状态立即回落，绝不能继续显示「生效中」
            fs::write(official_file(&sdk, Region::Cn), OFFICIAL).unwrap();
            let off = status(Region::Cn, &data);
            assert!(!off.installed, "{}", off.note);

            // 国际版：如实说「不支持」，而不是给一个永远不生效的开关
            let g = status(Region::Global, &data);
            assert!(!g.installed);
            assert!(g.note.contains("尚未支持"), "{}", g.note);
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    // ── 事件日志 ──────────────────────────────────────────────────────────

    #[test]
    fn journal_records_real_changes_only() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            // 没装过就卸载：不该留下事件（噪声会让诊断误判）
            uninstall(Region::Cn, &data).unwrap();
            assert!(journal_read(&data).is_empty());

            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            uninstall(Region::Cn, &data).unwrap();
            let events = journal_read(&data);
            assert_eq!(
                events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
                vec!["install", "uninstall"],
                "只记真实发生的变更：{events:?}"
            );
            // 对客通知说人话：哪个客户端、扣谁的钱
            assert!(events[0].detail.contains("国内版"), "{}", events[0].detail);
            assert!(events[1].detail.contains("国内版"));
            // 而「写到了哪个文件、用的哪个 env 键、端点值是什么」是排查细节 ——
            // 它们必须还在，只是搬到了调试日志里（界面上看不到）。
            let log = fs::read_to_string(debug_log_path(&data)).unwrap();
            assert!(
                log.contains(&format!("127.0.0.1:{}", default_port(&data))),
                "调试日志要留下真实注入的端点：{log}"
            );
            assert!(log.contains("QODERCN_SERVER_ENDPOINT"), "{log}");
            assert!(log.contains("[debug]"), "调试事件要能被 grep 出来：{log}");
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 不设上限：会话内的历史必须一条不丢。
    ///
    /// 旧实现只留 200 条，而 `proxy_request` 是**每个模型请求一条**、界面又不显示它，
    /// 于是「看得见的事件」会被它成批挤出去 —— 用户看到的就是「接管动态自己清空了」。
    #[test]
    fn journal_keeps_every_entry_without_a_cap() {
        let (_sdk, data) = sandbox();
        let n = 512;
        for i in 0..n {
            journal_append(&data, "install", &format!("e{i}"));
        }
        let all = journal_read(&data);
        assert_eq!(all.len(), n, "不应再有任何裁剪");
        assert_eq!(all[0].detail, "e0", "最旧的必须还在，且顺序不变");
        assert_eq!(all[n - 1].detail, format!("e{}", n - 1));

        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    /// 分级契约：调试事件**只进日志文件**，界面时间线一个都不许出现。
    ///
    /// 这条曾经真的坏过：`proxy_auth` 原先走的是对客通道，于是接管动态整屏都是
    /// 「反代收到 POST /algo/…（鉴权：Bearer COSY.eyJ…）」，而用户真正要看的
    /// 「这轮对话扣的是哪个账号」被淹在中间、一条都没有。
    #[test]
    fn debug_events_never_reach_the_ui_feed() {
        let (_sdk, data) = sandbox();
        journal_append(&data, "install", "接管已开启（国内版）");
        debug_append(&data, "proxy_auth", "收到 POST /algo/xxx | 鉴权 Bearer COSY.eyJ…");
        debug_append(&data, "proxy_conn_closed", "客户端连上后未发数据即断开");

        let raw = journal_read(&data);
        assert_eq!(
            raw.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["install"],
            "调试事件不该写进对客 journal：{raw:?}"
        );

        // 两份都要在调试日志里 —— 排查时看一个文件就够，不必两份对照
        let log = fs::read_to_string(debug_log_path(&data)).unwrap();
        assert!(log.contains("proxy_auth"), "{log}");
        assert!(log.contains("proxy_conn_closed"), "{log}");
        assert!(log.contains("[user ] install"), "对客通知要带受众标记：{log}");
        assert!(log.contains("[debug] proxy_auth"), "调试事件要带受众标记：{log}");

        // 读取侧还要兜住**升级前**那份 journal（里面混着 proxy_auth）
        fs::write(
            journal_path(&data),
            concat!(
                r#"{"at_ms":1,"at":"","event":"install","detail":"开"}"#,
                "\n",
                r#"{"at_ms":2,"at":"","event":"proxy_auth","detail":"旧版混进来的细节"}"#,
                "\n",
                r#"{"at_ms":3,"at":"","event":"session_start","detail":"本次对话由账号「A」提供"}"#,
                "\n",
            ),
        )
        .unwrap();
        let shown: Vec<String> = visible_events(journal_read(&data))
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(
            shown,
            vec!["session_start", "install"],
            "界面只看对客通知，且新的在前"
        );

        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    /// 调试日志是**一行一条**：detail 里的换行必须转义，否则 `tail` / `grep` 全乱
    /// （坏请求那条要附收到的头部前缀，一定带换行）。
    #[test]
    fn debug_log_stays_one_line_per_entry() {
        let (_sdk, data) = sandbox();
        debug_append(&data, "proxy_bad_request", "第一行\n第二行");
        let log = fs::read_to_string(debug_log_path(&data)).unwrap();
        assert_eq!(log.lines().count(), 1, "一条记录只能占一行：{log}");
        assert!(log.contains(r"第一行\n第二行"), "{log}");
        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    /// 开启接管 = 新的一轮会话：历史整体重置，文件里只剩这一条 install。
    #[test]
    fn install_starts_a_fresh_journal() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            journal_append(&data, "route_start", "上一轮：使用账号 A");
            journal_append(&data, "uninstall", "上一轮：接管已关闭");
            assert_eq!(journal_read(&data).len(), 2);

            install(Region::Cn, &data, default_port(&data), CA).unwrap();
            let events = journal_read(&data);
            assert_eq!(events.len(), 1, "开启接管应清空历史：{events:?}");
            assert_eq!(events[0].event, "install");
            assert!(
                events[0].detail.contains("已清空上一轮动态 2 条"),
                "首条事件要说明清掉了什么：{}",
                events[0].detail
            );
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 幂等 install（端口没变）不是新会话，不能把本轮会话里的记录抹掉。
    /// 应用重启得足够快时租约还新鲜，走的就是这条早返回路径。
    #[test]
    fn idempotent_reinstall_keeps_the_current_session() {
        let (sdk, data) = sandbox();
        with_client(&sdk, || {
            let p = default_port(&data);
            install(Region::Cn, &data, p, CA).unwrap();
            journal_append(&data, "route_start", "本轮：使用账号 A");
            install(Region::Cn, &data, p, CA).unwrap();
            let events = journal_read(&data);
            assert_eq!(
                events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
                vec!["install", "route_start"],
                "重复 install 不该重置日志：{events:?}"
            );
        });
        let _ = fs::remove_dir_all(sdk.parent().unwrap());
    }

    /// 纯追加下，半条记录（写在途中 / 崩在半路）不算一条。
    /// 而且它必须被隔开——否则**下一条**会粘在它后面一起变成坏行、一起丢掉。
    #[test]
    fn journal_tolerates_a_half_written_tail() {
        let (_sdk, data) = sandbox();
        journal_append(&data, "install", "完整的一条");
        let path = journal_path(&data);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"at_ms\":1,\"at\":\"\",\"event\":\"inst");
        fs::write(&path, text).unwrap();

        assert_eq!(journal_read(&data).len(), 1, "没写完的那条不进时间线");

        journal_append(&data, "route_start", "后续照常追加");
        let all = journal_read(&data);
        assert_eq!(all.len(), 2, "残句不该吞掉后来的记录：{all:?}");
        assert_eq!(all[1].detail, "后续照常追加");

        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    /// 回归：日志是 `read-modify-write`，多线程并发追加若不串行化就会互相覆盖。
    /// 这直接对应线上场景 —— 代理每个连接一个线程，出故障时恰恰是并发最高的时候。
    #[test]
    fn journal_loses_nothing_under_concurrency() {
        use std::collections::BTreeSet;
        use std::sync::Arc;

        let (_sdk, data) = sandbox();
        let data = Arc::new(data);
        let threads = 8usize;
        // 取消上限后没有「裁剪边界」可卡了，这里纯粹验证并发追加一条不丢
        let per_thread = 40usize;

        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let dir = Arc::clone(&data);
                std::thread::spawn(move || {
                    for i in 0..per_thread {
                        journal_append(dir.as_path(), "proxy_request", &format!("t{t}-i{i}"));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("追加线程 panic");
        }

        let all = journal_read(&data);
        let expected: BTreeSet<String> = (0..threads)
            .flat_map(|t| (0..per_thread).map(move |i| format!("t{t}-i{i}")))
            .collect();
        let actual: BTreeSet<String> = all.iter().map(|e| e.detail.clone()).collect();
        assert_eq!(actual, expected, "并发追加丢事件（缺条目见上方集合差异）");
        assert_eq!(all.len(), threads * per_thread, "并发追加出现重复条目");
        assert!(all.iter().all(|e| e.event == "proxy_request"), "事件名被串改");

        let _ = fs::remove_dir_all(data.parent().unwrap());
    }
}
