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
//! | 1 | `GET {infer_base}/algo/api/v2/model/list` | Qoder 官方目录，最全；通了就落盘 |
//! | 2 | `models-cache-{region}.json`（落盘快照） | 上次成功的结果，抗网络抖动与重启 |
//! | 3 | `~/.qoder` 的本地痕迹 | 纯本地、必定可用；只覆盖「真用过的」模型 |
//!
//! **三层都拿不到时返回空列表**，由界面显示空态。这里绝不再退回任何写死的模型名
//! —— 那正是把 `hy3` 冒充成 Qoder 模型的根源。
//!
//! # 第 1 层的两把钥匙：`/algo` 前缀 与 COSY 自签（2026-09-19 实测）
//!
//! 这一层先前被判成「对常规客户端不开放」，据此写下的结论（包括本模块头的旧版本、
//! 以及「只能去 spawn 客户端 CLI 走控制协议」的方案）**都是错的**。真相是两处都缺了东西：
//!
//! 1. **路径少了 `/algo` 前缀。** 客户端签名用的 path 是 `/api/v2/model/list`
//!    （[`crate::cosy::signing_path`] 会剥掉它），但**发出去的 URL** 是
//!    `{infer_base}/algo/api/v2/model/list`。少了前缀直接打到网关是 ALB 的 `503`
//!    —— 看着像「网络契约拒绝第三方客户端」，其实只是走错了门。
//!    （旧结论里那串「换 httpdns 落点 IP、换 HTTP 版本、换 UA 一律 503」的排查，
//!    全部是在错的 URL 上做的。）
//! 2. **认证不是 `Bearer dt-…`，是 COSY。** 与其余业务接口一样，凭据封在
//!    `Authorization: Bearer COSY.…` + `Cosy-User/Key/Date` 四件套里 ——
//!    而这套头我们本来就能自己签发（[`crate::cosy`]，接管换号靠的就是它）。
//!    用普通 Bearer 打过去必然进不去。
//!
//! 两处都补齐后：`200`，**明文 JSON**，14 个模型、带 `price_factor`（积分倍率）
//! 与 `promotion`（错峰折扣）。样本已落在快照的 `raw` 里。
//! （这条是**国内版**实测；国际版走同一条代码路径、尚未验过，拉不到时自动退到
//!   下面两层，所以不影响界面。`probe_real_catalog_fetch` 那条 ignored 测试两边都跑，
//!   跑一次就有结论。）
//!
//! 顺带两条实测结论，省得下次再试：
//! - 客户端发的 `?Encode=1` **可以去掉**。签名只覆盖剥掉 query 的 path，所以去掉它
//!   签名照样有效，而上游就不加密响应体了（带 `Encode=1` 也是 200，但这里没必要）。
//! - 宿主取 [`Region::infer_base`]（CLI 日志里 `endpointType:"infer"` 的那个域），
//!   **不是**早先猜的 `api3.qoder.sh`。
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

// 模型目录的宿主**不是常量**：两个区域不同（国际版 / 国内版的 `infer_base`，
// 见 `region` 模块与本模块头）。上一版这里是一处写死的 `pub const CATALOG_BASE`，
// 等于宣告「另一套部署永远拉不到目录」。

/// 模型目录的**发出去**的路径。
///
/// `/algo` 是网关的路由前缀，**必须有**（少了它 ALB 直接 503，见模块头）；
/// 而 COSY 签名里用的是剥掉它之后的 path（[`crate::cosy::signing_path`] 负责剥）。
/// 两者不一致不是 bug，是客户端自己的行为，这里照抄。
const CATALOG_PATH: &str = "/algo/api/v2/model/list";

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
/// （形如 `Qwen3.8-Max`，目录里的 `display_name`）。走第 1 层时两者都是真值；
/// 退到第 3 层（本机痕迹）时可能只有 id —— 那时 `name` 留空串，界面退回显示 `id`，
/// 而不是编一个名字。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 是否 0 积分模型（恒生效、UI 锁定勾选）。判据是 `price_factor == 0`，见 [`free_of`]。
    pub free: bool,
    /// 积分倍率展示串（如 `"x0.2"`）；空串 = 倍率未知
    #[serde(default)]
    pub multiplier: String,
    /// **折扣前**的倍率（错峰促销期间目录给的 `original_price_factor` /
    /// `promotion.before_promotion_price_factor`）；空串 = 没有折扣，界面上就不画划线价
    #[serde(default)]
    pub original_multiplier: String,
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
    /// 光看「来源：本机 Qoder 的记录」会以为是网络抖动，而不知道该去检查登录态。
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// 解析：模型目录响应 → 统一的模型列表
// ---------------------------------------------------------------------------

/// 下面四组键来自**真实响应样本**（第 1 层实测拉到的 14 条目录），不再是一堆
/// 「万一长这样」的候选名 —— 原先那份探测表里 `credits` / `multiplier` / `ratio`
/// 一个都不存在，所以就算接口通了也解析不出倍率（这正是界面上「倍率未知」的成因）。
/// 每组留两个是驼峰写法：客户端解析自己那套时两种都认（`priceFactor??price_factor`），
/// 说明服务端存在两种形态。
const ID_KEYS: [&str; 2] = ["key", "model_key"];
const NAME_KEYS: [&str; 2] = ["display_name", "name"];
const PRICE_KEYS: [&str; 2] = ["price_factor", "priceFactor"];
/// 折扣前倍率：目录里有两个来路 —— 顶层的 `original_price_factor`，
/// 以及促销对象里的 `promotion.before_promotion_price_factor`（错峰活动走后者）。
const ORIGINAL_PRICE_KEYS: [&str; 2] = ["original_price_factor", "originPriceFactor"];

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

/// 倍率 → 数字。目录给的是数字（`0.2`），驼峰样本里也见过字符串，两种都收。
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

/// 倍率 → 展示串（`0.2` → `"x0.2"`，`0.0` → `"x0"`）。
fn fmt_multiplier(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("x{}", n as i64)
    } else {
        format!("x{n}")
    }
}

/// 促销对象是否生效、生效时给出折扣前的倍率。
///
/// 目录里错峰活动的形态是 `promotion: {active, discount_factor,
/// before_promotion_price_factor, badge{en,zh}, window_start/window_end, …}`。
/// 只取「折扣前」这一个数：界面上要的是「0.5× 划掉、显示 0.2×」，
/// 而徽章文案、时区、活动窗口这些是运营内容，抄过来只会跟着版本漂。
///
/// `active` 要的是**值为 true**，不是「这个字段存在」：整个 `promotion` 对象是活动的
/// 配置，活动关掉时服务端未必会把它抹掉，只看存不存在就会给一个不打折的模型画出划线价。
fn promotion_original(m: &Value) -> Option<f64> {
    let p = m.get("promotion")?;
    p.get("active")
        .and_then(Value::as_bool)
        .filter(|active| *active)?;
    p.get("before_promotion_price_factor")
        .and_then(parse_multiplier)
}

/// 倍率字段 → `(是否免费, 当前倍率串, 折扣前倍率串)`。
///
/// # 免费判定为什么看 `price_factor` 而不是 `is_free`
///
/// 目录里**有**一个 `is_free` 字段，但它不是「0 积分」。实测样本：`qmodel_38max`
/// 同时是 `is_free: true` 与 `price_factor: 0.2`，而客户端自己给它展示的仍是 `x0.2`
/// —— 客户端的「Free」标签是另一条规则（`tags` 含 `limited_time_free` 才写 Free，
/// 否则一律显示倍率）。拿 `is_free` 当免费用，等于把一个 0.2 倍率的模型在 429 时
/// 无感换到别的账号上悄悄扣积分。
///
/// **拿不到倍率同样判「不免费」**：猜错的代价不对称 —— 猜「免费」多烧别人积分，
/// 猜「不免费」只是少一次自动切换，用户在界面勾一下就有了。
fn price_of(m: &Value) -> (bool, String, String) {
    let Some(raw) = PRICE_KEYS.iter().find_map(|k| m.get(*k)).and_then(parse_multiplier) else {
        return (false, String::new(), String::new());
    };
    let before = ORIGINAL_PRICE_KEYS
        .iter()
        .find_map(|k| m.get(*k))
        .and_then(parse_multiplier)
        .or_else(|| promotion_original(m))
        .filter(|o| *o != raw);
    (
        raw == 0.0,
        fmt_multiplier(raw),
        before.map(fmt_multiplier).unwrap_or_default(),
    )
}

/// 响应里的「模型数组」们 —— 顶层按 **scene 分组**（`chat` / `app` / `developer` /
/// `assistant` / `inline` / `quest` / …，实测 11 个键，其中 `byok_*` 是空数组）。
///
/// 这里**跨 scene 合并**而不是挑一个：路由只回答「这个 id 能不能免费切」，
/// 与用户从哪个入口发起无关，而各 scene 的条目实测同源同倍率。挑一个 scene 反而要
/// 猜「接管该看哪个」（CLI 用 `chat`、桌面端用 `app`），猜错就是少一批模型。
fn model_arrays(root: &Value) -> Vec<&Vec<Value>> {
    match root {
        Value::Object(map) => map
            .values()
            .filter_map(Value::as_array)
            .filter(|a| !a.is_empty())
            .collect(),
        // 兜一种「顶层直接就是数组」的形态（接口哪天不按 scene 包时不至于整个空掉）
        Value::Array(arr) if !arr.is_empty() => vec![arr],
        _ => Vec::new(),
    }
}

/// 响应 → 模型列表（免费排前、其余按 id 升序）。纯函数，便于单测。
pub fn parse_list(root: &Value) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for arr in model_arrays(root) {
        for m in arr {
            let Some(id) = id_of(m) else { continue };
            if !seen.insert(id.to_string()) {
                continue; // 同一个模型出现在多个 scene 里，只留先到的一条
            }
            let (free, multiplier, original_multiplier) = price_of(m);
            out.push(ModelInfo {
                name: name_of(m, id),
                id: id.to_string(),
                free,
                multiplier,
                original_multiplier,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    // 免费排前：界面里「恒生效」的那些该在最显眼处
    out.sort_by(|a, b| b.free.cmp(&a.free));
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
            // 本地痕迹给不出倍率 ⇒ 一律「不免费」（理由见 [`price_of`]）
            free: false,
            multiplier: String::new(),
            original_multiplier: String::new(),
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
/// # 认证：自签 COSY，不是 `Bearer dt-…`
///
/// 这条路径与接管热路径用的是**同一个签名器**（[`crate::cosy::sign`]），差别只在
/// 「没有客户端的原件可借环境特征」，所以走从零签的那一份。
/// 拿普通 Bearer 打这里必然进不去（凭据不在头上，服务端解不出账号）。
///
/// 宿主是 [`Region::infer_base`]（CLI 日志里的 `endpointType:"infer"`），
/// 且必须带 `/algo` 前缀 —— 两处都是踩过才知道的，详见模块头。
pub async fn fetch_remote(
    region: Region,
    dir: &Path,
    id: &crate::cosy::Identity,
) -> Option<Vec<ModelInfo>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 签名里是**剥掉 `/algo` 与 query** 的 path（`signing_path` 负责），所以这里
    // 直接把要发的 URL 路径交给它：两边天然一致。
    let reb = crate::cosy::sign(id, CATALOG_PATH, b"", now)?;
    let url = format!("{}{CATALOG_PATH}", region.infer_base());
    let resp = crate::http::api_client_direct()
        .get(&url)
        .header("authorization", reb.authorization.as_str())
        .header("cosy-user", reb.user.as_str())
        .header("cosy-key", reb.key.as_str())
        .header("cosy-date", reb.date.as_str())
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
        // （也可能是接口改了结构、我们还没跟上）。
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
/// **现算、不进缓存**：它取决于「这一次调用有没有可用的签名身份」，而那是会变的
/// （用户随时可能去登录）。把它塞进 [`memo`] 会让「刚登录完仍显示旧原因」活一小时。
fn explain_not_fetched(region: Region, had_identity: bool) -> String {
    if had_identity {
        // 有身份却没拉到：多半是网络或登录态（token 过期）。现在这条路是通的
        // （见模块头），所以不再甩锅给「接口不开放」。
        "联网拉取没成功（网络或登录态问题），本次显示的是缓存".to_string()
    } else {
        format!("「{}」下还没有可签名的账号，本次没联网", region.label())
    }
}

/// 取模型清单：内存缓存 → 网络 → 落盘快照 → 本机痕迹 → 空。
///
/// `identity` 为 `None`（该区域没有账号，或那个账号取不到 uid）时**跳过网络层**，
/// 但清单照样给 —— 三层里有两层是纯本地的。把「没身份」当失败是错的：用户常常只登
/// 一边，而接管页两个区域都要能打开。
///
/// `refresh = true` 跳过内存缓存（对应接管页那颗「刷新」按钮：用户明确要求重拉，
/// 就不该被 1 小时的缓存挡住）。注意它**只跳过内存缓存** —— 网络失败时仍然依次退到
/// 后两层，否则点一次刷新就会把界面变成空的。
///
/// 返回的 `source` / `note` 直接透给界面：前者说清单来自哪一层，后者说第 1 层为什么空。
pub async fn load(
    region: Region,
    dir: &Path,
    identity: Option<&crate::cosy::Identity>,
    refresh: bool,
) -> ModelReport {
    let had_identity = identity.is_some();
    if !refresh {
        if let Ok(g) = memo().lock() {
            if let Some((r, cred, at, list, src)) = g.as_ref() {
                if *r == region && *cred == had_identity && at.elapsed() < TTL {
                    return ModelReport {
                        models: list.clone(),
                        source: (*src).to_string(),
                        note: (*src != "fetched")
                            .then(|| explain_not_fetched(region, had_identity)),
                    };
                }
            }
        }
    }

    let (models, source) = match identity {
        Some(id) => match fetch_remote(region, dir, id).await {
            Some(list) => (list, "fetched"),
            None => offline_layers(region, dir),
        },
        None => offline_layers(region, dir),
    };

    if let Ok(mut g) = memo().lock() {
        *g = Some((region, had_identity, Instant::now(), models.clone(), source));
    }
    ModelReport {
        note: (source != "fetched").then(|| explain_not_fetched(region, had_identity)),
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
    use serde_json::json;

    fn ids(list: &[ModelInfo]) -> Vec<&str> {
        list.iter().map(|m| m.id.as_str()).collect()
    }

    fn model(id: &str, name: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut v = serde_json::json!({"key": id, "display_name": name});
        if let Some(map) = v.as_object_mut() {
            if let Some(o) = extra.as_object() {
                for (k, val) in o {
                    map.insert(k.clone(), val.clone());
                }
            }
        }
        v
    }

    /// **真实响应的精简样本**（2026-09-19 实测拉到的那份，字段名一个都没改）。
    ///
    /// 顶层按 scene 分组，同一个模型出现在多个 scene 里。这份测试钉的是这次故障的
    /// 正身：界面只有裸 id、倍率全显示「未知」—— 因为上一版探测的键名
    /// （`id` / `name` / `credits`）在这份响应里**一个都不存在**。
    fn real_fixture() -> Value {
        serde_json::json!({
            "app": [
                model("auto", "Auto", json!({"price_factor": 0.5, "is_default": true})),
                model("qmodel_38max", "Qwen3.8-Max", json!({
                    "price_factor": 0.2, "is_free": true,
                    "promotion": {"active": true, "before_promotion_price_factor": 0.5,
                                  "badge": {"zh": "错峰折扣进行中"}}
                })),
                model("qfmodel", "Qwen3.8-Flash", json!({
                    "price_factor": 0.0, "is_free": true, "original_price_factor": 0.1
                }))
            ],
            // chat 与 app 内容同源（实测 14 个键、每个 scene 倍率一致）
            "chat": [
                model("qmodel_latest", "Qwen3.7-Max", json!({
                    "price_factor": 0.1,
                    "promotion": {"active": true, "before_promotion_price_factor": 0.5}
                })),
                model("auto", "Auto", json!({"price_factor": 0.5}))
            ],
            // 空的 scene 要跳过，不能被当成「一条模型」
            "byok_teams": [],
            "byok_enterprise": []
        })
    }

    /// 真实形态：跨 scene 合并 + 去重 + 免费排前，倍率与折扣前倍率都解析出来。
    #[test]
    fn parses_the_real_scene_grouped_catalog() {
        let list = parse_list(&real_fixture());
        assert_eq!(
            ids(&list),
            ["qfmodel", "auto", "qmodel_38max", "qmodel_latest"],
            "免费的排最前，其余按 id 升序；重复的 auto 只留一条"
        );
        let m = |id: &str| list.iter().find(|x| x.id == id).unwrap();

        let flash = m("qfmodel");
        assert!(flash.free, "price_factor 0 ⇒ 免费");
        assert_eq!(flash.name, "Qwen3.8-Flash");
        assert_eq!(flash.multiplier, "x0");
        assert_eq!(flash.original_multiplier, "x0.1", "划线价来自顶层 original_price_factor");

        let max = m("qmodel_38max");
        assert!(!max.free, "is_free 不是「0 积分」，拿它判免费会多扣别人积分（见 price_of）");
        assert_eq!(max.multiplier, "x0.2");
        assert_eq!(max.original_multiplier, "x0.5", "划线价来自 promotion.before_promotion_…");

        assert_eq!(m("auto").multiplier, "x0.5");
        assert_eq!(m("auto").original_multiplier, "", "没有折扣就是空串，界面不画划线价");
    }

    /// **没有倍率的模型一律判「不免费」**（猜错的代价不对称，见 [`price_of`]）。
    #[test]
    fn a_model_without_a_price_is_not_free() {
        let v = serde_json::json!({"chat": [
            {"key": "qmodel_unknown"},
            {"key": "qmodel_unparsable", "price_factor": "credits"}
        ]});
        let list = parse_list(&v);
        assert_eq!(list.len(), 2, "认得出 id 就要进列表，哪怕倍率拿不到");
        assert!(list.iter().all(|m| !m.free));
        assert!(list.iter().all(|m| m.multiplier.is_empty()));
        assert_eq!(free_ids(&list), HashSet::new(), "路由侧也不该自动切到它们");
    }

    /// 裸数组与驼峰两种形态都还要认：服务端历史上两种写法都出现过
    /// （客户端自己解析时就是 `priceFactor ?? price_factor`）。
    #[test]
    fn tolerates_bare_array_and_camel_case() {
        let bare = serde_json::json!([{"key": "qmodel_a", "priceFactor": 0.0}]);
        let list = parse_list(&bare);
        assert_eq!(ids(&list), ["qmodel_a"]);
        assert!(list[0].free, "驼峰 + 数字 0 也要认");
        assert_eq!(list[0].name, "", "没有 display_name 就不编名字");

        let camel = serde_json::json!({"chat": [
            {"model_key": "qmodel_b", "name": "Qwen-B", "priceFactor": 0.05,
             "originPriceFactor": 0.5}
        ]});
        let list = parse_list(&camel);
        assert_eq!(list[0].id, "qmodel_b");
        assert_eq!(list[0].name, "Qwen-B", "只有 name 时也当显示名用");
        assert_eq!(list[0].multiplier, "x0.05");
        assert_eq!(list[0].original_multiplier, "x0.5");
    }

    /// 认不出模型 ⇒ 空列表（**不是**退回某个写死的模型名 —— 那正是 `hy3` 事故的根源）
    #[test]
    fn unrecognized_shape_yields_empty() {
        assert!(parse_list(&serde_json::json!({})).is_empty());
        assert!(parse_list(&serde_json::json!({"code": 0, "message": "ok"})).is_empty());
        assert!(
            parse_list(&serde_json::json!({"chat": [{"display_name": "没有 id 的条目"}]}))
                .is_empty()
        );
        assert!(parse_list(&serde_json::json!({"chat": [], "app": []})).is_empty());
    }

    /// 名字与 id 相同 ⇒ 不算「有独立的名字」；折扣与当前价相等 ⇒ 不画划线价
    #[test]
    fn drops_redundant_name_and_noop_discount() {
        let v = serde_json::json!({"chat": [
            {"key": "qmodel_d", "display_name": "qmodel_d", "price_factor": 0.3},
            {"key": "qmodel_e", "price_factor": 0.2, "original_price_factor": 0.2},
            {"key": "qmodel_f", "price_factor": 0.2,
             "promotion": {"active": false, "before_promotion_price_factor": 0.9}}
        ]});
        let list = parse_list(&v);
        assert_eq!(list[0].name, "", "id 与 name 相同等于没名字");
        assert_eq!(list[1].original_multiplier, "", "折扣前后一样，画出来是噪音");
        assert_eq!(list[2].original_multiplier, "", "promotion 没生效就不该报折扣");
    }

    #[test]
    fn multiplier_forms_parse_and_format() {
        assert_eq!(parse_multiplier(&serde_json::json!(0.05)), Some(0.05));
        assert_eq!(parse_multiplier(&serde_json::json!("x0.79")), Some(0.79));
        assert_eq!(parse_multiplier(&serde_json::json!(" x0.00 credits ")), Some(0.0));
        assert_eq!(parse_multiplier(&serde_json::json!("credits")), None);
        assert_eq!(parse_multiplier(&serde_json::json!(null)), None);

        assert_eq!(fmt_multiplier(0.0), "x0");
        assert_eq!(fmt_multiplier(1.0), "x1");
        assert_eq!(fmt_multiplier(0.2), "x0.2");
        assert_eq!(fmt_multiplier(0.04), "x0.04");
    }

    #[test]
    fn free_ids_only_takes_free_ones() {
        let list = vec![
            ModelInfo {
                id: "a".into(),
                name: String::new(),
                free: true,
                multiplier: "x0".into(),
                original_multiplier: String::new(),
            },
            ModelInfo {
                id: "b".into(),
                name: String::new(),
                free: false,
                multiplier: "x0.05".into(),
                original_multiplier: String::new(),
            },
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
            multiplier: "x0".into(),
            original_multiplier: "x0.1".into(),
        }];
        save_snapshot(&dir, Region::Global, &list, Some(&serde_json::json!({"raw":true})));
        let back = load_snapshot(&dir, Region::Global);
        assert_eq!(back, Some(list.clone()), "新字段要能过一遍落盘再读回来");

        // 原始响应要留下来（给日后收紧解析当样本）
        let text = std::fs::read_to_string(snapshot_path(&dir, Region::Global)).unwrap();
        assert!(text.contains("\"raw\""), "raw 样本应被保留");
        assert!(text.contains("at_ms"));
        assert!(text.contains("original_multiplier"));

        std::fs::write(snapshot_path(&dir, Region::Global), b"{ not json").unwrap();
        assert!(
            load_snapshot(&dir, Region::Global).is_none(),
            "坏文件 → None，不 panic"
        );

        // 老版本的快照（没有 original_multiplier）仍然要读得出来
        let old_snapshot = json!({
            "at_ms": 1,
            "models": [{"id": "old", "free": false, "multiplier": "x0.5"}]
        });
        std::fs::write(
            snapshot_path(&dir, Region::Global),
            serde_json::to_string(&old_snapshot).unwrap(),
        )
        .unwrap();
        let old = load_snapshot(&dir, Region::Global).expect("缺字段的老快照不能读成 None");
        assert_eq!(old[0].original_multiplier, "");

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
            multiplier: "x0".into(),
            original_multiplier: String::new(),
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
        for region in Region::ALL {
            let list = local_models(region);
            println!("== {}（{} 条）==", region.label(), list.len());
            for m in &list {
                println!(
                    "  id={:<20} name={:<16} free={} multiplier={:?}",
                    m.id, m.name, m.free, m.multiplier
                );
            }
            println!("免费集合 = {:?}", free_ids(&list));
        }
    }

    /// **没有可签名的账号不是错误。** 第 1 层缺席而已，清单照样给，并且必须说清为什么。
    ///
    /// 这条钉的是曾经的真实故障：`free_models` 在没有账号的区域直接返回
    /// `Err("xxx 下暂无账号，无法拉取模型列表")`。于是「只看一眼清单」——一件
    /// 纯本地、两层来源都不需要网络的事 —— 被一个与它无关的条件整个拦掉，
    /// 用户看到红字，而账号在另一个区域是登着的。
    #[tokio::test]
    async fn a_region_without_an_identity_still_gets_a_list_and_a_reason() {
        let dir = std::env::temp_dir().join(format!("qoder-models-noc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        for region in Region::ALL {
            // 第一圈走「现算」，第二圈命中刚写进 memo 的那条 —— 两条出口都要带原因
            let fresh = load(region, &dir, None, true).await;
            let cached = load(region, &dir, None, false).await;
            for (r, which) in [(&fresh, "现算"), (&cached, "命中缓存")] {
                assert_ne!(r.source, "fetched", "{which}：没身份不可能来自拉取");
                let note = r.note.as_deref().unwrap_or_else(|| {
                    panic!("{which}：{} 下没身份必须说明原因", region.label())
                });
                assert!(note.contains(region.label()), "{which}：原因要点名区域：{note}");
                assert!(note.contains("没联网"), "{which}：要说清没走网络：{note}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有落盘快照时：没身份也要走到第 2 层（**不能**因为没账号就只给「空」），
    /// 并且仍然把「第 1 层为什么空」讲清楚。
    #[tokio::test]
    async fn a_snapshot_is_used_even_without_an_identity() {
        let dir = std::env::temp_dir().join(format!("qoder-models-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = vec![ModelInfo {
            id: "qmodel_snap".into(),
            name: "Qwen-Snap".into(),
            free: true,
            multiplier: "x0".into(),
            original_multiplier: String::new(),
        }];
        save_snapshot(&dir, Region::Cn, &list, None);

        let r = load(Region::Cn, &dir, None, true).await;
        assert_eq!(r.source, "cache", "有快照就该用快照");
        assert_eq!(r.models, list);
        let note = r.note.expect("仍然要说清第 1 层为什么是空的");
        assert!(note.contains("国内版") && note.contains("还没有"), "{note}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 界面看到的那句话分两种来路，且**必须区分**：没身份是「去登录」，
    /// 有身份是「这次没拉到，先显示缓存」。混成一句话会让用户对着一个
    /// 与己无关的提示使劲，而不知道该检查登录态还是网络。
    #[test]
    fn the_reason_distinguishes_no_identity_from_a_failed_fetch() {
        let no_id = explain_not_fetched(Region::Global, false);
        let failed = explain_not_fetched(Region::Cn, true);
        assert!(no_id.contains("国际版") && no_id.contains("还没有"), "{no_id}");
        assert!(!no_id.contains("缓存"), "没身份时可能压根没有缓存可显示：{no_id}");
        assert!(!failed.contains("国内版"), "有身份那句不必点名区域：{failed}");
        assert!(failed.contains("登录态") || failed.contains("网络"), "{failed}");
        assert!(failed.contains("缓存"), "要告诉用户现在看的是缓存：{failed}");
        assert!(!failed.contains("还没有"), "{failed}");
    }

    /// 真机冒烟（手动跑）：走**生产路径**那一份（[`fetch_remote`]）打真实网关。
    ///
    /// 与上面的 `smoke_local_traces` 分工：那条看第 3 层（本机痕迹），这条看第 1 层
    /// （官方目录 + 自签 COSY + 解析 + 落盘快照）。跑完顺手把 `models-cache-cn.json`
    /// 写进临时目录，可以直接 `cat` 出来核对字段。
    ///
    /// 需要：本机 `accounts.json` 里有一个该区域、token 仍有效的账号。
    #[tokio::test]
    #[ignore = "打真实网关，需要本机已导入账号"]
    async fn probe_real_catalog_fetch() {
        let accounts_dir = std::env::var("QODER_ASSISTANT_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap())
                    .join("Library/Application Support/com.waxilo.qoder-assistant")
            });
        let dir = std::env::temp_dir().join(format!("qoder-catalog-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        for region in Region::ALL {
            let Some(acc) = crate::accounts::load_accounts(&accounts_dir)
                .into_iter()
                .filter(|a| a.region == region)
                .find(|a| !a.token.is_empty())
            else {
                println!("{}：本机没有账号，跳过", region.label());
                continue;
            };
            let client = crate::http::api_client_direct();
            let gateway = region.infer_base();
            let Some(id) =
                crate::accounts::cosy_identity(&accounts_dir, &acc, &client, gateway).await
            else {
                println!("{}：拿不到签名身份（token 过期？）", region.label());
                continue;
            };
            match fetch_remote(region, &dir, &id).await {
                None => println!("{}：拉取失败", region.label()),
                Some(list) => {
                    println!("{}：拉到 {} 个模型", region.label(), list.len());
                    for m in &list {
                        println!(
                            "  {:<16} {:<18} free={:<5} {}{}",
                            m.id,
                            m.name,
                            m.free,
                            m.multiplier,
                            if m.original_multiplier.is_empty() {
                                String::new()
                            } else {
                                format!("（原价 {}）", m.original_multiplier)
                            }
                        );
                    }
                    assert_eq!(load_snapshot(&dir, region).map(|v| v.len()), Some(list.len()));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
