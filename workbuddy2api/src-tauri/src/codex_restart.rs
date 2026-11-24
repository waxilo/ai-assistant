//! 强制重启 Codex（现名 ChatGPT）桌面应用。
//!
//! Codex 只在启动时读一次配置（`config.toml` / 模型目录），接入或断开之后
//! 不重启就不生效。这条命令把「重启」做成一步：找到 OpenAI 桌面应用的主进程，
//! 结束它的进程树，再按包装身份重新拉起。
//!
//! 两个刻意的收窄（Windows）：
//!
//! * 只结束「**有窗口** + 进程名 `codex*`/`chatgpt*` + 路径属于 OpenAI 桌面应用」
//!   的进程。终端里跑的 `codex` CLI 没有自己的窗口，`codex-windows-sandbox-service`
//!   这类无窗辅助进程也匹配不上 —— 用户可能同时在用它们，误杀一次就是一次事故。
//! * 拉起不走 exe 路径：OpenAI 桌面应用是 MSIX 包（装在 ACL 保护的 WindowsApps 下），
//!   直接 spawn 不可靠。解析 AUMID 后走 `explorer shell:appsFolder\<AUMID>`。
//!
//! macOS 按 bundle id（`com.openai.*`）识别，只对该 bundle 路径下的进程下手；
//! 拉起走 `open -b <bundle-id>`。
//!
//! 实现照抄 kiro2api 的 `codex_restart.rs`（同一个应用、同一套包装身份）。

use std::process::Command;
use std::time::Duration;

use serde::Serialize;

/// 重启任务的结果：应用当时在不在跑、有没有完成重启、主进程名。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRestartOutcome {
    pub running: bool,
    pub restarted: bool,
    pub app: Option<String>,
}

/// 给子进程抑制控制台窗口（`CREATE_NO_WINDOW`）。
///
/// 本进程是 GUI（没有控制台），从它 spawn 的 `powershell` / `taskkill` 若不指定
/// 这个标志，Windows 会为每个子进程新建一个黑框一闪而过。非 Windows 为空操作。
fn quiet(cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let _ = cmd;
}

/// Codex 桌面应用现在是否在跑。
///
/// 给「要不要弹重启确认」用：没在跑就没有「重启才生效」的问题，
/// 下次启动自然读到新配置，不该打扰用户。探测失败按**没在跑**处理 ——
/// 重启走的是同一条探测路，探测不出来时那颗按钮按下去必然报错。
pub fn app_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        discover_app_processes().is_ok_and(|apps| !apps.is_empty())
    }
    #[cfg(target_os = "macos")]
    {
        match discover_app_bundle() {
            Ok(Some(bundle)) => processes_running(&bundle.path).unwrap_or(true),
            Ok(None) => false,
            Err(_) => true,
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        true
    }
}

pub fn restart() -> Result<CodexRestartOutcome, String> {
    #[cfg(target_os = "windows")]
    {
        let apps = discover_app_processes()?;
        if apps.is_empty() {
            return Ok(CodexRestartOutcome {
                running: false,
                restarted: false,
                app: None,
            });
        }
        let name = apps[0].name.clone();
        kill_app_processes(&apps)?;
        // 文件句柄与单实例锁的释放需要一点时间；立刻拉起会偶发「起了又退」
        std::thread::sleep(Duration::from_millis(800));
        relaunch_app(&apps)?;
        Ok(CodexRestartOutcome {
            running: true,
            restarted: true,
            app: Some(name),
        })
    }
    #[cfg(target_os = "macos")]
    {
        let Some(bundle) = discover_app_bundle()? else {
            return Ok(CodexRestartOutcome {
                running: false,
                restarted: false,
                app: None,
            });
        };
        if !processes_running(&bundle.path).unwrap_or(false) {
            return Ok(CodexRestartOutcome {
                running: false,
                restarted: false,
                app: Some(bundle.name),
            });
        }
        terminate_app(&bundle)?;
        relaunch_app(&bundle)?;
        Ok(CodexRestartOutcome {
            running: true,
            restarted: true,
            app: Some(bundle.name),
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Err("重启 Codex 应用目前只支持 Windows 与 macOS".to_string())
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// 一个可识别为「OpenAI 桌面应用」的运行中进程。
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppProcess {
    pid: u32,
    name: String,
    path: String,
}

/// 有窗口（排除后台服务与无窗辅助进程）+ 名字像 Codex/ChatGPT +
/// 路径属于 OpenAI 桌面应用（MSIX 包目录，或传统的 Programs 安装位置）。
#[cfg(target_os = "windows")]
fn discover_app_processes() -> Result<Vec<AppProcess>, String> {
    let script = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Get-Process | Where-Object {
  $_.MainWindowTitle -and $_.Path -and
  ($_.ProcessName -match '^(codex|chatgpt)') -and
  ($_.Path -match '\\WindowsApps\\OpenAI\.|\\OpenAI\\(Codex|ChatGPT)\\')
} | ForEach-Object { "{0}`t{1}`t{2}" -f $_.Id, $_.ProcessName, $_.Path }
"#;
    let stdout = run_powershell(script)?;
    Ok(parse_app_processes(&stdout))
}

/// 解析 PowerShell 的「pid\t名字\t路径」行；坏行（取不满三段或 pid 不是数字）直接丢弃。
#[cfg(target_os = "windows")]
fn parse_app_processes(stdout: &str) -> Vec<AppProcess> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let pid = parts.next()?.trim().parse::<u32>().ok()?;
            let name = parts.next()?.trim().to_string();
            let path = parts.next()?.trim().to_string();
            if name.is_empty() || path.is_empty() {
                return None;
            }
            Some(AppProcess { pid, name, path })
        })
        .collect()
}

/// `/T` 连子进程树一起结束（WebView2、runtime 等辅助进程随之退场）；
/// 逐个尝试、只要有一个成功就算数 —— 全军覆没才报错。
#[cfg(target_os = "windows")]
fn kill_app_processes(apps: &[AppProcess]) -> Result<(), String> {
    let mut killed = 0usize;
    let mut last_error = String::new();
    for app in apps {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/T", "/PID", &app.pid.to_string()]);
        quiet(&mut cmd);
        match cmd.output() {
            Ok(out) if out.status.success() => killed += 1,
            Ok(out) => {
                last_error = String::from_utf8_lossy(&out.stderr).trim().to_string();
            }
            Err(e) => last_error = e.to_string(),
        }
    }
    if killed == 0 {
        return Err(format!("结束 Codex 进程失败：{last_error}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn relaunch_app(apps: &[AppProcess]) -> Result<(), String> {
    // 优先走包装身份：MSIX 应用的 exe 在 ACL 保护的 WindowsApps 下，直接 spawn 不可靠
    if resolve_app_aumid().is_ok_and(|aumid| spawn_via_aumid(&aumid)) {
        return Ok(());
    }
    // 解析不到 AUMID（例如非商店版安装）再退回 exe 直启；失败时错误里带上路径，
    // 用户至少知道自己可以手动点哪个图标
    let exe = apps
        .first()
        .map(|app| app.path.clone())
        .ok_or_else(|| "没有可用于重新启动的 Codex 进程信息".to_string())?;
    let mut cmd = Command::new(&exe);
    quiet(&mut cmd);
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("已结束 Codex 进程，但重新启动失败：{e}（可手动启动 {exe}）"))
}

#[cfg(target_os = "windows")]
fn spawn_via_aumid(aumid: &str) -> bool {
    let mut cmd = Command::new("explorer");
    cmd.arg(format!("shell:appsFolder\\{aumid}"));
    quiet(&mut cmd);
    cmd.spawn().is_ok()
}

/// 解析 OpenAI 桌面应用的 AUMID。不写死包族名 —— 包名可能换代
/// （`OpenAI.Codex` → `OpenAI.ChatGPT`），前缀匹配两代通吃。
#[cfg(target_os = "windows")]
fn resolve_app_aumid() -> Result<String, String> {
    let script = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Get-StartApps |
  Where-Object { $_.AppID -match '^OpenAI\.(Codex|ChatGPT)_[^!]+!' } |
  Select-Object -First 1 -ExpandProperty AppID
"#;
    let stdout = run_powershell(script)?;
    let id = stdout.trim();
    if id.is_empty() {
        Err("没有找到 ChatGPT（Codex）应用的启动标识".to_string())
    } else {
        Ok(id.to_string())
    }
}

#[cfg(target_os = "windows")]
fn run_powershell(script: &str) -> Result<String, String> {
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    quiet(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("无法启动 PowerShell：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("PowerShell 查询失败：{stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

/// macOS 上一个可识别为「OpenAI 桌面应用」的 .app 包。
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct AppBundle {
    path: std::path::PathBuf,
    bundle_id: String,
    name: String,
}

/// OpenAI 桌面应用的 bundle id 前缀。不写死完整 id —— 应用可能换代
/// （`com.openai.chat` → `com.openai.chatgpt` → `com.openai.codex`），前缀通吃。
#[cfg(target_os = "macos")]
fn is_openai_bundle_id(id: &str) -> bool {
    id.strip_prefix("com.openai.")
        .is_some_and(|rest| rest.starts_with("codex") || rest.starts_with("chat"))
}

/// 在 /Applications 与 ~/Applications 里找 OpenAI 桌面应用（取第一个命中）。
#[cfg(target_os = "macos")]
fn discover_app_bundle() -> Result<Option<AppBundle>, String> {
    let mut roots = vec![std::path::PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(std::path::PathBuf::from(home).join("Applications"));
    }
    for root in roots {
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            let plist = path.join("Contents").join("Info.plist");
            if !plist.is_file() {
                continue;
            }
            let Ok(bundle_id) = run_capture(
                "plutil",
                &["-extract", "CFBundleIdentifier", "raw", "-o", "-"],
                Some(&plist),
            ) else {
                continue;
            };
            let bundle_id = bundle_id.trim().to_string();
            if !is_openai_bundle_id(&bundle_id) {
                continue;
            }
            let name = run_capture(
                "plutil",
                &["-extract", "CFBundleName", "raw", "-o", "-"],
                Some(&plist),
            )
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("ChatGPT")
                    .to_string()
            });
            return Ok(Some(AppBundle {
                path,
                bundle_id,
                name,
            }));
        }
    }
    Ok(None)
}

/// 该 bundle 路径下是否还有进程在跑（主进程与全部 helper 都以
/// `…/Foo.app/Contents/…` 为命令行前缀，一个模式全覆盖）。
/// `None` = pgrep 本身失败，调用方自行决定兜底方向。
#[cfg(target_os = "macos")]
fn processes_running(bundle_path: &std::path::Path) -> Option<bool> {
    let mut cmd = Command::new("pgrep");
    cmd.arg("-f").arg(bundle_path.as_os_str());
    quiet(&mut cmd);
    let output = cmd.output().ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// 先 TERM 等它自己退（比 Windows 的 taskkill /F 温和），4 秒不退再 KILL。
#[cfg(target_os = "macos")]
fn terminate_app(bundle: &AppBundle) -> Result<(), String> {
    let mut cmd = Command::new("pkill");
    cmd.args(["-TERM", "-f"]).arg(bundle.path.as_os_str());
    quiet(&mut cmd);
    let _ = cmd.output();

    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < deadline {
        if processes_running(&bundle.path) == Some(false) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let mut cmd = Command::new("pkill");
    cmd.args(["-9", "-f"]).arg(bundle.path.as_os_str());
    quiet(&mut cmd);
    let _ = cmd.output();
    std::thread::sleep(Duration::from_millis(500));

    if processes_running(&bundle.path) == Some(false) {
        Ok(())
    } else {
        Err(format!(
            "无法结束 {} 的进程，请手动退出后重试",
            bundle.name
        ))
    }
}

/// 走 LaunchServices 按 bundle id 拉起（等价于点图标）；解不开再退回直接开路径。
#[cfg(target_os = "macos")]
fn relaunch_app(bundle: &AppBundle) -> Result<(), String> {
    let mut cmd = Command::new("open");
    cmd.args(["-b", &bundle.bundle_id]);
    quiet(&mut cmd);
    if cmd.status().is_ok_and(|s| s.success()) {
        return Ok(());
    }
    let mut cmd = Command::new("open");
    cmd.arg(&bundle.path);
    quiet(&mut cmd);
    cmd.status()
        .ok()
        .filter(|s| s.success())
        .map(|_| ())
        .ok_or_else(|| format!("已结束 {} 的进程，但重新启动失败（可手动启动）", bundle.name))
}

#[cfg(target_os = "macos")]
fn run_capture(
    program: &str,
    args: &[&str],
    file: Option<&std::path::Path>,
) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(file) = file {
        cmd.arg(file);
    }
    quiet(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("无法启动 {program}：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("{program} 查询失败：{stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_pid_name_path_lines() {
        let out = concat!(
            "1234\tChatGPT\tC:\\Program Files\\WindowsApps\\OpenAI.Codex_1.0_x64__pub\\ChatGPT.exe\r\n",
            "5678\tCodex\tC:\\Users\\w\\AppData\\Local\\Programs\\OpenAI\\Codex\\Codex.exe\r\n",
        );
        let apps = parse_app_processes(out);
        assert_eq!(
            apps,
            vec![
                AppProcess {
                    pid: 1234,
                    name: "ChatGPT".to_string(),
                    path: "C:\\Program Files\\WindowsApps\\OpenAI.Codex_1.0_x64__pub\\ChatGPT.exe"
                        .to_string(),
                },
                AppProcess {
                    pid: 5678,
                    name: "Codex".to_string(),
                    path: "C:\\Users\\w\\AppData\\Local\\Programs\\OpenAI\\Codex\\Codex.exe"
                        .to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_drops_malformed_lines() {
        let out = "not-a-pid\tX\tY\n\n42\tSandbox\t\n\t\t\n99\tCodex\tC:\\a.exe\n";
        assert_eq!(
            parse_app_processes(out),
            vec![AppProcess {
                pid: 99,
                name: "Codex".to_string(),
                path: "C:\\a.exe".to_string(),
            }]
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn bundle_id_prefix_matches_all_openai_app_generations() {
        assert!(is_openai_bundle_id("com.openai.codex"));
        assert!(is_openai_bundle_id("com.openai.chatgpt"));
        assert!(is_openai_bundle_id("com.openai.chat"));
        assert!(!is_openai_bundle_id("com.apple.finder"));
        assert!(!is_openai_bundle_id("org.openai.codex"));
        assert!(!is_openai_bundle_id("openai.codex"));
    }

    #[test]
    fn discovery_finds_openai_app_when_installed() {
        // 不装应用也必须返回 Ok(None)；装了则必须能读出 bundle id 与名字
        let bundle = discover_app_bundle().unwrap();
        if let Some(bundle) = bundle {
            assert!(is_openai_bundle_id(&bundle.bundle_id));
            assert!(bundle.path.is_dir());
            assert!(!bundle.name.is_empty());
        }
    }

    #[test]
    fn process_probe_answers_false_for_path_without_processes() {
        // 系统目录下必然没有进程，命令行里不会出现这个路径
        assert_eq!(
            processes_running(std::path::Path::new("/Library/TestScripts")),
            Some(false)
        );
    }
}
