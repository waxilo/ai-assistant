//! WorkBuddy 全局网络配置的**诊断**与**一键恢复**。
//!
//! 背景：调试本地反代时，如果把「自定义服务端点」写到 WorkBuddy 的**全局**配置里，
//! 受影响的不是某个子进程，而是**整个 WorkBuddy**（含正在运行的桌面端）——表现为
//! 所有请求都打到那个端点，端点没在听，于是「网络不通 / 502 连接被拒绝」。
//! 这类污染的写入点是固定的几处，本模块就是围绕它们做扫描与清除。
//!
//! 两条硬规则：
//!
//! 1. `diagnose` **只读**，不改任何文件；
//! 2. `restore` 只碰「明确是本项目调试残留」的键，改动前一律备份，且不新增任何键。
//!
//! 之所以自己拼 JSON 而不是复用 `Settings`：被扫的是 **WorkBuddy 自己的**配置文件，
//! 里面还有 `sandbox` / `claw` / `enabledPlugins` 等我们不认识的键。用 `serde_json::Value`
//! 原样保留未知字段，避免「恢复」变成「清空别人的配置」。
//!
//! 家目录是**参数**而不是到处调 `dirs::home_dir()`：这样整条恢复流程能在临时目录里跑完
//! 单测，不会碰到用户真实配置（见文末 tests）。

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// 会改变 base url 的环境变量。取自在 CLI 产物中实际出现的名字
/// （`codebuddy.js` 里 grep 到的 URL/HOST 类变量），不凭印象编。
///
/// 有意**不包含** `CODEBUDDY_GATEWAY_HOST` / `CODEBUDDY_HOST` / `CODEBUDDY_IDE_HOST`
/// —— 它们是沙箱/IDE 管道的地址，与「模型请求打到哪」无关，报出来只会造成误判。
pub const ENV_KEYS: &[&str] = &[
    "CODEBUDDY_BASE_URL",
    "CODEBUDDY_REMOTE_CONTROL_BASE_URL",
    "CODEBUDDY_SERVICE_PROXY_URL",
];

/// `settings.json` 里我们从没写过、但正是本项目调试时留下的顶层键
const POLLUTION_TOP_KEYS: &[&str] = &["endpoint"];

/// 连接探测超时：只用来区分「端口有没有人在听」，不需要等太久
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// 报告结构（直接给前端渲染）
// ---------------------------------------------------------------------------

/// 一个可疑点
#[derive(Serialize, Clone, Debug)]
pub struct NetIssue {
    /// 稳定标识，前端用作 key
    pub id: String,
    /// 分类：配置文件 / launchd 全局环境 / shell 启动脚本 / 本应用反代
    pub scope: String,
    /// 具体位置：文件路径、变量名或设置项
    pub target: String,
    /// 当前值
    pub value: String,
    /// `block` = 会让网络不通；`warn` = 残留但当前不影响连通性
    pub level: String,
    /// 人话解释该拿它怎么办
    pub note: String,
    /// 是否属于「一键恢复」能自动处理的范畴
    pub fixable: bool,
}

/// 诊断结果
#[derive(Serialize, Clone, Debug)]
pub struct NetReport {
    /// 没有任何 `block` 级问题时为 true
    pub healthy: bool,
    pub issues: Vec<NetIssue>,
    /// 已扫描的位置 —— 让用户能确认「确实查过了」，而不是空手而归
    pub scanned: Vec<String>,
}

/// 恢复过程中的一个动作
#[derive(Serialize, Clone, Debug)]
pub struct NetStep {
    pub action: String,
    pub ok: bool,
    pub detail: String,
}

/// 恢复结果
#[derive(Serialize, Clone, Debug)]
pub struct NetRestoreReport {
    pub steps: Vec<NetStep>,
    /// 本次改动过、已备份的文件路径
    pub backups: Vec<String>,
    /// 本地反代是否被本次恢复关掉
    pub proxy_disabled: bool,
    /// 恢复后的复检结果
    pub report: NetReport,
}

// ---------------------------------------------------------------------------
// 扫描位置（家目录一律由调用方传入）
// ---------------------------------------------------------------------------

/// WorkBuddy 可能写「服务端点」的配置文件
fn config_files(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".workbuddy").join("settings.json"),
        home.join(".codebuddy").join("settings.json"),
    ]
}

/// 可能导出全局环境变量的 shell 启动脚本
fn shell_rcs(home: &Path) -> Vec<PathBuf> {
    [".zshrc", ".zprofile", ".zshenv", ".bash_profile", ".bashrc", ".profile"]
        .iter()
        .map(|n| home.join(n))
        .collect()
}

/// `~/.workbuddy/settings.json` → `~/.workbuddy/settings.json`（家目录缩写成 `~`）
fn short_path(home: &Path, p: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

// ---------------------------------------------------------------------------
// URL / 端口小工具
// ---------------------------------------------------------------------------

/// 从 `http(s)://host:port/...` 里取出 `(host, port)`；取不到返回 None。
/// 丢进 socket 之前只做最朴素的切分——这串 URL 是我们自己写进去的，不需要完整解析器。
fn http_target(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        // 没写协议头时按 http 处理（端口默认 80）
        None => ("http", url),
    };
    let default_port = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
    let authority = rest.split(['/', '?', '#']).next()?;
    // 去掉可能存在的 user:pass@ 前缀
    let authority = authority.rsplit('@').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// 是否指向本机回环
fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// 该 host:port 上是否有人监听
fn tcp_alive(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
}

/// 把一个「端点值」翻译成结论。回环 + 没人听 = 就是它在断你的网。
///
/// 注意：这些字符串会**原样渲染在前端**（不是 Markdown），所以不要用 `**` 或反引号，
/// 需要强调就用「」。踩过一次：界面上直接显示了星号。
fn judge_value(value: &str) -> (String, String) {
    let short = truncate(value, 120);
    let Some((host, port)) = http_target(value) else {
        return (
            "warn".into(),
            format!("值 {short} 不是可解析的 http(s) 地址，建议清掉。"),
        );
    };
    if !is_loopback(&host) {
        return (
            "warn".into(),
            format!("指向外部地址 {host}:{port}。若非你本人有意配置，建议清掉。"),
        );
    }
    if tcp_alive(&host, port) {
        (
            "warn".into(),
            format!(
                "指向本机 {host}:{port}，该端口当前「有服务在听」。WorkBuddy 的流量会被引到本地，\
                 不在该端口时就会断网；确认不需要就清掉。"
            ),
        )
    } else {
        (
            "block".into(),
            format!(
                "指向本机 {host}:{port}，但「该端口没有任何服务在监听」——这就是「连接被拒绝 / \
                 502」的直接原因，清掉即可恢复。"
            ),
        )
    }
}

/// 超长值截断，避免把长串整屏铺开
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------------
// 诊断
// ---------------------------------------------------------------------------

/// 扫一遍所有污染点。**只读**。
///
/// `home` 用于定位 WorkBuddy 的全局配置；`data_dir` 是本应用自己的数据目录，
/// 用来读它的反代开关。
pub fn diagnose(home: &Path, data_dir: &Path) -> NetReport {
    let mut issues = Vec::new();
    let mut scanned = Vec::new();

    // 1) 配置文件里的 endpoint / env.*
    for path in config_files(home) {
        let label = short_path(home, &path);
        if !path.exists() {
            scanned.push(format!("{label}（不存在）"));
            continue;
        }
        scanned.push(format!("{label}（已检查）"));
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };

        for key in POLLUTION_TOP_KEYS {
            if let Some(val) = v.get(*key).and_then(Value::as_str) {
                if val.trim().is_empty() {
                    continue;
                }
                issues.push(NetIssue {
                    id: format!("file:{label}:{key}"),
                    scope: "配置文件".into(),
                    target: format!("{label} · {key}"),
                    value: truncate(val, 120),
                    // 实测该键不参与 CLI 的路由决策，留着只是脏，不会断网
                    level: "warn".into(),
                    note: "调试残留。它不会改变实际请求路由，但属于本项目写进去的键，建议清除。"
                        .into(),
                    fixable: true,
                });
            }
        }

        if let Some(env) = v.get("env").and_then(Value::as_object) {
            for key in ENV_KEYS {
                let Some(val) = env.get(*key).and_then(Value::as_str) else {
                    continue;
                };
                if val.trim().is_empty() {
                    continue;
                }
                // 先问一句「这是本应用正在工作的接管吗」——是就不能当污染报，
                // 否则用户点一次「一键恢复」就把自己刚开的功能关了。
                if let Some(issue) = self_issue(data_dir, &label, key, val) {
                    issues.push(issue);
                    continue;
                }
                let (level, note) = judge_value(val);
                issues.push(NetIssue {
                    id: format!("file:{label}:env.{key}"),
                    scope: "配置文件".into(),
                    target: format!("{label} · env.{key}"),
                    value: truncate(val, 120),
                    level,
                    // 这个键**会**被整个 WorkBuddy 进程继承（含桌面端），是真凶
                    note: format!("{note}（写在这里会被整个 WorkBuddy 进程继承，含正在运行的桌面端。）"),
                    fixable: true,
                });
            }
        }
    }

    // 2) launchd 全局环境变量 —— 影响所有从图形界面启动的程序
    for key in ENV_KEYS {
        match launchctl_getenv(key).as_deref().filter(|v| !v.trim().is_empty()) {
            Some(val) => {
                let (level, note) = judge_value(val);
                issues.push(NetIssue {
                    id: format!("launchd:{key}"),
                    scope: "launchd 全局环境".into(),
                    target: (*key).to_string(),
                    value: truncate(val, 120),
                    level,
                    note: format!(
                        "{note}（launchd 全局变量会影响所有从图形界面启动的程序，改动需重启对应程序才生效。）"
                    ),
                    fixable: true,
                });
            }
            None => scanned.push(format!("launchctl {key}（未设置）")),
        }
    }

    // 3) shell 启动脚本里的 export —— 只报告，不自动改
    for path in shell_rcs(home) {
        let label = short_path(home, &path);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let hits: Vec<(usize, String)> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim();
                !t.starts_with('#') && ENV_KEYS.iter().any(|k| t.contains(k))
            })
            .map(|(i, l)| (i + 1, l.trim().to_string()))
            .collect();
        if hits.is_empty() {
            continue;
        }
        let detail = hits
            .iter()
            .map(|(n, l)| format!("第 {n} 行: {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        issues.push(NetIssue {
            id: format!("shell:{label}"),
            scope: "shell 启动脚本".into(),
            target: label.clone(),
            value: truncate(&detail, 240),
            level: "warn".into(),
            note: "只报告、不自动改：这些行可能是你给别的工具配的。确认无用后请手动删除对应行。"
                .into(),
            fixable: false,
        });
    }
    scanned.push("shell 启动脚本（.zshrc / .zprofile / .bash_profile 等）".into());

    // 4) 本应用反代：开着不是故障，但「一键恢复」会顺带关掉，先说清楚。
    //    如果上面已识别为「智能接管 / 正常」（活的接管），这里不再单列一条，
    //    否则用户会以为有两个东西在跑。只在没有识别到活的接管时（异常状态）才报出来。
    let settings = crate::accounts::load_settings(data_dir);
    if settings.proxy_enabled {
        let alive_takeover = issues
            .iter()
            .any(|i| i.scope == "智能接管" && i.level == "ok");
        if !alive_takeover {
            let port = settings.proxy_port;
            issues.push(NetIssue {
                id: "app:proxy".into(),
                scope: "本应用反代".into(),
                target: "本地反代开关".into(),
                value: format!("已开启，监听 127.0.0.1:{port}"),
                level: "warn".into(),
                note: "反代本身不会导致 WorkBuddy 断网。若你正在排查「网络不通」，建议一并关掉以排除干扰。"
                    .into(),
                fixable: true,
            });
        }
    }

    // 5) 长驻 CLI host 是否使用过随后被摘除的接管端点。
    //    settings 键删除后，旧值仍可能留在 process.env；只能用代理请求事件、卸载事件
    //    与当前主进程启动时间交叉定位。
    match desktop_stale_takeover(home, data_dir) {
        Some(issue) => issues.push(issue),
        None => scanned.push("接管事件日志 × WorkBuddy 进程启动时间（未见异常）".into()),
    }

    NetReport {
        healthy: !issues.iter().any(|i| i.level == "block"),
        issues,
        scanned,
    }
}

/// 判定核心：某次接管确实收到过模型请求，随后端点被摘除，而当前 WorkBuddy
/// 主进程又早于摘除时刻启动，说明长驻 CLI host 尚未通过重启清掉旧 process.env。
fn stale_cli_cache(events: &[crate::stealth::JournalEvent], desktop_start_ms: i64) -> Option<i64> {
    let mut open = false;
    let mut used = false;
    for e in events {
        match e.event.as_str() {
            "install" => {
                open = true;
                used = false;
            }
            "proxy_request" if open => used = true,
            "uninstall" if open => {
                if used && desktop_start_ms <= e.at_ms {
                    return Some(e.at_ms);
                }
                open = false;
                used = false;
            }
            _ => {}
        }
    }
    None
}

fn workbuddy_start_ms() -> Option<i64> {
    let out = Command::new("pgrep")
        .args(["-f", "^/Applications/WorkBuddy.app/Contents/MacOS/Electron$"])
        .output()
        .ok()?;
    let pid = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    if pid.is_empty() {
        return None;
    }
    let out = Command::new("ps").args(["-p", &pid, "-o", "lstart="]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).to_string();
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(
        &format!("{} {} {}", parts[1], parts[2], parts[4]),
        "%b %e %H:%M:%S %Y",
    )
    .ok()?;
    use chrono::TimeZone;
    chrono::Local.from_local_datetime(&naive).single().map(|t| t.timestamp_millis())
}

fn desktop_stale_takeover(home: &Path, data_dir: &Path) -> Option<NetIssue> {
    let events = crate::stealth::journal_read(data_dir);
    let removed_at = stale_cli_cache(&events, workbuddy_start_ms()?)?;
    if crate::stealth::current_endpoint(home).is_some() {
        return None;
    }
    let at = chrono::DateTime::from_timestamp_millis(removed_at)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default();
    Some(NetIssue {
        id: "desktop:stale-takeover".into(),
        scope: "桌面端进程".into(),
        target: "WorkBuddy 长驻 CLI host（运行中）".into(),
        value: "仍可能缓存已摘除的接管端点".into(),
        level: "block".into(),
        note: format!(
            "接管期间代理确实收到过模型请求，端点已在 {at} 摘除；\
             WorkBuddy 的长驻 CLI host 会把该值留在 process.env，删除配置键不会清掉缓存。\
             请重启 WorkBuddy 与 CLI host 后恢复直连。"
        ),
        fixable: false,
    })
}

/// 这个端点是「本应用智能接管」自己装的吗？
///
/// 是且心跳还在 → 报成 `ok`（正常工作中），不污染 `healthy`，免得用户点「一键恢复」
/// 把自己刚开的功能关掉；心跳停了 → 那是崩溃留下的僵尸，按常规判定，会被判成会断网。
fn self_issue(data_dir: &Path, label: &str, key: &str, val: &str) -> Option<NetIssue> {
    let lease = crate::stealth::load_lease(data_dir)?;
    if lease.url != val.trim() {
        return None; // 值不是我们写进去的，不是自己人
    }
    let alive = crate::stealth::is_alive(&lease);
    Some(NetIssue {
        id: format!("file:{label}:env.{key}"),
        scope: if alive {
            "智能接管".into()
        } else {
            "配置文件".into()
        },
        target: format!("{label} · env.{key}"),
        value: truncate(val, 120),
        // ok 不参与 healthy 判定，界面上显示成「正常」
        level: if alive { "ok".into() } else { "block".into() },
        note: if alive {
            format!(
                "本应用「智能接管」正在工作：对话请求经 127.0.0.1:{} 转发，按最旧积分自动选账号。\
                 这是你自己开的功能，不需要处理；想停用请在上方关闭「智能接管」。",
                lease.port
            )
        } else {
            "这是本应用「智能接管」写下的端点，但心跳已停（应用可能崩溃退出）——\
             它就是现在断网的原因，清掉即可恢复。".into()
        },
        fixable: true,
    })
}

fn launchctl_getenv(key: &str) -> Option<String> {
    let out = Command::new("launchctl").args(["getenv", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---------------------------------------------------------------------------
// 恢复
// ---------------------------------------------------------------------------

/// 从一份 settings.json 文本里摘掉污染键，返回（新文本, 被摘掉的键名）。
/// 解析失败时原样返回，绝不覆盖一个我们读不懂的文件。
fn strip_pollution(text: &str) -> (String, Vec<String>) {
    let Ok(mut v) = serde_json::from_str::<Value>(text) else {
        return (text.to_string(), Vec::new());
    };
    let Some(obj) = v.as_object_mut() else {
        return (text.to_string(), Vec::new());
    };

    let mut removed = Vec::new();
    for key in POLLUTION_TOP_KEYS {
        if obj.remove(*key).is_some() {
            removed.push((*key).to_string());
        }
    }
    // 先在小作用域里借用 env，出了作用域再决定要不要把空掉的 env 一起删掉
    let env_became_empty = match obj.get_mut("env").and_then(Value::as_object_mut) {
        Some(env) => {
            for key in ENV_KEYS {
                if env.remove(*key).is_some() {
                    removed.push(format!("env.{key}"));
                }
            }
            env.is_empty()
        }
        None => false,
    };
    if env_became_empty {
        obj.remove("env");
    }

    if removed.is_empty() {
        return (text.to_string(), removed);
    }
    match serde_json::to_string_pretty(&v) {
        Ok(s) => (format!("{s}\n"), removed),
        Err(_) => (text.to_string(), Vec::new()),
    }
}

/// 清理两个配置文件里的污染键。返回（步骤, 备份路径）。
/// 单独抽出来是为了能在临时目录里被单测完整跑一遍。
fn clean_config_files(home: &Path, stamp: &str) -> (Vec<NetStep>, Vec<String>) {
    let mut steps = Vec::new();
    let mut backups = Vec::new();

    for path in config_files(home) {
        let label = short_path(home, &path);
        if !path.exists() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            steps.push(NetStep {
                action: format!("读取 {label}"),
                ok: false,
                detail: "读不出来，跳过（可能是权限问题）。".into(),
            });
            continue;
        };
        let (next, removed) = strip_pollution(&text);
        if removed.is_empty() {
            steps.push(NetStep {
                action: format!("清理 {label}"),
                ok: true,
                detail: "没有需要清理的键。".into(),
            });
            continue;
        }
        // 备份优先：写坏了还能还原
        let backup = path.with_file_name(format!(
            "{}.bak.{stamp}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        if fs::copy(&path, &backup).is_err() {
            steps.push(NetStep {
                action: format!("备份 {label}"),
                ok: false,
                detail: "备份失败，为安全起见没有改动该文件。".into(),
            });
            continue;
        }
        backups.push(backup.display().to_string());

        match write_atomic(&path, &next) {
            Ok(()) => steps.push(NetStep {
                action: format!("清理 {label}"),
                ok: true,
                detail: format!(
                    "已移除 {}（备份：{}）",
                    removed.join("、"),
                    short_path(home, &backup)
                ),
            }),
            Err(e) => steps.push(NetStep {
                action: format!("清理 {label}"),
                ok: false,
                detail: format!("写入失败：{e}；原文件已备份，可手动还原。"),
            }),
        }
    }
    (steps, backups)
}

/// 取消 launchd 全局环境变量
fn clear_launchd() -> Vec<NetStep> {
    let mut steps = Vec::new();
    for key in ENV_KEYS {
        if launchctl_getenv(key).is_none() {
            continue;
        }
        let ok = Command::new("launchctl")
            .args(["unsetenv", key])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        steps.push(NetStep {
            action: format!("取消全局变量 {key}"),
            ok,
            detail: if ok {
                "已取消。已在运行的程序仍持有旧值，重启对应程序后生效。".into()
            } else {
                "取消失败（可能需要权限）。".into()
            },
        });
    }
    steps
}

/// 关掉智能接管（本地反代 + 接管是一个开关）—— 就是用户要的「在页面上操作关闭」
///
/// **顺序要紧**：必须先关掉开关，再去摘端点。反着来的话，反代的监督线程
/// 会在下一次轮询（2 秒内）发现开关还开着，立刻又把端点装回去，恢复等于白做。
fn disable_proxy(data_dir: &Path) -> (Vec<NetStep>, bool) {
    let mut settings = crate::accounts::load_settings(data_dir);
    if !settings.proxy_enabled {
        return (Vec::new(), false);
    }
    settings.proxy_enabled = false;
    let ok = crate::accounts::save_settings(data_dir, &settings).is_ok();
    let steps = vec![NetStep {
        action: "关闭智能接管".into(),
        ok,
        detail: if ok {
            "已关闭，监听端口已释放，监督线程不会再写端点。".into()
        } else {
            "关闭失败：设置文件写入异常。".into()
        },
    }];
    (steps, true)
}

/// 关开关 → 摘接管端点 → 清配置文件 → 取消 launchd 全局变量 → 复检。
///
/// 每一步都记进 `steps`，失败不中断（尽量多救一点），最后给出复检结果。
pub fn restore(home: &Path, data_dir: &Path) -> NetRestoreReport {
    restore_impl(home, data_dir, true)
}

/// `touch_launchd=false` 时只处理文件与设置，不碰 launchd。
/// 单测必须走这条路 —— 否则跑一次 `cargo test` 就会动到整机的全局环境变量。
fn restore_impl(home: &Path, data_dir: &Path, touch_launchd: bool) -> NetRestoreReport {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

    let mut steps = Vec::new();

    // 1) 先关开关：否则反代监督线程会把端点重新装回去
    let (proxy_steps, proxy_disabled) = disable_proxy(data_dir);
    steps.extend(proxy_steps);

    // 2) 摘掉接管端点（会还原成装载前的值，并删掉租约文件）
    match crate::stealth::uninstall(home, data_dir) {
        Ok(()) => {
            if crate::stealth::load_lease(data_dir).is_none() {
                steps.push(NetStep {
                    action: "摘除接管端点".into(),
                    ok: true,
                    detail: "已还原为装载前的值（原本没有则已删除），租约已清除。".into(),
                });
            }
        }
        Err(e) => steps.push(NetStep {
            action: "摘除接管端点".into(),
            ok: false,
            detail: format!("失败：{e}"),
        }),
    }

    // 3) 兜底清掉其余污染键（上面已删的会报「没有需要清理」，无害）
    let (clean_steps, backups) = clean_config_files(home, &stamp);
    steps.extend(clean_steps);
    if touch_launchd {
        steps.extend(clear_launchd());
    }

    if steps.is_empty() {
        steps.push(NetStep {
            action: "检查".into(),
            ok: true,
            detail: "没有发现需要恢复的项。".into(),
        });
    }

    NetRestoreReport {
        steps,
        backups,
        proxy_disabled,
        // 复检：让用户直接看到结果，而不是"我点了但不知道好没好"
        report: diagnose(home, data_dir),
    }
}

/// 同目录 tmp + rename，避免写到一半留下半截文件；
/// 沿用原文件权限（WorkBuddy 自己的配置文件通常是 0600）。
///
/// `pub(crate)`：`stealth` 装卸端点时也用同一套原子写，保证两处行为一致。
pub(crate) fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.netfix.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&tmp, content)?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "无法定位家目录，无法诊断 WorkBuddy 全局配置。".to_string())
}

#[tauri::command]
pub fn net_diagnose(app: tauri::AppHandle) -> Result<NetReport, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    Ok(diagnose(&home_dir()?, &dir))
}

#[tauri::command]
pub fn net_restore(app: tauri::AppHandle) -> Result<NetRestoreReport, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    let current = crate::accounts::load_settings(&dir);
    if current.proxy_enabled {
        let mut disabled = current;
        disabled.proxy_enabled = false;
        crate::commands::apply_settings_inner(&app, disabled)?;
    }
    Ok(restore(&home_dir()?, &dir))
}

/// 在 Finder / 资源管理器里定位一个文件（用于查看备份）
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在：{path}"));
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg("-R").arg(&p);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("explorer");
        c.arg(format!("/select,{}", p.display()));
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(p.parent().unwrap_or(&p));
        c
    };
    cmd.status()
        .map_err(|e| format!("调用系统文件管理器失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 造一个隔离的「假家目录 + 假应用数据目录」。
    /// 返回 (假家目录, 假数据目录)；调用方负责在结束时删掉假家目录。
    fn sandbox() -> (PathBuf, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "wb-netfix-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let home = base.join("home");
        let data = base.join("data");
        fs::create_dir_all(home.join(".workbuddy")).unwrap();
        fs::create_dir_all(&data).unwrap();
        (home, data)
    }

    #[test]
    fn strips_only_known_pollution_keys() {
        let src = r#"{
          "sandbox": {"allow": true},
          "claw": 1,
          "endpoint": "http://127.0.0.1:8799",
          "env": {"CODEBUDDY_BASE_URL": "http://127.0.0.1:8799", "OTHER": "keep-me"}
        }"#;
        let (out, removed) = strip_pollution(src);
        assert!(removed.contains(&"endpoint".to_string()));
        assert!(removed.contains(&"env.CODEBUDDY_BASE_URL".to_string()));
        let v: Value = serde_json::from_str(&out).unwrap();
        // 我们不认识的键必须原样留着
        assert_eq!(v["sandbox"]["allow"], Value::Bool(true));
        assert_eq!(v["claw"], Value::from(1));
        // 只摘掉该摘的，同一层其它键保留
        assert_eq!(v["env"]["OTHER"], Value::from("keep-me"));
        assert!(v.get("endpoint").is_none());
        assert!(v["env"].get("CODEBUDDY_BASE_URL").is_none());
    }

    #[test]
    fn drops_env_object_when_it_becomes_empty() {
        let src = r#"{"env":{"CODEBUDDY_BASE_URL":"http://127.0.0.1:8799"}}"#;
        let (out, _) = strip_pollution(src);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("env").is_none(), "清空后的 env 不该留个空壳");
    }

    #[test]
    fn leaves_clean_and_unparsable_files_untouched() {
        let clean = r#"{"sandbox":true}"#;
        assert_eq!(strip_pollution(clean).0, clean);
        // 读不懂的文件绝不能"顺手"重写成 {}
        let broken = "{not json";
        assert_eq!(strip_pollution(broken).0, broken);
        assert!(strip_pollution(broken).1.is_empty());
    }

    #[test]
    fn parses_target_from_urls() {
        assert_eq!(
            http_target("http://127.0.0.1:8799"),
            Some(("127.0.0.1".into(), 8799))
        );
        assert_eq!(
            http_target("https://copilot.tencent.com/v2/x?y=1"),
            Some(("copilot.tencent.com".into(), 443))
        );
        // 无端口时按协议取默认：http 80 / https 443
        assert_eq!(http_target("http://localhost/x"), Some(("localhost".into(), 80)));
        assert_eq!(http_target("https://localhost/x"), Some(("localhost".into(), 443)));
        // 带凭据的前缀要剥掉
        assert_eq!(
            http_target("http://u:p@127.0.0.1:8787/a"),
            Some(("127.0.0.1".into(), 8787))
        );
    }

    #[test]
    fn loopback_detection_covers_aliases() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("copilot.tencent.com"));
        assert!(!is_loopback("192.168.1.10"));
    }

    #[test]
    fn dead_loopback_port_is_reported_as_blocker() {
        // 端口 1 几乎不可能有人监听
        let (level, note) = judge_value("http://127.0.0.1:1");
        assert_eq!(level, "block", "回环 + 无人监听 = 断网主因");
        assert!(note.contains("没有任何服务在监听"));
        // 非回环地址只算残留
        let (level, _) = judge_value("https://example.com");
        assert_eq!(level, "warn");
    }

    /// 端到端：在临时目录里复现一次「被污染的配置」，跑诊断 + 恢复，验证结果。
    /// 全程不碰真实家目录，也不会执行 launchctl。
    #[test]
    fn diagnose_flags_pollution_and_restore_cleans_it() {
        let (home, data) = sandbox();
        let cfg = home.join(".workbuddy").join("settings.json");
        fs::write(
            &cfg,
            r#"{
  "sandbox": {"network": true},
  "claw": "keep",
  "endpoint": "http://127.0.0.1:9",
  "env": {"CODEBUDDY_BASE_URL": "http://127.0.0.1:9"}
}"#,
        )
        .unwrap();

        let rep = diagnose(&home, &data);
        assert!(!rep.healthy, "回环死端口必须被判成会断网");
        let block = rep
            .issues
            .iter()
            .find(|i| i.level == "block")
            .expect("应有一条 block 级问题");
        assert!(block.target.contains("env.CODEBUDDY_BASE_URL"));
        // 只读：诊断不得改动文件
        assert!(
            fs::read_to_string(&cfg).unwrap().contains("CODEBUDDY_BASE_URL"),
            "诊断必须是只读的"
        );

        let (steps, backups) = clean_config_files(&home, "teststamp");
        assert!(steps.iter().all(|s| s.ok), "清理步骤应当全部成功");
        assert_eq!(backups.len(), 1, "改动前必须留下一份备份");

        let after = fs::read_to_string(&cfg).unwrap();
        let v: Value = serde_json::from_str(&after).unwrap();
        assert!(v.get("endpoint").is_none());
        assert!(v.get("env").is_none());
        assert_eq!(v["sandbox"]["network"], Value::Bool(true), "别人的配置要留着");
        assert_eq!(v["claw"], Value::from("keep"));

        // 备份里必须还是原始内容，才谈得上"可还原"
        let backup = fs::read_to_string(&backups[0]).unwrap();
        assert!(backup.contains("CODEBUDDY_BASE_URL"));

        // 复检：文件已干净，不应再有配置文件类问题
        let rep2 = diagnose(&home, &data);
        assert!(
            !rep2
                .issues
                .iter()
                .any(|i| i.scope == "配置文件" || i.scope == "shell 启动脚本"),
            "恢复后不该还有配置文件/shell 类问题：{:?}",
            rep2.issues
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn diagnose_reports_shell_rc_exports_but_marks_them_unfixable() {
        let (home, data) = sandbox();
        fs::write(
            home.join(".zshrc"),
            "# CODEBUDDY_BASE_URL=commented-out-should-not-count\nexport CODEBUDDY_BASE_URL=http://127.0.0.1:9\n",
        )
        .unwrap();

        let rep = diagnose(&home, &data);
        let hit = rep
            .issues
            .iter()
            .find(|i| i.scope == "shell 启动脚本")
            .expect("应扫出 .zshrc 里的 export");
        assert!(!hit.fixable, "shell 脚本只报告不自动改");
        assert!(hit.value.contains("第 2 行"), "应给出准确行号：{}", hit.value);
        assert!(
            !hit.value.contains("commented-out"),
            "被注释掉的行不该被算进去"
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 正在工作的智能接管**不是**污染 —— 否则用户点一次「一键恢复」就把自己刚开的功能关了。
    #[test]
    fn live_stealth_takeover_is_reported_as_healthy_not_pollution() {
        let (home, data) = sandbox();
        crate::stealth::install(&home, &data, 8787).unwrap();

        let rep = diagnose(&home, &data);
        let hit = rep
            .issues
            .iter()
            .find(|i| i.scope == "智能接管")
            .expect("应把本应用的接管认出来");
        assert_eq!(hit.level, "ok", "活的接管不该报成 block/warn");
        assert!(rep.healthy, "接管生效时不能显示成「网络有问题」");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 心跳停了的接管就是崩溃残留，必须照常判为会断网，好让「一键恢复」能清掉它。
    #[test]
    fn stale_stealth_takeover_is_reported_as_blocker() {
        let (home, data) = sandbox();
        crate::stealth::install(&home, &data, 8787).unwrap();

        // 把心跳拨到很早以前，模拟应用被 kill -9 后再没起来
        let lease = crate::stealth::lease_path(&data);
        let mut v: Value =
            serde_json::from_str(&fs::read_to_string(&lease).unwrap()).unwrap();
        v["heartbeat_ms"] = Value::from(0);
        fs::write(&lease, v.to_string()).unwrap();

        let rep = diagnose(&home, &data);
        let hit = rep
            .issues
            .iter()
            .find(|i| i.target.contains("CODEBUDDY_BASE_URL"))
            .expect("僵尸端点必须被报出来");
        assert_eq!(hit.level, "block", "心跳已停 = 断网现场");
        assert!(!rep.healthy);
        assert!(hit.note.contains("心跳已停"), "要说清为什么：{}", hit.note);

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    /// 一键恢复必须「先关开关再摘端点」，且两者都落在磁盘上。
    /// 反过来的话，反代监督线程会在 2 秒内把端点重新装回去，用户会以为恢复没生效。
    #[test]
    fn restore_disables_takeover_and_removes_the_endpoint() {
        let (home, data) = sandbox();
        let mut s = crate::accounts::load_settings(&data);
        s.proxy_enabled = true;
        crate::accounts::save_settings(&data, &s).unwrap();
        crate::stealth::install(&home, &data, 8787).unwrap();
        assert_eq!(
            crate::stealth::current_endpoint(&home).as_deref(),
            Some("http://127.0.0.1:8787")
        );

        // 不走 launchctl，免得测试动到整机环境
        let rep = restore_impl(&home, &data, false);
        assert!(rep.proxy_disabled, "恢复应报告反代被关掉");

        let after = crate::accounts::load_settings(&data);
        assert!(!after.proxy_enabled, "接管开关必须被关掉");
        assert_eq!(
            crate::stealth::current_endpoint(&home),
            None,
            "端点要摘干净"
        );
        assert!(
            crate::stealth::load_lease(&data).is_none(),
            "租约要删掉，否则 supervisor 会以为还装着"
        );
        assert!(rep.report.healthy, "恢复后应复检为健康");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn diagnose_is_clean_on_untouched_home() {
        let (home, data) = sandbox();
        let rep = diagnose(&home, &data);
        // 只断言「与机器无关」的部分：干净的空目录里不该报出文件类问题。
        // （launchd 那部分是整机状态，不能拿它去断言，否则换台机器就红。）
        assert!(
            !rep.issues
                .iter()
                .any(|i| i.scope == "配置文件" || i.scope == "shell 启动脚本"),
            "干净的家目录不该报出文件类问题：{:?}",
            rep.issues
        );
        assert!(
            !rep.issues.iter().any(|i| i.level == "block" && i.scope == "配置文件"),
            "空配置目录不该被判成会断网"
        );
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    fn ev(at_ms: i64, event: &str) -> crate::stealth::JournalEvent {
        crate::stealth::JournalEvent {
            at_ms,
            at: String::new(),
            event: event.into(),
            detail: String::new(),
        }
    }

    #[test]
    fn stale_cli_cache_requires_a_real_proxy_request_before_uninstall() {
        let used = vec![
            ev(0, "install"),
            ev(20, "proxy_request"),
            ev(100, "uninstall"),
        ];
        assert_eq!(stale_cli_cache(&used, 10), Some(100));
        assert_eq!(stale_cli_cache(&used, 101), None, "卸载后启动的新进程没有缓存");

        let unused = vec![ev(0, "install"), ev(100, "uninstall")];
        assert_eq!(stale_cli_cache(&unused, 10), None, "从未走过代理就没有缓存");
        let open = vec![ev(0, "install"), ev(20, "proxy_request")];
        assert_eq!(stale_cli_cache(&open, 10), None, "端点仍在线时不是故障");
        assert_eq!(stale_cli_cache(&[], 10), None);
    }

    #[test]
    fn journal_preserves_proxy_request_evidence() {
        let (home, data) = sandbox();
        let now = chrono::Utc::now().timestamp_millis();
        fs::write(
            journal_path_of(&data),
            format!(
                "{}\n{}\n{}\n",
                serde_json::json!({"at_ms": now - 60_000, "at": "", "event": "install", "detail": ""}),
                serde_json::json!({"at_ms": now - 45_000, "at": "", "event": "proxy_request", "detail": ""}),
                serde_json::json!({"at_ms": now - 30_000, "at": "", "event": "uninstall", "detail": ""})
            ),
        )
        .unwrap();
        let hit = stale_cli_cache(&crate::stealth::journal_read(&data), now - 50_000);
        assert_eq!(hit, Some(now - 30_000));
        let _ = diagnose(&home, &data);
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    fn journal_path_of(data_dir: &Path) -> PathBuf {
        crate::stealth::journal_path(data_dir)
    }
}
