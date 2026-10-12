//! 「这个目录能不能写」的记忆。
//!
//! 判定「能不能写进别的应用包」只能靠**真写一次**：macOS 的「App 管理」(TCC) 拦住
//! 对已签名应用包的修改时回的是 `EPERM`，而权限位、owner、卷属性全都正常；
//! 只读卷（从 DMG 里直接运行）同理 —— 只有真写才分得清。
//!
//! 但「真写」就是在**改别的应用**，每次都会触发一次系统权限请求。把它挂在
//! `status()` 这类**查询**路径上（界面一打开就在轮询）等于把权限弹窗变成高频事件，
//! 而这正是「每次启动都要重新授权」的直接成因。所以探测结果必须被记住：
//!
//! * [`lookup`]：**查询路径专用**，只读记忆，绝不碰任何文件；
//! * [`probe_and_remember`]：**即将真写之前**调一次，真写 + 记账。
//!
//! 记忆**只在进程内**存活。助手是托盘常驻进程，一次登录会话内足够；重启后忘记的
//! 代价只是「下次点按钮时重新实测一次」—— 而那一刻本来就要真写，谈不上多一次弹窗。
//! 反过来，把结论落盘等于把它绑死在「应用版本 + 卷 + TCC 状态」上，一个过期的
//! 「可写」比「未知」更容易骗人。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 探针文件名。用点开头，尽量不在别人的安装目录里太扎眼。
pub const PROBE_FILE: &str = ".twa_write_probe";

fn memo() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 查询路径用：读记忆。**绝不写文件。**
///
/// 没实测过时返回 `true`（乐观）—— 把没验证过的功能直接灰掉，比「让用户点一下、
/// 失败了再告诉他怎么授权」更糟：后者至少带着可操作的提示
/// （见 [`crate::endpoint::unwritable_hint`]）。
pub fn lookup(dir: &Path) -> bool {
    memo()
        .lock()
        .ok()
        .and_then(|m| m.get(dir).copied())
        .unwrap_or(true)
}

/// 记下结论，供后续 [`lookup`] 使用。
pub fn remember(dir: &Path, writable: bool) {
    if let Ok(mut m) = memo().lock() {
        m.insert(dir.to_path_buf(), writable);
    }
}

/// 真写一次探针，并把结论记入记忆。
///
/// ⚠️ 这是**写别的应用**的动作，只允许在「马上就要写这个应用」之前调用。
pub fn probe_and_remember(dir: &Path) -> bool {
    let ok = probe_once(dir);
    remember(dir, ok);
    ok
}

/// 裸探测：真写一次再看结果，**不动记忆**（给单测与需要原始事实的地方）。
pub fn probe_once(dir: &Path) -> bool {
    let probe = dir.join(PROBE_FILE);
    let ok = std::fs::write(&probe, b"1").is_ok();
    // 探针必须自清理 —— 别在人家安装目录里留垃圾
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reflects_reality_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("twa-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(probe_once(&dir), "临时目录应当可写");
        assert!(!dir.join(PROBE_FILE).exists(), "探针文件应当被删掉");

        let missing = dir.join("no-such-dir");
        assert!(!probe_once(&missing), "不存在的目录不可写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 查询路径必须零副作用：这是「打开界面就弹权限」那个 bug 的回归测试。
    #[test]
    fn lookup_never_touches_disk() {
        let dir = std::env::temp_dir().join(format!("twa-memo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(lookup(&dir), "没实测过时应当乐观放行，而不是把功能灰掉");
        assert!(!dir.join(PROBE_FILE).exists(), "lookup 绝不能留下任何文件");

        remember(&dir, false);
        assert!(!lookup(&dir), "记下的结论要能读到");
        assert!(!dir.join(PROBE_FILE).exists(), "读记忆同样不该碰盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 探测要顺手把结论记下来，否则查询路径永远学不到东西。
    #[test]
    fn probe_and_remember_updates_lookup() {
        let dir = std::env::temp_dir().join(format!("twa-pnr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(probe_and_remember(&dir));
        assert!(lookup(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
