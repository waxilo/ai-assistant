//! 端口占用诊断：把「端口被占了」从死路变成出路。
//!
//! 为什么需要它：`TcpListener::bind` 只会回一句 `Address already in use (os error 48)`。
//! 用户拿到这句话之后**没有任何下一步可做** —— 不知道谁占着，也不知道该换哪个端口。
//! 这里补两件事：**占用者是谁**、**哪个端口是空的**。
//!
//! 探测全部「尽力而为」：拿不到占用者也绝不把动作搞失败（探测失败 ≠ 端口空闲）。

use std::net::TcpListener;

/// 从 `base` 起向后探多少个端口找空闲。
const PROBE_SPAN: u16 = 32;
/// 1024 以下需要特权，不给人当建议。
const MIN_PORT: u16 = 1024;

/// 占用者。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// 进程名（lsof 给的是可执行文件名；Windows 走 `tasklist` 补）。
    pub command: String,
    pub pid: u32,
}

/// 解析 `lsof -nP +c 0 -iTCP:<port> -sTCP:LISTEN -Fcn` 的字段化输出。
///
/// `-F` 输出是「每行一个字段、首字符即字段名」：`p<pid>` 开进程、`c<command>` 给命令名。
/// 同一进程若有 IPv4/IPv6 两条监听会各出现一次，故按 PID 去重。
fn parse_lsof_fields(text: &str) -> Vec<Holder> {
    let mut out: Vec<Holder> = Vec::new();
    let mut pid: Option<u32> = None;
    let mut command = String::new();
    for line in text.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => {
                pid = value.trim().parse::<u32>().ok();
                command.clear();
            }
            "c" => command = value.trim().to_string(),
            _ => continue,
        }
        // `c` 跟在 `p` 后面；两者都有了才收一条
        if let (Some(p), false) = (pid, command.is_empty()) {
            if !out.iter().any(|h| h.pid == p) {
                out.push(Holder {
                    command: command.clone(),
                    pid: p,
                });
            }
        }
    }
    out
}

/// 解析 `netstat -ano` 的输出，取**本地地址命中该端口且状态为 LISTENING** 的 PID（去重）。
// 非 Windows 下没人调用，但单测跑在各平台——按键留着，只压警告。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_netstat_listen(text: &str, port: u16) -> Vec<u32> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // 典型：TCP  127.0.0.1:8788  0.0.0.0:0  LISTENING  88564
        if f.len() < 5 || !f[0].eq_ignore_ascii_case("tcp") {
            continue;
        }
        if !f[3].eq_ignore_ascii_case("listening") {
            continue;
        }
        let local_port = f[1].rsplit(':').next().unwrap_or_default();
        if local_port != port.to_string() {
            continue;
        }
        if let Ok(pid) = f[4].parse::<u32>() {
            if !out.contains(&pid) {
                out.push(pid);
            }
        }
    }
    out
}

/// 解析 `tasklist /FO CSV /NH` 的一行，取进程名（首个引号字段）。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_tasklist_csv(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.trim_start().starts_with('"'))?;
    let mut parts = line.split('"');
    parts.next()?; // 首个引号前的空串
    parts.next().map(|s| s.to_string())
}

/// 归一化进程名用于「这是不是我们自己」的比较。
///
/// 要先剥掉可执行后缀再滤噪声字符：Windows 的 `tasklist` 给的是 `xxx.exe`、
/// macOS 的 `lsof` 给的是可执行文件名 —— 两者得能对上同一个名字，
/// 否则 `.exe` 里的 `exe` 三个字母会参与比较，永远认不出自己人。
fn normalize(name: &str) -> String {
    let lower = name.to_lowercase();
    let base = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".app"))
        .unwrap_or(&lower);
    base.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// 这个进程是不是本助手自己（含另一个实例）。
fn is_self(holder: &Holder) -> bool {
    let mine = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_default();
    let h = normalize(&holder.command);
    (!mine.is_empty() && h == normalize(&mine)) || h == "traeworkassistant"
}

#[cfg(target_os = "macos")]
fn sys_holders(port: u16) -> Vec<Holder> {
    let out = crate::proc::cmd("lsof")
        .args([
            "-nP",
            "+c",
            "0",
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
            "-Fcn",
        ])
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => parse_lsof_fields(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}

#[cfg(target_os = "windows")]
fn sys_holders(port: u16) -> Vec<Holder> {
    let out = crate::proc::cmd("netstat")
        .args(["-ano"])
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(o) = out else { return Vec::new() };
    parse_netstat_listen(&String::from_utf8_lossy(&o.stdout), port)
        .into_iter()
        .map(|pid| {
            // `args` 要求同类型元素，`&format!(..)` 是 `&String` 不能和 `&str` 混放，故先绑定
            let filter = format!("PID eq {pid}");
            let name = crate::proc::cmd("tasklist")
                .args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
                .stdin(std::process::Stdio::null())
                .output()
                .ok()
                .and_then(|t| parse_tasklist_csv(&String::from_utf8_lossy(&t.stdout)))
                .unwrap_or_default();
            Holder {
                command: name,
                pid,
            }
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn sys_holders(_port: u16) -> Vec<Holder> {
    Vec::new()
}

/// 占用该端口的进程（尽力而为；空 = 没探到，不代表端口空闲）。
pub fn holders(port: u16) -> Vec<Holder> {
    sys_holders(port)
}

/// 从 `base` 起找一个**现在真能绑上**的端口。
///
/// 真绑一次而不是查表：只有 bind 成功才算数。
pub fn first_free_port(base: u16) -> Option<u16> {
    (base..=base.saturating_add(PROBE_SPAN))
        .find(|p| *p >= MIN_PORT && TcpListener::bind(("127.0.0.1", *p)).is_ok())
}

/// 面向用户的一句话：谁占着、以及建议换到哪个端口。
///
/// 调用方只要把它接在「端口 X 无法监听」后面即可 —— 这句话必须自带下一步动作。
pub fn busy_hint(port: u16) -> String {
    let who = match holders(port).first() {
        Some(h) if h.pid == std::process::id() => "被本进程占用".to_string(),
        Some(h) if is_self(h) => {
            format!("被另一个助手实例占用（{}，PID {}）", h.command, h.pid)
        }
        Some(h) if h.command.is_empty() => format!("被 PID {} 占用", h.pid),
        Some(h) => format!("被 {}（PID {}）占用", h.command, h.pid),
        None => "已被占用（未能识别占用者，可用 lsof -nP -iTCP:PORT 查看）".to_string(),
    };
    match first_free_port(port.saturating_add(1)) {
        Some(p) => format!("{who}；可把端口改到 {p}（当前空闲）"),
        None => who,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_fields_and_dedups_by_pid() {
        // lsof -Fcn 的真实形态：p 开进程、c 给命令名，IPv4/IPv6 各一条会重复出现
        let text = "p88564\nctraework-assistant\np88564\nctraework-assistant\np901\ncnginx\n";
        assert_eq!(
            parse_lsof_fields(text),
            vec![
                Holder {
                    command: "traework-assistant".into(),
                    pid: 88564
                },
                Holder {
                    command: "nginx".into(),
                    pid: 901
                },
            ]
        );
    }

    #[test]
    fn lsof_parser_ignores_incomplete_records() {
        // 只有 p 没有 c：不能凭半条记录编出占用者
        assert!(parse_lsof_fields("p123\n").is_empty());
        assert!(parse_lsof_fields("").is_empty());
        assert!(parse_lsof_fields("not field output\n").is_empty());
    }

    #[test]
    fn parses_only_listening_rows_on_that_port() {
        let text = "\
  TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       88564
  TCP    127.0.0.1:8788         127.0.0.1:51234        ESTABLISHED     88564
  TCP    [::]:8788              [::]:0                 LISTENING       88564
  TCP    127.0.0.1:87880        0.0.0.0:0              LISTENING       1
  TCP    127.0.0.1:8789         0.0.0.0:0              LISTENING       2
";
        // 8788 的两条 LISTENING（IPv4+IPv6）同一 PID → 去重成 1 个；
        // 87880 是另一个端口（后缀匹配陷阱），8789 不是 8788
        assert_eq!(parse_netstat_listen(text, 8788), vec![88564]);
    }

    #[test]
    fn parses_tasklist_csv_first_field() {
        let text = "\"traework-assistant.exe\",\"88564\",\"Console\",\"1\",\"51,099 K\"\n";
        assert_eq!(
            parse_tasklist_csv(text).as_deref(),
            Some("traework-assistant.exe")
        );
        assert_eq!(parse_tasklist_csv("INFO: No tasks are running.\n"), None);
    }

    #[test]
    fn normalizes_noisy_process_names() {
        // macOS lsof 给可执行文件名、Windows tasklist 给 .exe —— 必须归一到同一个串
        assert_eq!(normalize("TraeWork-Assistant"), "traeworkassistant");
        assert_eq!(normalize("traework_assistant.app"), "traeworkassistant");
        assert_eq!(normalize("traework-assistant.exe"), "traeworkassistant");
        assert_eq!(normalize("TRAEWORK-ASSISTANT.EXE"), "traeworkassistant");
        assert_eq!(normalize("nginx"), "nginx");
        // 只有裸的 "exe" 才当后缀剥，正常含 exe 的名字不受影响
        assert_eq!(normalize("codex"), "codex");
    }

    #[test]
    fn busy_port_reports_itself_and_offers_a_free_one() {
        // 自己占住一个端口，然后问它「谁占着」——应当认出是同类进程并给出可用的替代端口
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let free = first_free_port(port.saturating_add(1));
        assert!(free.is_some(), "总该能找到一个空闲端口");
        assert_ne!(free, Some(port), "不能把被占的端口当建议");

        // 探不到占用者时也必须给出一句可读的话，而不是 panic 或空串
        assert!(!busy_hint(port).is_empty());
    }

    #[test]
    fn free_port_probe_skips_privileged_range() {
        assert!(first_free_port(0).map_or(true, |p| p >= MIN_PORT));
    }

    /// 真机诊断：打印某个端口的占用者与建议（默认 8788）。
    ///
    /// 用法（先人为占住端口再看它怎么描述）：
    /// ```bash
    /// python3 -c 'import socket,time; s=socket.socket(); s.bind(("127.0.0.1",8788)); s.listen(); time.sleep(60)' &
    /// cargo test --lib -- --ignored --nocapture dump_busy_hint
    /// ```
    #[test]
    #[ignore]
    fn dump_busy_hint() {
        let port: u16 = std::env::var("TWA_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8788);
        println!("[portcheck] 原始 lsof 输出:");
        println!("{}", raw_lsof(port));
        println!("[portcheck] holders = {:?}", holders(port));
        println!("[portcheck] 面向用户的一句 = 端口 {port} {}", busy_hint(port));
    }

    /// 只给上面的 `dump_busy_hint` 用的：把 lsof 原始输出打出来，便于核对解析。
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    fn raw_lsof(port: u16) -> String {
        crate::proc::cmd("lsof")
            .args(["-nP", "+c", "0", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fcn"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|e| format!("(lsof 执行失败：{e})"))
    }

    #[cfg(not(target_os = "macos"))]
    #[allow(dead_code)]
    fn raw_lsof(_port: u16) -> String {
        "(仅 macOS 有原始 lsof 输出)".to_string()
    }
}
