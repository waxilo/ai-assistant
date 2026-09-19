//! Qoder 模型目录 —— 接管页「限流切换」的模型清单与免费判定。
//!
//! # 为什么要单独一个模块
//!
//! 这段逻辑原本长在 `proxy.rs` 里，而且整块是 workbuddy(clone) 时代的遗留：
//! 接口打的是 CodeBuddy 的 `{base}/v2/enterprises/personal/models`，兜底常量是
//! 腾讯的 `hy3`。而 Qoder 的网关没有那条路径 —— 实测恒 `404`，于是拉取永远失败、
//! 永远退回兜底，界面显示「来源：内置兜底列表」、列表里只有一个 `hy3`：
//! **一个 Qoder 根本不认识的模型名，冒充成了 Qoder 的模型。**
//!
//! 抽成模块不只是为了「分文件」：模型清单有**两个消费方**（界面要展示、
//! 路由要判免费），它们必须拿到同一份数（见下节）。
//!
//! # 三层来源
//!
//! | 层 | 来源 | 特点 |
//! |---|---|---|
//! | 1 | `GET {CATALOG_BASE}/api/v2/model/list` | Qoder 官方目录，最全；通了就落盘 |
//! | 2 | `models-cache.json`（落盘快照） | 上次成功的结果，抗网络抖动与重启 |
//! | 3 | `~/.qoder` 的本地痕迹 | 纯本地、必定可用；只覆盖「真用过的」模型 |
//!
//! **三层都拿不到时返回空列表**，由界面显示空态。这里绝不再退回任何写死的模型名
//! —— 那正是把 `hy3` 冒充成 Qoder 模型的根源。
//!
//! # 第 1 层为什么**基本永远**拉不到（2026-09-19 实测，别再重复排查）
//!
//! Qoder CLI 拉这个目录是 **status=200**（见 `~/.qoder-cn/logs/runs/*/qodercli.log` 的
//! `operation=modelCatalogFetch`），所以「接口是好的」。但从**常规 HTTPS 客户端**
//! 进去，两个区域都进不去，而且**失败形态各不相同**：
//!
//! | 区域 | 宿主 | 常规客户端结果 |
//! |---|---|---|
//! | 国际版 | `api3.qoder.sh` | **空 `404`**（任何路径、带不带认证都一样） |
//! | 国内版 | `gateway.qoder.com.cn` | **`503`**（响应体是阿里云 ALB 的 HTML） |
//!
//! 国内版这一条是照着 CLI 自己的日志逐项复现后仍然失败的：用日志里那两个 httpdns
//! 落点 IP（`120.24.46.217` / `120.76.131.237`）`--resolve`、HTTP/1.1 与 2、GET 与 POST、
//! 带与不带 UA / 契约头，**一律 503**。也就是说拦的不是 DNS 也不是路径，而是 CLI
//! 之外的客户端根本走不通那层网络契约。CLI 自己倒是把整份目录缓存在
//! `<cli_dir>/.models/<uid>/catalog-v6`，但那是 `QMC\x01` 魔数开头的密文
//! （熵 7.997 bits/byte），没有 CLI 手里的密钥解不开 —— 别去啃它。
//!
//! ## 由此推出的一条硬结论
//!
//! **凭证（token）对这份清单几乎没有价值。** 所以：
//!
//! - [`load`] 的 token 是 `Option<&str>` —— `None` 只是让第 1 层缺席，
//!   不是错误。「这个区域还没有账号」是**常态**（用户只登了一边），
//!   把它做成失败会让一个纯本地的查询在无账号时整个报错（这正是 `free_models`
//!   早先那颗红字提示的来源）；
//! - 真正兜住可用性的是第 2、3 层，第 1 层「能通就好、不通不阻塞」。
//!
//! # 与路由的契约
//!
//! [`load`] 的结果同时供两处消费：界面清单（要 `name`/`multiplier` 给人看）与
//! 路由的免费集合（只要 `free` 的 id，见 [`free_ids`]）。**两者必须同源** ——
//! 否则会出现「界面显示免费、路由却不切换」这种对不上的状态。

use crate::region::Region;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// 模型目录的宿主**不是常量**：两个区域的宿主不同 —— 国际版 `api3.qoder.sh`、
// 国内版 `gateway.qoder.com.cn`（实测日志见 `region` 模块头）。
// 上一版这里是一处写死的 `pub const CATALOG_BASE`，等于宣告「国内版永远拉不到目录」。

/// 模型目录路径（基址由 `region::Region::catalog_base` 按账号区域给出）
const CATALOG_PATH: &str = "/api/v2/model/list";

/// 一次拉取的有效期。官方目录一天之内不会大改，1 小时足够跟手，
/// 又不至于让接管页每次打开都打一次接口。
pub const TTL: Duration = Duration::from_secs(3600);

/// 原始响应落盘的上限。
///
/// 留原文是给「日后收紧解析」当样本用的（见 [`Snapshot::raw`]），不是数据本体 ——
/// 超出就只留解析结果，别把用户磁盘当仓库。
const RAW_KEEP_MAX: usize = 256 * 1024;

/// 落盘快照的文件名前缀（与账号 / 台账同目录）。
///
/// 文件名里**必须带区域**：两套部署的模型目录是两份互不相干的清单，
/// 共用一份快照的后果是「国内版界面显示国际版的模型、并且信以为真去判免费与否」。
/// 这是纯缓存，所以不保留旧文件名（`models-cache.json`）的回退 —— 最坏也就是
/// 第一次多打一次网络。
const SNAPSHOT_PREFIX: &str = "models-cache";

// ---------------------------------------------------------------------------
// 数据形态
// ---------------------------------------------------------------------------

/// 单个模型的描述。
///
/// `id` 是**给网关用的**（Qoder 形如 `qmodel_38max`），`name` 是**给人看的**
/// （形如 `Qwen3.8-Max`）。这一对字段是实测出来的：CLI 会话日志同一行里
/// `data.model` 给 id、`data.hook_input.model` 给显示名。取不到 `name` 时留空串，
/// 界面退回显示 `id` —— 而不是编一个名字。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 是否 0 积分免费模型（恒生效、UI 锁定勾选）
    pub free: bool,
    /// 积分倍率原始串（如 `"x0.00 credits"`），仅展示用；空串 = 倍率未知
    #[serde(default)]
    pub multiplier: String,
}

/// 「限流切换」模型清单（供接管页展示与手动刷新）。
#[derive(Serialize)]
pub struct ModelReport {
    pub models: Vec<ModelInfo>,
    /// 清单是从哪来的，界面直接显示它：
    /// `fetched` = 刚从 Qoder 目录拉取 / `cache` = 落盘快照 /
    /// `local` = 本机 Qoder 痕迹 / `empty` = 三层都没拿到
    pub source: String,
    /// **为什么不是刚拉取的**，界面上直接显示（`source == "fetched"` 时恒 `None`）。
    ///
    /// 与 [`source`](Self::source) 不重复：那个字段说的是「这份清单来自哪一层」，
    /// 这个说的是「第 1 层为什么没结果」。两件事用户都要知道才看得懂界面 ——
    /// 光看「来源：本机 Qoder 的记录」会以为是网络抖动，于是反复点刷新；
    /// 而真相是这接口对常规客户端不开放（见模块头），点多少次都一样。
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// 解析：任何形态的响应 → 统一的模型列表
// ---------------------------------------------------------------------------

/// 响应里可能装着模型数组的键，按序探测。
///
/// **为什么是探测而不是写死**：Qoder 这个接口的确切结构我们**还没拿到真实样本**
/// —— `api3.qoder.sh` 从普通 DNS 入口一律 404（见模块头），只有走 httpdns 的 CLI
/// 拿得到。所以宁可容错：与 `usage.rs` 的 `find_key` 同一套思路，
/// **拿到样本之后再收紧**。
///
/// 注：一旦能落样，[`Snapshot::raw`] 会把它留下来。
const ARRAY_KEYS: [&str; 6] = ["models", "list", "data", "result", "items", "modelList"];

/// 模型 id 的候选键（Qoder 用 `qmodel_*` 形态）
const ID_KEYS: [&str; 6] = ["id", "model", "key", "modelId", "model_id", "code"];

/// 模型显示名的候选键
const NAME_KEYS: [&str; 6] = [
    "name",
    "displayName",
    "display_name",
    "title",
    "label",
    "modelName",
];

/// 积分倍率的候选键
const MULT_KEYS: [&str; 5] = [
    "credits",
    "multiplier",
    "creditMultiplier",
    "ratio",
    "cost",
];

/// 从多个候选键里取第一个非空字符串
fn pick_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|s| !s.is_empty())
}

/// 模型 id（认不出来返回 `None` —— 没有 id 的条目对网关毫无用处）
fn id_of(m: &Value) -> Option<&str> {
    pick_str(m, &ID_KEYS)
}

/// 模型显示名；与 id 相同视为「没有独立的名字」，留空
fn name_of(m: &Value, id: &str) -> String {
    pick_str(m, &NAME_KEYS)
        .filter(|n| *n != id)
        .unwrap_or("")
        .to_string()
}

/// 倍率 → 数字。
///
/// 形态实测有两种：数字（`0`）与字符串（`"x0.00 credits"` / `"x0.05"`）。
/// 字符串取第一个 token 解析：`"x0.00 credits"` → `0.0`，`"credits"` → `None`。
fn parse_multiplier(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .trim()
            .trim_start_matches(['x', 'X', '*'])
            .split_whitespace()
            .next()?
            .parse()
            .ok(),
        _ => None,
    }
}

/// 倍率字段 → `(是否免费, 原始串)`。
///
/// **拿不到倍率一律判「不免费」**：猜错的代价不对称 —— 猜「免费」会让一个付费模型
/// 在 429 时被无感换号，悄悄烧掉别的账号的积分；猜「不免费」只是少一次自动切换，
/// 用户在界面勾一下就有了。
fn free_of(m: &Value) -> (bool, String) {
    for k in MULT_KEYS {
        let Some(raw) = m.get(k) else { continue };
        let Some(n) = parse_multiplier(raw) else { continue };
        let text = match raw {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        return (n == 0.0, text);
    }
    (false, String::new())
}

/// 深度优先找一个「装着模型对象的数组」。
///
/// 判据不是「键名对不对」而是「元素像不像模型」：数组里任一元素是对象且能取出 id
/// 就算命中。这样无论响应是 `{data:{models:[…]}}` 还是 `{result:[…]}` 都能落到同一个出口。
fn find_model_array(root: &Value) -> Option<&Vec<Value>> {
    /// 下钻上限：响应体积不大，几层足够；写死是为了「既容错又不会无限下钻」
    const MAX_DEPTH: usize = 4;

    fn walk(v: &Value, depth: usize) -> Option<&Vec<Value>> {
        if depth > MAX_DEPTH {
            return None;
        }
        match v {
            Value::Array(arr) => {
                if arr
                    .iter()
                    .take(4)
                    .any(|m| m.is_object() && id_of(m).is_some())
                {
                    return Some(arr);
                }
                arr.iter().find_map(|x| walk(x, depth + 1))
            }
            Value::Object(map) => ARRAY_KEYS
                .iter()
                .filter_map(|k| map.get(*k))
                .find_map(|child| walk(child, depth + 1)),
            _ => None,
        }
    }
    walk(root, 0)
}

/// 响应 → 模型列表（免费排前、其余按 id 升序）。纯函数，便于单测。
pub fn parse_list(root: &Value) -> Vec<ModelInfo> {
    let Some(arr) = find_model_array(root) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = id_of(m)?.to_string();
            let (free, multiplier) = free_of(m);
            Some(ModelInfo {
                name: name_of(m, &id),
                id,
                free,
                multiplier,
            })
        })
        .collect();
    // 容错探测可能撞上嵌套的同名数组，同一个 id 出现多次时只留一条
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    // 免费排前：界面里「恒生效」的那些该在最显眼处
    out.sort_by(|a, b| match (a.free, b.free) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.id.cmp(&b.id),
    });
    out
}

// ---------------------------------------------------------------------------
// 第 3 层：本机 Qoder 的痕迹
// ---------------------------------------------------------------------------

/// 追加一条（去重），`name` 为空时不覆盖已有的非空名字
fn push_unique(found: &mut Vec<ModelInfo>, seen: &mut HashSet<String>, id: &str, name: &str) {
    if id.is_empty() {
        // 实测：没显式指定模型的会话，`data.model` 就是**空串**（在 `~/Documents/Qoder`
        // 之外跑的、`permission_mode: yolo` 那种）。空 id 对网关毫无意义，直接丢。
        return;
    }
    if seen.insert(id.to_string()) {
        found.push(ModelInfo {
            id: id.to_string(),
            name: name.to_string(),
            // 本地痕迹给不出倍率 ⇒ 一律「不免费」（理由见 `free_of`）
            free: false,
            multiplier: String::new(),
        });
    } else if !name.is_empty() {
        if let Some(m) = found.iter_mut().find(|m| m.id == id) {
            if m.name.is_empty() {
                m.name = name.to_string();
            }
        }
    }
}

/// 往字符串集合里塞一条（忽略空串、去重、保持出现顺序）
fn push_str_unique(out: &mut Vec<String>, s: &str) {
    if !s.is_empty() && !out.iter().any(|x| x == s) {
        out.push(s.to_string());
    }
}

/// 扫一个会话日志文件 →（用过的模型 id, 出现过的显示名）。
///
/// # 这两个字段**不在同一行**（这里踩过一次，别再改回去）
///
/// 上一版以为 id 与显示名「同一行里天然成对」，于是**逐行**配对 —— 实测行不通：
///
/// - `data.model` 在文件**第 1 行**（`type: session.config.loaded`，如 `qmodel_38max`）
/// - `data.hook_input.model` 在 **SessionStart 那条 hook** 里（如 `Qwen3.8-Max`）
///
/// 逐行配对的后果是**一个名字都配不上**：本地这层返回的全是无名 id，界面只能显示裸 id。
/// 正确的粒度是**整个文件**，见 [`pair_name`]。
fn scan_session(path: &Path) -> (Vec<String>, Vec<String>) {
    let mut ids: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return (ids, names);
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(d) = v.get("data") else { continue };
        if let Some(id) = d.get("model").and_then(|m| m.as_str()) {
            push_str_unique(&mut ids, id);
        }
        if let Some(n) = d
            .get("hook_input")
            .and_then(|h| h.get("model"))
            .and_then(|m| m.as_str())
        {
            push_str_unique(&mut names, n);
        }
    }
    (ids, names)
}

/// 按文件配对：**只有「恰好一个 id + 恰好一个名字」才敢配**。
///
/// 一个文件里出现多个模型（会话中途换过模型）时，两边已经不保证一一对应 ——
/// 这时宁可不给名字（界面退回显示 id），也不要张冠李戴地把 A 的名字贴到 B 上。
fn pair_name<'a>(ids: &[String], names: &'a [String]) -> Option<&'a str> {
    match (ids.len(), names.len()) {
        (1, 1) => Some(names[0].as_str()),
        _ => None,
    }
}

/// 递归收集 `*.jsonl`（带修改时间，便于按新旧截断）
fn collect_jsonl(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>, depth: usize) {
    const MAX_DEPTH: usize = 5;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out, depth + 1);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            let at = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            out.push((at, p));
        }
    }
}

/// 会话日志文件，**新的在前、最多若干个**。
///
/// 目录会随会话数无限增长（一次会话一个文件夹），全量扫在积累几个月后要好几秒 ——
/// 而我们要的只是「这台机器用过哪些模型」，最近若干个文件足够覆盖。
fn session_logs(root: &Path) -> Vec<PathBuf> {
    const TAKE: usize = 20;
    let mut all: Vec<(SystemTime, PathBuf)> = Vec::new();
    collect_jsonl(&root.join("logs").join("sessions"), &mut all, 0);
    all.sort_by(|a, b| b.0.cmp(&a.0));
    all.into_iter().take(TAKE).map(|(_, p)| p).collect()
}

/// 本机 Qoder 的痕迹 → 「这台机器真用过的模型」。
///
/// 这是三层的最后一道：纯本地、不依赖网络，保证界面至少不是空的。
/// 覆盖范围有限（只有用过的模型），但拿到的是**真值**，不是编的。
///
/// 两个来源都是实测出来的（路径里的 `~/.qoder` 对国内版是 `~/.qoder-cn`，
/// 由 [`Region::cli_dir_name`] 给）：
/// - `<cli_dir>/.models/default` → `{"key":"qmodel_38max", …}`，当前选中的模型 id
/// - `<cli_dir>/logs/sessions/**/*.jsonl` → 模型 id 与显示名**在同一个文件的不同行**上，
///   按文件配对（见 [`scan_session`] / [`pair_name`]）。
///
/// 实机样例（这台机器，2026-09-18）：
///
/// | `data.model` | `data.hook_input.model` |
/// |---|---|
/// | `qmodel_38max` | `Qwen3.8-Max` |
/// | `qfmodel` | `Qwen3.8-Flash` |
pub fn local_models(region: Region) -> Vec<ModelInfo> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let root = home.join(region.cli_dir_name());
    let mut found: Vec<ModelInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // ① 当前选中的模型：只给 id，没有显示名
    if let Ok(text) = std::fs::read_to_string(root.join(".models").join("default")) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(id) = pick_str(&v, &["key", "id", "model"]) {
                push_unique(&mut found, &mut seen, id, "");
            }
        }
    }

    // ② 会话日志：id 与显示名在同一个文件里（不同行），按文件配对
    for path in session_logs(&root) {
        let (ids, names) = scan_session(&path);
        let name = pair_name(&ids, &names).unwrap_or("");
        for id in &ids {
            push_unique(&mut found, &mut seen, id, name);
        }
    }

    // 免费排前的排序与网络路径保持一致，界面才不会因为来源不同而换一副样子
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

// ---------------------------------------------------------------------------
// 第 2 层：落盘快照
// ---------------------------------------------------------------------------

/// 落盘快照。
#[derive(Serialize, Deserialize)]
struct Snapshot {
    /// 写入时刻（毫秒）—— 只为人看
    at_ms: u64,
    models: Vec<ModelInfo>,
    /// **原始响应**，只为日后收紧 [解析逻辑](parse_list) 时留一份样本；
    /// 超 [`RAW_KEEP_MAX`] 就不留（它没有任何运行期用途）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raw: Option<Value>,
}

fn snapshot_path(dir: &Path, region: Region) -> PathBuf {
    dir.join(format!("{SNAPSHOT_PREFIX}-{}.json", region.key()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 读回落盘快照。文件不存在 / 坏掉都回 `None`（只是少一层，不该让调用方出错）。
pub fn load_snapshot(dir: &Path, region: Region) -> Option<Vec<ModelInfo>> {
    let text = std::fs::read_to_string(snapshot_path(dir, region)).ok()?;
    let snap: Snapshot = serde_json::from_str(&text).ok()?;
    (!snap.models.is_empty()).then_some(snap.models)
}

/// 写落盘快照（临时文件 + rename，避免与另一个进程读到半个文件）。
///
/// 失败**只当没发生** —— 缓存写不进去不该让「拉取成功」变成失败。
fn save_snapshot(dir: &Path, region: Region, models: &[ModelInfo], raw: Option<&Value>) {
    let raw = raw
        .filter(|r| serde_json::to_string(r).map(|s| s.len() <= RAW_KEEP_MAX).unwrap_or(false))
        .cloned();
    let snap = Snapshot {
        at_ms: now_ms(),
        models: models.to_vec(),
        raw,
    };
    let Ok(text) = serde_json::to_string(&snap) else {
        return;
    };
    let target = snapshot_path(dir, region);
    let tmp = target.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, target);
    }
}

// ---------------------------------------------------------------------------
// 第 1 层：网络
// ---------------------------------------------------------------------------

/// 从 Qoder 模型目录拉一次；成功即落盘（连原始响应一起），任何失败回 `None`。
///
/// 用的是 [`crate::http::api_client_direct`]：这是**模型网关**的接口，
/// 与 `openapi.qoder.sh` 那套（`qoder_api::client` 的 `Cosy-ClientType` 身份头）
/// 不是一族，别把两套身份混到一条路径上。
pub async fn fetch_remote(region: Region, dir: &Path, token: &str) -> Option<Vec<ModelInfo>> {
    let url = format!("{}{CATALOG_PATH}", region.catalog_base());
    let resp = crate::http::api_client_direct()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let list = parse_list(&body);
    if list.is_empty() {
        // 拿到 200 却解析不出任何模型：**不落盘**。
        // 落一份空快照会把上一次的好数据覆盖掉，而这份空未必是真相
        // （也可能是我们还没认对结构）。
        return None;
    }
    save_snapshot(dir, region, &list, Some(&body));
    Some(list)
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 进程内缓存：(区域, 当时有没有凭证, 取到时刻, 清单, 来源)。
///
/// **区域是键的一部分**：两套部署的模型目录是两份清单，用国际版那份去判国内版模型的
/// 免费与否，结果是静默错判。
///
/// **「有没有凭证」也是键的一部分**（别删）：没凭证的那次会直接跳过第 1 层、退到本地层，
/// 如果把那份结果缓存成一个区域级的条目，用户登录之后一小时内都会拿到「本机痕迹」那份
/// 残缺清单 —— 而且看起来完全正常。分开存就不会互相顶掉。
fn memo() -> &'static Mutex<Option<(Region, bool, Instant, Vec<ModelInfo>, &'static str)>> {
    static MEMO: OnceLock<Mutex<Option<(Region, bool, Instant, Vec<ModelInfo>, &'static str)>>> =
        OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(None))
}

/// 第 2、3 层（纯本地，不需要网络也不需要凭证）：落盘快照 → 本机痕迹 → 空。
fn offline_layers(region: Region, dir: &Path) -> (Vec<ModelInfo>, &'static str) {
    match load_snapshot(dir, region) {
        Some(list) => (list, "cache"),
        None => {
            let local = local_models(region);
            if local.is_empty() {
                (Vec::new(), "empty")
            } else {
                (local, "local")
            }
        }
    }
}

/// 「第 1 层为什么没有结果」→ 直接给界面看的一句话。
///
/// **现算、不进缓存**：它取决于「这一次调用有没有凭证」，而那是会变的
/// （用户随时可能去登录）。把它塞进 [`memo`] 会让「刚登录完仍显示旧原因」活一小时。
fn explain_not_fetched(region: Region, had_credential: bool) -> String {
    if had_credential {
        // 有凭证却没拉到：两个区域实测都进不去（国际版 404 / 国内版 503，见模块头）。
        // 所以这里必须说「属常态」，否则用户会以为是自己网络的问题、反复点刷新。
        "联网拉取没成功（该接口对常规客户端不开放，属常态）".to_string()
    } else {
        format!("「{}」下还没有账号，本次没联网", region.label())
    }
}

/// 取模型清单：内存缓存 → 网络 → 落盘快照 → 本机痕迹 → 空。
///
/// `token` 为 `None`（该区域还没有可用账号）时**跳过网络层**，但清单照样给 ——
/// 三层里有两层是纯本地的。把「没凭证」当失败是错的，理由见模块头。
///
/// `refresh = true` 跳过内存缓存（对应接管页那颗「刷新」按钮：用户明确要求重拉，
/// 就不该被 1 小时的缓存挡住）。注意它**只跳过内存缓存** —— 网络失败时仍然依次退到
/// 后两层，否则点一次刷新就会把界面变成空的。
///
/// 返回的 `source` / `note` 直接透给界面：前者说清单来自哪一层，后者说第 1 层为什么空。
pub async fn load(region: Region, dir: &Path, token: Option<&str>, refresh: bool) -> ModelReport {
    let had_credential = token.is_some();
    if !refresh {
        if let Ok(g) = memo().lock() {
            if let Some((r, cred, at, list, src)) = g.as_ref() {
                if *r == region && *cred == had_credential && at.elapsed() < TTL {
                    return ModelReport {
                        models: list.clone(),
                        source: (*src).to_string(),
                        note: (*src != "fetched")
                            .then(|| explain_not_fetched(region, had_credential)),
                    };
                }
            }
        }
    }

    let (models, source) = match token {
        Some(t) => match fetch_remote(region, dir, t).await {
            Some(list) => (list, "fetched"),
            None => offline_layers(region, dir),
        },
        None => offline_layers(region, dir),
    };

    if let Ok(mut g) = memo().lock() {
        *g = Some((region, had_credential, Instant::now(), models.clone(), source));
    }
    ModelReport {
        note: (source != "fetched").then(|| explain_not_fetched(region, had_credential)),
        models,
        source: source.to_string(),
    }
}

/// 路由侧要的免费集合（与界面同源，见模块头）。
pub fn free_ids(models: &[ModelInfo]) -> HashSet<String> {
    models.iter().filter(|m| m.free).map(|m| m.id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[ModelInfo]) -> Vec<&str> {
        list.iter().map(|m| m.id.as_str()).collect()
    }

    /// 典型形态：外面包一层 `data`
    #[test]
    fn parses_wrapped_array() {
        let v = serde_json::json!({"data":{"models":[
            {"id":"qmodel_38max","name":"Qwen3.8-Max","credits":"x0.00 credits"},
            {"id":"qmodel_09pro","name":"Qwen3-Pro","credits":"x0.79 credits"}
        ]}});
        let list = parse_list(&v);
        assert_eq!(ids(&list), ["qmodel_38max", "qmodel_09pro"], "免费的要排前面");
        assert!(list[0].free);
        assert_eq!(list[0].name, "Qwen3.8-Max");
        assert_eq!(list[0].multiplier, "x0.00 credits");
        assert!(!list[1].free);
    }

    /// 结构还没定，所以裸数组 / result 包一层 / data 直接是数组都要认
    #[test]
    fn tolerates_other_shapes() {
        let bare = serde_json::json!([{"id":"qmodel_a","credits":0}]);
        assert_eq!(ids(&parse_list(&bare)), ["qmodel_a"]);
        assert!(parse_list(&bare)[0].free, "倍率写成数字 0 也要认");

        let res = serde_json::json!({"result":{"list":[{"model":"qmodel_b"}]}});
        assert_eq!(ids(&parse_list(&res)), ["qmodel_b"]);

        let direct = serde_json::json!({"data":[{"key":"qmodel_c"}]});
        assert_eq!(ids(&parse_list(&direct)), ["qmodel_c"]);
    }

    /// 倍率缺失 ⇒ 判「不免费」：猜错的代价不对称（见 `free_of` 的说明）
    #[test]
    fn missing_multiplier_is_not_free() {
        let v = serde_json::json!({"models":[{"id":"qmodel_x"},{"id":"qmodel_y","credits":"credits"}]});
        let list = parse_list(&v);
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|m| !m.free));
        assert!(list.iter().all(|m| m.multiplier.is_empty()));
    }

    /// 名字与 id 相同 ⇒ 不留重复的名字；同一个 id 出现两次只出一条
    #[test]
    fn dedups_and_drops_duplicate_name() {
        let v = serde_json::json!({"models":[
            {"id":"qmodel_d","name":"qmodel_d"},
            {"id":"qmodel_d","name":"Qwen-D"}
        ]});
        let list = parse_list(&v);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "", "id 与 name 相同等于没名字");

        let dup = serde_json::json!({"data":{"models":[{"id":"qmodel_e"}]},"list":[{"id":"qmodel_e"}]});
        assert_eq!(ids(&parse_list(&dup)), ["qmodel_e"]);
    }

    /// 认不出模型数组 ⇒ 空列表（**不是**退回某个写死的模型名）
    #[test]
    fn unrecognized_shape_yields_empty() {
        assert!(parse_list(&serde_json::json!({})).is_empty());
        assert!(parse_list(&serde_json::json!({"code":0,"message":"ok"})).is_empty());
        assert!(
            parse_list(&serde_json::json!({"models":[{"no_id":1}]})).is_empty(),
            "没有 id 的条目对网关没用"
        );
    }

    #[test]
    fn multiplier_parsing_accepts_both_forms() {
        assert_eq!(parse_multiplier(&serde_json::json!("x0.00 credits")), Some(0.0));
        assert_eq!(parse_multiplier(&serde_json::json!(" x0.79 ")), Some(0.79));
        assert_eq!(parse_multiplier(&serde_json::json!(0.05)), Some(0.05));
        assert_eq!(parse_multiplier(&serde_json::json!("credits")), None);
    }

    #[test]
    fn free_ids_only_takes_free_ones() {
        let list = vec![
            ModelInfo { id: "a".into(), name: String::new(), free: true, multiplier: "x0.00".into() },
            ModelInfo { id: "b".into(), name: String::new(), free: false, multiplier: "x0.05".into() },
        ];
        let set = free_ids(&list);
        assert!(set.contains("a"));
        assert!(!set.contains("b"));
    }

    /// 快照读写：坏文件 / 空文件不该让调用方出错
    #[test]
    fn snapshot_roundtrip_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!("qoder-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(snapshot_path(&dir, Region::Global));

        assert!(load_snapshot(&dir, Region::Global).is_none(), "没写过就该是 None");

        let list = vec![ModelInfo {
            id: "qmodel_s".into(),
            name: "Qwen-S".into(),
            free: true,
            multiplier: "x0.00".into(),
        }];
        save_snapshot(&dir, Region::Global, &list, Some(&serde_json::json!({"raw":true})));
        assert_eq!(load_snapshot(&dir, Region::Global), Some(list));

        // 原始响应要留下来（给日后收紧解析当样本）
        let text = std::fs::read_to_string(snapshot_path(&dir, Region::Global)).unwrap();
        assert!(text.contains("\"raw\""), "raw 样本应被保留");
        assert!(text.contains("at_ms"));

        std::fs::write(snapshot_path(&dir, Region::Global), b"{ not json").unwrap();
        assert!(
            load_snapshot(&dir, Region::Global).is_none(),
            "坏文件 → None，不 panic"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两个区域的快照**必须各存各的**：共用一份会让「国内版界面显示国际版的模型」
    /// 一直活下去，而界面看不出这份清单是从哪来的。
    #[test]
    fn snapshots_are_kept_per_region() {
        let dir = std::env::temp_dir().join(format!("qoder-models-r-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |id: &str| ModelInfo {
            id: id.into(),
            name: id.into(),
            free: true,
            multiplier: "x0.00".into(),
        };
        save_snapshot(&dir, Region::Global, &[mk("g")], None);
        save_snapshot(&dir, Region::Cn, &[mk("c")], None);

        assert_eq!(load_snapshot(&dir, Region::Global), Some(vec![mk("g")]));
        assert_eq!(load_snapshot(&dir, Region::Cn), Some(vec![mk("c")]));
        assert_ne!(
            snapshot_path(&dir, Region::Global),
            snapshot_path(&dir, Region::Cn),
            "文件名必须按区域分开"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话日志的形态是实测出来的，这里用**精简过的真实片段**回归：
    /// id 在第 1 行（`session.config.loaded`）、显示名在 SessionStart 那条 hook —— **不同行**。
    ///
    /// 这条测试存在的意义就是钉住「按文件配对」这个结论：改回逐行配对会立刻红。
    #[test]
    fn session_log_pairs_name_across_lines_within_a_file() {
        let dir = std::env::temp_dir().join(format!("qoder-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("seg.jsonl");
        std::fs::write(
            &p,
            [
                r#"{"seq":7,"type":"session.config.loaded","data":{"model":"qmodel_38max"}}"#,
                r#"{"seq":10,"type":"cli.route.entered","data":{"route":"headlessStreamJson"}}"#,
                r#"{"seq":20,"type":"hook.started","data":{"hook_input":{"model":"Qwen3.8-Max"}}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let (ids, names) = scan_session(&p);
        assert_eq!(ids, ["qmodel_38max"]);
        assert_eq!(names, ["Qwen3.8-Max"]);
        assert_eq!(pair_name(&ids, &names), Some("Qwen3.8-Max"));

        // 空 id（没显式指定模型的会话）必须丢掉：留着会变成一个既没 id 也没名字的条目
        std::fs::write(
            &p,
            r#"{"seq":7,"type":"session.config.loaded","data":{"model":""}}"#,
        )
        .unwrap();
        assert!(scan_session(&p).0.is_empty());
        let (mut found, mut seen) = (Vec::new(), HashSet::new());
        push_unique(&mut found, &mut seen, "", "Qwen3.8-Max");
        assert!(found.is_empty(), "空 id 不该进列表");

        // 一个文件里出现两个模型 ⇒ 对不上号，宁可不给名字
        let two = vec!["qmodel_a".to_string(), "qmodel_b".to_string()];
        let one = vec!["Qwen-X".to_string()];
        assert_eq!(pair_name(&two, &one), None);
        assert_eq!(pair_name(&one, &one), Some("Qwen-X"));
        assert_eq!(pair_name(&[], &one), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机冒烟：把第 3 层（`~/.qoder` 痕迹）读到的模型逐条打出来。
    ///
    /// 接管页显示不出模型时**先跑这个**，它能一眼分开两种情况：
    /// 「这台机器压根没有 Qoder 痕迹」（正常，去用一次 CLI）与
    /// 「痕迹在、但解析没认出来」（是我们的 bug）。
    ///
    /// 依赖跑测试这台机器的 `~/.qoder`，所以不进默认用例。
    #[test]
    #[ignore = "读本机 ~/.qoder，结果随环境变"]
    fn smoke_local_traces() {
        println!("== 国际版（~/.qoder）==");
        for m in &local_models(Region::Global) {
            println!(
                "  id={:<20} name={:<16} free={} multiplier={:?}",
                m.id, m.name, m.free, m.multiplier
            );
        }
        println!("== 国内版（~/.qoder-cn）==");
        let list = local_models(Region::Cn);
        println!("本机痕迹读到 {} 个模型：", list.len());
        for m in &list {
            println!(
                "  id={:<20} name={:<16} free={} multiplier={:?}",
                m.id, m.name, m.free, m.multiplier
            );
        }
        println!("免费集合 = {:?}", free_ids(&list));
    }

    /// **没有凭证不是错误。** 第 1 层缺席而已，清单照样给，并且必须说清为什么。
    ///
    /// 这条钉的是曾经的真实故障：`free_models` 在没有账号的区域直接返回
    /// `Err("xxx 下暂无账号，无法拉取模型列表")`。于是「只看一眼清单」——一件
    /// 纯本地、两层来源都不需要网络的事 —— 被一个与它无关的条件整个拦掉，
    /// 用户看到红字，而账号在另一个区域是登着的。
    #[tokio::test]
    async fn a_region_without_a_credential_still_gets_a_list_and_a_reason() {
        let dir = std::env::temp_dir().join(format!("qoder-models-noc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        for region in Region::ALL {
            // 第一圈走「现算」，第二圈命中刚写进 memo 的那条 —— 两条出口都要带原因
            let fresh = load(region, &dir, None, true).await;
            let cached = load(region, &dir, None, false).await;
            for (r, which) in [(&fresh, "现算"), (&cached, "命中缓存")] {
                assert_ne!(r.source, "fetched", "{which}：没凭证不可能来自拉取");
                let note = r.note.as_deref().unwrap_or_else(|| {
                    panic!("{which}：{} 下没凭证必须说明原因", region.label())
                });
                assert!(note.contains(region.label()), "{which}：原因要点名区域：{note}");
                assert!(note.contains("还没有账号"), "{which}：{note}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有落盘快照时：无凭证也要走到第 2 层（**不能**因为没账号就只给「空」），
    /// 并且仍然把「第 1 层为什么空」讲清楚。
    #[tokio::test]
    async fn a_snapshot_is_used_even_without_a_credential() {
        let dir = std::env::temp_dir().join(format!("qoder-models-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = vec![ModelInfo {
            id: "qmodel_snap".into(),
            name: "Qwen-Snap".into(),
            free: true,
            multiplier: "x0.00".into(),
        }];
        save_snapshot(&dir, Region::Cn, &list, None);

        let r = load(Region::Cn, &dir, None, true).await;
        assert_eq!(r.source, "cache", "有快照就该用快照");
        assert_eq!(r.models, list);
        let note = r.note.expect("仍然要说清第 1 层为什么是空的");
        assert!(note.contains("国内版") && note.contains("还没有账号"), "{note}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 界面看到的那句话分两种来路，且**必须区分**：没凭证是「去登录」，
    /// 有凭证是「这接口就不给常规客户端用」（见模块头的实测）。
    /// 混成一句话会让用户对着一个永远点不好的「刷新」按钮使劲。
    #[test]
    fn the_reason_distinguishes_no_credential_from_a_failed_fetch() {
        let no_cred = explain_not_fetched(Region::Global, false);
        let failed = explain_not_fetched(Region::Cn, true);
        assert!(no_cred.contains("国际版") && no_cred.contains("还没有账号"), "{no_cred}");
        assert!(!no_cred.contains("常态"), "没凭证与「拉不到是常态」是两回事：{no_cred}");
        assert!(failed.contains("常态"), "有凭证却拉不到必须说明这是常态：{failed}");
        assert!(!failed.contains("还没有账号"), "{failed}");
    }
}
