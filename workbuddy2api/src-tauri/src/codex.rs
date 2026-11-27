//! 一键接入 Codex：改写 `~/.codex/config.toml` + 生成模型目录。
//!
//! # 为什么用 toml_edit
//!
//! 用户的 `config.toml` 里有 `plugins`、`marketplaces`、`notify`、Windows 的
//! `\\?\C:\...` 转义路径一大堆。`toml::to_string` 整体重写会打乱注释、重排键序、
//! 把 `\?\` 这类字面量重新转义 —— 相当于为了改 3 行而重写整个文件。
//!
//! `toml_edit` 是保留原始格式的可编辑文档：只改我们关心的三个顶层键
//! （`model_provider`、`model_catalog_json`、`model`）、`[features]` 里[受管的那几个开关]
//! （[`DISABLED_FEATURES`]）与一个表（`[model_providers.workbuddy2api]`），
//! 其余注释、键序与字面量原样保留。
//!
//! # 为什么写之前要备份
//!
//! 这是用户机器上**别人的**配置文件，改坏了 Codex 就起不来。备份写在应用自己的
//! 数据目录（`<app_data>/codex-config.bak.toml`），不往 `.codex/` 里扔垃圾。
//! 断开时按备份恢复那几个键的原值。
//!
//! # 实测结论（2026-11-24 / 11-25，Codex 桌面端 26.1117.41037 / Windows）
//!
//! ## 一、turn 准备阶段会去 chatgpt.com 的几条链路（本模块关掉的就是这些）
//!
//! 本网关给的是本地 provider、本机也没有 ChatGPT 账号，这些请求注定失败；失败方式
//! 决定体验好坏：直连不通会立刻 RST（无感），经企业代理/DLP 时常见「TCP 能建连、
//! 永不回包」，每条都要等满超时。
//!
//! * `[features] apps = false` —— Codex 内置的 `codex_apps` MCP 服务要走
//!   `https://chatgpt.com/backend-api/*` 拉 apps/connectors 目录。用
//!   `codex debug prompt-input` 量化：不关时 30 秒都走不完（日志里那句
//!   `omitting MCP server without an exact ready client server_name=codex_apps`），
//!   关掉后 6.0~6.1 秒。
//! * `[features] tool_suggest = false` / `remote_plugin = false` —— turn 前还有两条
//!   要 ChatGPT 账号的插件发现与远程插件目录刷新，同样打在 chatgpt.com 上。
//! * `[analytics] enabled = false` —— 见 [`DISABLED_TABLES`]。
//!
//! ## 二、「思考很久」（几十秒到两分钟）在本机的真凶不是上面这些
//!
//! 装了企业 EDR（本机是微步 Threatbook / `Kuafu.exe`）的机器上，
//! `C:\ProgramData\OpenAI\Codex\requirements.toml` 会下发 **UserPromptSubmit hook**。
//! 该文件本机已累积 **392 条完全相同的块**（同一句 `Kuafu.exe codex_userpromptsubmit`），
//! Codex 每条都会执行，且在模型请求**之前**：
//!
//! * 2026-11-25 11:30 那轮：`task_started` 之后 **118 秒**零日志，随后一次性刷出
//!   300+ 条 `hook/completed`，才去发 `POST /v1/responses`（网关自身 3.1 秒）。
//! * 14:07 那轮（`apps = false` 已生效、app-server 也重启过）：仍是 **153 秒**空白，
//!   392 个 hook 齐刷刷跑完才发请求。
//! * 复现/量化不要用 `debug prompt-input` —— 那条路径**不跑 hook**，会把人骗了
//!   （它测出来永远是 6 秒）。用 `codex debug app-server send-message-v2 "你好"`，
//!   一次 turn 里能数到 **392 次 `hook/started`**；单次 `Kuafu.exe`
//!   `codex_userpromptsubmit` 实测 ~0.5 秒。
//!
//! **这是机器级受管配置，用户级 `config.toml` 删不掉、覆盖不了**（requirements 里
//! 还写了 `features.hooks = true`，`hooks` 不可关）。只能把那份文件去重成 1 条
//! （需要管理员权限、且要跟 IT 确认），或由 IT 修 EDR 的注入逻辑（它会反复追加）。
//! 详见 `docs/responses-wire.md` 的「思考很久」一节。
//!
//! 所以：本模块关掉的那几项是**应该关**（省掉注定失败的联网等待），但**不等于**
//! 解决「思考很久」。装 EDR 的 Windows 机器上要另外处理 hook 重复。
//!
//! ## 三、别往配置里加的东西
//!
//! **不能**写 `disable_response_storage`（0.171 已废弃，写了只会出现在
//! "unrecognized configuration setting" 里）；`[cloud.skills]` 也不是可识别的键，
//! 关它零收益。
//!
//! `codex exec` 在本机会被安全策略中途杀掉，别拿它做计时。
//!
//! 改完 `config.toml` 要**重启 app-server**（桌面端彻底退出后重开；有托管 daemon 时
//! `codex app-server daemon restart`）才生效 —— 只重开对话不算，配置是 daemon 启动时读的。

use crate::catalog::ChatModel;
use serde::Serialize;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};

/// 写进 `config.toml` 的 provider id
pub const PROVIDER_ID: &str = "workbuddy2api";
/// 模型目录文件名（相对 `~/.codex`）
pub const CATALOG_FILE: &str = "workbuddy2api-model-catalog.json";
/// 备份文件名（相对应用数据目录）
pub const BACKUP_FILE: &str = "codex-config.bak.toml";

/// 接入时必须关掉的 feature 开关（`(键, 值)`，写进 `[features]`）。
///
/// 三个开关背后都要 ChatGPT 账号、都要打 `chatgpt.com`；本网关给的是本地
/// provider，这些请求注定失败。失败方式决定体验好坏：直连不通会立刻 RST（无感），
/// 而经企业代理/DLP 时常见「TCP 能建连、永不回包」，每条要等 10 秒超时。
///
/// **注意：关掉它们只省掉这部分联网等待，并不解决「思考很久」**——那件事在本机是
/// EDR 下发的 392 条重复 UserPromptSubmit hook（见文件头第二节），用户配置改不动。
///
/// 断开时按备份逐个还原（原文没有的键连空表头一起删），见 [`disconnect_in`]。
pub const DISABLED_FEATURES: [(&str, &str); 3] = [
    // 内置 apps/connectors 后台：`world_state.build` 会等它拿到工具目录，
    // 本机关不掉时 30 秒都走不完（2026-11-25 实测）。
    ("apps", "false"),
    // turn 前的插件发现 `list_tool_suggest_discoverable_plugins`
    ("tool_suggest", "false"),
    // 远程插件目录刷新 `POST /backend-api/ps/plugins/installed`
    ("remote_plugin", "false"),
];

/// 与 [`DISABLED_FEATURES`] 同样受管、但要写进**独立表**的开关
/// （`(表名, 键, 值)`）。
///
/// `[analytics]` 不是 feature，是官方文档里明写的表（app-server 默认关闭，
/// 桌面端会显式打开）。它每 10 秒往 `chatgpt.com/backend-api/codex/analytics-events`
/// 打一次、每次等满超时；turn 里的 `run_hooks_and_record_inputs` 还会等这一批
/// 刷完才发模型请求（2026-11-25 实测：124.3s → 134.3s，整整 10 秒）。
pub const DISABLED_TABLES: [(&str, &str, &str); 1] = [("analytics", "enabled", "false")];

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
// 前端 `CodexState`（src/types.ts）按 camelCase 读：`configPath` / `baseUrl` /
// `catalogModels`。少了这行，IPC 发出去的是 `config_path` 这套蛇形键，
// 页面拿 `catalogModels` 就是 undefined —— 在「已接入」分支里 `.join()` 一调，
// 整个 React 树卸载，窗口只剩一片灰（2026-11-24 实测踩过）。
#[serde(rename_all = "camelCase")]
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
    if let Ok(doc) = parse_config(&text) {
        st.connected = top_level_str(&doc, "model_provider").as_deref() == Some(PROVIDER_ID);
        st.base_url = provider_base_url(&doc).unwrap_or_default();
        st.model = top_level_str(&doc, "model").unwrap_or_default();
    }
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
    // 先解析再动手：文件不是有效 TOML 时直接报错，绝不拿空文档覆盖用户配置
    let mut doc = parse_config(&original)?;

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

    // 关掉那几个「要 ChatGPT 账号」的开关（见文件头）：它们会在 turn 开始前串行阻塞，
    // 界面表现就是一直「正在思考」。feature 进 `[features]`，analytics 进 `[analytics]`。
    apply_disabled_features(&mut doc)?;
    upsert_provider(&mut doc, port, key)?;
    set_top_level(&mut doc, "model_provider", PROVIDER_ID);
    set_top_level(&mut doc, "model_catalog_json", CATALOG_FILE);
    if !model.trim().is_empty() {
        set_top_level(&mut doc, "model", model.trim());
    }
    std::fs::write(&config, doc.to_string()).map_err(|e| format!("写 Codex 配置失败：{e}"))?;

    // 回读确认：写成功不代表写对了（权限、并发编辑都可能让结果不如预期）
    let st = state_at(home);
    if !st.connected {
        return Err("写入完成但回读发现未生效，请检查 config.toml 是否可写".into());
    }
    Ok(st)
}

/// 断开：删 provider 段，三个顶层键与 `[features]` 里的受管开关
/// 恢复成备份里的原值（备份里没有就删掉）。
pub fn disconnect(backup_dir: &Path) -> Result<CodexState, String> {
    let home = codex_home().ok_or("找不到用户主目录，无法定位 Codex 配置")?;
    disconnect_in(&home, backup_dir)
}

fn disconnect_in(home: &Path, backup_dir: &Path) -> Result<CodexState, String> {
    if !home.is_dir() {
        return Err(format!("没有找到 Codex 配置目录（{}）", home.display()));
    }
    let config = home.join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let mut doc = parse_config(&text)?;
    remove_provider(&mut doc);

    let backup = backup_dir.join(BACKUP_FILE);
    let orig_text = std::fs::read_to_string(&backup).unwrap_or_default();
    let orig = parse_config(&orig_text).unwrap_or_default();
    // `model` 也要恢复：留着我们的模型名，换回官方/别的 provider 后 Codex
    // 会因为「模型不存在」直接起不来
    for key in ["model_provider", "model_catalog_json", "model"] {
        match orig.get(key) {
            // 原样搬回：原文是字符串就还是字符串，不重新拼引号
            Some(item) => {
                doc.insert(key, item.clone());
            }
            None => {
                doc.remove(key);
            }
        }
    }
    // `[features]` 里我们改过的那几个开关，断开必须逐个还原：备份里原本没有这个键
    // 就删掉它（表空了连表头一起删），有则按**原文的值类型**写回（布尔不能变成字符串）。
    restore_disabled_features(&mut doc, &orig)?;
    std::fs::write(&config, doc.to_string()).map_err(|e| format!("写 Codex 配置失败：{e}"))?;
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

fn provider_table(port: u16, key: &str) -> Table {
    let mut table = Table::new();
    // 用 127.0.0.1 而不是 localhost：Windows 上 localhost 可能先解析到 ::1，
    // 而网关只绑 127.0.0.1（IPv4），会连不上
    table.insert("name", toml_edit::value("WorkBuddy2API"));
    table.insert(
        "base_url",
        toml_edit::value(format!("http://127.0.0.1:{port}/v1")),
    );
    table.insert("wire_api", toml_edit::value("responses"));
    table.insert("experimental_bearer_token", toml_edit::value(key));
    table
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
// TOML 编辑（纯函数，全部可测）
// ---------------------------------------------------------------------------

/// 解析用户配置。解析失败必须让调用方报错，不能拿空文档覆盖原文件。
fn parse_config(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>()
        .map_err(|e| format!("Codex 配置不是有效的 TOML：{e}"))
}

fn top_level_str(doc: &DocumentMut, key: &str) -> Option<String> {
    doc.get(key).and_then(Item::as_str).map(str::to_string)
}

fn provider_base_url(doc: &DocumentMut) -> Option<String> {
    doc.get("model_providers")
        .and_then(Item::as_table)
        .and_then(|t| t.get(PROVIDER_ID))
        .and_then(Item::as_table)
        .and_then(|t| t.get("base_url"))
        .and_then(Item::as_str)
        .map(str::to_string)
}

fn set_top_level(doc: &mut DocumentMut, key: &str, val: &str) {
    doc.insert(key, toml_edit::value(val));
}

/// 整段替换 `[model_providers.workbuddy2api]`；`model_providers` 下别的 provider 原样保留。
fn upsert_provider(doc: &mut DocumentMut, port: u16, key: &str) -> Result<(), String> {
    let providers = doc
        .entry("model_providers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or("Codex 配置里的 model_providers 不是表，无法写入网关 provider")?;
    providers.insert(PROVIDER_ID, Item::Table(provider_table(port, key)));
    Ok(())
}

/// 删掉网关 provider；`model_providers` 因此变空时连父表一起删。
fn remove_provider(doc: &mut DocumentMut) {
    let empty = {
        let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) else {
            return;
        };
        providers.remove(PROVIDER_ID);
        providers.is_empty()
    };
    if empty {
        doc.remove("model_providers");
    }
}

/// 表内设置一个键：表不存在就建，键已存在就替换值。
fn set_table_key(doc: &mut DocumentMut, table: &str, key: &str, val: Item) -> Result<(), String> {
    let t = doc
        .entry(table)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("Codex 配置里的 [{table}] 不是表"))?;
    t.insert(key, val);
    Ok(())
}

/// 删表内的一个键；删完表体空了，连表头一起删。
fn drop_table_key(doc: &mut DocumentMut, table: &str, key: &str) {
    let empty = {
        let Some(t) = doc.get_mut(table).and_then(Item::as_table_mut) else {
            return;
        };
        t.remove(key);
        t.is_empty()
    };
    if empty {
        doc.remove(table);
    }
}

/// 把 [`DISABLED_FEATURES`] 逐个写进 `[features]`、把 [`DISABLED_TABLES`] 写进各自的表：
/// 只碰这几行，同表其它键（用户自己开的开关、别人的 analytics 设置）原样保留。
fn apply_disabled_features(doc: &mut DocumentMut) -> Result<(), String> {
    for &(key, val) in DISABLED_FEATURES.iter() {
        set_table_key(doc, "features", key, toml_edit::value(val == "true"))?;
    }
    for &(table, key, val) in DISABLED_TABLES.iter() {
        set_table_key(doc, table, key, toml_edit::value(val == "true"))?;
    }
    Ok(())
}

fn restore_table_key(
    doc: &mut DocumentMut,
    orig: &DocumentMut,
    table: &str,
    key: &str,
) -> Result<(), String> {
    let original = orig
        .get(table)
        .and_then(Item::as_table)
        .and_then(|t| t.get(key))
        .cloned();
    match original {
        Some(item) => set_table_key(doc, table, key, item),
        None => {
            drop_table_key(doc, table, key);
            Ok(())
        }
    }
}

/// 按备份把 [`DISABLED_FEATURES`] 与 [`DISABLED_TABLES`] 逐个还原：原文里没有这个键就
/// 删掉（表空了连表头一起删），有则按**原值**写回 —— 布尔不能变成字符串。
fn restore_disabled_features(doc: &mut DocumentMut, orig: &DocumentMut) -> Result<(), String> {
    for &(key, _) in DISABLED_FEATURES.iter() {
        restore_table_key(doc, orig, "features", key)?;
    }
    for &(table, key, _) in DISABLED_TABLES.iter() {
        restore_table_key(doc, orig, table, key)?;
    }
    Ok(())
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

    fn parse(text: &str) -> DocumentMut {
        parse_config(text).expect("测试配置必须是有效 TOML")
    }

    #[test]
    fn state_values_come_from_the_top_level_and_provider_table() {
        let doc = parse(
            "model = \"outer\"\nmodel_provider = \"workbuddy2api\"\n\
             [model_providers.workbuddy2api]\nbase_url = \"http://127.0.0.1:8790/v1\"\n\
             [t]\nmodel = \"inside\"\n",
        );
        assert_eq!(top_level_str(&doc, "model").as_deref(), Some("outer"));
        assert_eq!(
            top_level_str(&doc, "model_provider").as_deref(),
            Some(PROVIDER_ID)
        );
        assert_eq!(
            provider_base_url(&doc).as_deref(),
            Some("http://127.0.0.1:8790/v1")
        );
    }

    #[test]
    fn same_named_key_inside_a_table_is_not_a_top_level_key() {
        let doc = parse("[t]\nmodel = \"inside\"\n");
        assert_eq!(top_level_str(&doc, "model"), None);
    }

    #[test]
    fn set_top_level_does_not_touch_same_named_key_inside_a_table() {
        let mut doc = parse("[t]\nmodel = \"inside\"\n");
        set_top_level(&mut doc, "model", "outside");
        let out = doc.to_string();
        assert!(out.contains("model = \"inside\""), "表里的同名键不能改");
        assert!(out.contains("model = \"outside\""));
    }

    #[test]
    fn top_level_reads_both_quote_styles() {
        let doc = parse("a = \"x\"\nb = 'y'\nc = 3\n");
        assert_eq!(top_level_str(&doc, "a").as_deref(), Some("x"));
        assert_eq!(top_level_str(&doc, "b").as_deref(), Some("y"));
    }

    #[test]
    fn upsert_provider_replaces_only_that_table() {
        let mut doc = parse(&real_config());
        upsert_provider(&mut doc, 8790, "sk-wb-test").unwrap();
        let out = doc.to_string();
        assert!(out.contains("[model_providers.workbuddy2api]"));
        assert!(out.contains("base_url = \"http://127.0.0.1:8790/v1\""));
        assert!(out.contains("experimental_bearer_token = \"sk-wb-test\""));
        // 邻居表原样保留
        assert!(out.contains("[model_providers.custom]"));
        assert!(out.contains("https://example.com/v1"));
        assert!(out.contains("[model_providers.kiro2api]"));
        assert!(out.contains("k2a-xxx"));
        assert!(out.contains("[plugins.\"browser@openai-bundled\"]"));
        assert!(out.contains("enabled = true"));
    }

    #[test]
    fn upsert_provider_replaces_its_old_body() {
        let mut doc = parse(&real_config());
        upsert_provider(&mut doc, 8790, "old-key").unwrap();
        upsert_provider(&mut doc, 8791, "new-key").unwrap();
        let out = doc.to_string();
        assert_eq!(out.matches("[model_providers.workbuddy2api]").count(), 1);
        assert!(out.contains("http://127.0.0.1:8791/v1"));
        assert!(!out.contains("old-key"), "旧表体必须整段丢掉");
    }

    #[test]
    fn remove_provider_leaves_neighbours_intact() {
        let mut doc = parse(&real_config());
        upsert_provider(&mut doc, 8790, "sk-wb-test").unwrap();
        remove_provider(&mut doc);
        let out = doc.to_string();
        assert!(!out.contains("workbuddy2api"));
        assert!(out.contains("[model_providers.kiro2api]"));
        assert!(out.contains("k2a-xxx"));
        assert!(out.contains("[windows]\nsandbox = \"elevated\""));
        assert!(
            out.contains("model_provider = \"kiro2api\""),
            "顶层键不归 remove_provider 管"
        );
    }

    /// 接入 → 断开 必须回到原样（这是"改了用户文件"的最低要求）
    #[test]
    fn connect_then_disconnect_restores_the_original_top_level_keys() {
        let orig_text = real_config();
        let orig = parse(&orig_text);
        let mut doc = parse(&orig_text);
        apply_disabled_features(&mut doc).unwrap();
        upsert_provider(&mut doc, 8790, "sk-wb-1").unwrap();
        set_top_level(&mut doc, "model_provider", PROVIDER_ID);
        set_top_level(&mut doc, "model_catalog_json", CATALOG_FILE);
        set_top_level(&mut doc, "model", "hy4flash-free");

        assert_eq!(
            top_level_str(&doc, "model_provider").as_deref(),
            Some(PROVIDER_ID)
        );
        assert_eq!(
            provider_base_url(&doc).as_deref(),
            Some("http://127.0.0.1:8790/v1")
        );

        // 断开
        remove_provider(&mut doc);
        for key in ["model_provider", "model_catalog_json", "model"] {
            match orig.get(key) {
                Some(item) => {
                    doc.insert(key, item.clone());
                }
                None => {
                    doc.remove(key);
                }
            }
        }
        restore_disabled_features(&mut doc, &orig).unwrap();
        for key in ["model_provider", "model_catalog_json", "model"] {
            assert_eq!(
                top_level_str(&doc, key),
                top_level_str(&orig, key),
                "{key} 没恢复"
            );
        }
        assert!(!doc.to_string().contains("workbuddy2api"));
    }

    #[test]
    fn provider_table_pins_ipv4_loopback() {
        let b = provider_table(8790, "sk-wb-abc").to_string();
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
        let mut doc = parse(&config_with_features());
        set_table_key(&mut doc, "features", "apps", toml_edit::value(false)).unwrap();
        let out = doc.to_string();
        assert!(out.contains("apps = false"), "{out}");
        assert!(!out.contains("apps = true"), "旧值必须被替换掉：\n{out}");
        assert!(
            out.contains("multi_agent = true"),
            "用户自己的开关被吞了：\n{out}"
        );
    }

    #[test]
    fn set_table_key_appends_when_the_key_is_missing() {
        let mut doc =
            parse("[features]\nmulti_agent = true\n\n[windows]\nsandbox = \"elevated\"\n");
        set_table_key(&mut doc, "features", "apps", toml_edit::value(false)).unwrap();
        let features = doc.get("features").and_then(Item::as_table).unwrap();
        assert_eq!(features.get("apps").and_then(Item::as_bool), Some(false));
        let windows = doc.get("windows").and_then(Item::as_table).unwrap();
        assert!(windows.get("apps").is_none(), "apps 不能写进 windows 表");
    }

    #[test]
    fn set_table_key_creates_the_table_when_absent() {
        let mut doc = parse("model = \"x\"\n");
        set_table_key(&mut doc, "features", "apps", toml_edit::value(false)).unwrap();
        let out = doc.to_string();
        assert!(out.contains("[features]"), "{out}");
        assert!(out.contains("apps = false"), "{out}");
    }

    #[test]
    fn drop_table_key_keeps_the_table_when_others_remain() {
        let mut doc = parse("[features]\napps = false\nmulti_agent = true\n");
        drop_table_key(&mut doc, "features", "apps");
        assert!(doc.get("features").is_some());
        let out = doc.to_string();
        assert!(!out.contains("apps ="), "{out}");
        assert!(out.contains("multi_agent = true"), "{out}");
    }

    #[test]
    fn drop_table_key_removes_the_emptied_table() {
        let mut doc = parse("model = \"x\"\n\n[features]\napps = false\n");
        drop_table_key(&mut doc, "features", "apps");
        assert!(doc.get("features").is_none(), "不该留一张空表");
        assert!(!doc.to_string().contains("[features]"));
    }

    /// 三个受管开关都要落进 `[features]`，且用户自己开的开关一个都不能少。
    ///
    /// 只关 `apps` 时 turn 开始前的插件发现（`tool_suggest` / `remote_plugin`）仍会
    /// 去打 chatgpt.com，访问不到的机器上就是「一直正在思考」。
    #[test]
    fn apply_disabled_features_writes_all_three_and_keeps_user_switches() {
        let mut doc = parse(&config_with_features());
        apply_disabled_features(&mut doc).unwrap();
        let out = doc.to_string();
        for (key, val) in DISABLED_FEATURES {
            assert!(
                out.contains(&format!("{key} = {val}")),
                "{key} 没写进去：\n{out}"
            );
        }
        assert!(
            out.contains("multi_agent = true"),
            "用户自己的开关被吞了：\n{out}"
        );
        assert_eq!(out.matches("[features]").count(), 1, "不该重复建表：\n{out}");
    }

    /// `[analytics]` 是被单独管的表：要写 `enabled = false`，且不能顺手清掉表里
    /// 用户自己的其它键（那是他的配置）。
    #[test]
    fn apply_disabled_features_also_disables_analytics_table() {
        for (table, key, val) in DISABLED_TABLES {
            // 没有这张表 → 建表
            let mut doc = parse("model = \"x\"\n");
            apply_disabled_features(&mut doc).unwrap();
            let out = doc.to_string();
            assert!(
                out.contains(&format!("[{table}]")),
                "没建出 [{table}]：\n{out}"
            );
            assert!(out.contains(&format!("{key} = {val}")), "没写进去：\n{out}");
            // 表在、里面有用户自己的键 → 只加我们这行
            let mut doc = parse(&format!("[{table}]\nsome_user_key = true\n"));
            apply_disabled_features(&mut doc).unwrap();
            let t = doc.get(table).and_then(Item::as_table).unwrap();
            assert_eq!(t.get("some_user_key").and_then(Item::as_bool), Some(true));
            assert_eq!(t.get(key).and_then(Item::as_bool), Some(false));
            assert_eq!(doc.to_string().matches(&format!("[{table}]")).count(), 1);
        }
    }

    /// 重复接入不能把同一行写两遍（第二次接入门槛最低，最容易出这事）
    #[test]
    fn apply_disabled_features_is_idempotent() {
        let mut once = parse(&real_config());
        apply_disabled_features(&mut once).unwrap();
        let once = once.to_string();
        let mut twice = parse(&once);
        apply_disabled_features(&mut twice).unwrap();
        assert_eq!(once, twice.to_string());
        for (key, _) in DISABLED_FEATURES {
            assert_eq!(
                once.matches(&format!("{key} = ")).count(),
                1,
                "{key} 重复了：\n{once}"
            );
        }
        for (_, key, val) in DISABLED_TABLES {
            assert_eq!(
                once.matches(&format!("{key} = {val}")).count(),
                1,
                "{key} 重复了：\n{once}"
            );
        }
    }

    /// 断开：原文没有的键删掉、有的按原值写回；表里只剩用户自己的开关
    #[test]
    fn restore_disabled_features_follows_the_backup() {
        let orig_text = "[features]\napps = true\nmulti_agent = true\n";
        let orig = parse(orig_text);
        let mut doc = parse(orig_text);
        apply_disabled_features(&mut doc).unwrap();
        restore_disabled_features(&mut doc, &orig).unwrap();
        let out = doc.to_string();
        assert!(out.contains("apps = true"), "{out}");
        assert!(!out.contains("apps = false"), "{out}");
        assert!(out.contains("multi_agent = true"), "{out}");

        // 原文根本没有 `[features]` / `[analytics]` → 三个键全删，表头一起消失
        let orig = parse("model = \"x\"\n");
        let mut doc = parse("model = \"x\"\n");
        apply_disabled_features(&mut doc).unwrap();
        restore_disabled_features(&mut doc, &orig).unwrap();
        let out = doc.to_string();
        assert!(!out.contains("[features]"), "{out}");
        assert!(!out.contains("[analytics]"), "{out}");
        assert!(out.contains("model = \"x\""), "{out}");
    }

    /// 接入把三个「要 ChatGPT 账号」的 feature 都关掉，且**不**碰已废弃的
    /// `disable_response_storage`
    #[test]
    fn connect_disables_auth_gated_features_and_leaves_the_deprecated_key_alone() {
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
        for (key, val) in DISABLED_FEATURES {
            assert!(
                cfg.contains(&format!("{key} = {val}")),
                "{key} 没写进去，turn 开始前会卡在 chatgpt.com：\n{cfg}"
            );
        }
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
        for (key, val) in DISABLED_FEATURES {
            assert!(
                after.contains(&format!("{key} = {val}")),
                "{key} 必须关掉（否则 turn 前会卡在 chatgpt.com）"
            );
        }

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
        for (key, _) in DISABLED_FEATURES {
            assert!(!back.contains(&format!("{key} = ")), "断开了还留着我们加的 {key}：\n{back}");
        }
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
        let mut doc = match parse_config(&text) {
            Ok(doc) => doc,
            Err(e) => {
                println!("{e}");
                return;
            }
        };
        let orig = doc.clone();
        apply_disabled_features(&mut doc).unwrap();
        upsert_provider(&mut doc, 8790, "sk-wb-preview").unwrap();
        set_top_level(&mut doc, "model_provider", PROVIDER_ID);
        set_top_level(&mut doc, "model_catalog_json", CATALOG_FILE);
        set_top_level(&mut doc, "model", "hy4flash-free");
        let after = doc.to_string();

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
            println!("{k} = {:?}", top_level_str(&orig, k));
        }
        println!(
            "features.apps = {:?}（原文；原来是 None 就删掉这个键）",
            orig.get("features")
                .and_then(Item::as_table)
                .and_then(|t| t.get("apps"))
        );
    }

    /// IPC 序列化契约：前端 `CodexState` 读的是 camelCase。
    ///
    /// 2026-11-24 的「接入 Codex 整屏灰」就是这里少了 rename：`catalogModels`
    /// 拿到 undefined，`codex.catalogModels.join()` 抛错，整个 React 树被卸载。
    #[test]
    fn state_serializes_camel_case_keys() {
        let st = CodexState {
            found: true,
            config_path: "~/.codex".into(),
            connected: true,
            base_url: "http://127.0.0.1:8790/v1".into(),
            model: "hy3".into(),
            catalog_models: vec!["hy3".into()],
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["catalogModels"][0], "hy3");
        assert_eq!(v["configPath"], "~/.codex");
        assert_eq!(v["baseUrl"], "http://127.0.0.1:8790/v1");
        assert!(
            v.get("catalog_models").is_none(),
            "蛇形键不该出现在 IPC 载荷里"
        );
    }
}
