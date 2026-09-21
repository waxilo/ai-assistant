//! 官方客户端的进程操作：探活 / 优雅退出 / 重新拉起。
//!
//! # 为什么需要它
//!
//! 接管的落点是客户端 `app.asar.unpacked` 里的 worker 产物，而**客户端只在启动时读它**。
//! 2026-09-21 真机实测纠正了此前的判断（当时以为「每次会话新起进程、会重新读产物」，
//! 于是开/关接管都不需要重启客户端）—— 实际是**必须重启 Qoder 才生效**：
//! 运行中的客户端一直用启动那一刻加载的产物，磁盘改了它也不看。
//! 所以「开/关接管」这个动作的最后一环，就是把正在运行的客户端请出去、再拉起来。
//!
//! # 三条硬规矩（前两条是 traework 助手用事故换来的结论）
//!
//! 1. **退出是「请求」不是「强杀」**：Windows 上 `taskkill` 不带 `/f`（这是编辑器，
//!    强杀会丢未保存内容）；也**不带 `/t`** —— Chromium 系应用那十几个没有窗口的
//!    子进程收不到关闭消息，`/t` 会要求「先退子进程」，父进程于是卡在
//!    「一个或多个子进程仍在运行」上，**一个都不退**（traework 2026-09-16 实测定论：
//!    去掉 `/t` 后同一台机器 2 秒内干净退出）。
//! 2. **`spawn()` 之后绝不 `wait()`**：等的是「它有没有出现」（几百毫秒），
//!    不是「它什么时候退出」（可能数小时）—— 后者会把执行线程占死。
//! 3. **三种结局都要留痕**：真重启了 / 客户端没在运行 / 关了却拉不起来。
//!    把用户正在用的应用关掉是一笔必须交代的账，「都没在运行」这种笼统一句
//!    会把「被关掉且没拉起来」盖住（traework 2026-09-16 实测被坑过一次）。
//!
//! 一切外部命令都经 [`crate::proc::cmd`] 构造（Windows 上不弹黑框）。

use crate::region::Region;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 请求退出后等它退干净的上限。
pub const GRACE_QUIT_MS: u64 = 15_000;
/// 拉起命令发出后，确认它**出现在进程表里**的上限（只覆盖「出现」，不是「启动完成」）。
pub const RELAUNCH_VERIFY_MS: u64 = 8_000;
/// 轮询间隔
const POLL_MS: u64 = 400;

/// 本机这份官方客户端。
pub struct Client {
    region: Region,
    /// 应用包（macOS 的 `.app`）或安装目录（Windows）。
    bundle: PathBuf,
}

/// 一次「收尾重启」的结局。三种都要能报出去（见模块文档第 3 条）。
#[derive(Debug, Clone, PartialEq)]
pub enum RestartOutcome {
    /// 原先在跑：已优雅退出并重新拉起。
    Restarted,
    /// 原先就没在跑（或客户端不在这台机器）—— 下次打开自然读到新产物，不是失败。
    NotRunning,
    /// 退不掉、或退掉了没拉起来。**必须报出去的坏消息**。
    Failed(String),
}

impl Client {
    /// 按区域的安装根候选找一个真的存在的。客户端没装时为 None。
    pub fn discover(region: Region) -> Option<Client> {
        region
            .client_install_dirs()
            .into_iter()
            .find(|p| p.is_dir())
            .map(|bundle| Client { region, bundle })
    }

    pub fn name(&self) -> &'static str {
        self.region.client_name()
    }

    /// 它在不在跑。
    pub fn running(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            // 用**完整 bundle 路径**而不是应用名：Electron 的 helper 进程 argv 里都带着
            // 这个前缀，能一并匹配上；而「应用名」会误伤我们刚发出的 `open` 命令
            // （它的 argv 只含名字）。`pgrep -f` 的 pattern 是 ERE，路径里的 `.`
            // 会被当通配符 —— 无害，换来不引正则转义的复杂度。
            let pattern = self.bundle.to_string_lossy().to_string();
            crate::proc::cmd("pgrep")
                .arg("-f")
                .arg(&pattern)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(target_os = "windows")]
        {
            image_running(&process_images(), &self.exe_name())
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            false
        }
    }

    /// 请求它退出并等到退干净。失败把原因带回去（见模块文档第 1 条）。
    pub fn quit_graceful(&self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            // `quit app "<路径>"` 而不是 `tell application "<名字>"`：路径能精确定位到
            // 这一个应用包，不依赖 LaunchServices 的名字解析（`~/Applications` 里的
            // 同名副本会被解析成谁，不该由我们猜）。
            let out = crate::proc::cmd("osascript")
                .arg("-e")
                .arg(format!("quit app {:?}", self.bundle.to_string_lossy()))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .map_err(|e| format!("调用 osascript 失败：{e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "系统拒绝了结束「{}」的请求（osascript 退出码 {:?}）",
                    self.name(),
                    out.status.code()
                ));
            }
        }
        #[cfg(target_os = "windows")]
        {
            let out = crate::proc::cmd("taskkill")
                .args(graceful_kill_args(&self.exe_name()))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .map_err(|e| format!("调用 taskkill 失败：{e}"))?;
            // **退出码是这里唯一不受系统语言影响的信号**：非 0 = 系统根本没接受这次请求。
            // 报错文本是本地化的（中文系统上是 GBK），塞进界面只会是一串乱码 —— 只留码。
            if !out.status.success() {
                return Err(format!(
                    "系统拒绝了结束「{}」的请求（taskkill 退出码 {:?}）",
                    self.name(),
                    out.status.code()
                ));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            return Err("当前平台不支持结束客户端进程".into());
        }

        if self.wait_for_presence(GRACE_QUIT_MS, false) {
            return Ok(());
        }
        Err(format!(
            "「{}」没有在 {} 秒内退出 —— 它可能正卡在一个确认框上。请手动关闭它后重试。",
            self.name(),
            GRACE_QUIT_MS / 1000
        ))
    }

    /// 重新拉起它。`spawn()` 成功 ≠ 起来了，所以要确认它**出现**（见模块文档第 2 条）。
    pub fn relaunch(&self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            // `open <路径>` 而不是 `open -a <名字>`：名字要经 LaunchServices 解析，
            // 而路径就是我们要的那一个应用包。
            crate::proc::cmd("open")
                .arg(&self.bundle)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("启动 {} 失败：{e}", self.bundle.display()))?;
        }
        #[cfg(target_os = "windows")]
        {
            let exe = self.exe_path()?;
            crate::proc::cmd(&exe)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("启动 {} 失败：{e}", exe.display()))?;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            return Err("当前平台不支持启动客户端".into());
        }

        if !self.wait_for_presence(RELAUNCH_VERIFY_MS, true) {
            return Err(format!(
                "启动命令已发出，但 {} 秒内进程表里还是没有「{}」—— 请确认它是否真的打开了",
                RELAUNCH_VERIFY_MS / 1000,
                self.name()
            ));
        }
        Ok(())
    }

    /// 等「在跑 / 不在跑」与 `want` 一致（或超时）。两个方向共用同一套判据 ——
    /// 否则会出现「以为退干净了、其实没退」或反过来的错配。
    fn wait_for_presence(&self, timeout_ms: u64, want: bool) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.running() == want {
                return true;
            }
            if Instant::now() >= deadline {
                // 超时后再问一次：最后一次轮询与这里之间它可能刚好变了。
                return self.running() == want;
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    /// 它在进程表里的映像名（Windows）。找不到 exe 时返回主猜测而不是报错：
    /// 结束进程的调用点拿主猜测去试一次，比在这里直接失败有意义得多。
    #[cfg(target_os = "windows")]
    fn exe_name(&self) -> String {
        self.exe_path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| format!("{}.exe", self.region.client_name()))
    }

    /// 该客户端在 Windows 上真正的可执行文件。
    #[cfg(target_os = "windows")]
    fn exe_path(&self) -> Result<PathBuf, String> {
        pick_exe(&self.bundle, self.region.client_name())
    }
}

/// 关一次客户端的收尾：在跑就重启，没在跑就如实说。
///
/// 客户端不在这台机器（没装 / 目录结构不对）时按 [`RestartOutcome::NotRunning`] 处理 ——
/// 「没有可重启的东西」和「它自己没开」对调用方是同一件事。
pub fn restart_if_running(region: Region) -> RestartOutcome {
    let Some(c) = Client::discover(region) else {
        return RestartOutcome::NotRunning;
    };
    if !c.running() {
        return RestartOutcome::NotRunning;
    }
    if let Err(e) = c.quit_graceful() {
        return RestartOutcome::Failed(e);
    }
    match c.relaunch() {
        Ok(()) => RestartOutcome::Restarted,
        Err(e) => RestartOutcome::Failed(e),
    }
}

// ---------------------------------------------------------------------------
// 进程表（Windows）
// ---------------------------------------------------------------------------

/// `tasklist /fo csv /nh` 的**全部映像名**（已小写化）。
///
/// ⚠️ 小写化就发生在这里 —— 比较方（[`image_running`]）必须大小写不敏感，
/// 因为另一侧 `client_name()` 保留着品牌名的大写（`Qoder CN.exe`）。
/// traework 助手曾在这里吃过一次大亏：严格相等让「应用在不在跑」**恒为 false**，
/// 后果是「开关接管却不重启应用」而日志里没有任何异常。
#[cfg(target_os = "windows")]
fn process_images() -> Vec<String> {
    let Ok(o) = crate::proc::cmd("tasklist")
        .args(["/fo", "csv", "/nh"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    csv_image_names(&String::from_utf8_lossy(&o.stdout))
}

/// 解析 `tasklist /fo csv /nh` 的输出（纯函数，测试用）。
#[cfg(any(target_os = "windows", test))]
fn csv_image_names(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split("\",\"").next())
        .map(|s| s.trim_matches('"').trim().to_lowercase())
        .collect()
}

/// 进程表里有没有这个映像名。**必须大小写不敏感**（见 [`process_images`]）。
#[cfg(any(target_os = "windows", test))]
fn image_running(images: &[String], exe: &str) -> bool {
    images.iter().any(|n| n.eq_ignore_ascii_case(exe))
}

/// 结束一个客户端时 `taskkill` 的**全部参数**。
///
/// **只有这两个，这是刻意的**：
/// - 不加 `/f` —— 这是编辑器，强杀会丢未保存内容；
/// - 不加 `/t` —— ⚠️ traework 2026-09-16 实测定论，机理见模块文档第 1 条。
///   **别再把它加回来。**
#[cfg(any(target_os = "windows", test))]
fn graceful_kill_args(exe: &str) -> [String; 2] {
    ["/im".to_string(), exe.to_string()]
}

/// 安装目录下的可执行文件定位。
///
/// 主猜测是「目录名 + `.exe`」（`Qoder CN` → `Qoder CN.exe`，本机实测正确），
/// 猜不中时退一步：**目录里唯一那个不是卸载器 / 更新器的 exe 就是它**。
/// 猜不中又不唯一时**宁可报错也不猜** —— 静默失配（关不掉、也拉不起）是这里最贵的错。
#[cfg(any(target_os = "windows", test))]
fn pick_exe(bundle: &Path, client_name: &str) -> Result<PathBuf, String> {
    let primary = bundle.join(format!("{client_name}.exe"));
    if primary.is_file() {
        return Ok(primary);
    }
    let mut cands: Vec<PathBuf> = std::fs::read_dir(bundle)
        .map_err(|e| format!("读安装目录失败（{}）：{e}", bundle.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .map(|x| x.eq_ignore_ascii_case("exe"))
                    .unwrap_or(false)
        })
        .filter(|p| {
            // 卸载器 / 更新器 / 安装器都不是「应用本体」
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            !n.starts_with("unins") && !n.starts_with("update") && !n.contains("setup")
        })
        .collect();
    match cands.len() {
        1 => Ok(cands.remove(0)),
        n => Err(format!(
            "在 {} 里找不到「{client_name}.exe」，也无法唯一定位可执行文件（{n} 个候选）",
            bundle.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qoder-client-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 映像名比较必须大小写不敏感 —— 锁住 traework 那次「接管不重启应用」的根因。
    #[test]
    fn image_running_is_case_insensitive() {
        let images = vec!["qoder cn.exe".to_string(), "explorer.exe".to_string()];
        assert!(image_running(&images, "Qoder CN.exe"), "tasklist 给的是全小写");
        assert!(image_running(&images, "qoder cn.exe"));
        assert!(!image_running(&images, "Qoder.exe"), "国际版与国内版是两个映像");
    }

    /// `tasklist /fo csv /nh` 的行是 `"映像名","PID",…`；首列带引号、名字里可能有空格。
    #[test]
    fn csv_image_names_takes_the_first_column() {
        let out = "\"Qoder CN.exe\",\"12345\",\"Console\",\"1\",\"1,234 K\"\r\n\
                   \"explorer.exe\",\"678\",\"Console\",\"1\",\"55,000 K\"\r\n";
        assert_eq!(
            csv_image_names(out),
            vec!["qoder cn.exe".to_string(), "explorer.exe".to_string()]
        );
    }

    /// 只有这两个参数：`/f` 会丢未保存内容，`/t` 在 Chromium 系应用上一个进程都退不掉。
    #[test]
    fn graceful_kill_args_never_forces_and_never_takes_the_tree() {
        let args = graceful_kill_args("Qoder CN.exe");
        assert_eq!(args, ["/im".to_string(), "Qoder CN.exe".to_string()]);
        assert!(!args.iter().any(|a| a.eq_ignore_ascii_case("/f")), "不许强杀");
        assert!(!args.iter().any(|a| a.eq_ignore_ascii_case("/t")), "不许带 /t");
    }

    #[test]
    fn pick_exe_prefers_the_install_dir_name() {
        let dir = temp_dir("primary");
        std::fs::write(dir.join("Qoder CN.exe"), b"").unwrap();
        std::fs::write(dir.join("其他.exe"), b"").unwrap();
        let got = pick_exe(&dir, "Qoder CN").unwrap();
        assert_eq!(got.file_name().unwrap().to_string_lossy(), "Qoder CN.exe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录名与 exe 名是两件独立的事：主猜测失配时靠「唯一那个不是卸载器/更新器的 exe」兜住。
    #[test]
    fn pick_exe_falls_back_to_the_only_real_exe() {
        let dir = temp_dir("fallback");
        std::fs::write(dir.join("QoderCN.exe"), b"").unwrap();
        std::fs::write(dir.join("Uninstall Qoder CN.exe"), b"").unwrap();
        std::fs::write(dir.join("update.exe"), b"").unwrap();
        let got = pick_exe(&dir, "Qoder CN").unwrap();
        assert_eq!(got.file_name().unwrap().to_string_lossy(), "QoderCN.exe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两个候选说不清是哪个 —— 宁可报错（调用方会把它端给用户），也不猜。
    #[test]
    fn pick_exe_refuses_to_guess_between_two_candidates() {
        let dir = temp_dir("ambiguous");
        std::fs::write(dir.join("a.exe"), b"").unwrap();
        std::fs::write(dir.join("b.exe"), b"").unwrap();
        assert!(pick_exe(&dir, "Qoder CN").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机探针（**只读**）：本机装没装、可执行文件定位到哪个、现在在不在跑。
    ///
    /// 故意**没有**「重启一次试试」的探针 —— 那会把用户正开着的编辑器关掉再拉起来，
    /// 是只有用户自己按得下去的动作。这里能自动验的是它的判据：
    /// exe 主猜测（`<安装目录名>.exe`）与 `tasklist` 的映像名比较是否真能对上。
    ///
    /// ```bash
    /// cargo test --lib -- --ignored --nocapture probe_real_client_presence
    /// ```
    #[test]
    #[ignore = "读本机真实安装（只读，不碰任何进程）"]
    fn probe_real_client_presence() {
        for region in Region::ALL {
            let Some(c) = Client::discover(region) else {
                println!("{}：没装 —— {}", region.client_name(), region.install_hint());
                continue;
            };
            #[cfg(target_os = "windows")]
            let exe = match c.exe_path() {
                Ok(p) => p.display().to_string(),
                Err(e) => format!("定位失败：{e}"),
            };
            #[cfg(not(target_os = "windows"))]
            let exe = c.bundle.display().to_string();
            println!(
                "{}：安装根={}，可执行={}，正在运行={}",
                c.name(),
                c.bundle.display(),
                exe,
                c.running()
            );
        }
    }
}
