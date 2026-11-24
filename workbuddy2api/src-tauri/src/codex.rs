//! 一键接入 Codex：改写 `~/.codex/config.toml` + 生成模型目录。
//!
//! # 为什么不用 TOML 库
//!
//! 用户的 `config.toml` 里有 `plugins`、`marketplaces`、`notify`、Windows 的
//! `\\?\C:\...` 转义路径一大堆。用 `toml::to_string` 整体重写会：打乱注释、
//! 重排键序、把 `\?\` 这类字面量重新转义 —— 相当于为了改 3 行而重写整个文件。
//!
//! 所以这里是**外科手术式文本编辑**：只碰四个顶层键（`model_provider`、
//! `model_catalog_json`、`model`、`[features] apps`）与一个表
//! （`[model_providers.workbuddy2api]`），其余字节不动。
//!
//! # 为什么写之前要备份
//!
//! 这是用户机器上**别人的**配置文件，改坏了 Codex 就起不来。备份写在应用自己的
//! 数据目录（`<app_data>/codex-config.bak.toml`），不往 `.codex/` 里扔垃圾。
//! 断开时按备份恢复那几个键的原值。
//!
//! # 两个实测结论（2026-11-24，codex-cli 0.171.0-alpha.13 / Windows MSIX）
//!
//! * **必须**写 `[features] apps = false`。不写时 Codex 会每 10 秒去
//!   `POST chatgpt.com/backend-api/ps/mcp` 重试（那是它内置的 apps/connectors 后台），
//!   连不上就一直重试，一个 turn 永远走不到结束 —— 表现为「卡住不动」。
//! * **不能**写 `disable_response_storage`。0.171 已废弃这个键，写进去日志会报
//!   `is ignored`，等于往用户配置里塞一行无效噪声。

use crate::catalog::ChatModel;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// 写进 `config.toml` 的 provider id
pub const PROVIDER_ID: &str = "workbuddy2api";
/// 模型目录文件名（相对 `~/.codex`）
pub const CATALOG_FILE: &str = "workbuddy2api-model-catalog.json";
/// 备份文件名（相对应用数据目录）
pub const BACKUP_FILE: &str = "codex-config.bak.toml";

/// catalog 里声明的上下文窗口。
///
/// 上游不公开模型的窗口大小。给 128k 是**保守估计**：给大了 Codex 会塞超长
/// 上下文被上游 400，给小了只是早点压缩历史，代价小得多。
const CONTEXT_WINDOW: u32 = 128_000;

/// catalog 里每条模型的系统提示。
///
/// 刻意用最短的一句：Codex 自己会在运行时注入完整的 agent 提示、工具说明与
/// `AGENTS.md`，这里写太长会和它打架。这个措辞取自 kiro2api（Codex 实测认可）。
const BASE_INSTRUCTIONS: &str =
    "You are Codex, a coding agent. You and the user share the same workspace and collaborate to achieve the user's goals.";

#[derive(Clone, Default, Debug, Serialize)]
pub struct CodexState {
    /// `~/.codex` 目录是否存在（没装 Codex 就没得接）
    pub found: bool,
    pub config_path: String,
    /// `model_provider` 是否已指向本网关
    pub connected: bool,
    /// provider 段里声明的 base_url（判别"接的是不是同一个端口"）
    pub base_url: String,
    /// 当前 `model` 顶层键
    pub model: String,
    /// 我们写进 `model_catalog_json` 的模型 slug（按文件里的顺序）。
    ///
    /// 页面上的模型多选要**回读已写入的集合**：只按"上次勾了什么"回填的话，
    /// 用户手改过目录文件、或换过一台机器，界面就会显示一份并不存在的勾选状态。
    /// 空 = 还没接过 / 目录文件被删了 / 那份目录不是我们的。
    pub catalog_models: Vec<String>,
}

/// Codex 的配置目录。支持官方的 `CODEX_HOME` 覆盖。
fn codex_home() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("CODEX_HOME") {
        if !v.trim().is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// 读当前接入状态（不写任何东西）
pub fn state() -> CodexState {
    match codex_home() {
        Some(home) => state_at(&home),
        None => CodexState::default(),
    }
}

/// 指定目录下的状态（测试用：不碰真实的 `~/.codex`）
fn state_at(home: &Path) -> CodexState {
    let config = home.join("config.toml");
    let mut st = CodexState {
        found: home.is_dir(),
        config_path: config.display().to_string(),
        ..Default::default()
    };
    if !st.found {
        return st;
    }
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    st.connected = get_top_level(&text, "model_provider").as_deref() == Some(PROVIDER_ID);
    st.base_url = table_value(&text, &format!("model_providers.{PROVIDER_ID}"), "base_url")
        .unwrap_or_default();
    st.model = get_top_level(&text, "model").unwrap_or_default();
    st.catalog_models = catalog_slugs(
        &std::fs::read_to_string(home.join(CATALOG_FILE)).unwrap_or_default(),
    );
    st
}

/// 读我们写的模型目录，抽出 slug 列表。
///
/// 结构不对（不是我们的文件、被手改成别的形状）一律当空，不猜。
fn catalog_slugs(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("slug").and_then(|s| s.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 接入：改 provider 段 + 三个顶层键 + 写模型目录。
///
/// `models` 是**用户勾选后**的 `(slug, display_name)`，来自上游模型清单
/// （已过滤掉非对话模型）。目录里写哪些模型由它决定 —— 多选即多模型，
/// Codex 的 `/model` 就会列出这几项；没勾的不会出现在里面。
///
/// `model` 是默认模型，必须在这批 `models` 里（调用方已校验过上游清单，
/// 这里再校验一次子集关系，防止两处口径漂移）。
pub fn connect(
    port: u16,
    key: &str,
    model: &str,
    models: &[(String, String)],
    backup_dir: &Path,
) -> Result<CodexState, String> {
    let home = codex_home().ok_or("找不到用户主目录，无法定位 Codex 配置")?;
    connect_in(&home, port, key, model, models, backup_dir)
}

fn connect_in(
    home: &Path,
    port: u16,
    key: &str,
    model: &str,
    models: &[(String, String)],
    backup_dir: &Path,
) -> Result<CodexState, String> {
    if !home.is_dir() {
        return Err(format!(
            "没有找到 Codex 配置目录（{}）。请先安装并运行一次 Codex。",
            home.display()
        ));
    }
    if models.is_empty() {
        return Err("至少要勾选一个模型：模型目录是空的，Codex 里就一个模型都选不出来".into());
    }
    // 选了一个清单里没有的模型 → 直接拒绝。静默换成别的会让用户以为自己的选择生效了，
    // 而 Codex 里显示的是另一个名字，很难对上因果。
    let model = model.trim();
    if !model.is_empty() && !models.iter().any(|(s, _)| s == model) {
        return Err(format!(
            "模型 `{model}` 不在已勾选的模型里（网关可能刚清过缓存或清单变了），请刷新模型列表后重选"
        ));
    }
    let config = home.join("config.toml");
    let original = std::fs::read_to_string(&config).unwrap_or_default();

    // 只在**首次**接入时备份：重复接入时原文件已经是改过的版本，
    // 再存一次就把「原件」覆盖成了「改过的」，断开时会恢复不回去
    let backup = backup_dir.join(BACKUP_FILE);
    if !backup.exists() {
        if let Some(p) = backup.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(&backup, &original).map_err(|e| format!("备份 Codex 配置失败：{e}"))?;
    }

    let catalog = build_catalog(models, model);
    std::fs::write(home.join(CATALOG_FILE), catalog)
        .map_err(|e| format!("写模型目录失败：{e}"))?;

    let mut text = original;
    // 关掉内置 apps/connectors 后台：它只认 chatgpt.com，连不上就每 10s 重试一次，
    // 一个 turn 永远走不到结束（见文件头「两个实测结论」）。只改这一个键 ——
    // 用户在这个表里可能还开着别的 feature，整表覆盖会把它们吞掉。
    text = set_table_key(&text, "features", "apps", "false");
    text = upsert_table(
        &text,
        &format!("model_providers.{PROVIDER_ID}"),
        &provider_block(port, key),
    );
    text = set_top_level(&text, "model_provider", &format!("\"{PROVIDER_ID}\""));
    text = set_top_level(&text, "model_catalog_json", &format!("\"{CATALOG_FILE}\""));
    if !model.trim().is_empty() {
        text = set_top_level(&text, "model", &format!("\"{}\"", model.trim()));
    }
    std::fs::write(&config, text).map_err(|e| format!("写 Codex 配置失败：{e}"))?;

    // 回读确认：写成功不代表写对了（权限、并发编辑都可能让结果不如预期）
    let st = state_at(home);
    if !st.connected {
        return Err("写入完成但回读发现未生效，请检查 config.toml 是否可写".into());
    }
    Ok(st)
}

/// 断开：删 provider 段，三个顶层键恢复成备份里的原值（备份里没有就删掉）。
pub fn disconnect(backup_dir: &Path) -> Result<CodexState, String> {
    let home = codex_home().ok_or("找不到用户主目录，无法定位 Codex 配置")?;
    disconnect_in(&home, backup_dir)
}

fn disconnect_in(home: &Path, backup_dir: &Path) -> Result<CodexState, String> {
    if !home.is_dir() {
        return Err(format!("没有找到 Codex 配置目录（{}）", home.display()));
    }
    let config = home.join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap_or_default();
    text = remove_table(&text, &format!("model_providers.{PROVIDER_ID}"));

    let backup = backup_dir.join(BACKUP_FILE);
    let orig = std::fs::read_to_string(&backup).unwrap_or_default();
    // `model` 也要恢复：留着我们的模型名，换回官方/别的 provider 后 Codex
    // 会因为「模型不存在」直接起不来
    for key in ["model_provider", "model_catalog_json", "model"] {
        text = match get_top_level(&orig, key) {
            Some(v) => set_top_level(&text, key, &format!("\"{v}\"")),
            None => remove_top_level(&text, key),
        };
    }
    // `[features] apps` 是我们为了绕开 chatgpt 依赖关掉的，断开必须还原：
    // 备份里原本没有这个键就删掉它（表空了连表头一起删），有则按原文写回
    // （原文可能是不带引号的布尔，套上引号就变成了字符串，语义不同）。
    text = match table_raw_value(&orig, "features", "apps") {
        Some(v) => set_table_key(&text, "features", "apps", &v),
        None => drop_table_key(&text, "features", "apps"),
    };
    std::fs::write(&config, text).map_err(|e| format!("写 Codex 配置失败：{e}"))?;
    // 模型目录是我们生成的文件（固定文件名、只写不读别的），断开时一并清掉：
    // 留着它，下次换回官方 provider 后 Codex 里会多出一份「模型目录」残留。
    // 删不掉（被占用/权限）不算失败 —— 配置已经还原，一个孤儿 json 不致命。
    let _ = std::fs::remove_file(home.join(CATALOG_FILE));
    Ok(state_at(home))
}

/// 按用户勾选过滤上游清单，得到要写进模型目录的子集。
///
/// 顺序沿用上游（免费 → 倍率升序 → 未知垫底）：勾选顺序是「点了几下」的产物，
/// 拿它排目录会让 Codex 的 `/model` 列表按点击先后乱跳。
///
/// 勾了一个上游已经没有的模型（清单换过、缓存过期）→ **报错**，不静默丢弃：
/// 用户会以为那项还接得进去，而 Codex 里少了它，因果对不上。
pub fn pick_models(all: &[ChatModel], picked: &[String]) -> Result<Vec<(String, String)>, String> {
    if picked.is_empty() {
        return Err("至少要勾选一个模型".into());
    }
    let unknown: Vec<&str> = picked
        .iter()
        .map(String::as_str)
        .filter(|p| !all.iter().any(|m| m.slug == *p))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "这些模型不在上游清单里了：{}（点「刷新模型」重新拉一次）",
            unknown.join("、")
        ));
    }
    Ok(all
        .iter()
        .filter(|m| picked.iter().any(|p| *p == m.slug))
        .map(|m| (m.slug.clone(), m.name.clone()))
        .collect())
}

fn provider_block(port: u16, key: &str) -> String {
    format!(
        // 用 127.0.0.1 而不是 localhost：Windows 上 localhost 可能先解析到 ::1，
        // 而网关只绑 127.0.0.1（IPv4），会连不上
        "name = \"WorkBuddy2API\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nwire_api = \"responses\"\nexperimental_bearer_token = \"{key}\""
    )
}

/// 生成 Codex 的 `model_catalog_json`。
///
/// 每个上游模型一条，`display_name` 用上游给的中文名，`priority` 让默认模型排最前。
pub fn build_catalog(models: &[(String, String)], default_model: &str) -> String {
    let items: Vec<serde_json::Value> = models
        .iter()
        .enumerate()
        .map(|(i, (slug, name))| {
            let priority = if slug == default_model { 0 } else { 1000 + i as i64 };
            serde_json::json!({
                "slug": slug,
                "display_name": if name.trim().is_empty() { slug } else { name },
                "description": format!("WorkBuddy2API 本地网关 · {slug}"),
                "base_instructions": BASE_INSTRUCTIONS,
                "context_window": CONTEXT_WINDOW,
                "max_context_window": CONTEXT_WINDOW,
                "effective_context_window_percent": 95,
                "truncation_policy": { "mode": "tokens", "limit": CONTEXT_WINDOW },
                "priority": priority,
                "visibility": "list",
                "supported_in_api": true,
                // 上游多模态支持情况未知，只声明文本：Codex 就不会发图片，
                // 不会因为一张截图把整轮对话打成 400
                "input_modalities": ["text"],
                "supports_parallel_tool_calls": false,
                "supports_reasoning_summaries": false,
                "supports_search_tool": false,
                "supports_image_detail_original": false,
                "support_verbosity": false,
                "default_verbosity": serde_json::Value::Null,
                "default_reasoning_level": serde_json::Value::Null,
                // 空数组：Codex 就不显示 effort 选择器，也不会发 reasoning.effort
                "supported_reasoning_levels": [],
                "apply_patch_tool_type": serde_json::Value::Null,
                "shell_type": "shell_command",
                "experimental_supported_tools": [],
                "additional_speed_tiers": [],
                "service_tiers": [],
                "availability_nux": serde_json::Value::Null,
                "upgrade": serde_json::Value::Null,
            })
        })
        .collect();
    serde_json::json!({ "models": items }).to_string()
}

// ---------------------------------------------------------------------------
// 文本手术（纯函数，全部可测）
// ---------------------------------------------------------------------------

/// 顶层键区域的字节长度：第一个 `[table]` 行之前的部分。
fn head_span(text: &str) -> usize {
    let mut off = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            return off;
        }
        off += line.len();
    }
    off
}

fn is_key_line(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('#') || t.starts_with('[') {
        return false;
    }
    match t.split_once('=') {
        Some((k, _)) => k.trim().trim_matches('"') == key,
        None => false,
    }
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    let bytes = v.as_bytes();
    if bytes.len() >= 2 {
        let (a, b) = (bytes[0], bytes[bytes.len() - 1]);
        if (a == b'"' && b == b'"') || (a == b'\'' && b == b'\'') {
            return &v[1..v.len() - 1];
        }
    }
    v
}

/// 读一个顶层键（只在第一个表头之前找，避免把表里的同名键当成顶层键）
fn get_top_level(text: &str, key: &str) -> Option<String> {
    let span = head_span(text);
    for line in text[..span].lines() {
        if is_key_line(line, key) {
            if let Some((_, v)) = line.split_once('=') {
                return Some(unquote(v).to_string());
            }
        }
    }
    None
}

/// 设置一个顶层键：有就原地替换，没有就插在顶层区末尾
fn set_top_level(text: &str, key: &str, val: &str) -> String {
    let span = head_span(text);
    let (head, rest) = text.split_at(span);
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in head.lines() {
        if is_key_line(line, key) {
            lines.push(format!("{key} = {val}"));
            found = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        // 插在顶层键块的末尾（尾部空行**之前**）：跟在空行后面会让新键看起来
        // 像是属于下一张表的
        let mut at = lines.len();
        while at > 0 && lines[at - 1].trim().is_empty() {
            at -= 1;
        }
        lines.insert(at, format!("{key} = {val}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out.push_str(rest);
    out
}

/// 删一个顶层键
fn remove_top_level(text: &str, key: &str) -> String {
    let span = head_span(text);
    let (head, rest) = text.split_at(span);
    let lines: Vec<&str> = head
        .lines()
        .filter(|l| !is_key_line(l, key))
        .collect();
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(rest);
    out
}

/// 读某张表里的一个键的**原文**（保留引号）—— 「原样写回」必须靠它：
/// `apps = false` 是布尔、`"false"` 是字符串，去引号再写回去语义就变了。
fn table_raw_value(text: &str, table: &str, key: &str) -> Option<String> {
    let header = format!("[{table}]");
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == header;
            continue;
        }
        if inside && is_key_line(line, key) {
            if let Some((_, v)) = line.split_once('=') {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// 读某张表里的一个键（去掉引号）
fn table_value(text: &str, table: &str, key: &str) -> Option<String> {
    table_raw_value(text, table, key).map(|v| unquote(&v).to_string())
}

/// 在列表末尾、但**在尾部空行之前**插一行。
///
/// 跟在空行后面会让这一行看起来属于下一张表 —— TOML 里那等于把键写进了别的表。
fn push_before_blanks(out: &mut Vec<String>, line: String) {
    let mut at = out.len();
    while at > 0 && out[at - 1].trim().is_empty() {
        at -= 1;
    }
    out.insert(at, line);
}

/// 表内设置一个键：有就原地替换，没有就追加到表体末尾，表不存在就新建整张表。
///
/// 与 [`upsert_table`] 的分工：`[model_providers.workbuddy2api]` 是**我们自己的**表，
/// 整段覆盖才对；`[features]` 是用户本来就有的表，只许碰 `apps` 这一行 ——
/// 整表替换会把用户开的其它 feature 一起吞掉。
fn set_table_key(text: &str, table: &str, key: &str, val: &str) -> String {
    let header = format!("[{table}]");
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut done = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            // 这张表到头了：该键还没出现过就补在表体末尾（有空行则在空行之前）
            if inside && !done {
                push_before_blanks(&mut out, format!("{key} = {val}"));
                done = true;
            }
            inside = t == header;
            out.push(line.to_string());
            continue;
        }
        if inside {
            if is_key_line(line, key) {
                if !done {
                    out.push(format!("{key} = {val}"));
                    done = true;
                }
                continue; // 同名的重复行丢掉，只留一行
            }
            out.push(line.to_string());
            continue;
        }
        out.push(line.to_string());
    }
    if inside && !done {
        push_before_blanks(&mut out, format!("{key} = {val}"));
        done = true;
    }
    if !done {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(header);
        out.push(format!("{key} = {val}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// 删表内的一个键；删完表体空了，连表头一起删 —— 空表是只在文件里多出来的东西。
fn drop_table_key(text: &str, table: &str, key: &str) -> String {
    let header = format!("[{table}]");
    // 表头是最后压进去的一行 ⇒ 表体一行都没留下 ⇒ 连表头（与其前空行）一起摘掉
    fn prune(out: &mut Vec<String>, header: &str) {
        if out.last().is_some_and(|l| l.trim() == header) {
            out.pop();
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if inside {
                prune(&mut out, &header);
            }
            inside = t == header;
            out.push(line.to_string());
            continue;
        }
        if inside && is_key_line(line, key) {
            continue;
        }
        out.push(line.to_string());
    }
    if inside {
        prune(&mut out, &header);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        return String::new();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// 整段替换一张表（不存在就追加到文件末尾）
fn upsert_table(text: &str, table: &str, body: &str) -> String {
    let header = format!("[{table}]");
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if t == header {
            if !replaced {
                out.push(header.clone());
                for b in body.lines() {
                    out.push(b.to_string());
                }
                replaced = true;
            }
            skipping = true;
            continue;
        }
        if skipping {
            if t.starts_with('[') {
                skipping = false; // 落到下面照常输出
            } else {
                continue; // 丢掉旧表体
            }
        }
        out.push(line.to_string());
    }
    if !replaced {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(header);
        for b in body.lines() {
            out.push(b.to_string());
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// 整段删掉一张表
fn remove_table(text: &str, table: &str) -> String {
    let header = format!("[{table}]");
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if t == header {
            skipping = true;
            continue;
        }
        if skipping {
            if t.starts_with('[') {
                skipping = false;
            } else {
                continue;
            }
        }
        out.push(line.to_string());
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        return String::new();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 config.toml 的骨架（2026-11-24 本机现状，已脱敏）
    fn real_config() -> String {
        r#"model_reasoning_effort = "high"
disable_response_storage = true
model_provider = "kiro2api"
model_catalog_json = "kiro2api-model-catalog.json"
model = "gpt-5.6-luna"
notify = [ "C:\\Users\\someone\\codex-computer-use.exe", "turn-ended" ]

[model_providers.custom]
name = "My Codex"
base_url = "https://example.com/v1"
wire_api = "responses"
requires_openai_auth = true

[model_providers.kiro2api]
name = "kiro2api"
base_url = "http://127.0.0.1:5680/openai/v1"
wire_api = "responses"
experimental_bearer_token = "k2a-xxx"

[windows]
sandbox = "elevated"

[plugins."browser@openai-bundled"]
enabled = true
"#
        .to_string()
    }

    #[test]
    fn head_span_stops_at_the_first_table() {
        let text = "a = 1\nb = 2\n\n[t]\nc = 3\n";
        assert_eq!(&text[..head_span(text)], "a = 1\nb = 2\n\n");
    }

    #[test]
    fn head_span_of_a_table_only_file_is_zero() {
        assert_eq!(head_span("[t]\na = 1\n"), 0);
    }

    #[test]
    fn set_top_level_replaces_in_place() {
        let out = set_top_level("a = 1\nmodel_provider = \"old\"\nb = 2\n", "model_provider", "\"new\"");
        assert!(out.contains("model_provider = \"new\""));
        assert!(!out.contains("\"old\""));
        // 位置不变：还在 a 与 b 之间
        assert_eq!(out.lines().nth(1).unwrap(), "model_provider = \"new\"");
    }

    #[test]
    fn set_top_level_inserts_when_missing() {
        let out = set_top_level("a = 1\n\n[t]\nb = 2\n", "model_provider", "\"wb\"");
        assert_eq!(out, "a = 1\nmodel_provider = \"wb\"\n\n[t]\nb = 2\n");
    }

    #[test]
    fn set_top_level_does_not_touch_same_named_key_inside_a_table() {
        let text = "[t]\nmodel = \"inside\"\n";
        let out = set_top_level(text, "model", "\"outside\"");
        assert!(out.contains("model = \"inside\""), "表里的同名键不能改");
        assert!(out.contains("model = \"outside\""));
    }

    #[test]
    fn get_top_level_ignores_table_values() {
        let text = "[t]\nmodel = \"inside\"\n";
        assert_eq!(get_top_level(text, "model"), None);
        let text = "model = \"outer\"\n[t]\nmodel = \"inside\"\n";
        assert_eq!(get_top_level(text, "model").as_deref(), Some("outer"));
    }

    #[test]
    fn unquotes_both_quote_styles() {
        let text = "a = \"x\"\nb = 'y'\nc = 3\n";
        assert_eq!(get_top_level(text, "a").as_deref(), Some("x"));
        assert_eq!(get_top_level(text, "b").as_deref(), Some("y"));
        assert_eq!(get_top_level(text, "c").as_deref(), Some("3"));
    }

    #[test]
    fn upsert_table_replaces_only_that_table() {
        let out = upsert_table(&real_config(), "model_providers.kiro2api", "name = \"replaced\"");
        assert!(out.contains("[model_providers.kiro2api]\nname = \"replaced\""));
        assert!(!out.contains("k2a-xxx"), "旧表体必须整段丢掉");
        // 邻居表原样保留
        assert!(out.contains("[model_providers.custom]"));
        assert!(out.contains("https://example.com/v1"));
        assert!(out.contains("[plugins.\"browser@openai-bundled\"]"));
        assert!(out.contains("enabled = true"));
    }

    #[test]
    fn upsert_table_appends_when_absent() {
        let out = upsert_table(&real_config(), "model_providers.workbuddy2api", "name = \"new\"");
        assert!(out.ends_with("[model_providers.workbuddy2api]\nname = \"new\"\n"));
        assert!(out.contains("[model_providers.kiro2api]"), "已有的表不能被碰到");
    }

    #[test]
    fn remove_table_leaves_neighbours_intact() {
        let out = remove_table(&real_config(), "model_providers.kiro2api");
        assert!(!out.contains("[model_providers.kiro2api]"));
        assert!(!out.contains("k2a-xxx"));
        assert!(out.contains("[windows]\nsandbox = \"elevated\""));
        assert!(out.contains("model_provider = \"kiro2api\""), "顶层键不归 remove_table 管");
    }

    #[test]
    fn remove_top_level_drops_the_line() {
        let out = remove_top_level("a = 1\nmodel_catalog_json = \"x.json\"\n[t]\nb = 2\n", "model_catalog_json");
        assert!(!out.contains("model_catalog_json"));
        assert!(out.contains("[t]\nb = 2"));
    }

    /// 接入 → 断开 必须回到原样（这是"改了用户文件"的最低要求）
    #[test]
    fn connect_then_disconnect_restores_the_original_top_level_keys() {
        let orig = real_config();
        let mut t = orig.clone();
        t = upsert_table(&t, "model_providers.workbuddy2api", &provider_block(8790, "sk-wb-1"));
        t = set_top_level(&t, "model_provider", "\"workbuddy2api\"");
        t = set_top_level(&t, "model_catalog_json", "\"workbuddy2api-model-catalog.json\"");
        t = set_top_level(&t, "model", "\"hy4flash-free\"");

        assert_eq!(get_top_level(&t, "model_provider").as_deref(), Some("workbuddy2api"));
        assert_eq!(table_value(&t, "model_providers.workbuddy2api", "base_url").as_deref(), Some("http://127.0.0.1:8790/v1"));

        // 断开
        let mut back = remove_table(&t, "model_providers.workbuddy2api");
        for key in ["model_provider", "model_catalog_json", "model"] {
            back = match get_top_level(&orig, key) {
                Some(v) => set_top_level(&back, key, &format!("\"{v}\"")),
                None => remove_top_level(&back, key),
            };
        }
        for key in ["model_provider", "model_catalog_json", "model"] {
            assert_eq!(get_top_level(&back, key), get_top_level(&orig, key), "{key} 没恢复");
        }
        assert!(!back.contains("workbuddy2api"));
    }

    #[test]
    fn provider_block_pins_ipv4_loopback() {
        let b = provider_block(8790, "sk-wb-abc");
        assert!(b.contains("http://127.0.0.1:8790/v1"));
        assert!(!b.contains("localhost"), "Windows 上 localhost 可能先走 ::1");
        assert!(b.contains("wire_api = \"responses\""));
        assert!(b.contains("experimental_bearer_token = \"sk-wb-abc\""));
    }

    #[test]
    fn catalog_has_one_entry_per_model_with_default_first() {
        let models = vec![
            ("a-free".to_string(), "A".to_string()),
            ("b-paid".to_string(), "B".to_string()),
        ];
        let v: serde_json::Value =
            serde_json::from_str(&build_catalog(&models, "b-paid")).unwrap();
        let arr = v["models"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let b = arr.iter().find(|m| m["slug"] == "b-paid").unwrap();
        let a = arr.iter().find(|m| m["slug"] == "a-free").unwrap();
        assert!(b["priority"].as_i64() < a["priority"].as_i64(), "默认模型要排前面");
        assert_eq!(a["display_name"], "A");
        assert_eq!(a["supported_reasoning_levels"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn catalog_falls_back_to_slug_when_display_name_is_blank() {
        let models = vec![("m1".to_string(), "  ".to_string())];
        let v: serde_json::Value = serde_json::from_str(&build_catalog(&models, "m1")).unwrap();
        assert_eq!(v["models"][0]["display_name"], "m1");
    }

    /// 多选：只把勾到的写进目录，且顺序沿用上游（不看点击顺序）
    ///
    /// 清单解析与排序由 [`crate::catalog`] 负责（那边有自己的单测），这里只验
    /// 「勾选 → 目录子集」这一步：它必须保序、必须拒绝空选、必须点名拒绝未知 slug。
    #[test]
    fn pick_models_keeps_upstream_order_and_drops_the_rest() {
        let body = r#"{"object":"list","data":[
            {"id":"a-free","workbuddy":{"name":"A","free":true,"credits":"x0.00"}},
            {"id":"b-mid","workbuddy":{"name":"B","free":false,"credits":"x0.29"}},
            {"id":"c-deep","workbuddy":{"name":"C","free":false,"credits":"x1.00"}}
        ]}"#;
        let all = crate::catalog::chat_models(body);
        // 用户先点了 c 再点 a：写进目录的仍是上游顺序 a、c
        let picked = vec!["c-deep".to_string(), "a-free".to_string()];
        let got = pick_models(&all, &picked).unwrap();
        assert_eq!(
            got,
            vec![
                ("a-free".to_string(), "A".to_string()),
                ("c-deep".to_string(), "C".to_string()),
            ]
        );
        // 一个都没勾：报错而不是写空目录
        assert!(pick_models(&all, &[]).is_err());
        // 勾了上游已经没有的模型：报错并点名，不静默丢弃
        let err = pick_models(&all, &["gone-model".to_string()]).unwrap_err();
        assert!(err.contains("gone-model"), "{err}");
    }

    #[test]
    fn catalog_slugs_reads_our_own_file_and_ignores_junk() {
        let json = r#"{"models":[{"slug":"a"},{"slug":"b"}]}"#;
        assert_eq!(catalog_slugs(json), vec!["a".to_string(), "b".to_string()]);
        assert!(catalog_slugs("").is_empty());
        assert!(catalog_slugs("not json").is_empty());
        assert!(catalog_slugs(r#"{"models":"nope"}"#).is_empty());
    }

    /// 用户真实配置里可能已经有 `[features]`，且开着别的开关 —— 只许碰 `apps` 那一行
    fn config_with_features() -> String {
        format!(
            "{}\n[features]\napps = true\nmulti_agent = true\n",
            real_config().trim_end()
        )
    }

    #[test]
    fn set_table_key_replaces_only_that_key() {
        let out = set_table_key(&config_with_features(), "features", "apps", "false");
        assert!(
            out.contains("[features]\napps = false\nmulti_agent = true\n"),
            "{out}"
        );
        assert!(!out.contains("apps = true"), "旧值必须被替换掉：\n{out}");
    }

    #[test]
    fn set_table_key_appends_when_the_key_is_missing() {
        // 表在、键不在（用户开了别的 feature，但没写 apps）
        let src = "[features]\nmulti_agent = true\n\n[windows]\nsandbox = \"elevated\"\n";
        let out = set_table_key(src, "features", "apps", "false");
        // 必须插在空行**之前** —— 跟在空行后面就成了 `[windows]` 的键
        assert_eq!(
            out,
            "[features]\nmulti_agent = true\napps = false\n\n[windows]\nsandbox = \"elevated\"\n"
        );
    }

    #[test]
    fn set_table_key_creates_the_table_when_absent() {
        let out = set_table_key("model = \"x\"\n", "features", "apps", "false");
        assert_eq!(out, "model = \"x\"\n\n[features]\napps = false\n");
    }

    #[test]
    fn drop_table_key_keeps_the_table_when_others_remain() {
        let src = "[features]\napps = false\nmulti_agent = true\n\n[windows]\nsandbox = \"elevated\"\n";
        let out = drop_table_key(src, "features", "apps");
        assert_eq!(
            out,
            "[features]\nmulti_agent = true\n\n[windows]\nsandbox = \"elevated\"\n"
        );
    }

    #[test]
    fn drop_table_key_removes_the_emptied_table() {
        // 这个键是我们加的、表里原本没别的 → 表头也要一起删，不留空表
        let out = drop_table_key("model = \"x\"\n\n[features]\napps = false\n", "features", "apps");
        assert_eq!(out, "model = \"x\"\n");
    }

    /// 接入写的是 `apps = false`，且**不**碰已废弃的 `disable_response_storage`
    #[test]
    fn connect_writes_features_apps_and_leaves_the_deprecated_key_alone() {
        let base = std::env::temp_dir().join("wb2api_codex_features");
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        let orig = real_config();
        std::fs::write(home.join("config.toml"), &orig).unwrap();

        let models = vec![("hy4flash-free".to_string(), "Hy4 flash".to_string())];
        connect_in(&home, 8790, "sk-wb-test", "hy4flash-free", &models, &base).unwrap();

        let cfg = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            cfg.contains("[features]\napps = false"),
            "少了 apps = false，Codex 会一直重试 chatgpt 的 mcp 端点、turn 走不完：\n{cfg}"
        );
        // 0.171 会把这个键报成 is ignored：我们既不写、也不替用户删（那是他自己的配置）
        assert_eq!(
            cfg.matches("disable_response_storage").count(),
            orig.matches("disable_response_storage").count(),
            "不该新增或删除这个已废弃的键：\n{cfg}"
        );
        assert!(cfg.contains("[plugins.\"browser@openai-bundled\"]"), "别的表不能被碰");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 用户原本 `apps = true` → 接入改 false，断开必须恢复 true
    #[test]
    fn features_apps_original_value_is_restored_on_disconnect() {
        let base = std::env::temp_dir().join("wb2api_codex_features_restore");
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(home.join("config.toml"), config_with_features()).unwrap();

        let models = vec![("hy4flash-free".to_string(), "Hy4 flash".to_string())];
        connect_in(&home, 8790, "k", "hy4flash-free", &models, &base).unwrap();
        let cfg = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(cfg.contains("apps = false"), "{cfg}");

        disconnect_in(&home, &base).unwrap();
        let back = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            back.contains("[features]\napps = true\nmulti_agent = true\n"),
            "断开必须把 apps 还原成 true、且不动同表的其它开关：\n{back}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 用户原本根本没有 `[features]` 表 → 断开后整张表消失，不留空表
    #[test]
    fn features_table_that_never_existed_is_removed_on_disconnect() {
        let base = std::env::temp_dir().join("wb2api_codex_features_gone");
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(home.join("config.toml"), real_config()).unwrap();

        let models = vec![("hy4flash-free".to_string(), "Hy4 flash".to_string())];
        connect_in(&home, 8790, "k", "hy4flash-free", &models, &base).unwrap();
        assert!(std::fs::read_to_string(home.join("config.toml"))
            .unwrap()
            .contains("[features]"));
        disconnect_in(&home, &base).unwrap();

        let back = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(!back.contains("[features]"), "不该留一张空表：\n{back}");
        assert!(back.contains("model = \"gpt-5.6-luna\""), "原值要还原：\n{back}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejecting_a_model_outside_the_catalog() {
        let base = std::env::temp_dir().join("wb2api_codex_bad_model");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config.toml"), "model = \"old\"\n").unwrap();
        let models = vec![("a".to_string(), "A".to_string())];
        let err = connect_in(&base, 8790, "k", "not-in-list", &models, &base).unwrap_err();
        assert!(err.contains("不在已勾选的模型里"), "{err}");
        // 拒绝＝一个字节都没写
        let after = std::fs::read_to_string(base.join("config.toml")).unwrap();
        assert_eq!(after, "model = \"old\"\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 真实文件往返：接入 → 重复接入 → 断开，最后必须回到原样。
    ///
    /// 用临时目录，绝不碰真实的 `~/.codex`。
    #[test]
    fn file_round_trip_restores_the_original_config() {
        let base = std::env::temp_dir().join("wb2api_codex_roundtrip");
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("codex");
        let data = base.join("appdata");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let orig = real_config();
        std::fs::write(home.join("config.toml"), &orig).unwrap();

        let models = vec![
            ("hy4flash-free".to_string(), "Hy4 flash".to_string()),
            ("deepseek-v4.1-flash".to_string(), "DeepSeek".to_string()),
        ];

        assert!(!state_at(&home).connected, "初始应是未接入");
        let st = connect_in(&home, 8790, "sk-wb-test", "hy4flash-free", &models, &data)
            .expect("接入失败");
        assert!(st.connected);
        assert_eq!(st.base_url, "http://127.0.0.1:8790/v1");
        assert_eq!(st.model, "hy4flash-free");

        // 模型目录要真的落盘
        let cat = std::fs::read_to_string(home.join(CATALOG_FILE)).unwrap();
        assert!(cat.contains("\"slug\":\"hy4flash-free\""));
        assert!(cat.contains("\"slug\":\"deepseek-v4.1-flash\""));
        // 状态要能回读出「目录里有哪几个模型」（页面按它回填勾选）
        assert_eq!(
            state_at(&home).catalog_models,
            vec![
                "hy4flash-free".to_string(),
                "deepseek-v4.1-flash".to_string()
            ]
        );

        // 用户其它配置一字不动
        let after = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(after.contains("[plugins.\"browser@openai-bundled\"]"));
        assert!(after.contains("notify = ["));
        assert!(after.contains("[model_providers.kiro2api]"), "别人的 provider 段不能删");
        assert!(after.contains("[features]\napps = false"), "必须关掉内置 apps 后台");

        // 重复接入（换端口、换模型）：备份必须还是最初那份
        connect_in(&home, 8791, "sk-wb-test2", "deepseek-v4.1-flash", &models, &data).unwrap();
        let backup = std::fs::read_to_string(data.join(BACKUP_FILE)).unwrap();
        assert_eq!(backup, orig, "备份被覆盖成改过的版本，断开就恢复不回去了");

        let st = disconnect_in(&home, &data).expect("断开失败");
        assert!(!st.connected);
        assert!(st.catalog_models.is_empty(), "断开后目录不算数了");
        assert!(
            !home.join(CATALOG_FILE).exists(),
            "断开要顺手删掉我们生成的模型目录，否则换回官方 provider 后会留一份孤儿目录"
        );
        let back = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(!back.contains("workbuddy2api"), "断开后不该留痕迹：\n{back}");
        assert!(back.contains("model_provider = \"kiro2api\""));
        assert!(back.contains("model = \"gpt-5.6-luna\""));
        assert!(back.contains("model_catalog_json = \"kiro2api-model-catalog.json\""));
        assert!(!back.contains("[features]"), "原文件没有 [features]，断开后不该多出一张空表");
        // 行数只允许因为删掉 provider 段而减少，不该多出空行
        assert!(
            back.lines().count() <= orig.lines().count(),
            "还原后比原文件还长，说明留了垃圾：\n{back}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_codex_dir_is_a_clear_error() {
        let base = std::env::temp_dir().join("wb2api_codex_missing");
        let _ = std::fs::remove_dir_all(&base);
        let err = connect_in(&base, 8790, "k", "m", &[], &base).unwrap_err();
        assert!(err.contains("没有找到 Codex 配置目录"), "{err}");
    }

    /// 只读演练：拿**真实**的 `~/.codex/config.toml` 在内存里跑一遍变换，
    /// 打印 diff 供人工核对。**一个字节都不写。**
    ///
    /// ```bash
    /// cargo test --offline --lib -- --ignored --nocapture preview_real_config
    /// ```
    #[test]
    #[ignore]
    fn preview_real_config() {
        let Some(home) = codex_home() else {
            println!("找不到 Codex 目录");
            return;
        };
        let path = home.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            println!("读不到 {}", path.display());
            return;
        };
        let mut after = text.clone();
        after = set_table_key(&after, "features", "apps", "false");
        after = upsert_table(&after, &format!("model_providers.{PROVIDER_ID}"), &provider_block(8790, "sk-wb-preview"));
        after = set_top_level(&after, "model_provider", &format!("\"{PROVIDER_ID}\""));
        after = set_top_level(&after, "model_catalog_json", &format!("\"{CATALOG_FILE}\""));
        after = set_top_level(&after, "model", "\"hy4flash-free\"");

        let a: Vec<&str> = text.lines().collect();
        let b: Vec<&str> = after.lines().collect();
        println!("=== {} ({} 行 → {} 行) ===", path.display(), a.len(), b.len());
        for l in &b {
            if !a.contains(l) {
                println!("+ {l}");
            }
        }
        for l in &a {
            if !b.contains(l) {
                println!("- {l}");
            }
        }
        println!("=== 断开应恢复成 ===");
        for k in ["model_provider", "model_catalog_json", "model"] {
            println!("{k} = {:?}", get_top_level(&text, k));
        }
        println!(
            "features.apps = {:?}（原文；原来是 None 就删掉这个键）",
            table_raw_value(&text, "features", "apps")
        );
    }
}
