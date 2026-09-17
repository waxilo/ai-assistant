//! 「接管哪些应用」—— 目标发现、身份与进程控制。
//!
//! ## 为什么不再是「一个写死的名字列表」
//!
//! 旧实现的 `endpoint::app_dir()` 在一张写死的候选名里取**第一个存在**的：
//! `["TRAE SOLO CN", "TRAE", "Trae TRAE", "TRAE CN", "Trae CN"]`。
//! 本机同时装了 `TRAE SOLO CN.app` 与 `Trae CN.app`（同一次构建出的两个 shell，
//! `product.json` 的 version/commit 完全一致），于是后者**永远轮不到** ——
//! 端点改写与免证书补丁都会打在 SOLO 上。这不是配置问题，是结构问题。
//!
//! 现在改成**扫描发现**：在系统应用目录里枚举 `.app`（Windows 下枚举安装目录），
//! 逐个读它的 `Resources/app/product.json`，**只收带 `bootConfig` 的构建**。
//! 于是：
//!
//! - 列表里的每一项都对应**真实存在**的一个应用，界面可以直接把它们列出来让用户勾选；
//! - 上游改版 / 换目录 / 出新区域版都自动跟上，不需要改代码；
//! - 判据是**文件内容**（`bootConfig`），不是名字 —— 名字只用来做稳定 id。
//!
//! ## id 是什么，为什么是它
//!
//! macOS 下 id = `.app` 文件名去掉 `.app`（`TRAE SOLO CN` / `Trae CN`）；
//! Windows 下 = 安装目录名。这一个字符串同时承担三件事：
//!
//! 1. 设置里记录「接管哪些应用」的键（`Settings.takeover_apps`）；
//! 2. 界面上的标签；
//! 3. macOS 上 `open` / `quit app` 与 Windows 上 `taskkill` / 启动 exe 的依据。
//!
//! 一个值三处用，不需要维护任何映射表 —— 也用不着再猜「这个名字属于哪个产品」。
//!
//! ## 进程控制为什么住在这里
//!
//! 「这个应用在不在跑 / 怎么让它退出、再拉起来」是关于**应用**的事实，与「怎么改写它的
//! `product.json`」无关。旧实现把两者都塞在 `endpoint.rs` 里，于是多目标改造时
//! 它们会被迫一起变形。分开之后 `endpoint.rs` 只谈配置、这里只谈进程。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 产品配置文件（相对 [`AppTarget::app_dir`]）。
pub const PRODUCT_FILE: &str = "product.json";
/// 闸门补丁所在文件（相对 [`AppTarget::app_dir`]）。
pub const MAIN_JS_REL: &str = "out/main.js";
/// 等应用优雅退出的上限。
///
/// 实测（Trae CN，15 个进程）**2 秒内**就退干净了，15 秒是留给「它在存盘」的余量。
/// 上界不能太长：`with_restart` 是同步命令里的阻塞段，用户在界面上等着。
pub const GRACE_QUIT_MS: u64 = 15_000;
/// 发出启动命令后，等应用**出现**在进程表里的上限。
///
/// 正常几百毫秒内就出现；这是「它到底起来了没有」的确认窗口，不是启动超时。
/// 见 [`AppTarget::relaunch`] —— 实测见过「spawn 成功但进程 2 秒后静默消失」。
pub const RELAUNCH_VERIFY_MS: u64 = 4_000;

/// 一个**本机真实存在**的可接管应用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTarget {
    /// 稳定 id，见模块文档。
    pub id: String,
    /// 应用包（macOS 的 `.app`）或安装目录（Windows）。
    pub bundle: PathBuf,
    /// `<bundle>/…/Resources/app`，内含 `product.json` 与 `out/main.js`。
    pub app_dir: PathBuf,
}

impl AppTarget {
    pub fn product_path(&self) -> PathBuf {
        self.app_dir.join(PRODUCT_FILE)
    }

    pub fn main_js_path(&self) -> PathBuf {
        self.app_dir.join(MAIN_JS_REL)
    }

    /// 该应用当前是否在运行（自己取一次进程快照）。
    ///
    /// ⚠️ **一次状态查询里有几个应用就会调用几次** —— Windows 实现是 spawn 一次
    /// `tasklist`，那是几百毫秒的开销，而且每次都会弹一个控制台窗口（见 `proc.rs`）。
    /// 要在一次查询里问多个应用，请先用 [`process_images`] 取**一份**快照，
    /// 再用 [`running_in`] 逐个判断（`commands::build_status` 就是这么做的）。
    pub fn running(&self) -> bool {
        self.running_in(&process_images())
    }

    /// 用一份**已取好的**进程映像快照判断是否在运行。
    ///
    /// `images` 只在 Windows 上有意义（`tasklist /fo csv /nh` 的全部映像名）；
    /// macOS 走 `pgrep -f <bundle 路径>`，不需要进程表；其它平台恒 false。
    /// 参数保留是为了让调用点跨平台统一 —— 否则 `commands::build_status`
    /// 得为三个平台各写一遍取快照的代码。
    pub fn running_in(&self, images: &[String]) -> bool {
        #[cfg(target_os = "macos")]
        {
            let _ = images;
            // ⚠️ 用**完整 bundle 路径**而不是应用名：既能把 Electron 的主进程与各 helper
            //    一起匹配上（它们都在 `<bundle>/Contents/…` 里），又不会误伤
            //    「我们自己刚发出的 `open` 命令」那种 argv 只含名字的进程。
            //    `pgrep -f` 的 pattern 是 ERE，路径里的 `.` 会被当通配符 —— 无害
            //    （多匹配一个字符不会命中别的应用），换来不引正则转义的复杂度。
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
            image_running(images, &process_image_name(&self.id))
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = images;
            false
        }
    }

    /// 请求该应用退出并等待其结束。**失败时必须把原因带回去**（返回 `Err`）。
    ///
    /// Windows 用 `taskkill /im <exe>`，两个「不加」都是刻意的：
    /// - **不加 `/f`** —— 这是编辑器，强杀可能丢未保存内容；
    /// - **不加 `/t`** —— ⚠️ 2026-09-16 实测定论。`/t` 会让 `taskkill` 要求「先把子进程退掉」，
    ///   而 Chromium 系应用那十几个**没有窗口**的子进程收不到关闭消息（它对每个都报
    ///   「只能强行终止这个进程(带 /F 选项)」），于是父进程被卡在
    ///   「一个或多个此进程的子进程仍然在运行」上 ⇒ **15 个进程一个都不退**，
    ///   界面上就是「已发起退出请求，但「Trae CN」未在限时内退出，请手动关闭后重试」。
    ///   去掉 `/t` 后同一台机器 **2 秒内全部干净退出**（实测，无 `/f`）。
    ///
    /// 为什么这个 bug 藏了这么久：[`AppTarget::running`] 曾经恒为 false（见 [`image_running`]），
    /// 于是**根本走不到这里**；修掉那个之后，这里又把 `taskkill` 的退出码和输出一起
    /// `let _ =` 丢掉了 —— 「它拒绝执行」（`/t` 那种：退出码 128、满屏错误）和
    /// 「它执行了但应用不理它」在日志里长得一模一样。**同一个盲区换了层皮。**
    pub fn quit_graceful(&self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            // `quit app "<路径>"` 而不是 `tell application "<名字>"`：路径能精确定位到
            // 这一个应用包，不依赖 LaunchServices 的名字解析（`~/Applications` 里的同名副本
            // 与 `/Applications` 里的会被解析成谁，不该由我们猜）。
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
                    self.id,
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
                    self.id,
                    out.status.code()
                ));
            }
        }
        if wait_for_exit(self, GRACE_QUIT_MS) {
            return Ok(());
        }
        // 报「还剩几个进程」而不是笼统的「没退出」：「一个都没退」和「退了一半」
        // 是两种不同的病（前者系统没接受请求，后者应用自己卡住了），分开才查得动。
        Err(format!(
            "「{}」没有在 {} 秒内退出（进程表里还剩 {} 个进程）—— \
             它没有响应系统的关闭请求，可能正卡在一个确认框上。请手动关闭它后重试。",
            self.id,
            GRACE_QUIT_MS / 1000,
            self.image_count()
        ))
    }

    /// 进程表里还剩下几个属于它的进程。见 [`quit_graceful`](Self::quit_graceful) 末尾。
    fn image_count(&self) -> usize {
        #[cfg(target_os = "windows")]
        {
            image_count(&process_images(), &self.exe_name())
        }
        #[cfg(not(target_os = "windows"))]
        {
            usize::from(self.running())
        }
    }

    /// 重新启动该应用。**失败要把原因带回去**（见 [`RestartOutcome`]）。
    ///
    /// ⚠️ `spawn()` 之后**必须立刻返回，绝不能 `wait()`**：`wait()` 会阻塞到应用进程退出
    /// 为止（可能数小时），一旦被同步命令调用就会把执行线程彻底占死 —— 这正是历史上
    /// 「助手卡死」的根因。这里等的只是「它有没有**出现**」（几百毫秒），不是「它什么时候退出」。
    ///
    /// ⚠️ 以前这里返回 `bool`（`spawn().is_ok()`），把错误**吞掉了** —— 于是「关掉了却拉不起来」
    /// 和「本来就没在跑」在日志里长得一模一样。2026-09-16 实测被坑：用户的 Trae 被我们退出、
    /// 又没被拉起来（他只能自己手动开），而接管动态写的是「都没在运行」。
    /// **关掉别人正在用的应用是一件必须交代的事**，所以这里一律把原因带回调用方。
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
            // 「文件在不在」交给 [`Self::exe_path`] 判 —— 它同时负责「目录名与 exe 名不一致」
            // 这个并不罕见的情况（报出来比一句 system cannot find the file 清楚得多）。
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
            return Err("当前平台不支持重启应用".into());
        }

        // ⚠️ `spawn()` 成功 **不等于** 应用起来了。2026-09-16 实测撞到过这种：某些执行环境里
        // 启动这些 Chromium 应用，进程会在 2~3 秒内**以退出码 0 静默消失**（连日志目录都不建，
        // 也不留任何 stderr），而 `spawn()` 全程返回成功。不确认一下的话，
        // 「关得掉、拉不回来」就又是一个静默失败 —— 恰好是这一整轮在消灭的东西。
        // 代价是几百毫秒（只在重启时做一次），换「它到底起来了没有」这个确切答案。
        if !wait_for_start(self, RELAUNCH_VERIFY_MS) {
            return Err(format!(
                "启动命令已发出，但 {} 秒内进程表里还是没有「{}」—— 请确认它是否真的打开了",
                RELAUNCH_VERIFY_MS / 1000,
                self.id
            ));
        }
        Ok(())
    }

    /// 该应用在 Windows 上真正的可执行文件。
    ///
    /// 主猜测是「安装目录名 + `.exe`」（`Trae CN` → `Trae CN.exe`），本机实测正确。
    /// 但**目录名与 exe 名是两件独立的事** —— 这台机器上窗口标题写着 `TraeCode CN`，
    /// 说明品牌名在 Windows 上确实有第二种写法；哪天目录名不改而 exe 改名，主猜测就会
    /// **静默失配**，代价是「关不掉 + 拉不起」而且不留一句错误信息（今天已因大小写吃过一次亏）。
    /// 所以猜不中时退一步：**目录里唯一那个不是卸载器/更新器的 exe 就是它**。
    #[cfg(target_os = "windows")]
    fn exe_path(&self) -> Result<PathBuf, String> {
        let primary = self.bundle.join(process_image_name(&self.id));
        if primary.is_file() {
            return Ok(primary);
        }
        let mut cands: Vec<PathBuf> = std::fs::read_dir(&self.bundle)
            .map_err(|e| format!("读安装目录失败（{}）：{e}", self.bundle.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .map(|x| x.eq_ignore_ascii_case("exe"))
                        .unwrap_or(false)
            })
            .filter(|p| {
                // 卸载器 / 更新器 / 安装器都不是「应用本体」。
                let n = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase();
                !n.starts_with("unins") && !n.starts_with("update") && !n.contains("setup")
            })
            .collect();
        match cands.len() {
            1 => Ok(cands.remove(0)),
            n => Err(format!(
                "在 {} 里找不到「{}」，也无法唯一定位可执行文件（{} 个候选）",
                self.bundle.display(),
                process_image_name(&self.id),
                n
            )),
        }
    }

    /// 它在进程表里的映像名（Windows）。
    ///
    /// 找不到 exe 时**返回主猜测而不是报错**：结束进程的调用点拿主猜测去试一次，
    /// 比在这里直接失败有意义得多（进程可能正跑着，而文件恰好读不到）。
    #[cfg(target_os = "windows")]
    fn exe_name(&self) -> String {
        self.exe_path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| process_image_name(&self.id))
    }
}

/// 轮询等待该应用完全退出，直到 `timeout_ms` 毫秒。
pub fn wait_for_exit(target: &AppTarget, timeout_ms: u64) -> bool {
    wait_for_presence(target, timeout_ms, false)
}

/// 轮询等待该应用**出现**在进程表里，直到 `timeout_ms` 毫秒。
///
/// 见 [`AppTarget::relaunch`]：`spawn()` 成功不等于应用起来了。
pub fn wait_for_start(target: &AppTarget, timeout_ms: u64) -> bool {
    wait_for_presence(target, timeout_ms, true)
}

/// 等到「在跑 / 不在跑」与 `want_running` 一致（或超时）。
///
/// 抽出来是因为两个方向**必须是同一套判据**：退出用「进程表里还有没有」，
/// 启动也用同一个 —— 否则会出现「以为退干净了、其实没退」或反过来的错配。
fn wait_for_presence(target: &AppTarget, timeout_ms: u64, want_running: bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if target.running() == want_running {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            // 超时后再问一次：最后一次轮询与这里之间它可能刚好变了。
            return target.running() == want_running;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

/// [`with_restart`] 的结果。
///
/// ⚠️ 以前这里只有一个 `Vec<String>`（成功拉起来的那些），**失败被 `is_ok()` 吞掉**——
/// 于是「关掉了但没拉起来」和「本来就没在跑」在日志里长得一模一样，都用一句「没有应用重启」打发。
/// 2026-09-16 实测被坑：用户的 Trae 被我们退出、又没被拉回来（他只能自己手动开），
/// 而接管动态写的是「都没在运行」—— 一句话把真相盖住。
///
/// **把应用从用户手里关掉，是一笔必须交代清楚的账**，所以成功与失败分开记。
#[derive(Debug, Default, PartialEq)]
pub struct RestartOutcome {
    /// 退出后成功拉起来的应用 id。
    pub restarted: Vec<String>,
    /// 退出成功、但**没能拉起来**的 `(应用 id, 原因)`。这是必须报出去的坏消息：
    /// 他的应用被关掉了，而且不会自己回来。
    pub failed: Vec<(String, String)>,
}

impl RestartOutcome {
    /// 有没有应用被我们关掉过（不管后来拉起来没有）。
    pub fn touched(&self) -> bool {
        !self.restarted.is_empty() || !self.failed.is_empty()
    }
}

/// 在「这些应用运行中则先退出」的前提下执行 `op`，执行完再把**原先在跑的那些**拉起来。
///
/// 返回 `(op 结果, 重启结果)`。
///
/// 四件事是刻意的：
/// 1. **只碰传进来的目标** —— 改一个应用的配置不该重启另一个应用；
/// 2. `op` 失败也要把应用拉回来（`op` 被闸门挡住时应用已经被我们关掉了，
///    不能因为返回 `Err` 就把它留在关闭状态，2026-09-14 实测踩过）；
/// 3. 中途有应用退不掉时，把**已经退出的那些**先拉回来再报错 —— 半关半开是最糟的状态。
/// 4. **拉起失败不算致命，但必须回传**：`op` 已经写盘成功，此刻报错会让用户以为整件事失败了；
///    真正要做的是告诉他「应用被关了、没拉起来、请手动打开」（见 [`RestartOutcome`]）。
pub fn with_restart<T>(
    targets: &[AppTarget],
    op: impl FnOnce() -> Result<T, String>,
) -> Result<(T, RestartOutcome), String> {
    let mut stopped: Vec<&AppTarget> = Vec::new();
    for t in targets {
        if !t.running() {
            continue;
        }
        if let Err(e) = t.quit_graceful() {
            // 半关半开是最糟的状态：把**已经退出的那些**先拉回来，再如实报错。
            // 报「拉回来了谁」是因为它们确实被我们关过一次 —— 那笔账不能省。
            let back: Vec<String> = stopped
                .iter()
                .filter(|d| d.relaunch().is_ok())
                .map(|d| d.id.clone())
                .collect();
            return Err(if back.is_empty() {
                e
            } else {
                format!("{e}（已把先退出的 {} 重新拉起）", back.join("、"))
            });
        }
        stopped.push(t);
    }

    let out = op();

    let mut outcome = RestartOutcome::default();
    for t in &stopped {
        match t.relaunch() {
            Ok(()) => outcome.restarted.push(t.id.clone()),
            Err(e) => outcome.failed.push((t.id.clone(), e)),
        }
    }
    Ok((out?, outcome))
}

// ---------------------------------------------------------------------------
// 发现
// ---------------------------------------------------------------------------

/// 枚举本机的应用容器目录（macOS：`/Applications`、`~/Applications`）。
///
/// 顺序即优先级：同名 id 以**先到的**为准（用户目录里的副本就当不存在，避免同一个应用
/// 出现两行、勾了一个另一个没勾）。
pub fn roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from("/Applications"));
        if let Some(home) = dirs::home_dir() {
            out.push(home.join("Applications"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        for base in [dirs::data_local_dir(), dirs::data_dir()].into_iter().flatten() {
            // 用户级安装（默认）：%LOCALAPPDATA%\Programs\<app>；少数安装器直接落在 %LOCALAPPDATA%\<app>
            out.push(base.join("Programs"));
            out.push(base);
        }
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(key) {
                out.push(PathBuf::from(pf));
            }
        }
    }
    out
}

/// `<bundle>` → `<bundle>/…/Resources/app`。
pub fn app_dir_of(bundle: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        bundle.join("resources").join("app")
    }
    #[cfg(not(target_os = "windows"))]
    {
        bundle.join("Contents").join("Resources").join("app")
    }
}

/// 这个 `Resources/app` 是不是一个可接管的 Trae 构建。
///
/// 判据只有一条：`product.json` 能被解析、且顶层有 **`bootConfig` 对象**。
/// 那正是决定端点域名的那份配置（见 `endpoint.rs` 模块文档），也是 Trae 系构建独有的字段
/// —— VS Code / 其它 Electron 应用的同名文件里没有它（本机 `Kiro.app` 就是反例）。
///
/// 本助手自己也不会有这一份文件（Tauri 的 `Resources/` 里没有 `app/product.json`），
/// 所以不需要为「别把自己列成目标」再写一条特例。
pub fn is_trae_app(app_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(app_dir.join(PRODUCT_FILE)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    v.get("bootConfig").map(serde_json::Value::is_object).unwrap_or(false)
}

/// `.app` / 安装目录 → 稳定 id。
fn id_of(bundle: &Path) -> Option<String> {
    let name = bundle.file_name()?.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    {
        name.strip_suffix(".app")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (!name.is_empty()).then_some(name)
    }
}

/// 本机**全部**可接管的 Trae 应用，按 id 升序（顺序稳定，界面不会跳）。
pub fn discover() -> Vec<AppTarget> {
    let mut out: Vec<AppTarget> = Vec::new();
    for root in roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue; // 目录不存在 / 读不了都不是错误（Windows 上 ProgramFiles(x86) 常见缺席）
        };
        for e in entries.flatten() {
            let bundle = e.path();
            if !bundle.is_dir() {
                continue;
            }
            // macOS 上先按后缀筛掉绝大部分条目，再去看那个 200 字节的 product.json。
            // 目录遍历本身很便宜，真正的成本只有「文件确实存在」时的 read + parse。
            let app_dir = app_dir_of(&bundle);
            if !is_trae_app(&app_dir) {
                continue;
            }
            let Some(id) = id_of(&bundle) else {
                continue;
            };
            if out.iter().any(|t| t.id == id) {
                continue; // 同名副本（用户目录 vs /Applications）以先到的为准
            }
            out.push(AppTarget { id, bundle, app_dir });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 把设置里的 id 名单解析成**真实存在的**目标。
///
/// **空名单 = 全部**（与「参与扣费的账号」`billing_account_ids` 同一套语义：空 = 没配置过
/// = 全部参与）。非空时按发现顺序取交集，名单里本机没有的 id 被静默忽略
/// （要报「你选的应用不在本机」请用 [`missing`]）。
pub fn select(ids: &[String]) -> Vec<AppTarget> {
    let all = discover();
    if ids.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|t| ids.iter().any(|i| i == &t.id))
        .collect()
}

/// 名单里本机**不存在**的 id。界面靠它说清「你点名的应用已经不在了」。
pub fn missing(ids: &[String]) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    let all = discover();
    ids.iter()
        .filter(|i| !all.iter().any(|t| &t.id == *i))
        .cloned()
        .collect()
}

/// 某个 id 是否在接管名单里（**空名单 = 全部命中**，与 [`select`] 同一套语义）。
pub fn is_selected(ids: &[String], id: &str) -> bool {
    ids.is_empty() || ids.iter().any(|i| i == id)
}

/// 「默认目标」= 发现列表里的第一个。
///
/// ⚠️ **只给与接管无关的读用途**（`x-app-version` 这种「这个应用是什么版本」的问题）。
/// 接管本身一律显式带目标 —— 这正是本次改造要消灭的那种隐式单目标假设。
pub fn default_target() -> Option<AppTarget> {
    discover().into_iter().next()
}

// ---------------------------------------------------------------------------
// 进程辅助
// ---------------------------------------------------------------------------

/// 进程映像表里有没有这个可执行文件。
///
/// ⚠️ **必须大小写不敏感** —— 两侧的大小写是各自决定的：
/// [`process_images`] 把 `tasklist` 的映像名**统一转成小写**（`trae cn.exe`），
/// 而 [`process_image_name`] 直接拼 `{id}.exe`，保留了 id 里的大写（`Trae CN.exe`）。
/// 原来这里是 `images.iter().any(|n| *n == process_image_name(&self.id))` 的**严格相等**，
/// 于是在 Windows 上**恒为 false**。后果（2026-09-16 由用户提问
/// 「为什么我开启接管不会重启 Trae」定位到）：
/// - [`with_restart`] 认为「没有任何目标在运行」⇒ **从不重启**，端点改写要等用户自己重开应用；
/// - `commands::build_status` 里每个应用的「运行中」也永远是假。
///
/// 之所以抽成自由函数：原来那句藏在 `#[cfg(target_os = "windows")]` 分支里，
/// 任何跨平台测试都碰不到它 —— 这个 bug 就活过了整套单测。
#[cfg(any(target_os = "windows", test))]
fn image_running(images: &[String], exe: &str) -> bool {
    images.iter().any(|n| n.eq_ignore_ascii_case(exe))
}

/// 进程表里还剩几个 `exe`。
///
/// 与 [`image_running`] 分开是因为它们回答**不同的问题**：那个说「还在不在」，
/// 这个说「还剩几个」。杀掉一个 15 进程的应用时，这个数能把两种失败区分开 ——
/// 一个都没退（系统压根没接受请求）vs 退了一半（应用自己卡在某一步）。
#[cfg(any(target_os = "windows", test))]
fn image_count(images: &[String], exe: &str) -> usize {
    images.iter().filter(|n| n.eq_ignore_ascii_case(exe)).count()
}

/// 结束一个应用时 `taskkill` 的**全部参数**。
///
/// **只有这两个，这是刻意的：**
/// - 不加 `/f` —— 这是编辑器，强杀会丢未保存内容；
/// - 不加 `/t` —— ⚠️ 它是 2026-09-16 那个「开关接管却不重启应用」的根因，
///   机理见 [`AppTarget::quit_graceful`]。**别再把它加回来。**
///
/// 抽成自由函数的理由和 [`image_running`] 一模一样：上一版把 `["/im", exe, "/t"]`
/// 直接写在 `#[cfg(target_os = "windows")]` 分支里，**任何单测都看不见它**，
/// 于是这个 bug 活过了整套测试。凡是「藏在平台 cfg 里的判断」，都得抽出来。
#[cfg(any(target_os = "windows", test))]
fn graceful_kill_args(exe: &str) -> [String; 2] {
    ["/im".to_string(), exe.to_string()]
}

/// Windows 进程映像名（`tasklist` 里那一列）。
#[cfg(target_os = "windows")]
fn process_image_name(id: &str) -> String {
    format!("{id}.exe")
}

/// 当前进程表里的全部映像名（小写）。
///
/// **一次查询取一份**，供多个应用共用（见 `running_in`）—— 每个应用各 spawn 一次
/// `tasklist` 会让「刷新一次接管状态」凭空多花几百毫秒。
#[cfg(target_os = "windows")]
pub fn process_images() -> Vec<String> {
    let Ok(o) = crate::proc::cmd("tasklist")
        .args(["/fo", "csv", "/nh"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.split("\",\"").next())
        .map(|s| s.trim_matches('"').to_lowercase())
        .collect()
}

/// 非 Windows 平台没有「映像名表」这个概念（macOS 走 `pgrep` 按 bundle 路径匹配）。
/// 返回空表即可 —— 这样调用点不必写 `#[cfg]`，「先取快照再逐个判断」的写法
/// 才能在三个平台上都编得过。
#[cfg(not(target_os = "windows"))]
pub fn process_images() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `running_in` 在 Windows 上曾经**恒为 false**：`process_images()` 给小写化的映像名，
    /// 却拿去跟 `process_image_name()` 产出的 `"Trae CN.exe"`（保留 id 的大写）做严格相等。
    /// 这条测试锁住「映像名比较必须大小写不敏感」—— 它是「开接管不重启应用」的根因。
    #[test]
    fn image_running_is_case_insensitive() {
        let images = vec!["trae cn.exe".to_string(), "explorer.exe".to_string()];
        assert!(
            image_running(&images, "Trae CN.exe"),
            "tasklist 给的是全小写，不能拿去跟带大写的映像名比"
        );
        assert!(image_running(&images, "trae cn.exe"));
        assert!(
            !image_running(&images, "TRAE SOLO CN.exe"),
            "别把本机另一个应用也算成它"
        );
        assert!(!image_running(&[], "Trae CN.exe"), "没有进程就是没在跑");
    }

    /// 结束一个应用时**绝不能带 `/t`，也不能带 `/f`**。
    ///
    /// `/t` 会要求「先把子进程退掉」，而 Chromium 系应用那十几个**无窗口**的子进程
    /// 收不到关闭消息（taskkill 对每个都报「只能强行终止这个进程(带 /F 选项)」），
    /// 于是父进程被卡在「一个或多个此进程的子进程仍然在运行」上 —— 实测 15 个进程
    /// **一个都不退**，界面上就是「已发起退出请求，但未在限时内退出」。
    /// 去掉 `/t` 后同一台机器 2 秒内全部干净退出。这条测试就是那次实测的存档。
    #[test]
    fn graceful_kill_must_not_force_or_recurse() {
        let args = graceful_kill_args("Trae CN.exe");
        assert_eq!(args, ["/im".to_string(), "Trae CN.exe".to_string()]);
        assert!(
            !args.iter().any(|a| a.eq_ignore_ascii_case("/t")),
            "带 /t 会把父进程卡在「子进程仍然在运行」上：{args:?}"
        );
        assert!(
            !args.iter().any(|a| a.eq_ignore_ascii_case("/f")),
            "带 /f 是强杀编辑器，会丢未保存内容：{args:?}"
        );
    }

    /// 「还剩几个进程」是区分两种失败的**唯一**依据，别把它退化成 bool：
    /// 「一个都没退」= 系统没接受请求，「退了一半」= 应用自己卡住了。
    /// 顺带确认它和 [`image_running`] 一样大小写不敏感。
    #[test]
    fn image_count_reports_survivors() {
        let images = vec![
            "trae cn.exe".to_string(),
            "TRAE CN.EXE".to_string(),
            "explorer.exe".to_string(),
        ];
        assert_eq!(image_count(&images, "Trae CN.exe"), 2);
        assert_eq!(image_count(&images, "TraeCode CN.exe"), 0);
        assert_eq!(image_count(&[], "Trae CN.exe"), 0);
    }

    /// exe 名的退路：**目录名与 exe 名是两件独立的事**。
    ///
    /// 本机实测目录/exe 都是 `Trae CN`，但窗口标题写着 `TraeCode CN` —— 不一致是真实存在的。
    /// 主猜测（`{目录名}.exe`）猜不中时，取目录里唯一那个非卸载器/更新器的 exe，
    /// 而不是静默失配成「关不掉 + 拉不起、且一句错误信息都没有」。
    #[cfg(target_os = "windows")]
    #[test]
    fn exe_path_falls_back_to_single_candidate() {
        // ⚠️ Windows 上 `bundle` 就是**安装目录**（exe 住在这一层），`app_dir` 是它的
        // `resources/app`。别把 exe 写进 `app_dir` —— 那会测出一个真实世界里不存在的布局。
        let root = fixture("exefall");
        let bundle = root.join("SomeApp.app");
        let app = app_dir_of(&bundle);
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join(PRODUCT_FILE), "{}").unwrap();
        std::fs::write(bundle.join("unins000.exe"), b"x").unwrap();
        std::fs::write(bundle.join("SomethingElse.exe"), b"x").unwrap();

        let t = AppTarget {
            id: "SomeApp".into(),
            bundle: bundle.clone(),
            app_dir: app,
        };
        // 目录名叫 SomeApp、exe 叫 SomethingElse ⇒ 靠退路找出来（卸载器必须被排除）
        assert_eq!(t.exe_path().unwrap(), bundle.join("SomethingElse.exe"));
        // 主猜测存在时优先用它
        std::fs::write(bundle.join("SomeApp.exe"), b"x").unwrap();
        assert_eq!(t.exe_path().unwrap(), bundle.join("SomeApp.exe"));
    }

    /// 造一个「长得像 Trae 的 Resources/app」和几个反例。
    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("twa-target-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn win_app(root: &Path, name: &str, product: &str) -> PathBuf {
        let app = app_dir_of(&root.join(format!("{name}.app")));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join(PRODUCT_FILE), product).unwrap();
        app
    }

    #[test]
    fn app_dir_layout_is_the_usual_one() {
        let d = app_dir_of(Path::new("/Applications/X.app"));
        assert!(d.ends_with("app"), "{}", d.display());
        // ⚠️ 资源目录名**必须分平台断言**：Electron 在 Windows 上发的是全小写的
        //    `resources`（`<安装目录>\resources\app`），只有 macOS 才是
        //    `Contents/Resources/app`。原来这里写死 `contains("Resources")`，
        //    于是这条测试在 Windows 上**永远失败** —— 与任何改动无关的既有问题
        //    （2026-09-16 修「切菜单弹命令框」时跑 `cargo test --lib` 才发现）。
        let res = d
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        #[cfg(target_os = "windows")]
        assert_eq!(res, "resources", "{}", d.display());
        #[cfg(not(target_os = "windows"))]
        assert_eq!(res, "Resources", "{}", d.display());
    }

    /// 判据是 `bootConfig`，不是名字 —— 这样上游出新区域版、或换个名字都不影响。
    #[test]
    fn only_builds_with_boot_config_are_targets() {
        let root = fixture("is-trae");
        let yes = win_app(&root, "TRAE SOLO CN", r#"{"bootConfig":{"remote":{"trae":{"normal":"https://a"}}}}"#);
        let no_boot = win_app(&root, "Kiro", r#"{"nameShort":"Kiro","version":"1.0"}"#);
        let not_json = win_app(&root, "Broken", "{ 这不是 JSON");
        assert!(is_trae_app(&yes));
        assert!(!is_trae_app(&no_boot), "没有 bootConfig 的不是目标（本机 Kiro.app 就是反例）");
        assert!(!is_trae_app(&not_json), "读不懂就不认");
        assert!(!is_trae_app(&root.join("no-such-app").join("app")), "不存在 = 不是");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `bootConfig` 必须是对象：`"bootConfig": "x"` 是别的什么也不该被接管。
    #[test]
    fn boot_config_must_be_an_object() {
        let root = fixture("boot-type");
        let p = win_app(&root, "Weird", r#"{"bootConfig":"nope"}"#);
        assert!(!is_trae_app(&p));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn id_drops_the_app_suffix() {
        assert_eq!(id_of(Path::new("/Applications/Trae CN.app")).as_deref(), Some("Trae CN"));
        assert_eq!(id_of(Path::new("/Applications/A.app")).as_deref(), Some("A"));
        assert_eq!(id_of(Path::new("/Applications/.app")), None);
    }

    /// 空名单 = 全部；非空 = 交集；本机没有的 id 被忽略且能被 [`missing`] 报出来。
    #[test]
    fn empty_selection_means_everything() {
        assert!(is_selected(&[], "任意"));
        assert!(is_selected(&["Trae CN".into()], "Trae CN"));
        assert!(!is_selected(&["Trae CN".into()], "TRAE SOLO CN"));
    }

    /// `discover()` 的顺序必须稳定：界面按它渲染，顺序跳会让「我点的那个」对不上号。
    #[test]
    fn discovery_order_is_stable() {
        let a = AppTarget {
            id: "TRAE SOLO CN".into(),
            bundle: PathBuf::from("/A.app"),
            app_dir: PathBuf::from("/A.app/Contents/Resources/app"),
        };
        let b = AppTarget {
            id: "Trae CN".into(),
            bundle: PathBuf::from("/B.app"),
            app_dir: PathBuf::from("/B.app/Contents/Resources/app"),
        };
        let mut v = vec![b.clone(), a.clone()];
        v.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(v[0].id, "TRAE SOLO CN", "大写先于小写 ⇒ SOLO 在 Trae CN 之前");
        assert_eq!(v[1].id, "Trae CN");
    }

    /// 真机只读：本机发现了哪些可接管应用（多目标改造的第一道验证）。
    ///
    /// ```text
    /// cargo test --lib -- --nocapture live_discover_apps
    /// ```
    #[test]
    fn live_discover_apps() {
        let found = discover();
        for t in &found {
            eprintln!(
                "  id={:<16} bundle={} running={} main.js={}",
                t.id,
                t.bundle.display(),
                t.running(),
                t.main_js_path().exists()
            );
        }
        eprintln!("本机发现 {} 个可接管应用", found.len());
        // 只要有发现，id 就必须唯一、且 product.json 真的在
        for t in &found {
            assert!(t.product_path().exists(), "{} 的 product.json 不见了", t.id);
        }
        let mut ids: Vec<&str> = found.iter().map(|t| t.id.as_str()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "id 必须唯一");
    }

}
