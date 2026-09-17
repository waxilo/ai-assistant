//! TraeWork **自己的**用户设置文件：`<userData>/User/settings.json`。
//!
//! ## 这个模块现在只做一件事：清扫旧版本留下的痕迹
//!
//! 它曾经是「经系统代理接管」（A″）的**写入方** —— 把下面三个键指向本机代理，
//! 让 TraeWork 的**全应用**流量经过本地反代做 TLS 中间人：
//!
//! | 键 | 值域 |
//! |---|---|
//! | `trae.network.proxy.mode` | `system` \| `direct` \| `manual` \| `cloud` |
//! | `trae.network.proxy.manual.proxy` | `http://host:port`（也认 socks4/5、https） |
//! | `trae.network.proxy.manual.noProxy` | 逗号分隔 |
//!
//! 那条路已按用户要求**整体移除**（连同 `tls.rs` / `tunnel.rs`）：它必须把自签 CA
//! 装进系统信任库，代价是动系统信任设置；而保留下来的「端点改写 + 免证书」不需要。
//!
//! ⚠️ **但清理代码必须留下，而且要一直跑。** 理由与下面「有没有备份」那条同源：
//! 改道动作在**用户的机器上**留了痕 —— 老版本可能已经把 `mode` 写成 `manual`、
//! 把 `manual.proxy` 指到了 `127.0.0.1:8788`。新版反代不再做正向代理
//! （收到 CONNECT 一律 405），那个设置留着就是**整个应用不可用**。
//! 所以 [`uninstall`] 会在**启动清扫**与**关闭接管**两处被调用，判据是
//! 「值确实指向本机回环」（[`is_loopback_proxy`]）。
//!
//! ## 铁律（清理路径上同样成立）
//!
//! 1. **只在确认是我们写的值时动手**：`manual.proxy` 必须指向本机回环。
//!    用户自己配的公司代理（`mode = manual` + 一个真实代理地址）一个字节都不许碰 ——
//!    抹掉它等于改坏他的环境。
//! 2. **读不懂就不动**。`settings.json` 不是合法 JSON 时直接报错返回，
//!    绝不「覆盖成一个干净的」——那是把用户配置整个抹掉。
//! 3. **无备份 ≠ 没做过**。备份文件在助手自己的数据目录里，会随用户清理、换机、重装一起
//!    消失（本机真发生过：整个数据目录被清空，备份随之没了）。所以没有备份时走
//!    [`clear_ours`]，而不是回一句「无需还原」。
//!
//! ## 已知的、无害的副作用
//!
//! 落盘时会**整份重新序列化**，`serde_json` 的 `Map` 是 `BTreeMap` ⇒ 键会按字母序排。
//! 键值一个都不会变（有测试钉住），只是顺序规整了 —— TraeWork 自己保存设置时也是整份重写，
//! 所以这不是新引入的差异。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// TraeWork 用户设置文件相对 userData 的路径。
pub const SETTINGS_REL: &str = "User/settings.json";

/// 我们写过哪些键的**原值**（放在助手自己的数据目录里，与 TraeWork 无关）。
const BACKUP_FILE: &str = "traework-proxy-backup.json";

pub const KEY_MODE: &str = "trae.network.proxy.mode";
pub const KEY_MANUAL_PROXY: &str = "trae.network.proxy.manual.proxy";
pub const KEY_NO_PROXY: &str = "trae.network.proxy.manual.noProxy";

/// 一个键的原始状态。`had = false` 表示原本没有这个键 —— 还原时要**删掉**而不是写回空值。
///
/// 只用来**读旧版本留下的备份**（[`uninstall`] 的精确还原路径）。新版不再写备份，
/// 所以这个结构体只进不出。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct KeySnapshot {
    pub key: String,
    pub had: bool,
    pub value: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Backup {
    /// 被改写的那个文件（换机器/换安装位置后能一眼看出来备份是不是对得上）。
    pub path: String,
    pub applied_at: String,
    pub port: u16,
    pub keys: Vec<KeySnapshot>,
}

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

/// Electron 的应用数据基目录（与 `app.getPath('userData')` 的父目录一致）。
fn app_support_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        dirs::data_dir() // %APPDATA%
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs::home_dir().map(|h| h.join("Library/Application Support"))
    }
}

/// TraeWork 的 userData 目录名 —— 从 `product.json` 的 `nameShort` 读，不硬编码。
///
/// Electron 的 `app.getPath('userData')` 默认就是 `<app support>/<app.getName()>`，
/// 而这个应用的 `package.json.name` 正是 `nameShort`。读它而不是写死，是为了将来
/// 应用改名（或出别的区域版）时不用改代码。
///
/// ⚠️ **一个应用一份目录**：本机可能同时装着 `TRAE SOLO CN` 与 `Trae CN`（两个 shell、
/// 两个 userData 目录）。旧版「经系统代理接管」往哪一份里写过是未知的，所以清理时
/// **每一份都要看** —— 只看默认目标会让另一个应用带着「指向本机回环」的代理设置
/// 一直断网（详见 [`uninstall`]）。
fn user_data_dir_of(target: &crate::target::AppTarget) -> Option<PathBuf> {
    let text = std::fs::read_to_string(target.product_path()).ok()?;
    let name = serde_json::from_str::<Value>(&text)
        .ok()?
        .get("nameShort")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())?;
    Some(app_support_dir()?.join(name))
}

/// 本机**所有**可能被旧版本写过代理设置的用户数据目录。
pub fn user_data_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for t in crate::target::discover() {
        if let Some(d) = user_data_dir_of(&t) {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    if out.is_empty() {
        // 兜底：一个目标都没发现时（应用已卸载/正在升级），仍然按历史上那个目录名找一遍
        // —— 痕迹留在磁盘上，不会因为应用不在了就消失。
        if let Some(base) = app_support_dir() {
            out.push(base.join("TRAE SOLO CN"));
        }
    }
    out
}

/// 所有候选的 `User/settings.json` 路径。
pub fn settings_paths() -> Vec<PathBuf> {
    user_data_dirs().into_iter().map(|d| d.join(SETTINGS_REL)).collect()
}

fn backup_path(data_dir: &Path) -> PathBuf {
    data_dir.join(BACKUP_FILE)
}

// ---------------------------------------------------------------------------
// 纯函数：还原 / 判据（可单测，不碰磁盘）
// ---------------------------------------------------------------------------

/// 按备份还原。返回是否真的改动了什么。
fn restore(doc: &mut Value, snaps: &[KeySnapshot]) -> bool {
    let Some(obj) = doc.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for s in snaps {
        match (&s.value, s.had) {
            (Some(v), true) => {
                if obj.get(&s.key) != Some(v) {
                    obj.insert(s.key.clone(), v.clone());
                    changed = true;
                }
            }
            _ => {
                if obj.remove(&s.key).is_some() {
                    changed = true;
                }
            }
        }
    }
    changed
}

/// 这三个键现在是否都指向我们写的值。
fn is_applied(doc: &Value, port: u16) -> bool {
    let Some(obj) = doc.as_object() else {
        return false;
    };
    let get = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or("");
    get(KEY_MODE) == "manual" && get(KEY_MANUAL_PROXY) == format!("http://127.0.0.1:{port}")
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

fn read_doc(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "{} 不是合法 JSON（{e}）。拒绝改写 —— 覆盖它等于把 TraeWork 的用户设置整个抹掉。",
            path.display()
        )
    })
}

fn write_doc(path: &Path, doc: &Value) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("创建 {} 失败：{e}", p.display()))?;
    }
    // 保持 TraeWork 自己的风格：制表符缩进。用文本编辑器手工改过也看不出来。
    let mut text = serde_json::to_string_pretty(doc).map_err(|e| format!("序列化设置失败：{e}"))?;
    if text.contains("\n  ") {
        text = text.replace("\n  ", "\n\t");
    }
    std::fs::write(path, format!("{text}\n")).map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

fn read_backup(data_dir: &Path) -> Option<Backup> {
    let t = std::fs::read_to_string(backup_path(data_dir)).ok()?;
    serde_json::from_str(&t).ok()
}

/// 本机回环代理的值长什么样 —— 用来判断「这三个键是不是我们写的」。
fn is_loopback_proxy(v: &str) -> bool {
    let rest = match v.strip_prefix("http://").or_else(|| v.strip_prefix("https://")) {
        Some(r) => r,
        None => return false,
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1"
}

/// 在没有备份的前提下，把**我们写的**代理键清掉。
///
/// 判据是「`manual.proxy` 指向本机回环」——不这样限定的后果很严重：用户自己配的公司代理
/// （`trae.network.proxy.mode = manual` + 一个真实代理地址）会被我们一并抹掉。
/// 反过来，只要还指着回环而代理已经停了，TraeWork 就是**断网**状态，必须清。
fn clear_ours(doc: &mut Value) -> bool {
    let Some(obj) = doc.as_object_mut() else {
        return false;
    };
    let loopback = obj
        .get(KEY_MANUAL_PROXY)
        .and_then(Value::as_str)
        .map(is_loopback_proxy)
        .unwrap_or(false);
    if !loopback {
        return false;
    }
    let mut changed = false;
    for k in [KEY_MODE, KEY_MANUAL_PROXY, KEY_NO_PROXY] {
        if obj.remove(k).is_some() {
            changed = true;
        }
    }
    changed
}

/// 精确还原（逐键写回原值 / 删掉原本不存在的键），然后删掉备份。
///
/// ⚠️ **没有备份 ≠ 什么都没做过**。备份文件在助手自己的数据目录里，它会随用户清理、
/// 换机、重装一起消失（本机真发生过：整个数据目录被清空，备份随之没了）。
/// 那种情况下如果直接返回「无需还原」，TraeWork 就会被**永久留在**「经本机代理」，
/// 而开关已经关掉、代理已经停止 —— 它指着的是一个没人接的端口，表现是**整个应用断网**。
///
/// 所以没有备份时走 [`clear_ours`]：只在三个键**确实指向本机回环**时才清掉，
/// 让它回到应用自己的默认（system）。
///
/// **多目标**：本机所有 Trae 应用的 userData 都过一遍（见 [`user_data_dirs`]）——
/// 旧版本往哪一份里写过是未知的，漏掉一个就等于把它永久留在断网状态。
pub fn uninstall(data_dir: &Path) -> Result<String, String> {
    let paths = settings_paths();
    if paths.is_empty() {
        return Err("找不到 TraeWork 的用户数据目录".to_string());
    }
    let mut notes: Vec<String> = Vec::new();
    for path in paths {
        notes.push(uninstall_one(data_dir, &path)?);
    }
    Ok(notes.join("；"))
}

/// 处理**一份** `User/settings.json`。文件不存在时是空操作（返回一句「无需还原」）。
fn uninstall_one(data_dir: &Path, path: &Path) -> Result<String, String> {
    let mut doc = read_doc(path)?;
    // 备份是**单个**文件，且记着它当时改的是哪个文件 ⇒ 只有路径对得上才算数。
    let bak = read_backup(data_dir).filter(|b| b.path == path.display().to_string());
    let Some(bak) = bak else {
        let changed = clear_ours(&mut doc);
        if changed {
            write_doc(path, &doc)?;
        }
        return Ok(if changed {
            format!(
                "没有备份，但 {} 里的代理键仍指向本机 —— 已清掉，该应用回到自带的默认代理设置",
                path.display()
            )
        } else {
            format!("{} 里没有残留的本机代理设置，无需还原", path.display())
        });
    };
    let changed = restore(&mut doc, &bak.keys);
    if changed {
        write_doc(path, &doc)?;
    }
    let _ = std::fs::remove_file(backup_path(data_dir));
    Ok(format!(
        "已还原 {}（改动 {}）",
        path.display(),
        if changed { "有" } else { "无" }
    ))
}

/// 本机**任一** Trae 应用当前是否正走（旧版本设置过的）本机代理 —— 决定要不要清扫。
///
/// 不需要 `data_dir`：判据全在各应用自己的 `User/settings.json` 里，而那个路径由
/// 各自的 `product.json` 推导（见 [`settings_paths`]）。曾经收过 `data_dir` 却根本没用上。
pub fn applied(port: u16) -> bool {
    settings_paths()
        .iter()
        .any(|p| read_doc(p).map(|d| is_applied(&d, port)).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    /// 构造一份「旧版本 `install` 之后」的文档 —— 清理路径要面对的就是它。
    fn applied_doc(port: u16) -> Value {
        let mut m = Map::new();
        m.insert(KEY_MODE.into(), Value::String("manual".into()));
        m.insert(
            KEY_MANUAL_PROXY.into(),
            Value::String(format!("http://127.0.0.1:{port}")),
        );
        m.insert(
            KEY_NO_PROXY.into(),
            Value::String("127.0.0.1,localhost".into()),
        );
        Value::Object(m)
    }

    #[test]
    fn restore_is_exact() {
        let original = doc(r#"{"trae.network.proxy.mode":"system","keep":1}"#);
        let mut d = applied_doc(8788);
        d["keep"] = Value::from(1);
        let snaps = vec![
            KeySnapshot {
                key: KEY_MODE.into(),
                had: true,
                value: Some(Value::String("system".into())),
            },
            KeySnapshot {
                key: KEY_MANUAL_PROXY.into(),
                had: false,
                value: None,
            },
            KeySnapshot {
                key: KEY_NO_PROXY.into(),
                had: false,
                value: None,
            },
        ];
        assert!(restore(&mut d, &snaps));
        assert_eq!(d, original, "还原后必须与原文件逐键一致");
        // 再还原一次应当是空操作
        assert!(!restore(&mut d, &snaps));
    }

    #[test]
    fn different_port_is_not_considered_applied() {
        let d = applied_doc(8788);
        assert!(is_applied(&d, 8788));
        assert!(
            !is_applied(&d, 9999),
            "端口变了就必须视为未生效（清扫判据才不会误伤）"
        );
    }

    /// 🔴 没有备份、但代理键还指着本机 ⇒ 必须清掉。
    ///
    /// 回归的是本机真事故：助手的数据目录被整个清空（备份随之消失），而 TraeWork 的
    /// `User/settings.json` 还写着 `manual / http://127.0.0.1:8788`。旧实现看到「没有备份」
    /// 就回一句「无需还原」，于是开关关掉、代理停止之后，TraeWork 指着的是一个**没人接的端口**
    /// —— 整个应用断网，而界面上一切「正常」。
    #[test]
    fn without_a_backup_a_stale_loopback_proxy_is_still_cleared() {
        let mut d = applied_doc(8788);
        d["AI.toolcall.v2.ide.command.mode"] = Value::String("whitelist".into());
        assert!(clear_ours(&mut d), "指向本机回环的代理键必须被清掉");
        assert!(d.get(KEY_MODE).is_none());
        assert!(d.get(KEY_MANUAL_PROXY).is_none());
        assert!(d.get(KEY_NO_PROXY).is_none());
        assert_eq!(
            d["AI.toolcall.v2.ide.command.mode"], "whitelist",
            "用户自己的设置一个字都不能动"
        );
        // 幂等
        assert!(!clear_ours(&mut d));
    }

    /// 但不能把**用户自己配的**代理当成我们的：那是公司代理/自建代理，抹掉等于改坏他的环境。
    #[test]
    fn clear_ours_never_touches_a_third_party_proxy() {
        let mut d = doc(r#"{"trae.network.proxy.mode":"manual","trae.network.proxy.manual.proxy":"http://proxy.corp.example:8080"}"#);
        assert!(!clear_ours(&mut d), "非回环的代理不是我们写的");
        assert_eq!(d[KEY_MODE], "manual");
        assert_eq!(d[KEY_MANUAL_PROXY], "http://proxy.corp.example:8080");

        // 只有 mode=manual、但我们没写过 manual.proxy 的情况也不该动
        let mut d2 = doc(r#"{"trae.network.proxy.mode":"manual"}"#);
        assert!(!clear_ours(&mut d2));

        // `system` 之类的值更不该动
        let mut d3 = doc(r#"{"trae.network.proxy.mode":"system"}"#);
        assert!(!clear_ours(&mut d3));
    }

    #[test]
    fn loopback_proxy_detection_covers_the_usual_spellings() {
        for v in [
            "http://127.0.0.1:8788",
            "https://127.0.0.1:8788",
            "http://localhost:8788",
            "http://127.0.0.1:8788/",
            "http://[::1]:8788",
        ] {
            assert!(is_loopback_proxy(v), "{v} 是回环");
        }
        for v in [
            "http://proxy.corp.example:8080",
            "socks5://127.0.0.1:1080",
            "127.0.0.1:8788",
            "",
        ] {
            assert!(!is_loopback_proxy(v), "{v} 不是我们写的那种回环 http 代理");
        }
    }

    /// 端到端（真文件）：**旧版本留下的**「已改写文件 + 备份」，走备份还原必须逐键回到原样。
    /// 这是升级用户的真实处境 —— 备份还躺在数据目录里，而新版只负责把它用掉。
    #[test]
    fn a_legacy_backup_restores_the_file_exactly() {
        let base = std::env::temp_dir().join(format!("twa-traework-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let user = base.join("User");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let file = user.join("settings.json");
        let original = "{\n\t\"trae.network.proxy.effectiveMode\": \"system\",\n\t\"AI.toolcall.v2.ide.command.mode\": \"whitelist\"\n}\n";
        std::fs::write(&file, original).unwrap();

        // 模拟旧版本改写之后的状态：三个代理键指向本机，用户自己的键原样在
        let mut d = applied_doc(8788);
        d["trae.network.proxy.effectiveMode"] = Value::String("system".into());
        d["AI.toolcall.v2.ide.command.mode"] = Value::String("whitelist".into());
        let absence = |k: &str| KeySnapshot {
            key: k.to_string(),
            had: false,
            value: None,
        };
        let bak = Backup {
            path: file.display().to_string(),
            applied_at: "t".into(),
            port: 8788,
            keys: vec![
                absence(KEY_MODE),
                absence(KEY_MANUAL_PROXY),
                absence(KEY_NO_PROXY),
            ],
        };
        std::fs::write(
            backup_path(&data),
            serde_json::to_string_pretty(&bak).unwrap(),
        )
        .unwrap();
        write_doc(&file, &d).unwrap();
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.contains("127.0.0.1:8788"));
        assert!(after.contains("whitelist"), "用户自己的设置不能被抹掉");

        // 还原
        let bak2 = read_backup(&data).unwrap();
        let mut d2 = read_doc(&file).unwrap();
        assert!(restore(&mut d2, &bak2.keys));
        write_doc(&file, &d2).unwrap();
        // ⚠️ 断言的是**语义等价**而不是逐字节：`serde_json::Map` 默认是 `BTreeMap`，
        //    重新序列化会把键按字母序排（原文件里 `effectiveMode` 在 `AI.toolcall` 之前，
        //    写回后会反过来）。这是**无害**的 —— JSON 对象无序，而且 TraeWork 自己保存设置时
        //    也是整份重写、重新排序。逐字节那条线留给「值」：任何键的值都不能变。
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let before: Value = serde_json::from_str(original).unwrap();
        assert_eq!(after, before, "还原后所有键值必须与原文件完全一致（顺序可变）");
        assert!(
            after.get(KEY_MANUAL_PROXY).is_none(),
            "原本不存在的键要删掉，而不是写回空值"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn invalid_json_is_refused_not_overwritten() {
        let base = std::env::temp_dir().join(format!("twa-badjson-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("settings.json");
        std::fs::write(&file, "{ 这不是 JSON").unwrap();
        let err = read_doc(&file).unwrap_err();
        assert!(err.contains("拒绝改写"), "必须明确拒绝而不是覆盖：{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ 这不是 JSON");
        let _ = std::fs::remove_dir_all(&base);
    }
}
