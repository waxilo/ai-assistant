//! 智能接管的**保险丝**：把「自定义端点」写进 Qoder 全局配置，并保证它一定能被取下来。
//!
//! # 为什么必须独立成模块
//!
//! 智能接管 = 让官方客户端的所有对话请求自动走本应用的反代。实现上只能是
//! 往**被接管那个区域**的 CLI 配置里写 `env.CODEBUDDY_BASE_URL` —— 而这**正是**
//! 历史上把整个 Qoder 搞到 502 断网的那个动作（`netfix.rs` 的清理目标）。
//!
//! # 区域（[`Region`]）
//!
//! 两套部署各有自己的 `settings.json`（`~/.qoder` / `~/.qoder-cn`），所以本模块的
//! **每一个入口都必须带区域**：装错目录的后果不是报错，而是「界面显示接管已开启、
//! 实际一个请求都没被接管」。租约也把区域记下来 —— 卸载 / 清扫时手里只有租约，
//! 而「该动哪个目录」正是它决定的。
//!
//! 所以「能装」不是本事，「保证一定能卸」才是。本模块用**租约 + 心跳**把这件事钉死：
//!
//! - 装之前先把原值存进租约，卸载时还原（不新增也不吞掉用户原有的值）；
//! - 反代活着就持续心跳；心跳停了（应用崩了 / 被 kill -9 / 端口没了）租约即过期；
//! - 应用启动时先 `sweep`：租约过期 = 僵尸，直接卸掉，绝不让上次崩溃留下断网残留。
//!
//! # 与 netfix 的关系
//!
//! `netfix` 扫到 `env.CODEBUDDY_BASE_URL` 时会来问本模块「这是自己人吗」：
//! 租约新鲜 → 正常工作中，不算问题；租约过期 → 僵尸残留，照常判为会断网。
//! 这样「一键恢复」清掉的是真残留，不会把正在工作的接管误伤掉。
//! 它同样是**逐区域**问的：两个区域各可能装一份，一个是工作中的、另一个是残留。

use crate::region::Region;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// 心跳间隔：反代监督线程按此频率续租
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// 超过这么久没有心跳，就认定接管方已经不在了
pub const LEASE_TTL: Duration = Duration::from_secs(30);

// 这里曾有 `pub const ENV_KEY = "CODEBUDDY_BASE_URL"`。变量名现在由
// [`Region::takeover_env_key`] 给出（两个区域目前共用同一个键，共用理由与那条
// 不确定性都写在它的注释里），本模块只消费、不再自己持有一份 ——
// 两处各写一份就等于「区域差异」又多了一个会静默漂移的副本。

const LEASE_FILE: &str = "stealth.json";

/// 接管事件日志（追加式 JSONL）。Qoder 的长驻 CLI host 会把 settings.env
/// 注入 process.env，但删除配置键时不会清掉旧值。日志记录 install / proxy_request /
/// uninstall / restart，用于判断端点已摘除后是否仍存在缓存旧地址的 CLI host。
const JOURNAL_FILE: &str = "takeover-journal.jsonl";

/// 一条接管事件
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalEvent {
    pub at_ms: i64,
    /// 本地时间（展示用）
    pub at: String,
    /// install / uninstall / restart_qoder
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

/// 追加一条事件。失败一律静默：日志是诊断辅助，绝不能反过来影响接管本身。
///
/// **不设条数上限**。曾经的「只留最近 200 条」会让 `proxy_request` 这类高频内部证据
/// 把真正要看的事件挤出窗口，表现成「接管动态自己清空了」。改为把日志与**一次接管
/// 会话**绑定：开启接管时整份重置（见 [`journal_append_reset`]），会话之内一条不丢。
pub fn journal_append(data_dir: &Path, event: &str, detail: &str) {
    write_events(data_dir, &[make_event(event, detail)], false);
}

/// 同 [`journal_append`]，但**先丢弃全部历史**。
///
/// 用在「开启接管」这一刻：日志描述的就是本轮会话，上一轮的话题已经结束。
pub fn journal_append_reset(data_dir: &Path, event: &str, detail: &str) {
    write_events(data_dir, &[make_event(event, detail)], true);
}

/// 落盘。`truncate = true` 先清空历史，否则纯追加。
///
/// 用纯追加而非「读全量 → 改 → 整体重写」：取消上限之后，后者每次追加的代价随文件
/// 长度线性增长（还附带一遍全量 JSON 解析），长会话会把每个反代请求越拖越慢。追加是
/// O(1)，顺带还绕开了 Windows 上 `rename` 会因文件被展示层占用而失败的问题。
fn write_events(data_dir: &Path, events: &[JournalEvent], truncate: bool) {
    let body: String = events
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .map(|l| format!("{l}\n"))
        .collect();
    if body.is_empty() {
        return;
    }
    let _guard = lock_journal();
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
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Lease {
    /// **这个租约装在哪个区域上**。卸载 / 清扫时手里只有租约，而「该动哪个
    /// `settings.json`」只有它知道，所以区域必须随租约落盘。
    /// 老租约没有这个字段 → `#[serde(default)]` 落成国际版（那个字段出现之前只有国际版）。
    #[serde(default)]
    pub region: Region,
    /// 写进配置的端点值，如 `http://127.0.0.1:8789`
    pub url: String,
    pub port: u16,
    /// 装载方进程号（仅用于展示与自查，不作为存活判据——PID 会被复用）
    pub pid: u32,
    /// 被覆盖前的原值；原本没有就是 None，卸载时把键整个删掉
    pub previous: Option<String>,
    pub installed_at_ms: i64,
    /// 最近一次心跳（毫秒）。存活判据只看这个。
    pub heartbeat_ms: i64,
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

/// 这个值是不是**当前这个端口**的接管端点
fn url_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// 装卸的目标配置文件：**被接管那个区域**的 CLI 配置
/// （`~/.qoder/settings.json` / `~/.qoder-cn/settings.json`）。
///
/// 只动 `region.cli_dir_name()` 给的那个目录，不碰 `.codebuddy`（更早一代的旧目录）。
/// `home` 仍然作为参数显式传入（而不是在函数里 `dirs::home_dir()`）：测试要指向
/// 一个临时家目录，而「能不能被测」正是这类写别人配置的代码最需要的属性。
fn target_config(home: &Path, region: Region) -> PathBuf {
    home.join(region.cli_dir_name()).join("settings.json")
}

/// 把一个值写进 `settings.json` 的 `env.<region.takeover_env_key()>`。
/// `value` 为 None 表示删除该键（`env` 空了就连 `env` 一起删）。
///
/// 用 `Value` 精确改而不是整份重写：那份文件里有 `sandbox` / `claw` / `enabledPlugins`
/// 等我们不认识的键，绝不能顺手清掉。
fn set_endpoint(path: &Path, region: Region, value: Option<&str>) -> Result<(), String> {
    let key = region.takeover_env_key();
    let text = if path.exists() {
        fs::read_to_string(path).map_err(|e| format!("读取配置失败：{e}"))?
    } else {
        // 文件不存在时从空对象开始——只在用户机器上确实没有该文件的极端情况下发生
        "{}".to_string()
    };
    let mut v: Value = if text.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&text).map_err(|e| format!("配置不是合法 JSON，未改动：{e}"))?
    };
    let Some(obj) = v.as_object_mut() else {
        return Err("配置顶层不是对象，未改动。".into());
    };

    match value {
        Some(val) => {
            let env = obj
                .entry("env")
                .or_insert_with(|| Value::Object(Default::default()));
            let env = env
                .as_object_mut()
                .ok_or_else(|| "配置里的 env 不是对象，未改动。".to_string())?;
            env.insert(key.to_string(), Value::String(val.to_string()));
        }
        None => {
            let empty = match obj.get_mut("env").and_then(Value::as_object_mut) {
                Some(env) => {
                    env.remove(key);
                    env.is_empty()
                }
                None => false,
            };
            if empty {
                obj.remove("env");
            }
        }
    }

    let out = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化失败：{e}"))?;
    crate::netfix::write_atomic(path, &format!("{out}\n")).map_err(|e| format!("写入失败：{e}"))
}

/// 读取**指定区域**的配置里当前的端点值（另一个区域装了什么都不影响这个答案）
pub fn current_endpoint(home: &Path, region: Region) -> Option<String> {
    let text = fs::read_to_string(target_config(home, region)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("env")
        .and_then(Value::as_object)
        .and_then(|e| e.get(region.takeover_env_key()))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// **装**：把端点写进**指定区域**的 CLI 配置，并落下租约。
///
/// 幂等：已装着、且区域与端口都一致 → 只续一次心跳，不重复写文件。
///
/// 区域不一致时**先按旧租约卸干净**再装新的：换区域等于换一个客户端接管，
/// 旧目录上那份配置必须一起摘掉，否则那个客户端会一直指向本机端口，
/// 而我们这边已经不为它服务了。
pub fn install(home: &Path, region: Region, data_dir: &Path, port: u16) -> Result<(), String> {
    let url = url_for_port(port);

    if let Some(mut lease) = load_lease(data_dir) {
        if lease.region == region && lease.port == port && lease.url == url {
            lease.heartbeat_ms = now_ms();
            let _ = save_lease(data_dir, &lease);
            // 值可能被别的程序改过，这里保证它仍是我们期望的那个
            if current_endpoint(home, region).as_deref() != Some(url.as_str()) {
                set_endpoint(target_config(home, region).as_path(), region, Some(&url))?;
            }
            return Ok(());
        }
        // 区域或端口变了：先按旧租约卸干净，再装新的
        let _ = uninstall(home, lease.region, data_dir);
    }

    let previous = current_endpoint(home, region);
    let lease = Lease {
        region,
        url: url.clone(),
        port,
        pid: std::process::id(),
        previous,
        installed_at_ms: now_ms(),
        heartbeat_ms: now_ms(),
    };
    // 先落租约再改配置：中途崩了也留有记录，sweep 能收尾
    save_lease(data_dir, &lease).map_err(|e| format!("写入租约失败：{e}"))?;
    set_endpoint(target_config(home, region).as_path(), region, Some(&url))?;
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
            "接管已开启（{}）：端点写入 {} 的 env.{}={url}；扣费备选：{names}{note}",
            region.label(),
            target_config(home, region).display(),
            region.takeover_env_key()
        ),
    );
    Ok(())
}

/// **卸**：把端点从配置里摘掉（还原成装载前的值），删掉租约。
///
/// 安全起见：只有当配置里的值**仍是我们写进去的那个**才动手；
/// 被别人改过就不碰，避免把用户的配置改坏。
pub fn uninstall(home: &Path, region: Region, data_dir: &Path) -> Result<(), String> {
    uninstall_with_note(home, region, data_dir, None)
}

/// 同 [`uninstall`]，但在关闭事件里追加一句备注（如「已重启 Qoder 清除长驻
/// CLI host 环境」），避免同一动作拆成两条同秒事件刷屏。
pub fn uninstall_with_note(
    home: &Path,
    region: Region,
    data_dir: &Path,
    note: Option<&str>,
) -> Result<(), String> {
    let lease = load_lease(data_dir);
    let path = target_config(home, region);
    let label = region.label();
    if !path.exists() {
        let _ = fs::remove_file(lease_path(data_dir));
        return Ok(());
    }

    let current = current_endpoint(home, region);
    let mut changed = false;
    match (&lease, &current) {
        // 租约确实装在这个区域、且值还是我们写进去的 → 正常路径：还原成装载前的值
        (Some(lease), Some(cur)) if lease.region == region && cur == &lease.url => {
            match lease.previous.as_deref() {
                Some(prev) => set_endpoint(&path, region, Some(prev))?,
                None => set_endpoint(&path, region, None)?,
            }
            changed = true;
        }
        // 没租约、或值已被改动：只在我们能确认端口归属时才清。
        // 端口是**全局唯一**的（整个应用只有一个反代端口），所以「值等于我们的端口」
        // 就足以判定这条是我们写的 —— 不看租约是为了兜住「租约文件被删了」那种残留。
        (_, Some(cur))
            if *cur == url_for_port(crate::accounts::load_settings(data_dir).proxy_port) =>
        {
            set_endpoint(&path, region, None)?;
            changed = true;
        }
        _ => {}
    }
    if changed {
        let base = format!("接管已关闭（{label}）：端点已从该区域的配置摘除，客户端恢复直连");
        let detail = match note {
            Some(n) => format!("{base}；{n}"),
            None => base,
        };
        journal_append(data_dir, "uninstall", &detail);
    }
    let _ = fs::remove_file(lease_path(data_dir));
    Ok(())
}

/// **把两个区域上「我们装的」端点全部摘掉**（一键网络恢复用）。
///
/// 先按租约走正常卸载（能把值还原成装载前的样子），再把其余区域上「值等于我们自己
/// 反代端口」的孤儿残留一并清掉。两个客户端都可能被上一轮接管过，只清一个区域
/// 会留下另一个客户端继续指向已经不存在的本机端口。
///
/// 返回第一个错误（清理会尽量做完，不会因为一个区域失败就半途而废）。
pub fn uninstall_all(home: &Path, data_dir: &Path) -> Result<(), String> {
    let lease_region = load_lease(data_dir).map(|l| l.region);
    let mut first_err: Option<String> = None;
    if let Some(region) = lease_region {
        if let Err(e) = uninstall(home, region, data_dir) {
            first_err = Some(e);
        }
    }
    let ours = url_for_port(crate::accounts::load_settings(data_dir).proxy_port);
    for region in Region::ALL {
        if Some(region) == lease_region {
            continue;
        }
        if current_endpoint(home, region).as_deref() == Some(ours.as_str()) {
            if let Err(e) = set_endpoint(&target_config(home, region), region, None) {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// **清扫僵尸**：启动时调用。
/// 上一轮可能是崩溃退出的（`kill -9`、系统重启、端口被抢），此时配置里还留着
/// 指向本机的端点而反代已经不在 —— 这就是断网现场。一律卸掉。
///
/// **两个区域都扫**：上一轮接管的是哪一个不一定还查得到（租约可能一起丢了，
/// 或者用户中途换过区域），而残留留在任何一个区域上都会让那个客户端断网。
/// 判据统一是「值等于我们自己的反代端口」—— 端口全局唯一，所以不会误伤
/// 另一个应用（如 workbuddy-assistant）写在同一个文件里的端点。
pub fn sweep(home: &Path, data_dir: &Path) {
    let port = crate::accounts::load_settings(data_dir).proxy_port;
    let ours = url_for_port(port);
    let lease = load_lease(data_dir);
    for region in Region::ALL {
        // 租约指向本区域：按死活判断，死了就按正常路径卸载（能还原用户原值）
        if lease.as_ref().is_some_and(|l| l.region == region) {
            let live = lease.as_ref().is_some_and(is_alive);
            if !live {
                let _ = uninstall(home, region, data_dir);
            }
            continue;
        }
        // 没租约（或租约记的是别的区域）但配置里却指向我们的端口 → 孤儿残留，直接摘
        if current_endpoint(home, region).as_deref() == Some(ours.as_str()) {
            let _ = set_endpoint(&target_config(home, region), region, None);
        }
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

/// 综合「设置 + 租约 + 配置实际值」给出状态。只读，不改动任何东西。
///
/// `region` 由调用方给（正常路径 = `settings.takeover_region`），这样界面在
/// **切换区域但还没保存**时也能预览目标区域的状态，而不是必须先把设置写坏。
pub fn status(home: &Path, region: Region, data_dir: &Path) -> StealthStatus {
    let settings = crate::accounts::load_settings(data_dir);
    let port = settings.proxy_port;
    let url = url_for_port(port);
    let lease = load_lease(data_dir);
    // 租约必须**也属于这个区域**才算「我们的」：另一个区域的旧租约还在心跳期时，
    // 不能让它把当前区域的状态说成「已装载」。
    let alive = lease
        .as_ref()
        .is_some_and(|l| l.region == region && is_alive(l));
    let installed = current_endpoint(home, region).as_deref() == Some(url.as_str());

    let label = region.label();
    let note = match (settings.proxy_enabled, installed, alive) {
        (false, _, _) => format!(
            "未开启。开启后 {label} 客户端的对话请求会自动走本应用反代，按最旧积分优先选账号。"
        ),
        (true, true, true) => format!(
            "接管生效中（{label}）。模型请求经本机代理分流；启停或换区域会自动安全重启长驻 CLI host。"
        ),
        (true, true, false) => "配置里有端点，但心跳已停 —— 接管进程可能已退出，请点「停止接管」清理。".into(),
        (true, false, _) => format!(
            "正在装载：稍等几秒后刷新；若一直卡在这里，检查 {} 是否可写。",
            target_config(home, region).display()
        ),
    };
    // 「接管开着、签到也都正常，只有对话全部 503」只有一个成因：被接管那个区域里
    // 一个账号都没有（路由只在同区域的账号里选）。这句话必须由状态本身说出来，
    // 而不是让用户去翻日志猜。
    let note = if settings.proxy_enabled
        && !crate::accounts::load_accounts(data_dir)
            .iter()
            .any(|a| a.region == region)
    {
        format!(
            "{note} ⚠️ 当前没有任何{}账号：接管时对话请求会全部 503，\
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

fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "无法定位家目录，无法读写 Qoder 全局配置。".to_string())
}

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
    Ok(status(&home_dir()?, region, &dir))
}

/// 接管事件流（新的在前）：开启 / 关闭 / 开始使用账号 / 重启 / 错误。
/// `proxy_request` 是网络救急判定「端点确实被用过」的内部证据，账号/会话信息
/// 已由「开始使用账号」事件承载，展示层过滤掉避免重复刷屏。
#[tauri::command]
pub fn takeover_events(app: tauri::AppHandle) -> Vec<JournalEvent> {
    let Ok(dir) = crate::commands::try_data_dir(&app) else {
        return Vec::new();
    };
    let all = journal_read(&dir)
        .into_iter()
        .filter(|e| e.event != "proxy_request")
        .collect::<Vec<_>>();
    let merged = merge_install_restart(all);
    let mut out = merged;
    out.reverse();
    out
}

/// 展示层聚合：开启接管后如果同秒（≤1s）紧跟着一条「重启 Qoder」，
/// 把后者合并进开启事件的 detail，避免同一动作拆成两条刷屏。
///
/// 注意：这里正向处理（时间从早到晚），因为 restart 一定发生在 install 之后。
fn merge_install_restart(events: Vec<JournalEvent>) -> Vec<JournalEvent> {
    if events.len() < 2 {
        return events;
    }
    let mut out = Vec::with_capacity(events.len());
    let mut i = 0;
    while i < events.len() {
        let mut cur = events[i].clone();
        if cur.event == "install"
            && i + 1 < events.len()
            && events[i + 1].event == "restart_qoder"
            && events[i + 1].at_ms.saturating_sub(cur.at_ms) <= 1000
        {
            let restart_detail = &events[i + 1].detail;
            if !restart_detail.is_empty() {
                cur.detail = format!("{}；{}", cur.detail, restart_detail);
            }
            out.push(cur);
            i += 2;
        } else {
            out.push(cur);
            i += 1;
        }
    }
    out
}

/// 清空接管动态（不可恢复）：把事件日志文件截断为空。
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

    fn sandbox() -> (PathBuf, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "wb-stealth-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let home = base.join("home");
        let data = base.join("data");
        // 两套部署的目录都建出来：区域化之后测试要在两边都能落文件
        for region in Region::ALL {
            fs::create_dir_all(home.join(region.cli_dir_name())).unwrap();
        }
        fs::create_dir_all(&data).unwrap();
        (home, data)
    }

    /// 往**国际版**的配置里写内容（绝大多数用例只关心一套部署）。
    fn write_cfg(home: &Path, json: &str) {
        write_cfg_in(home, Region::Global, json);
    }

    fn write_cfg_in(home: &Path, region: Region, json: &str) {
        fs::write(target_config(home, region), json).unwrap();
    }

    #[test]
    fn install_writes_endpoint_and_uninstall_restores_previous() {
        let (home, data) = sandbox();
        write_cfg(&home, r#"{"sandbox":{"a":1},"env":{"OTHER":"keep"}}"#);

        install(&home, Region::Global, &data, 8787).unwrap();
        assert_eq!(current_endpoint(&home, Region::Global).as_deref(), Some("http://127.0.0.1:8787"));
        let text = fs::read_to_string(target_config(&home, Region::Global)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["sandbox"]["a"], Value::from(1), "不认识的键必须原样保留");
        assert_eq!(v["env"]["OTHER"], Value::from("keep"));

        uninstall(&home, Region::Global, &data).unwrap();
        assert_eq!(current_endpoint(&home, Region::Global), None, "端点必须被摘掉");
        let v2: Value = serde_json::from_str(&fs::read_to_string(target_config(&home, Region::Global)).unwrap()).unwrap();
        assert!(v2["env"].get("CODEBUDDY_BASE_URL").is_none());
        // env 里还有用户的 OTHER，所以它不能整个消失——只能摘掉我们那一键
        assert_eq!(v2["env"]["OTHER"], Value::from("keep"));
        assert!(!lease_path(&data).exists(), "租约要一并删除");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn uninstall_restores_an_existing_user_value() {
        let (home, data) = sandbox();
        write_cfg(&home, r#"{"env":{"CODEBUDDY_BASE_URL":"https://my.own.gateway"}}"#);

        install(&home, Region::Global, &data, 8787).unwrap();
        uninstall(&home, Region::Global, &data).unwrap();
        assert_eq!(
            current_endpoint(&home, Region::Global).as_deref(),
            Some("https://my.own.gateway"),
            "卸载必须还原成用户原本的值，而不是删掉"
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn sweep_clears_stale_lease_left_by_a_crash() {
        let (home, data) = sandbox();
        install(&home, Region::Global, &data, 8787).unwrap();
        assert_eq!(current_endpoint(&home, Region::Global).as_deref(), Some("http://127.0.0.1:8787"));

        // 模拟崩溃：把心跳拨回很久以前，就像应用被 kill -9 后再也没起来
        let mut lease = load_lease(&data).unwrap();
        lease.heartbeat_ms = now_ms() - LEASE_TTL.as_millis() as i64 - 1_000;
        save_lease(&data, &lease).unwrap();
        assert!(!is_alive(&lease), "过期租约必须判定为不存活");

        sweep(&home, &data);
        assert_eq!(current_endpoint(&home, Region::Global), None, "僵尸端点必须被清掉");
        assert!(!lease_path(&data).exists());

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn sweep_keeps_a_live_lease() {
        let (home, data) = sandbox();
        install(&home, Region::Global, &data, 8787).unwrap();
        sweep(&home, &data);
        assert_eq!(
            current_endpoint(&home, Region::Global).as_deref(),
            Some("http://127.0.0.1:8787"),
            "心跳新鲜的接管不能被清扫掉"
        );
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn sweep_clears_orphan_endpoint_without_lease() {
        let (home, data) = sandbox();
        // 端口跟着默认值走，别写死 —— 否则下次改默认端口，这条测试会「失败得很有道理」
        // 却让人以为是代码坏了（8787 → 8789 时就是这样）。
        let port = crate::accounts::load_settings(&data).proxy_port;
        // 租约文件丢了（例如数据目录被清理过），但配置里还留着端点
        write_cfg(
            &home,
            &format!(r#"{{"env":{{"CODEBUDDY_BASE_URL":"http://127.0.0.1:{port}"}}}}"#),
        );

        sweep(&home, &data);
        assert_eq!(current_endpoint(&home, Region::Global), None, "无租约的孤儿端点要清掉");

        // 反过来：**别人**的端口不能被我们顺手清掉。
        // 8787 是同机 workbuddy-assistant 的反代端口，清它等于把另一个应用的接管打断。
        let others = if port == 8787 { 8788 } else { 8787 };
        write_cfg(
            &home,
            &format!(r#"{{"env":{{"CODEBUDDY_BASE_URL":"http://127.0.0.1:{others}"}}}}"#),
        );
        sweep(&home, &data);
        assert_eq!(
            current_endpoint(&home, Region::Global).as_deref(),
            Some(format!("http://127.0.0.1:{others}").as_str()),
            "别人的端口不是我们的残留，不该动"
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn install_is_idempotent_and_retargets_on_port_change() {
        let (home, data) = sandbox();
        install(&home, Region::Global, &data, 8787).unwrap();
        install(&home, Region::Global, &data, 8787).unwrap();
        assert_eq!(current_endpoint(&home, Region::Global).as_deref(), Some("http://127.0.0.1:8787"));

        install(&home, Region::Global, &data, 8899).unwrap();
        assert_eq!(current_endpoint(&home, Region::Global).as_deref(), Some("http://127.0.0.1:8899"));
        assert_eq!(load_lease(&data).unwrap().port, 8899);

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn installing_twice_does_not_lose_the_original_value() {
        // 连续换端口时，previous 必须是「最初的」那个值，而不是上一次我们写进去的
        let (home, data) = sandbox();
        write_cfg(&home, r#"{"env":{"CODEBUDDY_BASE_URL":"https://original"}}"#);
        install(&home, Region::Global, &data, 8787).unwrap();
        install(&home, Region::Global, &data, 8899).unwrap();
        uninstall(&home, Region::Global, &data).unwrap();
        assert_eq!(current_endpoint(&home, Region::Global).as_deref(), Some("https://original"));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn refuses_to_touch_unparsable_config() {
        let (home, data) = sandbox();
        write_cfg(&home, "{not json");
        assert!(install(&home, Region::Global, &data, 8787).is_err());
        assert_eq!(fs::read_to_string(target_config(&home, Region::Global)).unwrap(), "{not json");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn journal_records_real_changes_only() {
        let (home, data) = sandbox();
        // 没装过就卸载：不该留下事件（噪声会让诊断误判）
        uninstall(&home, Region::Global, &data).unwrap();
        assert!(journal_read(&data).is_empty());

        install(&home, Region::Global, &data, 8787).unwrap();
        uninstall(&home, Region::Global, &data).unwrap();
        let events = journal_read(&data);
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["install", "uninstall"],
            "只记真实发生的变更：{:?}",
            events
        );
        assert!(events[0].detail.contains("8787"));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn merge_install_restart_combines_same_second_events() {
        let mk = |ms: i64, event: &str| JournalEvent {
            at_ms: ms,
            at: String::new(),
            event: event.to_string(),
            detail: if event == "install" {
                "接管已开启：端点写入 env.CODEBUDDY_BASE_URL=http://127.0.0.1:8787".to_string()
            } else {
                "Qoder 已重启；长驻 CLI host 终止 1 个".to_string()
            },
        };
        // 开启后 200ms 紧接重启：应合并为一条 install，且 detail 带上重启信息
        let merged = merge_install_restart(vec![mk(1_000, "install"), mk(1_200, "restart_qoder")]);
        assert_eq!(merged.len(), 1, "两条同秒事件应合并为一条");
        assert_eq!(merged[0].event, "install");
        assert!(merged[0].detail.contains("已重启"), "detail 应含重启信息：{}", merged[0].detail);
        assert!(merged[0].detail.contains("终止 1 个"));

        // 间隔超过 1s：不合并（可能是用户隔了很久手动重启）
        let kept = merge_install_restart(vec![mk(1_000, "install"), mk(3_000, "restart_qoder")]);
        assert_eq!(kept.len(), 2, "非同秒不应合并");

        // 顺序颠倒（restart 在 install 前）：不合并，保持原样
        let reordered = merge_install_restart(vec![mk(1_000, "restart_qoder"), mk(1_200, "install")]);
        assert_eq!(reordered.len(), 2);
    }

    /// 不设上限：会话内的历史必须一条不丢。
    ///
    /// 旧实现只留 200 条，而 `proxy_request` 是**每个模型请求一条**、界面又不显示它，
    /// 于是「看得见的事件」会被它成批挤出去 —— 用户看到的就是「接管动态自己清空了」。
    #[test]
    fn journal_keeps_every_entry_without_a_cap() {
        let (home, data) = sandbox();
        let n = 512;
        for i in 0..n {
            journal_append(&data, "install", &format!("e{i}"));
        }
        let all = journal_read(&data);
        assert_eq!(all.len(), n, "不应再有任何裁剪");
        assert_eq!(all[0].detail, "e0", "最旧的必须还在，且顺序不变");
        assert_eq!(all[n - 1].detail, format!("e{}", n - 1));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 开启接管 = 新的一轮会话：历史整体重置，文件里只剩这一条 install。
    #[test]
    fn install_starts_a_fresh_journal() {
        let (home, data) = sandbox();
        journal_append(&data, "route_start", "上一轮：使用账号 A");
        journal_append(&data, "uninstall", "上一轮：接管已关闭");
        assert_eq!(journal_read(&data).len(), 2);

        install(&home, Region::Global, &data, 8787).unwrap();
        let events = journal_read(&data);
        assert_eq!(events.len(), 1, "开启接管应清空历史：{events:?}");
        assert_eq!(events[0].event, "install");
        assert!(
            events[0].detail.contains("已清空上一轮动态 2 条"),
            "首条事件要说明清掉了什么：{}",
            events[0].detail
        );
        assert!(events[0].detail.contains("8787"));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 幂等 install（端口没变）不是新会话，不能把本轮会话里的记录抹掉。
    /// 应用重启得足够快时租约还新鲜，走的就是这条早返回路径。
    #[test]
    fn idempotent_reinstall_keeps_the_current_session() {
        let (home, data) = sandbox();
        install(&home, Region::Global, &data, 8787).unwrap();
        journal_append(&data, "route_start", "本轮：使用账号 A");
        install(&home, Region::Global, &data, 8787).unwrap();
        let events = journal_read(&data);
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["install", "route_start"],
            "重复 install 不该重置日志：{events:?}"
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 纯追加下，半条记录（写在途中 / 崩在半路）不算一条。
    /// 而且它必须被隔开——否则**下一条**会粘在它后面一起变成坏行、一起丢掉。
    #[test]
    fn journal_tolerates_a_half_written_tail() {
        let (home, data) = sandbox();
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

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 回归：日志是 `read-modify-write`，多线程并发追加若不串行化就会互相覆盖。
    /// 这直接对应线上场景 —— 代理每个连接一个线程，出故障时恰恰是并发最高的时候。
    #[test]
    fn journal_loses_nothing_under_concurrency() {
        use std::collections::BTreeSet;
        use std::sync::Arc;

        let (home, data) = sandbox();
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

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    // ── 区域 ──────────────────────────────────────────────────────────────

    /// 装进哪个区域 = 写进哪个目录。装错目录**不会报错**，只会让界面显示
    /// 「接管已开启」而实际一个请求都没被接管 —— 所以这条要钉死。
    #[test]
    fn install_targets_the_config_of_the_chosen_region_only() {
        let (home, data) = sandbox();
        // 端口取设置里的默认值：`status` 判定 installed 时比的就是它
        let port = crate::accounts::load_settings(&data).proxy_port;
        install(&home, Region::Cn, &data, port).unwrap();

        assert_eq!(
            current_endpoint(&home, Region::Cn).as_deref(),
            Some(format!("http://127.0.0.1:{port}").as_str())
        );
        assert_eq!(
            current_endpoint(&home, Region::Global),
            None,
            "国内版接管不能顺手改国际版的配置"
        );
        assert!(home.join(".qoder-cn").join("settings.json").exists());
        assert!(
            !home.join(".qoder").join("settings.json").exists(),
            "国际版的配置文件不该被凭空创建"
        );

        // 状态也是逐区域的：同一时刻国际版是「没装」、国内版是「装了」
        let g = status(&home, Region::Global, &data);
        assert!(!g.installed);
        assert_eq!(g.region, Region::Global);
        let c = status(&home, Region::Cn, &data);
        assert!(c.installed);
        assert_eq!(c.region, Region::Cn);

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 换区域 = 从旧区域的配置里摘掉、再写进新区域。旧那份**必须**一起摘，
    /// 否则那个客户端会一直指向本机端口，而我们这边已经不为它服务了。
    #[test]
    fn switching_region_moves_the_endpoint_instead_of_duplicating_it() {
        let (home, data) = sandbox();
        install(&home, Region::Global, &data, 8787).unwrap();
        install(&home, Region::Cn, &data, 8787).unwrap();

        assert_eq!(
            current_endpoint(&home, Region::Global),
            None,
            "换到国内版后，国际版那份端点必须被摘掉"
        );
        assert_eq!(
            current_endpoint(&home, Region::Cn).as_deref(),
            Some("http://127.0.0.1:8787")
        );
        assert_eq!(load_lease(&data).unwrap().region, Region::Cn);

        // 再卸一次：只动国内版
        uninstall(&home, Region::Cn, &data).unwrap();
        assert_eq!(current_endpoint(&home, Region::Cn), None);
        assert!(!lease_path(&data).exists());

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 卸载只认**租约上那个区域**：给错区域不能把另一个区域（用户自己的）配置改坏。
    #[test]
    fn uninstall_does_not_touch_the_other_regions_config() {
        let (home, data) = sandbox();
        write_cfg_in(
            &home,
            Region::Global,
            r#"{"env":{"CODEBUDDY_BASE_URL":"https://user.own.gateway"}}"#,
        );
        write_cfg_in(&home, Region::Cn, "{}");
        install(&home, Region::Cn, &data, 8787).unwrap();

        uninstall(&home, Region::Cn, &data).unwrap();
        assert_eq!(
            current_endpoint(&home, Region::Global).as_deref(),
            Some("https://user.own.gateway"),
            "用户自己写在国际版配置里的端点不能被连带清掉"
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 清扫要照顾**两个区域**：上一轮接管的是哪一个不一定还查得到（租约可能
    /// 一起丢了），而残留留在任何一个区域上都会让那个客户端断网。
    #[test]
    fn sweep_cleans_orphans_in_both_regions() {
        let (home, data) = sandbox();
        let port = crate::accounts::load_settings(&data).proxy_port;
        let ours = format!(r#"{{"env":{{"CODEBUDDY_BASE_URL":"http://127.0.0.1:{port}"}}}}"#);
        write_cfg_in(&home, Region::Global, &ours);
        write_cfg_in(&home, Region::Cn, &ours);

        sweep(&home, &data);

        assert_eq!(current_endpoint(&home, Region::Global), None);
        assert_eq!(current_endpoint(&home, Region::Cn), None);

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }
}
