//! Qoder OpenAPI：**所有 Qoder 原生接口的唯一入口**。
//!
//! # 为什么要有这个模块
//!
//! 本项目从 workbuddy(clone) 改名而来，改造过程中额度接口落在 `usage.rs`、登录落在
//! `oauth.rs`，各自维护一份基址与请求头。那正是最该避免的形态：同一个后端会在不同代码
//! 路径上收到两套「身份」，而任何一处写错都表现为「这个功能悄悄不工作」。
//!
//! 所以这里把三件事收成一处，其余模块只消费：
//!
//! - **基址**：见下方四个 `pub const`（国内版实测域）—— 本模块是它们的唯一事实表；
//! - **请求头**：官方 `Bx()` 那一组（Accept / Authorization / Cosy-ClientType / User-Agent）；
//! - **解析**：照抄官方的校验规则（见 `basedata/20260918_Qoder缺失接口逆向.md` 第 3 节），
//!   官方会因为「分级不是 4 档」「日期重复」而报错，我们也一样 —— 静默接受坏数据比报错更糟。
//!
//! # 设计约定
//!
//! 每个接口都是「纯解析函数 + best-effort 拉取」两段：
//! `parse_*` 只吃 `&Value`、可单测；`fetch_*` 负责网络，失败一律 `Option::None` / 默认值，
//! 绝不 panic、也绝不影响调用方主流程。响应原文（实测）直接当 fixture 进单测。

use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

// 这里曾经有四个 `pub const &str` 基址，后被改成「由 `Region` 按账号所在区域现场取」——
// 那是两套部署并存年代的形态。2026-10-16 起国际版支持整体移除，只剩国内版一套部署，
// 基址回到常量，**本模块就是它们的唯一事实表**，别处只消费、不自己拼。

/// 登录 / OAuth 基址。
pub const AUTH_BASE: &str = "https://qoder.cn";
/// OpenAPI 基址：额度 / 活动权益 / 用户信息。
pub const OPENAPI_BASE: &str = "https://openapi.qoder.com.cn";
/// 模型网关基址：反代转发目标、模型目录、COSY uid 查询（国内版三条路径同域，实测
/// `~/.qoder-cn/logs/runs/*/qodercli.log` 里出现的就是它）。
pub const GATEWAY_BASE: &str = "https://gateway.qoder.com.cn";
/// OAuth 的 client id（客户端 app.asar 实测）。
pub const AUTH_CLIENT_ID: &str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";

/// Qoder CN 桌面端的用户数据目录名（Electron `app.getPath("userData")`）。
/// `auth.machine-id`、`auth.v1.dat` 都在它下面；钥匙串服务名也按它对得上
/// 「Qoder CN App Safe Storage」（见 [`crate::auth_file`]）。
pub(crate) const PROFILE_DIR_NAME: &str = "com.qodercn.app.stable";

/// 桌面端用户数据目录的候选路径（按平台）。
///
/// 目录里那个 `auth.v1.dat` 是 Chromium `safeStorage`（OSCrypt）写出来的，
/// 两个平台的密钥通道不同、但**都能端外解**：Windows 走 DPAPI，
/// macOS 走钥匙串（见 [`crate::auth_file`]）。
pub(crate) fn profile_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(roaming) = dirs::data_dir() {
            out.push(roaming.join(PROFILE_DIR_NAME));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            out.push(
                home.join("Library")
                    .join("Application Support")
                    .join(PROFILE_DIR_NAME),
            );
        }
    }
    let _ = &mut out;
    out
}

/// Qoder CN 的 CLI 目录（`~` 下的相对名）。
///
/// 会话日志、当前选中的模型等本地痕迹都在它下面（`models::local_models`），
/// 接管探测也按它找客户端配置。
pub(crate) const CLI_DIR_NAME: &str = ".qoder-cn";

/// 单次请求的超时。额度类接口都很小，慢就是不正常。
const TIMEOUT: Duration = Duration::from_secs(15);

/// Qoder 客户端身份头。
///
/// 对应官方 `Bx()`：
/// ```js
/// { Accept: "application/json", Authorization: `Bearer ${token}`,
///   "Cosy-ClientType": String(rl.clientType) /* = "10" */, "User-Agent": "Qoder" }
/// ```
///
/// 注意 `Cosy-ClientType` 的值就是 `10`（`String(10)`），**不带引号**。历史上这里写成过
/// 带引号的 `"10"`；实测两种都能拿到 200（服务端不校验该头），但既然源码是 `10`，
/// 就按源码写 —— 这种「反正都能过」的差异留着只会让下一个人再怀疑一次。
///
/// `Authorization` 不写死在客户端里：由调用方 `.bearer_auth(token)` 追加，
/// 这样同一个客户端也能用于不需要鉴权的登录类请求。
pub fn client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        "Cosy-ClientType",
        reqwest::header::HeaderValue::from_static("10"),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("Qoder"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(TIMEOUT)
        .build()
        .expect("构建 Qoder HTTP 客户端失败")
}

// ---------------------------------------------------------------- 请求封装

/// 发一个带鉴权的 GET（打 **OpenAPI**），拿 JSON。
/// 任何失败（网络 / 非 2xx / 非 JSON）都回 `None`。
///
/// `query` 里值为空的项会被跳过 —— 调用方因此可以无脑塞 `("product", product)`，
/// 不必自己判断「这个接口要不要带它」。
pub async fn get_json(
    token: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Option<Value> {
    get_json_with_auth_signal(token, path, query)
        .await
        .0
}

/// [`get_json`] 的**活性探针版**：把 HTTP 401 单独暴露出来。
///
/// 401 是官方 API 对这张 token 的亲自拒绝，是「票已被服务端吊销」的唯一可信证据
/// （`expires_at` 只是本地记的账，吊销可以发生在它之前 —— 2026-10-08 traework
/// 「本地死票锁死」事故的根源）。其余失败（5xx/超时/解析）归入 `(None, false)`，
/// 「不知道」不下结论。
pub async fn get_json_with_auth_signal(
    token: &str,
    path: &str,
    query: &[(&str, &str)],
) -> (Option<Value>, bool) {
    let pairs: Vec<(&str, &str)> = query
        .iter()
        .copied()
        .filter(|(_, v)| !v.is_empty())
        .collect();
    let Ok(resp) = client()
        .get(format!("{}{path}", OPENAPI_BASE))
        .query(&pairs)
        .bearer_auth(token)
        .send()
        .await
    else {
        return (None, false);
    };
    let auth_rejected = resp.status() == reqwest::StatusCode::UNAUTHORIZED;
    if !resp.status().is_success() {
        return (None, auth_rejected);
    }
    (resp.json::<Value>().await.ok(), false)
}

/// 从多个候选键里取第一个非空字符串（官方 `el()` 的同款语义）。
fn str_of<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k)?.as_str())
        .filter(|s| !s.is_empty())
}

/// 从多个候选键里取第一个有限数（官方 `oz()` 的宽松版）。
fn num_of(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| v.get(*k)?.as_f64())
        .filter(|n| n.is_finite())
}

/// 从多个候选键里取第一个非负安全整数（官方 `pV()`）。
fn uint_of(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| v.get(*k)?.as_u64())
}

/// 从多个候选键里取第一个布尔值（官方 `mV()`）。
fn bool_of(v: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| v.get(*k)?.as_bool())
}

// 这里曾经有「套餐 / 活跃度 / 近一年汇总 / 消耗热力图」四组接口封装：
//   GET /api/v2/user/plan
//   GET /sash/api/v1/ai-conversations/seat-activity
//   GET /sash/api/v1/ai-conversations/credits-summary
//   GET /sash/api/v1/ai-conversations/credits-heatmap?days=371
// 它们只服务于账号页顶部那排指标卡与热力图。2026-09-18 按产品决定改版：
// 顶部只保留「账号总数 / 已签到 / 未签到 / 剩余额度」，热力图不做 ——
// 四组封装因此**全部失去消费方**，按「只被测试调用的接口封装就是死代码」一并删除。
// 响应形态与实测证据保留在 `basedata/20260918_Qoder缺失接口逆向.md`，将来要接照那份接。
// 账号信息（`GET /api/v1/userinfo`）曾经在这里有一组 `UserInfoView` / `parse_userinfo` /
// `fetch_userinfo`：它把响应的 id/name/email/avatar 解析成一个结构体，但**没有任何消费方**
// （登录流程要的只是 uid + 昵称，那两样由 `oauth::parse_userinfo` 就地取；
// 账号页展示的名称/手机号来自本机登录文件）。只有冒烟测试在用它 ——
// 一个只被测试调用的接口封装就是死代码，留着还会让人以为「账号信息已经接进来了」。
// 接口本身与响应形态记录在 `basedata/20260918_Qoder缺失接口逆向.md`，需要时照那份接。

// ---------------------------------------------------------------- 活动权益（= 签到）

/// 活动状态接口（桌面端 `campaignMainService` 的 `LIST_PATH`）。
///
/// 桌面端只用它开「活动入口 + 自动弹窗」的开关，但响应里的 `campaigns[]` 才是本应用
/// 要的东西 —— 那条每日 `CLAIM_BENEFIT` 就是 Qoder 的「签到」。见
/// `basedata/20260918_Qoder活动权益接口逆向.md`。
const CAMPAIGN_PATH: &str = "/sash/api/v1/me/campaigns";

/// `campaignUrl` 允许的 host（桌面端 `ALLOWED_HOSTS` + `EXTRA_HOSTS`，含子域）。
///
/// 这个字段会被拿去开 webview，所以桌面端对它设了白名单；我们也照做 ——
/// 一个由服务端任意指定的跳转地址，只该落在自家域里。
const CAMPAIGN_ALLOWED_HOSTS: &[&str] = &[
    "qoder.sh",
    "qoder.com",
    "qoder.com.cn",
    "test.qoder.ai",
];

/// 活动里的一项权益（`benefit`）。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CampaignBenefit {
    /// 权益类型，实测只见过 `CREDITS`
    pub kind: String,
    /// 数量（Credits）
    pub amount: f64,
    /// 有效期模式：`RELATIVE_DAYS`（领取后 N 天）或 `FIXED_END`（固定到期）
    pub validity_mode: String,
    /// `RELATIVE_DAYS` 时的天数；其余模式为 0
    pub validity_days: u64,
    /// `FIXED_END` 时的到期时刻（ISO 串）；其余模式为空
    pub fixed_end: String,
}

/// 一条活动。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Campaign {
    /// `campaignId`（UUID）—— **领取路径的唯一凭据**，`campaignKey` 不能代替它
    pub id: String,
    /// `campaignKey`，带日期（如 `act-20260918-899`），每天换一个
    pub key: String,
    /// `CLAIM_BENEFIT`（要领取）/ `VIEW_DETAILS`（只跳详情页）
    pub action_type: String,
    /// `CLAIMABLE`（可领）/ `CLAIMED`（已领）/ 其余视为不可领
    pub claim_status: String,
    /// 活动窗口，**Unix 秒**
    pub start_at: i64,
    pub end_at: i64,
    /// 权益；`VIEW_DETAILS` 那类活动没有这个字段
    pub benefit: Option<CampaignBenefit>,
    pub title: String,
    pub description: String,
    pub button_text: String,
    pub detail_url: String,
}

impl Campaign {
    /// 这一条现在能不能领。三个条件缺一不可，且都是实测得出的：
    /// `actionType` 必须是 `CLAIM_BENEFIT`（`VIEW_DETAILS` 只是详情页入口）、
    /// `claimStatus` 必须是 `CLAIMABLE`（`CLAIMED` 再打就是 409）、
    /// 且必须有 `campaignId`（没有它拼不出领取路径）。
    pub fn is_claimable(&self) -> bool {
        self.action_type == "CLAIM_BENEFIT"
            && self.claim_status == "CLAIMABLE"
            && !self.id.is_empty()
    }

    /// 这是不是那条「每日一领」：`CLAIM_BENEFIT` 且带 CREDITS 权益。
    /// 界面用它区分「今天那条签到活动」与「别的运营活动」。
    pub fn is_daily_claim(&self) -> bool {
        self.action_type == "CLAIM_BENEFIT"
            && self
                .benefit
                .as_ref()
                .is_some_and(|b| b.kind == "CREDITS")
    }
}

/// `GET /sash/api/v1/me/campaigns` 的解析结果。
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CampaignView {
    /// 活动入口是否可见（桌面端「用量面板」那个入口靠它开关）
    pub show_campaign: bool,
    /// 是否有可领 —— 桌面端**自动弹窗**的判据（`source == "automatic"` 时只看它）
    pub claimable: bool,
    /// 活动页地址（已过白名单；不合法时为空串）
    pub campaign_url: String,
    pub campaigns: Vec<Campaign>,
}

impl CampaignView {
    /// 现在可领的那一条（没有就是 `None`）。
    pub fn claimable_campaign(&self) -> Option<&Campaign> {
        self.campaigns.iter().find(|c| c.is_claimable())
    }

    /// 当天那条「每日一领」，**领没领都返回** —— 界面要拿它显示「已领 / 可领」与倒计时。
    pub fn daily_claim(&self) -> Option<&Campaign> {
        self.campaigns.iter().find(|c| c.is_daily_claim())
    }
}

/// `campaignUrl` 白名单校验：必须 https，且 host 落在 [`CAMPAIGN_ALLOWED_HOSTS`]（含子域）。
///
/// 不合法时**只置空该字段**、不让整份解析失败 —— 桌面端同样如此（它丢的是这个字段）。
/// 「今天没有活动」是一个完全正常的状态，把它的一个附属字段当成致命错误就太严了。
fn allowed_campaign_url(raw: &str) -> String {
    let raw = raw.trim();
    let Ok(u) = reqwest::Url::parse(raw) else {
        return String::new();
    };
    if u.scheme() != "https" {
        return String::new();
    }
    let host = u.host_str().unwrap_or("");
    let ok = CAMPAIGN_ALLOWED_HOSTS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")));
    if ok {
        raw.to_string()
    } else {
        String::new()
    }
}

/// 取某条 placement 所属的本地化文案对象：`content.zh` 优先，退 `content.en`。
fn localized(content: &Value) -> Option<&Value> {
    content
        .get("zh")
        .filter(|v| v.is_object())
        .or_else(|| content.get("en").filter(|v| v.is_object()))
}

/// 从 `placements[]` 里取展示文案，返回 `(title, description, buttonText, detailUrl)`。
///
/// 文案**不在活动对象顶层**，而是按 placement（展示位）分平台给的：桌面端弹窗取的
/// 是 `type == "POPUP"` 那条，我们也一样；没有 POPUP 就退第一条（总比空白强）。
fn popup_text(c: &Value) -> (String, String, String, String) {
    let empty = || {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    };
    let Some(items) = c.get("placements").and_then(Value::as_array) else {
        return empty();
    };
    let pick = items
        .iter()
        .find(|p| str_of(p, &["type"]) == Some("POPUP"))
        .or_else(|| items.first());
    let Some(content) = pick.and_then(|p| p.get("content")).and_then(localized) else {
        return empty();
    };
    (
        str_of(content, &["title"]).unwrap_or("").to_string(),
        str_of(content, &["description"]).unwrap_or("").to_string(),
        str_of(content, &["buttonText", "button_text"])
            .unwrap_or("")
            .to_string(),
        // 实测英文那条带前导空格（`" https://…"`），去掉再交给界面
        str_of(content, &["detailUrl", "detail_url"])
            .unwrap_or("")
            .trim()
            .to_string(),
    )
}

/// 解析一条活动。
///
/// **没有 `campaignId` 的条目直接丢弃**：`campaignKey` 每天换一个且不能用于领取，
/// 留着它只会在界面多出一行「看着可领、点了没反应」的东西 —— 那比不显示更糟。
/// 因此 [`Campaign::is_claimable`] 不必再替调用方兜这一层。
fn parse_campaign(c: &Value) -> Option<Campaign> {
    if !c.is_object() {
        return None;
    }
    let id = str_of(c, &["campaignId", "campaign_id"])?.to_string();
    let key = str_of(c, &["campaignKey", "campaign_key"])
        .unwrap_or(id.as_str())
        .to_string();
    let (title, description, button_text, detail_url) = popup_text(c);
    Some(Campaign {
        id,
        key,
        action_type: str_of(c, &["actionType", "action_type"])
            .unwrap_or("")
            .to_string(),
        claim_status: str_of(c, &["claimStatus", "claim_status"])
            .unwrap_or("")
            .to_string(),
        start_at: num_of(c, &["startAt", "start_at"]).map(|n| n as i64).unwrap_or(0),
        end_at: num_of(c, &["endAt", "end_at"]).map(|n| n as i64).unwrap_or(0),
        benefit: c.get("benefit").and_then(parse_benefit),
        title,
        description,
        button_text,
        detail_url,
    })
}

/// 解析 `benefit`。缺 `amount` 按 0 处理（界面上显示「+0」也比没有强，且不影响领取）。
fn parse_benefit(b: &Value) -> Option<CampaignBenefit> {
    if !b.is_object() {
        return None;
    }
    let v = b.get("validity");
    Some(CampaignBenefit {
        kind: str_of(b, &["kind"]).unwrap_or("").to_string(),
        amount: num_of(b, &["amount"]).unwrap_or(0.0),
        validity_mode: v.and_then(|v| str_of(v, &["mode"])).unwrap_or("").to_string(),
        validity_days: v.and_then(|v| uint_of(v, &["days"])).unwrap_or(0),
        fixed_end: v
            .and_then(|v| str_of(v, &["fixedEnd", "fixed_end"]))
            .unwrap_or("")
            .to_string(),
    })
}

/// 解析活动状态。
///
/// **`showCampaign` 必须是布尔**：它是「这确实是活动接口的响应」的唯一判据，缺了就当
/// 解析失败（回 `None`）—— 桌面端也是这么判的（`typeof != boolean` 即丢弃整份）。
/// 其余字段一律宽松：`campaigns` 缺了就是空列表、`campaignUrl` 不在白名单就置空。
/// 「今天没有活动」是正常状态，不该被当成接口坏了。
pub fn parse_campaigns(root: &Value) -> Option<CampaignView> {
    let show_campaign = root.get("showCampaign")?.as_bool()?;
    let campaigns = root
        .get("campaigns")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_campaign).collect())
        .unwrap_or_default();
    Some(CampaignView {
        show_campaign,
        claimable: bool_of(root, &["claimable"]).unwrap_or(false),
        campaign_url: str_of(root, &["campaignUrl", "campaign_url"])
            .map(allowed_campaign_url)
            .unwrap_or_default(),
        campaigns,
    })
}

/// 拉活动状态。失败回 `None`。
pub async fn fetch_campaigns(token: &str) -> Option<CampaignView> {
    let resp = client()
        .get(format!("{}{CAMPAIGN_PATH}", OPENAPI_BASE))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    parse_campaigns(&resp.json::<Value>().await.ok()?)
}

/// 一次领取的**发放凭据** —— `claim` 响应里与「这一笔积分」有关的那部分事实。
///
/// 它是「积分的有效期」在 Qoder 后端**唯一给到毫秒级日期**的地方：
/// `/sash/api/v2/me/usage` 的 `expiresAt` 是**整个额度概览**的到期（计划周期终点，
/// 免费号还是「无期限」哨兵），而这里的是**这一笔**积分的到期。
///
/// ```json
/// { "grantId":"01a0b818-…","status":"CLAIMED","replayed":true,
///   "benefit":{"kind":"CREDITS","amount":100,"validity":{"mode":"RELATIVE_DAYS","days":30}},
///   "campaignId":"…","claimedAt":"2026-09-19T05:16:33.580914Z",
///   "grantedAt":"2026-09-19T05:16:33.827758Z","expiresAt":"2026-10-19T05:16:33.580914Z" }
/// ```
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimReceipt {
    /// 发放记录 id（`grantId`）。同一笔反复回放都是同一个值 —— 界面不展示，
    /// 但它是「两次响应说的是不是同一笔」的凭据，比时间戳可靠。
    pub grant_id: String,
    /// 服务端状态（成功即 `CLAIMED`）
    pub status: String,
    /// 这次是**回放**：这笔早领过了，响应只是把当初那张凭据又给了一遍（没有重复发放）。
    pub replayed: bool,
    /// 权益数量（`benefit.amount`）
    pub amount: Option<f64>,
    /// 这笔积分的**真实到期时刻**（毫秒）。源字段 `expiresAt` 是 ISO-8601 UTC 串
    /// （`2026-10-19T05:16:33.580914Z`，微秒精度），经 `timeutil::norm_ts` 归一。
    pub expires_at: Option<i64>,
    /// 当初领取的时刻（`claimedAt`，ISO-8601 串原文）—— 展示用，不参与计算。
    pub claimed_at: Option<String>,
}

/// 响应 → [`ClaimReceipt`]（纯函数）。
///
/// 只认必需的一件事：`status` 要在。其余字段缺失一律留空 ——
/// 老版本响应没有 `grantId` / `expiresAt` 时，**不能**因此把整次领取判成失败
/// （那会把「领到了但拿不到到期时间」误报成「没领到」）。
pub fn parse_receipt(root: &Value) -> Option<ClaimReceipt> {
    // 远端页也是这么解的：优先看 `data` 里那层
    let inner = root.get("data").filter(|d| d.is_object()).unwrap_or(root);
    let status = str_of(inner, &["status"])?.to_string();
    Some(ClaimReceipt {
        grant_id: str_of(inner, &["grantId", "grant_id"])
            .unwrap_or_default()
            .to_string(),
        status,
        replayed: bool_of(inner, &["replayed"]).unwrap_or(false),
        amount: inner
            .get("benefit")
            .and_then(|b| num_of(b, &["amount"]))
            .or_else(|| num_of(inner, &["amount"])),
        expires_at: crate::timeutil::norm_ts(inner.get("expiresAt")),
        claimed_at: str_of(inner, &["claimedAt", "claimed_at"]).map(str::to_string),
    })
}

/// 领取一条活动的权益 —— **本模块唯一的写操作**。
///
/// 契约来自远端活动页 `activity-iframe.js`（见逆向文档第 4 节）：
/// `POST {CAMPAIGN_PATH}/{campaignId}/claim`、**无请求体**，响应里 `status == "CLAIMED"`
/// 才算成功。其余一切（不可领、429 太频繁、非 JSON）都原样把原因带回去 ——
/// 这是唯一会改变账号权益的接口，宁可让调用方看到原因，也不要在这里自动重试或凭错误码猜结论。
///
/// # ⚠️ 已领取时它**不会** 409，而是**幂等回放**
///
/// 2026-09-19 实测更正：对当天那条 `claimStatus: "CLAIMED"` 的每日活动重打，返回的是
/// **`HTTP 200`** + `{"status":"CLAIMED","replayed":true, …}`，`grantId` 与 `expiresAt`
/// 与当初领取时**逐字相同**（`claimedAt + 30 天`），**没有**重复发放。旧注释里
/// 「已领的活动再打只会拿到 409」是错的 —— 它曾让本应用完全放弃这条路径。
///
/// 因此这个函数同时是「领取」与「**取回发放凭据**」两个用途的同一个入口：
/// 想要那笔积分的到期时间，不必等到当天首次领取，靠回放也能拿到。
/// 调用方仍应只在需要时打（见 `checkin::do_checkin`：每天每账号至多一次）。
pub async fn claim_campaign(token: &str, campaign_id: &str) -> Result<ClaimReceipt, String> {
    // 路径参数直接拼进 URL，所以只放行 UUID 的字符集。正常响应里它是 UUID，
    // 但「服务端给什么就拼什么」是路径穿越的经典入口（`../` 会被当成路径分隔符）。
    if campaign_id.is_empty()
        || !campaign_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("campaignId 形态异常，拒绝拼路径：{campaign_id:?}"));
    }
    let resp = client()
        .post(format!(
            "{}{CAMPAIGN_PATH}/{campaign_id}/claim",
            OPENAPI_BASE
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("领取请求失败：{e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let excerpt: String = text.trim().chars().take(200).collect();
    if !status.is_success() {
        return Err(format!("HTTP {} {}", status.as_u16(), excerpt));
    }
    let body: Value =
        serde_json::from_str(&text).map_err(|_| format!("响应不是 JSON：{excerpt}"))?;
    match parse_receipt(&body) {
        Some(r) if r.status == "CLAIMED" => Ok(r),
        Some(r) => Err(format!("领取未被确认（status={}）", r.status)),
        None => Err(format!("领取未被确认（响应里没有 status）：{excerpt}")),
    }
}

// 「账号概览」聚合层（`AccountOverview` + `fetch_overview`，并发拉 plan / seat-activity
// / credits-summary 三路）随上面四组接口一并删除：它存在的唯一理由是把那三样凑成一屏，
// 现在顶部四张卡改为「账号总数 / 已签到 / 未签到 / 剩余额度」，不再需要任何一路。

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **实测响应原文**（2026-09-18 18:17 UTC+8）：当天那条已领、另有一条只跳详情的。
    /// 字段一字未改（只截短了第二条的英文文案），**唯 `uid` 已脱敏**——它是真实账号 id，
    /// 尾部清零后与 `usage.rs` 的写法一致，其余字段（含 `campaignId`，那是全体用户共用的
    /// 活动标识、不含个人信息）保持原样。
    const CAMPAIGNS_REAL: &str = r#"{
      "uid": "01a0b380-0000-0000-0000-000000000000",
      "showCampaign": true,
      "claimable": false,
      "campaignUrl": "https://openapi.qoder.sh/growth-page/activity-iframe",
      "campaigns": [
        {
          "campaignId": "01a0b019-c2dc-772b-869a-66809daa8805",
          "campaignKey": "act-20260918-899",
          "actionType": "CLAIM_BENEFIT",
          "startAt": 1789696500, "endAt": 1789783140,
          "claimStatus": "CLAIMED",
          "benefit": {
            "kind": "CREDITS", "amount": 100,
            "modelScope": {"modelSeries": {"key": "ALL_MODELS"}},
            "validity": {"mode": "RELATIVE_DAYS", "days": 30}
          },
          "placements": [
            {"type": "POPUP",
             "campaignUrl": "https://openapi.qoder.sh/growth-page/activity-iframe",
             "content": {
               "en": {"buttonText": "", "description": "Daily reset: 10:00 (UTC+8). …",
                      "detailUrl": "https://docs.qoder.com/events/100credits",
                      "title": "Claim 100 Credits Daily"},
               "zh": {"buttonText": "", "description": "每日 10:00（UTC+8）刷新，领取后 30 天有效",
                      "detailUrl": "https://docs.qoder.com/zh/events/100credits",
                      "title": "每天领 100 Credits"}
             }},
            {"type": "USAGE", "content": {"zh": {"title": "每天领 100 Credits"}}}
          ]
        },
        {
          "campaignId": "01a05bce-e800-7494-a002-806e4438f483",
          "campaignKey": "act-20260901-493",
          "actionType": "VIEW_DETAILS",
          "startAt": 1788247200, "endAt": 1790783940,
          "claimStatus": "CLAIMED",
          "placements": [
            {"type": "POPUP", "content": {
               "en": {"buttonText": "", "description": "", "detailUrl": " https://docs.qoder.com/events/bogo",
                      "title": "September perk"},
               "zh": {"buttonText": "", "description": "首购 Pro 得 4,000，Pro+ 得 12,000。",
                      "detailUrl": "https://docs.qoder.com/zh/events/bogo",
                      "title": "9月限时福利，Pro/Pro+ 首月订阅 Credits 翻倍"}
             }}
          ]
        }
      ]
    }"#;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).expect("fixture 必须是合法 JSON")
    }

    /// **实测响应原文**（2026-09-19，国内版免费号 `personal_standard`，对当天那条
    /// 已领的每日活动重打）。字段一字未改，**只有 id 尾部清零脱敏**
    /// （`grantId` / `campaignId` 与真实账号绑定；`campaignKey` 是全体用户共用的活动标识，
    /// 保持原样，与 `CAMPAIGNS_REAL` 同一口径）。
    const CLAIM_REPLAY_REAL: &str = r#"{
      "grantId": "01a0b7f7-0000-0000-0000-000000000000",
      "status": "CLAIMED",
      "replayed": true,
      "benefit": {"kind":"CREDITS","amount":100,
                  "modelScope":{"modelSeries":{"key":"ALL_MODELS"}},
                  "validity":{"mode":"RELATIVE_DAYS","days":30}},
      "campaignId": "01a0b4aa-fbd4-720f-87bc-7eb1c6c9ddb6",
      "campaignKey": "act-20260918-628",
      "campaignVersion": 1,
      "claimedAt": "2026-09-19T04:40:55.460619Z",
      "grantedAt": "2026-09-19T04:40:55.722022Z",
      "expiresAt": "2026-10-19T04:40:55.460619Z"
    }"#;

    /// 回放响应里能拿到**这笔积分自己的到期时刻** —— 这正是免费号缺的那一个数
    /// （`/sash/api/v2/me/usage` 对它只给「无期限」哨兵）。
    #[test]
    fn claim_receipt_reads_the_grant_expiry_from_a_replayed_response() {
        let v: Value = serde_json::from_str(CLAIM_REPLAY_REAL).unwrap();
        let r = parse_receipt(&v).expect("有 status 就该解析出来");
        assert_eq!(r.status, "CLAIMED");
        assert!(r.replayed);
        assert_eq!(r.amount, Some(100.0));
        assert_eq!(r.claimed_at.as_deref(), Some("2026-09-19T04:40:55.460619Z"));
        // 2026-10-19T04:40:55.460619Z（比 claimedAt 整整晚 30 天，与 benefit.validity 一致）
        assert_eq!(r.expires_at, Some(1_792_384_855_460));
        assert_eq!(
            r.grant_id,
            "01a0b7f7-0000-0000-0000-000000000000",
            "grantId 必须原样带出来（它是「是不是同一笔」的凭据）"
        );
    }

    /// 幂等回放的响应里**没有** `replayed` 之外的新字段也不能崩；反过来，
    /// 老版本响应（无 `grantId` / `expiresAt`）**依旧算成功** ——
    /// 那只是「拿不到到期时间」，不是「没领到」。
    #[test]
    fn claim_receipt_tolerates_missing_grant_fields() {
        let v: Value = serde_json::json!({"status": "CLAIMED"});
        let r = parse_receipt(&v).expect("status 在就算解析成功");
        assert_eq!(r.status, "CLAIMED");
        assert!(!r.replayed);
        assert!(r.grant_id.is_empty());
        assert_eq!(r.expires_at, None);
        assert_eq!(r.amount, None);

        // 被 `data` 包一层（网关/Mock 会这么干）
        let wrapped: Value = serde_json::json!({"code": 0, "data": {
            "status": "CLAIMED", "replayed": true, "expiresAt": "2026-10-19T05:16:33.580914Z"
        }});
        let r = parse_receipt(&wrapped).expect("下钻 data 后要能解析");
        assert!(r.replayed);
        assert_eq!(r.expires_at, Some(1_792_386_993_580));

        // 没有 status → 认不出来（调用方会把它报成「领取未被确认」，而不是当成成功）
        assert!(parse_receipt(&serde_json::json!({"grantId": "g"})).is_none());
    }

    #[test]
    fn campaigns_parses_real_response() {
        let c = parse_campaigns(&v(CAMPAIGNS_REAL)).expect("实测响应必须能解析");
        assert!(c.show_campaign);
        assert!(!c.claimable);
        assert_eq!(
            c.campaign_url,
            "https://openapi.qoder.sh/growth-page/activity-iframe"
        );
        assert_eq!(c.campaigns.len(), 2);

        let daily = c.daily_claim().expect("应认出每日一领那条");
        assert_eq!(daily.id, "01a0b019-c2dc-772b-869a-66809daa8805");
        assert_eq!(daily.key, "act-20260918-899");
        assert_eq!(daily.claim_status, "CLAIMED");
        assert!(!daily.is_claimable(), "已领的不算可领");
        assert_eq!(daily.end_at, 1789783140);
        let b = daily.benefit.as_ref().expect("每日一领必须带权益");
        assert_eq!(b.kind, "CREDITS");
        assert_eq!(b.amount, 100.0);
        assert_eq!(b.validity_mode, "RELATIVE_DAYS");
        assert_eq!(b.validity_days, 30);
        // 文案取的是 POPUP 那条的 zh
        assert_eq!(daily.title, "每天领 100 Credits");
        assert_eq!(daily.description, "每日 10:00（UTC+8）刷新，领取后 30 天有效");
        assert_eq!(daily.detail_url, "https://docs.qoder.com/zh/events/100credits");

        // `VIEW_DETAILS` 那条没有 benefit，也不该被当成「每日一领」
        let detail = &c.campaigns[1];
        assert!(detail.benefit.is_none());
        assert!(!detail.is_daily_claim());
        assert_eq!(detail.title, "9月限时福利，Pro/Pro+ 首月订阅 Credits 翻倍");
    }

    #[test]
    fn campaigns_picks_the_claimable_one() {
        let mut root = v(CAMPAIGNS_REAL);
        root["claimable"] = json!(true);
        root["campaigns"][0]["claimStatus"] = json!("CLAIMABLE");
        let c = parse_campaigns(&root).unwrap();
        let t = c.claimable_campaign().expect("应挑出可领那条");
        assert_eq!(t.id, "01a0b019-c2dc-772b-869a-66809daa8805");
        assert!(t.is_claimable());
    }

    #[test]
    fn campaigns_requires_show_flag_as_a_boolean() {
        // 「这确实是活动接口的响应」的唯一判据：缺了或不是布尔就整份作废
        assert!(parse_campaigns(&json!({"campaigns": []})).is_none());
        assert!(parse_campaigns(&json!({"showCampaign": "true"})).is_none());
        // 空态是合法的：今天没有活动
        let c = parse_campaigns(&json!({"showCampaign": false, "claimable": false})).unwrap();
        assert!(c.campaigns.is_empty());
        assert!(c.campaign_url.is_empty());
        assert!(c.claimable_campaign().is_none());
    }

    #[test]
    fn campaigns_whitelists_the_redirect_url() {
        let ok = |u: &str| {
            parse_campaigns(&json!({"showCampaign": true, "campaignUrl": u}))
                .unwrap()
                .campaign_url
        };
        // 自家域（含子域）放行
        assert_ne!(ok("https://openapi.qoder.sh/growth-page/activity-iframe"), "");
        assert_ne!(ok("https://qoder.com/x"), "");
        // 非 https / 外域一律置空（这个字段会被拿去开 webview）
        assert_eq!(ok("http://openapi.qoder.sh/x"), "");
        assert_eq!(ok("https://evil.com/x"), "");
        assert_eq!(ok("https://qoder.sh.evil.com/x"), "");
        assert_eq!(ok("not a url"), "");
    }

    #[test]
    fn campaigns_drops_entries_without_campaign_id() {
        // 没有 campaignId 就拼不出领取路径 —— 留着只会多一行「点了没反应」
        let c = parse_campaigns(&json!({
            "showCampaign": true,
            "campaigns": [
                {"campaignKey": "act-1", "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE"},
                {"campaignId": "", "campaignKey": "act-2", "actionType": "CLAIM_BENEFIT"},
                {"campaignId": "uuid-3", "campaignKey": "act-3",
                 "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE"}
            ]
        }))
        .unwrap();
        assert_eq!(c.campaigns.len(), 1);
        assert_eq!(c.campaigns[0].id, "uuid-3");
        // 没有 campaignKey 时退回用 id 当 key（不编一个空 key 出来）
        let c = parse_campaigns(&json!({
            "showCampaign": true,
            "campaigns": [{"campaignId": "uuid-4", "actionType": "CLAIM_BENEFIT"}]
        }))
        .unwrap();
        assert_eq!(c.campaigns[0].key, "uuid-4");
    }

    #[test]
    fn campaign_claimability_needs_all_three_conditions() {
        let mk = |action: &str, status: &str, id: &str| Campaign {
            id: id.into(),
            key: "k".into(),
            action_type: action.into(),
            claim_status: status.into(),
            start_at: 0,
            end_at: 0,
            benefit: None,
            title: String::new(),
            description: String::new(),
            button_text: String::new(),
            detail_url: String::new(),
        };
        assert!(mk("CLAIM_BENEFIT", "CLAIMABLE", "u").is_claimable());
        assert!(!mk("VIEW_DETAILS", "CLAIMABLE", "u").is_claimable());
        assert!(!mk("CLAIM_BENEFIT", "CLAIMED", "u").is_claimable());
        assert!(!mk("CLAIM_BENEFIT", "CLAIMABLE", "").is_claimable());
    }

    /// 本机冒烟：活动权益状态（**只读**，绝不发 claim）。
    /// `cargo test --lib -- --ignored --nocapture smoke_real_campaigns`
    #[tokio::test]
    #[ignore = "真实网络调用，需本机已登录 Qoder"]
    async fn smoke_real_campaigns() {
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        let c = fetch_campaigns(&a.token).await.expect("活动接口应可用");
        println!(
            "showCampaign={} claimable={} url={:?}",
            c.show_campaign, c.claimable, c.campaign_url
        );
        for x in &c.campaigns {
            println!(
                "  - {} key={} action={} status={} {}..{} benefit={:?} title={:?}",
                x.id,
                x.key,
                x.action_type,
                x.claim_status,
                x.start_at,
                x.end_at,
                x.benefit.as_ref().map(|b| (b.kind.as_str(), b.amount, b.validity_days)),
                x.title
            );
        }
        // 只断言「能解析」——今天有没有可领的活动取决于时刻，不是稳定条件
        assert!(parse_campaigns(&serde_json::json!({})).is_none());
    }

    /// 本机数据目录（池内账号所在）。优先环境变量，否则按平台的 Tauri 约定。
    fn pool_data_dir() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("QODER_ASSISTANT_DATA_DIR") {
            return std::path::PathBuf::from(p);
        }
        if cfg!(target_os = "windows") {
            std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
                .join("com.waxilo.qoder-assistant")
        } else {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join("Library/Application Support/com.waxilo.qoder-assistant")
        }
    }

    /// 打一次活动接口，回 `(HTTP 状态, 响应体)`；连不上回 `(0, 错误原文)`。
    async fn raw_campaigns(token: &str) -> (u16, String) {
        let req = client()
            .get(format!("{}{CAMPAIGN_PATH}", OPENAPI_BASE))
            .bearer_auth(token);
        match req.send().await {
            Ok(r) => {
                let st = r.status().as_u16();
                (st, r.text().await.unwrap_or_default())
            }
            Err(e) => (0, e.to_string()),
        }
    }

    /// 前 `n` 个字符（打印响应体用）。
    fn head(s: &str, n: usize) -> String {
        s.chars().take(n).collect()
    }

    /// 真实接口冒烟（**只读**）：**池内每个账号**今天各自拿到什么活动。
    ///
    /// 「活动未开」有两个完全不同的来源，界面上的文案却是同一句：
    /// ① 服务端对这个账号没下发当天的每日活动（资格 / 时刻）；
    /// ② 本地把响应解错了（`showCampaign` 缺失、`claimStatus` 不认）。
    /// 这里逐个打印 HTTP 状态与每日活动那条的 `claimStatus`。
    ///
    /// 运行：`cargo test --lib -- --ignored --nocapture smoke_real_pool_campaigns`
    #[tokio::test]
    #[ignore = "真实网络调用（只读 GET），读本机 accounts.json"]
    async fn smoke_real_pool_campaigns() {
        let dir = pool_data_dir();
        let accounts = crate::accounts::load_accounts(&dir);
        println!("数据目录：{}（{} 个账号）", dir.display(), accounts.len());
        for a in &accounts {
            let who = a
                .phone
                .clone()
                .or_else(|| a.email.clone())
                .unwrap_or_else(|| a.name.clone());
            println!(
                "\n=== {} / token {} 字 ===",
                who,
                a.token.len()
            );
            let (st, body) = raw_campaigns(&a.token).await;
            match serde_json::from_str::<Value>(&body) {
                Ok(v) => match parse_campaigns(&v) {
                    Some(view) => {
                        println!(
                            "  HTTP {st} show={} claimable={} 活动数={}",
                            view.show_campaign,
                            view.claimable,
                            view.campaigns.len()
                        );
                        match view.daily_claim() {
                            Some(c) => println!(
                                "  每日活动：{} key={} status={} {}..{}",
                                c.id, c.key, c.claim_status, c.start_at, c.end_at
                            ),
                            None => println!("  每日活动：没有（campaigns 里没有当天的 CLAIM_BENEFIT）"),
                        }
                        for c in &view.campaigns {
                            println!(
                                "    - {} {} {} status={} title={:?} benefit={:?}",
                                c.id,
                                c.key,
                                c.action_type,
                                c.claim_status,
                                c.title,
                                c.benefit.as_ref().map(|b| (b.kind.as_str(), b.amount, b.validity_days))
                            );
                        }
                    }
                    None => println!("  HTTP {st} 解析失败（无 showCampaign 布尔）：{}", head(&body, 200)),
                },
                Err(_) => println!("  HTTP {st} 非 JSON：{}", head(&body, 200)),
            }
            let view = crate::usage::fetch_usage(&a.token).await;
            println!(
                "  额度：剩余={:?} 最早到期={:?} 包数={}",
                view.credits,
                view.earliest_expiry_ms,
                view.packages.len()
            );
        }
    }
}
