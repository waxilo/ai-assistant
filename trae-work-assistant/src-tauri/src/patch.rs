//! TraeWork 主进程「闸门补丁」—— 让**明文 `http://` 端点**可用，从而彻底不需要装证书。
//!
//! ## 为什么需要它
//!
//! TraeWork 的 `bootConfig.{remote,agent,ckg,cue,hub}.trae.normal` 会被它拼成 Electron
//! `webRequest` 的 URL pattern，规则是**「不是 `https://` 开头就补 `https://`」**
//! （`out/main.js` 的 `solo-lite-response-cors` 与 `ug` 收集器，同一段代码两份）：
//!
//! ```js
//! const l = c.startsWith("https://") ? `${c}/*` : `https://${c}/*`;
//! ```
//!
//! 于是端点写 `http://127.0.0.1:8788` 会被拼成 `https://http://127.0.0.1:8788/*`（非法 port），
//! TraeWork **启动即崩**（`TypeError: Invalid url pattern …` → 建窗口失败 → `Lifecycle#kill()`）。
//! 为了绕开它，我们之前只能让本地反代讲 TLS、并把自签 CA 装进登录钥匙串 ——
//! **证书只是这条约束的副产品，不是接管的前提。**
//!
//! ## 补丁内容（3 处，缺一不可）
//!
//! | # | 位置 | 改动 |
//! |---|---|---|
//! | 1 | 闸门·`ug` 收集器 | `X.startsWith("https://")` → `X.includes("://")` |
//! | 2 | 闸门·`solo-lite-response-cors` | 同上 |
//! | 3 | 身份头规则的 pattern 表 | 数组里加 `"http://*/trae/*"` |
//!
//! ⚠️ **第 3 处不是可选项。** 那张数组是 `solo-lite-websocket-headers` 规则的 pattern 来源，
//! 也就是**唯一给智能体请求注入 `Authorization: Cloud-IDE-JWT` + `X-User-Region` 的地方**。
//! 只打前两处 ⇒ 端点降成 http 后该规则不再命中 ⇒ 应用不带身份头 ⇒
//! 反代既没东西可换，**不在白名单里的透传路径还会因为缺 `Authorization` 直接 401**。
//! 结果是「看着能跑、实际全废」的**假成功** —— 比直接报错更难查。
//!
//! ## 一个应用一份补丁、一份指纹
//!
//! 本机可能同时装着几个 Trae shell（`TRAE SOLO CN` / `Trae CN`），每个都有自己的
//! `out/main.js`。所以本模块的每个操作都**显式带 [`AppTarget`]**，指纹记录也**按目标分文件**
//! （`patch_main_js.<id>.json`）—— 共用一个文件名会让两个应用互相覆盖对方的指纹，
//! 还原时的 sha256 自证就会拿 A 的原始指纹去比 B 的文件，直接把「还原」变成死路。
//!
//! 旧版本只在助手数据目录里写过**一个** `patch_main_js.json`。它**照旧能被读到**：
//! 记录里本来就存了 `target`（那一份 `out/main.js` 的绝对路径），只有路径吻合才用它
//! 做自证（见 [`read_record`]）—— 于是升级上来的机器不用重打补丁也能正常还原。
//!
//! ## 安全边界
//!
//! - 补丁后闸门变成 `includes("://")`：`https://` 与 `http://` **都**能过
//!   ⇒ **打过补丁的应用向下兼容**，https 端点照旧能用。
//! - 改动是**逐字节可还原**的：反向替换 + 去掉文件尾标记；若存在指纹记录，
//!   还原后还会比对 sha256 自证。
//! - **fail-closed**：闸门匹配数 ≠ 2 或 pattern 数组找不到 ⇒ 拒绝打补丁，绝不半打。
//! - 只动 `out/main.js`（`product.json.checksums` 的 14 项里**没有**它，
//!   且它自身不引用 `checksums`）。**绝不动渲染层的受校验文件。**

use crate::target::AppTarget;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// 文件尾标记：**检测与还原都靠它**，不依赖 2.85 MB 的备份。
const MARKER: &str = "//[twa-gate v1]";
/// 闸门必须恰好出现这么多次（`ug` 收集器 + `solo-lite-response-cors`）。
const GATE_WANT: usize = 2;
/// 指纹记录（落在助手自己的数据目录，不进 TraeWork）。
const RECORD_FILE: &str = "patch_main_js.json";
const RECORD_SCHEMA: u32 = 1;

/// 补丁点 1/2 · 打补丁**前**的闸门形态。
///
/// Rust 的 `regex` **不支持反向引用**，所以用三个独立捕获组表达「同一个变量名」，
/// 再在替换闭包里校验三者相等 —— 不等就原样返回，等于这一处不匹配。
static GATE_BEFORE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?P<v>[A-Za-z_$][\w$]*)\.startsWith\("https://"\)\?`\$\{(?P<a>[A-Za-z_$][\w$]*)\}/\*`:`https://\$\{(?P<b>[A-Za-z_$][\w$]*)\}/\*`"#,
    )
    .expect("GATE_BEFORE 正则必须可编译")
});

/// 补丁点 1/2 · 打补丁**后**的闸门形态（用于检测与反向还原）。
static GATE_AFTER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?P<v>[A-Za-z_$][\w$]*)\.includes\("://"\)\?`\$\{(?P<a>[A-Za-z_$][\w$]*)\}/\*`:`https://\$\{(?P<b>[A-Za-z_$][\w$]*)\}/\*`"#,
    )
    .expect("GATE_AFTER 正则必须可编译")
});

/// 补丁点 3 · 数组字面量（**不含变量名**，这样上游把 `aX` 改名也不影响定位）。
///
/// 注意不能写成带 `aX=` 的形式：那个名字是压缩产物，最容易随版本变。
/// 也不能只匹配数组内的片段 —— 同样的三个字符串在 `solo-lite-response-cors`
/// 的大 pattern 列表里也出现过一次，只有**带方括号的完整字面量**在文件里唯一。
const AX_BEFORE: &str =
    r#"["https://*/trae/*","wss://*/explorer/*","ws://*/explorer/*"]"#;
const AX_AFTER: &str =
    r#"["https://*/trae/*","http://*/trae/*","wss://*/explorer/*","ws://*/explorer/*"]"#;

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

/// 补丁状态（只读探测结果）。
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PatchStatus {
    /// 找到这个应用的 `out/main.js` 了吗（找不到 = 这份补丁不适用于它）。
    pub supported: bool,
    /// 被改的目标文件（`out/main.js` 的绝对路径）。
    pub target: Option<String>,
    /// 目标文件可写吗。
    pub writable: bool,
    /// **当前是否已打补丁**（文件尾标记 + 闸门为补丁后形态）。
    pub patched: bool,
    /// 版本是否被识别（闸门恰好 2 处 + 第 3 处数组字面量在）。
    /// `false` 时**拒绝**打补丁 —— 宁可不禁用证书，也不能把 TraeWork 弄坏。
    pub recognized: bool,
    /// 扫到的闸门处数（诊断用）。
    pub gates: usize,
    /// 第 3 处（身份头 pattern 数组）是否在位。
    pub identity_patterns: bool,
    pub message: String,
}

/// 只读探测某个目标的补丁状态。
pub fn status(target: &AppTarget) -> PatchStatus {
    let path = target.main_js_path();
    if !path.exists() {
        return PatchStatus {
            target: Some(path.display().to_string()),
            message: format!(
                "在「{}」里没找到 out/main.js，本助手无法给它打免证书补丁。",
                target.id
            ),
            ..Default::default()
        };
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return PatchStatus {
            target: Some(path.display().to_string()),
            message: format!(
                "读取「{}」的 out/main.js 失败（权限）。请检查「系统设置 → 隐私与安全性 → App 管理」。",
                target.id
            ),
            ..Default::default()
        };
    };

    let after = gates_after(&text);
    let before = gates_before(&text);
    let marker = has_marker(&text);
    let id_after = has_ax(&text, AX_AFTER);
    let id_before = has_ax(&text, AX_BEFORE);

    let patched = marker && after == GATE_WANT;
    let recognized = (before == GATE_WANT && id_before) || (after == GATE_WANT && id_after);
    let writable = crate::endpoint::is_writable_file(&path);
    let app = &target.id;

    let message = if patched && recognized {
        format!("「{app}」已打「免证书补丁」：端点可用明文 http://，无需安装证书。")
    } else if recognized && !writable {
        // 「能打」和「打得成」是两件事：版本认了但写不进去（TCC）时，如果说「可以打」，
        // 界面上就会演成「按钮能点、点了必失败」—— 所以这里就把不可写说出去。
        format!(
            "「{app}」的版本已识别（闸门 {before} 处，本助手认识这个版本），但它的安装目录不可写——{}",
            crate::endpoint::unwritable_hint(&path)
        )
    } else if recognized {
        format!(
            "「{app}」未打补丁（识别正常：闸门 {before} 处 / 身份头数组在位），可以打。\
             打完即可用明文 http:// 端点，不再需要证书。"
        )
    } else if patched {
        format!(
            "「{app}」检测到补丁标记，但闸门形态与预期不符——可能只打了一半，建议先还原再重来。"
        )
    } else {
        format!(
            "「{app}」版本未被识别（闸门扫到 {before} 处，期望 {GATE_WANT} 处）——\
             上游改过这段代码，本助手**拒绝**给这个应用打补丁（以免把它弄坏）。\
             请先不接管它，或等本助手适配该版本。"
        )
    };

    PatchStatus {
        supported: true,
        target: Some(path.display().to_string()),
        writable,
        patched,
        recognized,
        gates: if after == GATE_WANT { after } else { before },
        identity_patterns: id_after || id_before,
        message,
    }
}

/// 某个目标当前是否已打补丁（供 `endpoint::preflight` 的不变量使用）。
pub fn is_patched(target: &AppTarget) -> bool {
    status(target).patched
}

/// 已打补丁的闸门处数（只统计**三个变量名一致**的真匹配）。
fn gates_after(text: &str) -> usize {
    GATE_AFTER
        .captures_iter(text)
        .filter(|c| same_var(c))
        .count()
}

/// 未打补丁的闸门处数。
fn gates_before(text: &str) -> usize {
    GATE_BEFORE
        .captures_iter(text)
        .filter(|c| same_var(c))
        .count()
}

fn same_var(c: &regex::Captures<'_>) -> bool {
    let v = &c["v"];
    v == &c["a"] && v == &c["b"]
}

fn has_marker(text: &str) -> bool {
    text.lines().any(|l| l.trim() == MARKER)
}

fn has_ax(text: &str, needle: &str) -> bool {
    text.contains(needle)
}

// ---------------------------------------------------------------------------
// 纯文本变换（可单测，不碰文件系统）
// ---------------------------------------------------------------------------

fn gate_repl_before(v: &str) -> String {
    format!("{v}.startsWith(\"https://\")?`${{{v}}}/*`:`https://${{{v}}}/*`")
}

fn gate_repl_after(v: &str) -> String {
    format!("{v}.includes(\"://\")?`${{{v}}}/*`:`https://${{{v}}}/*`")
}

/// 把闸门从「只认 https」改成「认任何 scheme」，并在数组里补上 `http://*/trae/*`。
fn patch_text(text: &str) -> Result<String, String> {
    let before = gates_before(text);
    if before != GATE_WANT {
        return Err(format!(
            "拒绝打补丁：闸门匹配到 {before} 处，期望 {GATE_WANT} 处。\
             上游版本变过，本助手不认识这个版本 —— 不猜、不改。"
        ));
    }
    if !has_ax(text, AX_BEFORE) {
        return Err(
            "拒绝打补丁：没找到身份头规则的 pattern 数组。\
             只打闸门而漏掉它，会让应用不再给智能体请求带 Authorization（透传路径直接 401）。"
                .to_string(),
        );
    }

    let mut out = GATE_BEFORE
        .replace_all(text, |c: &regex::Captures<'_>| {
            if !same_var(c) {
                return c.get(0).map(|m| m.as_str()).unwrap_or_default().to_string();
            }
            gate_repl_after(&c["v"])
        })
        .into_owned();
    out = out.replacen(AX_BEFORE, AX_AFTER, 1);

    // 自检：新形态恰好 2 处、旧形态 0 处、数组已在
    if gates_after(&out) != GATE_WANT || gates_before(&out) != 0 || !has_ax(&out, AX_AFTER) {
        return Err("拒绝打补丁：替换后的自检没通过（内存里就已不一致）。".to_string());
    }

    // 收尾标记写成**恒定**的 `\n + MARKER + \n`，绝不「按需补换行」——
    // 那样还原时无法区分「这个换行是我加的」还是「原文件本来就有的」，逐字节还原就成了伪命题。
    // 原文件若本来就以 `\n` 结尾，这里会多出一个空行，对 JS 无害。
    out.push('\n');
    out.push_str(MARKER);
    out.push('\n');
    Ok(out)
}

/// 反向还原。**不依赖指纹记录**也能工作；有记录时由 [`revert`] 额外比对 sha256。
fn unpatch_text(text: &str) -> Result<String, String> {
    let after = gates_after(text);
    if after != GATE_WANT {
        return Err(format!(
            "拒绝还原：补丁后的闸门匹配到 {after} 处，期望 {GATE_WANT} 处。\
             应用可能已升级（文件被整份换过），此时无需还原。"
        ));
    }
    let mut out = GATE_AFTER
        .replace_all(text, |c: &regex::Captures<'_>| {
            if !same_var(c) {
                return c.get(0).map(|m| m.as_str()).unwrap_or_default().to_string();
            }
            gate_repl_before(&c["v"])
        })
        .into_owned();
    out = out.replacen(AX_AFTER, AX_BEFORE, 1);

    // 去掉收尾标记：恰好是 `\n + MARKER + \n` 这一整段（见 `patch_text` 的写法）。
    // 只认「整段贴在文件末尾」这一种形态；不满足就是形态不对，宁可报错也不猜。
    let needle = format!("\n{MARKER}\n");
    if !out.ends_with(&needle) {
        return Err(format!(
            "拒绝还原：文件尾没找到完整的补丁标记（期望以 `{MARKER}` 结尾）。\
             文件可能被改动过，避免写出可疑内容，本次**未做任何改动**。"
        ));
    }
    out.truncate(out.len() - needle.len());

    if gates_before(&out) != GATE_WANT || gates_after(&out) != 0 || has_marker(&out) {
        return Err("拒绝还原：反向替换后的自检没通过，一个字节都没写。".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 指纹记录（200 字节，不是备份；只用来「还原后自证」）
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug)]
struct Record {
    schema: u32,
    /// 被改的文件的绝对路径 —— 也是「这条记录还配不配得上现在这个目标」的判据。
    target: String,
    /// 旧版记录里没有这个字段（那时只有一个目标），所以必须能缺省。
    #[serde(default)]
    app_id: String,
    orig_len: usize,
    orig_sha256: String,
    applied_at: String,
}

/// 目标 id → 文件名安全片段。
///
/// 名字里可能有空格、中文、斜杠，直接拼进文件名会出事；而「只做替换」又会让
/// 两个不同的 id 撞成同一个文件名（互相覆盖指纹）。所以是「净化 + id 的哈希尾巴」。
fn slug(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let safe = safe.trim_matches('-');
    let safe = if safe.is_empty() { "app" } else { safe };
    format!("{safe}-{}", &sha256_hex(id)[..8])
}

fn record_path(dir: &Path, target: &AppTarget) -> PathBuf {
    dir.join(format!("patch_main_js.{}.json", slug(&target.id)))
}

/// 旧版的**单目标**记录文件（升级用户的机器上还留着它）。
fn legacy_record_path(dir: &Path) -> PathBuf {
    dir.join(RECORD_FILE)
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn write_record(dir: &Path, target: &AppTarget, orig: &str) {
    let rec = Record {
        schema: RECORD_SCHEMA,
        target: target.main_js_path().display().to_string(),
        app_id: target.id.clone(),
        orig_len: orig.len(),
        orig_sha256: sha256_hex(orig),
        applied_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&rec) {
        let _ = std::fs::write(record_path(dir, target), text);
    }
}

/// 读该目标的指纹记录：先看按目标分的那份，再回落到旧版的单目标记录
/// —— 后者只有 `target` 路径**确实指着这个应用**时才认（否则会拿 B 的原始指纹去比 A 的文件）。
fn read_record(dir: &Path, target: &AppTarget) -> Option<Record> {
    let mine = target.main_js_path().display().to_string();
    let parse = |p: PathBuf| -> Option<Record> {
        let text = std::fs::read_to_string(p).ok()?;
        let rec: Record = serde_json::from_str(&text).ok()?;
        (rec.schema == RECORD_SCHEMA && rec.target == mine).then_some(rec)
    };
    parse(record_path(dir, target)).or_else(|| parse(legacy_record_path(dir)))
}

/// 删掉该目标的记录；顺带把**旧版那份**（若确实属于它）一并清掉，
/// 免得留着一条已经过期的指纹在下次还原时跳出来自证。
fn remove_record(dir: &Path, target: &AppTarget) {
    let _ = std::fs::remove_file(record_path(dir, target));
    let mine = target.main_js_path().display().to_string();
    if let Ok(text) = std::fs::read_to_string(legacy_record_path(dir)) {
        if serde_json::from_str::<Record>(&text).map(|r| r.target == mine).unwrap_or(false) {
            let _ = std::fs::remove_file(legacy_record_path(dir));
        }
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("twa-tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入临时文件失败（{}）：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换 {} 失败：{e}", path.display())
    })
}

// ---------------------------------------------------------------------------
// 对外操作
// ---------------------------------------------------------------------------

/// 给一个目标打补丁（幂等）。已打过则直接返回当前状态。
pub fn apply(dir: &Path, target: &AppTarget) -> Result<PatchStatus, String> {
    let path = target.main_js_path();
    let st = status(target);
    if st.patched {
        return Ok(st);
    }
    if !st.supported {
        return Err(st.message);
    }
    if !st.recognized {
        return Err(st.message);
    }
    if !st.writable {
        return Err(format!(
            "「{}」的 out/main.js 写不进去——{}",
            target.id,
            crate::endpoint::unwritable_hint(&path)
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取 out/main.js 失败：{e}"))?;

    let patched = patch_text(&text)?;
    write_atomic(&path, &patched)?;
    write_record(dir, target, &text);
    crate::journal::append(
        dir,
        "patch_apply",
        &format!(
            "已给「{}」打「免证书补丁」（闸门接受 http:// + 身份头规则覆盖明文端点），\
             此后端点可用明文，无需安装证书",
            target.id
        ),
    );
    Ok(status(target))
}

/// 还原某个目标的补丁（幂等）。返回是否发生了改动。
pub fn revert(dir: &Path, target: &AppTarget) -> Result<bool, String> {
    let path = target.main_js_path();
    if !path.exists() {
        return Ok(false);
    }
    if !has_marker(&std::fs::read_to_string(&path).unwrap_or_default()) {
        // 没有标记 = 我们没碰过（或已被升级覆盖），**原样留着**
        return Ok(false);
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 out/main.js 失败：{e}"))?;
    let restored = unpatch_text(&text)?;

    // 有指纹记录 ⇒ 还原后用 sha256 自证；对不上就不写，如实报告
    if let Some(rec) = read_record(dir, target) {
        let got = sha256_hex(&restored);
        if got != rec.orig_sha256 {
            return Err(format!(
                "「{}」还原后的文件与记录的原始指纹不一致（期望 {}…，实得 {}…），\
                 为避免写入可疑内容，本次**未做任何改动**。\
                 若应用已升级，当前文件本身就是新的，无需还原。",
                target.id,
                &rec.orig_sha256[..12.min(rec.orig_sha256.len())],
                &got[..12.min(got.len())]
            ));
        }
    }

    write_atomic(&path, &restored)?;
    remove_record(dir, target);
    crate::journal::append(
        dir,
        "patch_revert",
        &format!(
            "已还原「{}」的主进程补丁，闸门恢复成「只认 https」的原始行为",
            target.id
        ),
    );
    Ok(true)
}

/// 测试用：一段与 `out/main.js` 同构的最小样本（两个闸门 + 身份头数组）。
///
/// 别的模块（`endpoint` 的清扫测试）也要造一个「已打补丁的应用」，与其各写一份、日后
/// 上游改版时只改了一处，不如从这里取。
#[cfg(test)]
pub fn sample_main_js() -> String {
    let a = r#"const u=l.startsWith("https://")?`${l}/*`:`https://${l}/*`;"#;
    let b = r#"forEach(c=>{const p=c.startsWith("https://")?`${c}/*`:`https://${c}/*`;r.push(p)});"#;
    let c = r#"aX=["https://*/trae/*","wss://*/explorer/*","ws://*/explorer/*"],cX=[".trae.cn"];"#;
    format!("{a}\n{b}\n{c}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段与 `out/main.js` 同构的最小样本（两个闸门 + 身份头数组）。
    fn sample() -> String {
        sample_main_js()
    }

    fn fake_target(dir: &Path, id: &str) -> AppTarget {
        let app_dir = dir.join(format!("{id}.app")).join("Contents/Resources/app");
        std::fs::create_dir_all(app_dir.join("out")).unwrap();
        AppTarget {
            id: id.to_string(),
            bundle: dir.join(format!("{id}.app")),
            app_dir,
        }
    }

    #[test]
    fn patch_then_revert_is_byte_identical() {
        let src = sample();
        let patched = patch_text(&src).unwrap();
        assert!(patched.contains("includes(\"://\")"));
        assert!(patched.contains(AX_AFTER));
        assert!(patched.trim_end().ends_with(MARKER));
        let back = unpatch_text(&patched).unwrap();
        assert_eq!(back, src, "还原必须逐字节一致");
    }

    #[test]
    fn patch_does_not_touch_the_cors_list_copy() {
        // solo-lite-response-cors 的大列表里也有同样三个字符串，但**没有方括号**
        let src = format!(
            r#"{}
r=["https://*.traeapi.us/*","https://*/trae/*","wss://*/explorer/*","ws://*/explorer/*","https://*/api/remote/*"];
"#,
            sample()
        );
        let patched = patch_text(&src).unwrap();
        assert_eq!(patched.matches(AX_AFTER).count(), 1, "只该替换那一处数组字面量");
        assert!(
            patched.contains(r#""https://*/trae/*","wss://*/explorer/*","ws://*/explorer/*","https://*/api/remote/*""#),
            "大列表必须原样保留"
        );
    }

    #[test]
    fn refuses_when_gate_count_is_wrong() {
        // 只有一处闸门 = 上游版本不认识 ⇒ fail-closed，绝不半打
        let src = r#"const u=l.startsWith("https://")?`${l}/*`:`https://${l}/*`;"#.to_string();
        let err = patch_text(&src).unwrap_err();
        assert!(err.contains("拒绝打补丁"), "{err}");
        assert!(err.contains("期望 2 处"), "{err}");
    }

    #[test]
    fn refuses_when_identity_array_is_missing() {
        let src = sample().replace(AX_BEFORE, r#"aX=["https://*/trae/*"]"#);
        let err = patch_text(&src).unwrap_err();
        assert!(err.contains("pattern 数组"), "{err}");
    }

    #[test]
    fn mismatched_variable_names_do_not_count() {
        // `${l}` / `${c}` 混用 ⇒ 不是真实形态，不能算一处理匹配
        let src = r#"const u=l.startsWith("https://")?`${c}/*`:`https://${l}/*`;"#;
        assert_eq!(gates_before(src), 0);
    }

    #[test]
    fn marker_is_detected_only_as_a_whole_line() {
        assert!(has_marker(&format!("x\n{MARKER}\n")));
        assert!(has_marker(&format!("x\n  {MARKER}  \n")));
        assert!(!has_marker("const s=\"//[twa-gate v1]\";"));
    }

    #[test]
    fn file_without_trailing_newline_still_reverts_byte_identical() {
        let src = sample().trim_end().to_string();
        let patched = patch_text(&src).unwrap();
        assert_eq!(unpatch_text(&patched).unwrap(), src);
    }

    /// 指纹文件名：可读 + 唯一。
    #[test]
    fn record_file_names_are_readable_and_unique() {
        assert!(slug("TRAE SOLO CN").starts_with("TRAE-SOLO-CN-"));
        assert!(slug("Trae CN").starts_with("Trae-CN-"));
        assert_ne!(slug("Trae CN"), slug("TRAE CN"));
        // 纯中文 / 纯符号也不会退化成空名或互相撞车
        assert_ne!(slug("某应用"), slug("另一应用"));
        assert!(slug("///").starts_with("app-"));
    }

    /// 端到端（真临时目录）：打补丁 → 还原，逐字节回到原样，且指纹只在磁盘上按目标分开。
    #[test]
    fn apply_then_revert_round_trips_on_disk_per_target() {
        let base = std::env::temp_dir().join(format!("twa-patch-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let a = fake_target(&base, "TRAE SOLO CN");
        let b = fake_target(&base, "Trae CN");
        let src = sample();
        for t in [&a, &b] {
            std::fs::write(t.main_js_path(), &src).unwrap();
        }

        // 两个应用都打上补丁：各自的指纹必须落在各自的文件里
        for t in [&a, &b] {
            let st = apply(&data, t).expect("打补丁失败");
            assert!(st.patched, "{}", st.message);
            assert!(st.recognized, "{}", st.message);
        }
        let ra = record_path(&data, &a);
        let rb = record_path(&data, &b);
        assert!(ra.exists() && rb.exists(), "指纹必须按目标分开存");
        assert_ne!(ra, rb);
        assert!(!data.join(RECORD_FILE).exists(), "新版不再写那份旧的单目标指纹");

        // 还原：两边回到逐字节原样，指纹被清掉
        for t in [&a, &b] {
            assert!(revert(&data, t).expect("还原失败"));
            assert_eq!(std::fs::read_to_string(t.main_js_path()).unwrap(), src);
            assert!(!record_path(&data, t).exists());
        }
        assert!(!revert(&data, &a).unwrap(), "没有标记 ⇒ 幂等空操作");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 升级用户的机器上留着**旧版**的单目标指纹：只要它的 `target` 指着这个应用的
    /// `out/main.js`，就得照旧参与还原自证 —— 否则「还原」会退化成无自证写盘。
    #[test]
    fn a_legacy_single_record_is_still_used_for_the_matching_app() {
        let base = std::env::temp_dir().join(format!("twa-patch-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let a = fake_target(&base, "TRAE SOLO CN");
        let b = fake_target(&base, "Trae CN");
        let src = sample();
        std::fs::write(a.main_js_path(), &src).unwrap();
        std::fs::write(b.main_js_path(), &src).unwrap();

        // 手写一份旧版记录：schema=1、没有 app_id，target 指着 A
        let legacy = format!(
            r#"{{"schema":1,"target":{:?},"orig_len":{},"orig_sha256":{},"applied_at":"t"}}"#,
            a.main_js_path().display().to_string(),
            src.len(),
            serde_json::to_string(&sha256_hex(&src)).unwrap()
        );
        std::fs::write(legacy_record_path(&data), legacy).unwrap();

        assert_eq!(
            read_record(&data, &a).map(|r| r.orig_sha256),
            Some(sha256_hex(&src)),
            "旧记录属于 A 时必须被 A 用到"
        );
        assert!(
            read_record(&data, &b).is_none(),
            "旧记录不能拿去给 B 自证（会把还原变成死路）"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 真机只读扫描：本机**每个**被发现的应用都要被识别（闸门恰好 2 处 + 身份头数组在位）。
    ///
    /// 这是「上游改版」的第一道预警，也是多目标改造的验收点：
    /// `Trae CN` 以前根本没机会走到这里，现在必须和 SOLO 一样通过。
    #[test]
    fn real_main_js_is_recognized_on_this_machine() {
        let targets = crate::target::discover();
        if targets.is_empty() {
            eprintln!("跳过：本机未发现可接管的 Trae 应用");
            return;
        }
        for t in &targets {
            let st = status(t);
            eprintln!(
                "[{}] supported={} patched={} recognized={} gates={} identity_patterns={}",
                t.id, st.supported, st.patched, st.recognized, st.gates, st.identity_patterns
            );
            assert!(
                st.recognized,
                "「{}」未被识别：{}；若上游改版，请更新 patch.rs 的正则后再打补丁",
                t.id, st.message
            );
        }
    }

    /// 敏感操作前的只读演练：把真文件读进来在**内存里**打一遍再还原，不写盘。
    #[test]
    fn real_main_js_roundtrips_in_memory() {
        let targets = crate::target::discover();
        if targets.is_empty() {
            eprintln!("跳过：本机未发现可接管的 Trae 应用");
            return;
        }
        for t in &targets {
            let Ok(text) = std::fs::read_to_string(t.main_js_path()) else {
                eprintln!("跳过「{}」：out/main.js 读不到", t.id);
                continue;
            };
            if gates_before(&text) != GATE_WANT {
                // 已打过补丁
                let back = unpatch_text(&text).expect("已打补丁的文件必须能还原");
                assert_eq!(gates_before(&back), GATE_WANT);
                continue;
            }
            let patched = patch_text(&text).expect("本机版本应可打补丁");
            assert_eq!(gates_after(&patched), GATE_WANT);
            assert_eq!(sha256_hex(&unpatch_text(&patched).unwrap()), sha256_hex(&text));
            eprintln!("[{}] 内存往返一致 ✓", t.id);
        }
    }
}
