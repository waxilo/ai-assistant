use crate::accounts::{Account, CheckinRecord};
use crate::ledger::PkgView;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

/// 签到接口路径（依次尝试，兼容 v1/v2）
const CHECKIN_PATHS: &[&str] = &[
    "/billing/meter/daily-checkin",
    "/v2/billing/meter/daily-checkin",
];
const STATUS_PATH: &str = "/billing/meter/checkin-status";
/// 账号剩余积分：官方 Web 端「计划与用量」同源接口
const RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
/// 资源包查询的结束时间跨度（官方前端查 101 年，照抄）
const RESOURCE_SPAN_DAYS: i64 = 365 * 101;

/// 业务返回码：10001 在文案命中“已签到”时表示今天已签到（幂等成功态）
const ALREADY_DONE_BIZ_CODE: i64 = 10001;

static INACTIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("未开启|未开始|未开放|已过期|无.*活动|活动.*(?:结束|关闭|暂停)").unwrap()
});
static ALREADY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("已签到|已领取|已经.*(?:签到|领取)|重复签到|already\\s*(?:checked[- ]?in|claimed)|already").unwrap()
});

#[derive(Clone, Copy)]
struct Outcome {
    ok: bool,
    already: bool,
    inactive: bool,
}

/// 仅接受官方签到接口的明确成功响应：
/// - code==0 且 HTTP 成功 → 成功
/// - code==10001 且文案命中“已签到”且非活动未开启 → 今日已签（幂等成功）
/// 网关可能返回空/非 JSON 的 200，所以单独 HTTP 200 不充分。
fn classify(http_ok: bool, code: Option<i64>, message: &str) -> Outcome {
    let inactive = INACTIVE_RE.is_match(message);
    let already = code == Some(ALREADY_DONE_BIZ_CODE) && !inactive && ALREADY_RE.is_match(message);
    let ok = !inactive && ((code == Some(0) && http_ok) || already);
    Outcome { ok, already, inactive }
}

/// 从 JWT 的 iss 字段推断签发方 origin（端口到 host 映射）
fn token_issuer_origin(token: &str) -> Option<String> {
    let part = token.split('.').nth(1)?;
    let mut padded = part.replace('-', "+").replace('_', "/");
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = STANDARD.decode(padded).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let iss = payload.get("iss")?.as_str()?;
    Some(iss.to_string())
}

/// 把 iss（如 https://www.workbuddy.ai/...）映射到签到 API host
fn host_for_iss(iss: &str) -> Option<&'static str> {
    if iss.contains("workbuddy.ai") {
        Some("https://www.workbuddy.ai")
    } else if iss.contains("workbuddy.cn") {
        Some("https://www.workbuddy.cn")
    } else if iss.contains("codebuddy.cn") {
        Some("https://www.codebuddy.cn")
    } else if iss.contains("codebuddy.ai") {
        Some("https://www.codebuddy.ai")
    } else {
        None
    }
}

/// 由 token 推断其来源 origin（JWT 的 iss → 签到 host 映射；非 JWT 返回 None）
pub fn issuer_host(token: &str) -> Option<String> {
    token_issuer_origin(token).and_then(|iss| host_for_iss(&iss).map(|s| s.to_string()))
}

fn normalize_host(url: &str) -> String {
    let u = url.trim();
    let u = if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{}", u)
    };
    u.trim_end_matches('/').to_string()
}

/// 候选 host 顺序：token iss 推断 > 账号自定义 base_url > 默认 base_url
fn candidate_hosts(token: &str, account_base: Option<&str>, default_base: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if let Some(origin) = token_issuer_origin(token) {
        if let Some(h) = host_for_iss(&origin) {
            hosts.push(h.to_string());
        }
    }
    if let Some(b) = account_base {
        let b = normalize_host(b);
        if !hosts.contains(&b) {
            hosts.push(b);
        }
    }
    let d = normalize_host(default_base);
    if !hosts.contains(&d) {
        hosts.push(d);
    }
    hosts
}

/// 对单个 host+path 发起签到请求并解析结果
async fn post_checkin(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<CheckinRecord, reqwest::Error> {
    // header 一律由 http::api_client 统一提供（UA / Accept / x-client-platform），
    // 这里不再手写——同一个模块里有的请求声明 web、有的不声明，本身就是识别点
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .timeout(crate::http::TIMEOUT)
        .send()
        .await?;

    let status = resp.status();
    let http_ok = status.is_success();
    let raw_text = resp.text().await.unwrap_or_default();
    let body: serde_json::Value = serde_json::from_str(&raw_text).unwrap_or(serde_json::Value::Null);

    let code = body.get("code").and_then(|v| v.as_i64());
    let mut msg = body
        .get("msg")
        .or_else(|| body.get("error"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    // 如果服务端返回的 msg 为空，或者 body 根本不是 JSON，
    // 用 HTTP 状态 + 原始 body 片段兜底，避免前端提示「签到失败：」后面空白。
    if msg.is_empty() {
        let snippet = if raw_text.trim().is_empty() {
            "返回体为空".to_string()
        } else {
            let s = raw_text.trim();
            s.chars().take(120).collect::<String>()
        };
        msg = format!("HTTP {} ({})", status.as_u16(), snippet);
    }

    let data = body.get("data");
    let credit = data
        .and_then(|d| d.get("today_credit"))
        .and_then(|v| v.as_i64());
    let streak = data
        .and_then(|d| d.get("streak_days"))
        .and_then(|v| v.as_i64());
    // 签到响应里的 `credit` 是**本次获得**（已记在 credit 字段），不是余额；
    // 余额统一由 fetch_credits 单独查询，这里刻意留空避免把 100 当成余额显示。

    let outcome = classify(http_ok, code, &msg);
    Ok(CheckinRecord {
        success: outcome.ok,
        already: outcome.already,
        inactive: outcome.inactive,
        message: msg,
        code,
        credit,
        balance: None,
        streak,
        host: None,
        at: String::new(),
    })
}

/// `get-user-resource` 的请求体（与官方 Web 端一致：查全部有效资源包）
fn resource_body() -> Value {
    let now = chrono::Local::now();
    let end = now + chrono::Duration::days(RESOURCE_SPAN_DAYS);
    let fmt = |d: chrono::DateTime<chrono::Local>| d.format("%Y-%m-%d %H:%M:%S").to_string();
    serde_json::json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": fmt(now),
        "PackageEndTimeRangeEnd": fmt(end),
    })
}

/// 从 `get-user-resource` 响应里取出资源包数组（兼容几种已知的嵌套位置）。
fn resource_accounts(body: &Value) -> Option<&Vec<Value>> {
    let d = body.get("data")?;
    d.get("Response")
        .and_then(|r| r.get("Data"))
        .and_then(|x| x.get("Accounts"))
        .and_then(Value::as_array)
        .or_else(|| d.get("accounts").and_then(Value::as_array))
        .or_else(|| {
            d.get("data")
                .and_then(|x| x.get("accounts"))
                .and_then(Value::as_array)
        })
}

/// 从 `get-user-resource` 响应里汇总剩余积分 —— **界面上那个「剩余积分」的口径**。
///
/// 官方返回 `data.Response.Data.Accounts[]`（也可能是 `data.accounts`），
/// 每个资源包按「周期剩余」优先取第一个有效值 —— 月度包用完后
/// `CapacityRemain*` 仍是满额，只有 `CycleCapacityRemain*` 会归零，顺序不能反。
///
/// 这一点与 [`parse_packages`] 是同一个事实：`Cycle*` 组才是活的，`Capacity*` 组接口不维护。
/// 余额从第一天起就取对了（所以界面上的数一直是对的），而台账曾经取的是死的那一组 ——
/// 两个口径现在都收敛到 `Cycle*`，不会再各说各话。
fn sum_credits(body: &Value) -> Option<f64> {
    const KEYS: &[&str] = &[
        "CycleCapacityRemainPrecise",
        "CycleCapacityRemain",
        "CapacityRemainPrecise",
        "CapacityRemain",
    ];
    let accounts = resource_accounts(body)?;

    let mut total = 0f64;
    let mut seen = false;
    for a in accounts {
        for k in KEYS {
            let Some(v) = a.get(*k) else { continue };
            let n = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            if let Some(n) = n {
                total += n;
                seen = true;
                break;
            }
        }
    }
    // 一个资源包都没解析出数值时返回 None（交给兜底），而不是谎报 0
    if seen {
        Some((total * 100.0).round() / 100.0)
    } else {
        None
    }
}

/// 解析 `CycleEndTime`（形如 `2026-09-20 23:48:22`，服务器按本地时区给出）→ 毫秒时间戳。
fn parse_cycle_end(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S")
        .ok()?
        .and_local_timezone(chrono::Local)
        .single()
        .map(|t| t.timestamp_millis())
}

/// 取「还有余量的资源包里最早的重置/过期时间」（毫秒）。
///
/// 这是反代「按积分过期时间优先路由」的依据：越早过期越先用，把快过期的积分消耗掉。
/// 已用完（余量为 0）的包不参与——拿它当“最早过期”没有意义。
fn earliest_cycle_end(body: &Value) -> Option<i64> {
    const REMAIN_KEYS: &[&str] = &["CycleCapacityRemainPrecise", "CycleCapacityRemain"];
    let accounts = resource_accounts(body)?;
    let mut earliest: Option<i64> = None;
    for a in accounts {
        // 余量 > 0 才算数
        let has_remain = REMAIN_KEYS.iter().any(|k| {
            a.get(*k)
                .and_then(|v| match v {
                    Value::Number(n) => n.as_f64(),
                    Value::String(s) => s.trim().parse::<f64>().ok(),
                    _ => None,
                })
                .map(|n| n > 0.0)
                .unwrap_or(false)
        });
        if !has_remain {
            continue;
        }
        if let Some(end) = a
            .get("CycleEndTime")
            .and_then(Value::as_str)
            .and_then(parse_cycle_end)
        {
            earliest = Some(earliest.map_or(end, |e: i64| e.min(end)));
        }
    }
    earliest
}

/// 兜底：从 `checkin-status` 的 data 里取累计积分（不同后端键名不一）
fn extract_total_credits(data: &Value) -> Option<f64> {
    for key in ["total_credits", "total_credit", "credit_balance", "balance"] {
        if let Some(v) = data.get(key) {
            let n = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            if let Some(n) = n {
                return Some(n);
            }
        }
    }
    None
}

/// 查一次剩余积分：主用 `get-user-resource`，取不到再退 `checkin-status`（都在视图里）。
///
/// 全程 best-effort：网络错误 / 非 JSON / 字段缺失都返回 None，绝不影响签到主流程。
///
/// 与 [`fetch_resource_view`] 是同一个请求：签到这条路径只要汇总值，但走的仍是
/// 「唯一入口」，不另造一套解析。
async fn fetch_credits(client: &reqwest::Client, host: &str, token: &str) -> Option<f64> {
    fetch_resource_view(client, host, token).await.credits
}

/// 单个字段取值：优先 `*Precise`（接口给的是带小数的字符串），再退整数型字段。
fn as_num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn num_field(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| as_num(v.get(*k)))
}

/// `get-user-resource` 的一次完整读结果：汇总值 + 逐包明细。
///
/// 它同时是**积分事实的唯一入口**：`credits`（剩余积分）给账户管理与路由用，
/// `packages`（逐包本周期授予/已用）给台账算消耗与新增，`earliest_expiry_ms` 给路由排序。
/// 三条拉取路径（手动刷新 / 简报采样 / 接管路由决策）拿到的都是它 ——
/// 拿到之后一律交给 `ledger` 落同一份盘，不各自再存一份。
#[derive(Default, Clone, Debug)]
pub struct ResourceView {
    /// 剩余积分合计（与 [`sum_credits`] 同口径）
    pub credits: Option<f64>,
    /// 还有余量的包里最早的重置/过期时间（毫秒）
    pub earliest_expiry_ms: Option<i64>,
    /// 逐包明细；响应里解析不到任何包时为空
    pub packages: Vec<PkgView>,
}

/// 从 `get-user-resource` 响应里解析每个资源包的**本周期**授予 / 已用。
///
/// **键必须用 `ResourceId`，不能用 `PackageCode`** —— 这是实测出来的：
/// 同一批「CodeBuddy个人版国内运营裂变包」共用一个 `PackageCode`
/// （`TCACA_code_007_nzdH5h4Nl0` 一次响应里对应 28 个彼此独立的包），
/// 拿它当键会把这些包全部合并成一条，于是每天签到新发的那 100 积分
/// 只有第一天能被算成「新增」，之后永远为 0。
/// 实测 29 个包中 `ResourceId` 两两不同，`PackageCode + CycleStartTime` 也能区分；
/// 这里前者优先、后者兜底，两者都没有就不记账（宁可漏，也不要错账）。
///
/// ⚠️ **数值必须取 `Cycle*` 那一组，不能用 `Capacity*`** —— 后者接口根本不维护：
/// 在账号确实在服务流量的 120 秒里间隔采样，`CapacityUsedPrecise` 增量是 `+0.0000`
/// （恒定等于 0），而 `CycleCapacityUsedPrecise` 增量是 `+1.4400`。
/// 曾经判反过一次，因为**包用满时两组数值天然相等**（`Used == Size`），
/// 于是「两组实测取值相同」被误读成「同义，取累计的那个」，代价是台账增量恒 0、
/// 积分简报一条都固化不出来。别凭一次静态观测换回 `Capacity*`，`Capacity*` 只留作兜底。
fn parse_packages(body: &Value) -> Vec<PkgView> {
    let Some(accounts) = resource_accounts(body) else {
        return Vec::new();
    };
    accounts
        .iter()
        .filter_map(|a| {
            let size = num_field(
                a,
                &[
                    "CycleCapacitySizePrecise",
                    "CycleCapacitySize",
                    "CapacitySizePrecise",
                    "CapacitySize",
                ],
            )?;
            // 缺 `used` 按 0 处理：刚发的包本来就还没被用过
            let used = num_field(
                a,
                &[
                    "CycleCapacityUsedPrecise",
                    "CycleCapacityUsed",
                    "CapacityUsedPrecise",
                    "CapacityUsed",
                ],
            )
            .unwrap_or(0.0);
            let cycle_start = a
                .get("CycleStartTime")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let resource = a
                .get("ResourceId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let code = a
                .get("PackageCode")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = if !resource.is_empty() {
                resource.to_string()
            } else if !code.is_empty() {
                format!("{code}@{cycle_start}")
            } else {
                return None;
            };
            Some(PkgView {
                key,
                name: a
                    .get("PackageName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                size,
                used,
                cycle_start,
            })
        })
        .collect()
}

/// 一次拉取「剩余积分 + 最早过期时间 + 逐包明细」（同一份 `get-user-resource` 响应）。
///
/// 抽出来是为了让「刷新 / 签到 / 每日结算」三条路径**共用同一次请求**：
/// 台账需要的逐包明细就附在同一份响应里，多解析几个字段而已，不额外打接口。
///
/// `checkin-status` 的兜底也收在这里 —— 只有一个地方定义「拿不到积分时怎么办」，
/// 否则换了调用路径就会悄悄丢掉兜底（`refresh_all` 正是这么换过来的）。
pub async fn fetch_resource_view(
    client: &reqwest::Client,
    host: &str,
    token: &str,
) -> ResourceView {
    let Ok(resp) = client
        .post(format!("{}{}", host, RESOURCE_PATH))
        .bearer_auth(token)
        .json(&resource_body())
        .timeout(Duration::from_secs(15))
        .send()
        .await
    else {
        return ResourceView::default();
    };
    let Ok(body) = resp.json::<Value>().await else {
        return ResourceView::default();
    };
    let mut view = ResourceView {
        credits: sum_credits(&body),
        earliest_expiry_ms: earliest_cycle_end(&body),
        packages: parse_packages(&body),
    };
    if view.credits.is_none() {
        // 兜底：checkin-status 的累计积分（没有过期时间概念）
        if let Ok(resp) = client
            .post(format!("{}{}", host, STATUS_PATH))
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            if let Ok(body) = resp.json::<Value>().await {
                view.credits = body.get("data").and_then(extract_total_credits);
            }
        }
    }
    view
}

/// 对一个账号执行签到：遍历候选 host，命中明确结果即返回。
pub async fn do_checkin(account: &Account, default_base: &str) -> CheckinRecord {
    let hosts = candidate_hosts(&account.token, account.base_url.as_deref(), default_base);
    let client = crate::http::api_client();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut last_unknown: Option<String> = None;

    // 已尝试过的候选数：换候选之前让一拍。正常情况首个候选即命中，这个等待根本不会
    // 发生；只有接口漂移、程序开始枚举 host × path 时才生效——「固定顺序 + 零间隔」的
    // 端点枚举正是扫描器签名，而多试几次本身很正常
    let mut probes = 0usize;
    for host in &hosts {
        for path in CHECKIN_PATHS {
            if probes > 0 {
                crate::http::probe_gap().await;
            }
            probes += 1;
            let url = format!("{}{}", host, path);
            match post_checkin(&client, &url, &account.token).await {
                Ok(rec) if rec.success || rec.already || rec.inactive => {
                    let mut r = rec;
                    r.at = now.clone();
                    r.host = Some(host.clone());
                    // 签到响应不带余额，单独查一次（best-effort，失败就留空）
                    if r.balance.is_none() {
                        if let Some(b) = fetch_credits(&client, host, &account.token).await {
                            r.balance = Some(b);
                        }
                    }
                    return r;
                }
                Ok(rec) => {
                    // 返回了但无法判定为成功/已签/活动未开，记录文案并尝试下一个 host
                    last_unknown = Some(rec.message.clone());
                }
                Err(e) => {
                    // 网络/超时/连接层错误也要记录，而不是静默跳过，便于排查
                    last_unknown = Some(format!("{} 请求失败: {}", url, e));
                }
            }
        }
    }

    CheckinRecord {
        success: false,
        already: false,
        inactive: false,
        message: last_unknown
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "所有候选 host 均请求失败".to_string()),
        code: None,
        credit: None,
        balance: None,
        streak: None,
        host: None,
        at: now,
    }
}

/// 只读查询「今日是否已签到」：主用 `checkin-activity-status`(v2)，退 `checkin-status`(v1)。
///
/// 零副作用——只发查询、绝不打签到接口，用于列表展示的真实状态。
///
/// 注意：官方 `today_checked_in` 偶发不可靠（签到成功后仍可能为 `false`，clawhub 实测），
/// 所以这只是「软」状态；真正权威的「今日已签」来自 `daily-checkin` 返回 `code=10001`
/// （见 `do_checkin` / `classify`）。前端会把「今天真实点过签到」的结果优先于此。
pub async fn query_checked_today(account: &Account, default_base: &str) -> Option<bool> {
    let hosts = candidate_hosts(&account.token, account.base_url.as_deref(), default_base);
    let client = crate::http::api_client();
    // 候选路径按「最可能命中」排序；任一能解析出 today_checked_in 即返回，不必穷尽。
    let paths = [
        "/v2/billing/meter/checkin-activity-status",
        "/billing/meter/checkin-status",
        "/v2/billing/meter/checkin-status",
    ];
    // 同 do_checkin：候选之间的间隔只为消灭「零间隔枚举」的形态，命中即停不受影响
    let mut probes = 0usize;
    for host in &hosts {
        for path in paths {
            if probes > 0 {
                crate::http::probe_gap().await;
            }
            probes += 1;
            let url = format!("{}{}", host, path);
            let Ok(resp) = client
                .post(&url)
                .bearer_auth(&account.token)
                .json(&serde_json::json!({}))
                .timeout(Duration::from_secs(10))
                .send()
                .await
            else {
                continue;
            };
            let Ok(body) = resp.json::<Value>().await else {
                continue;
            };
            if let Some(b) = parse_checked_today(&body) {
                return Some(b);
            }
        }
    }
    None
}

/// 从 `checkin-status` / `checkin-activity-status` 响应里解析「今日是否已签到」。
///
/// 优先读显式布尔字段（兼容 `today_checked_in` / `today_checked` / `is_checked_today` /
/// `checked_today` / `signed_today` / `checked` / `has_checked` 等多种命名，也接受字符串
/// 形态的 `"true"` / `"false"`）；
/// 退路：响应里若有「最近签到日期」且等于今天 → 已签，含日期但不是今天 → 未签。
fn parse_checked_today(body: &Value) -> Option<bool> {
    let d = body.get("data")?;
    const BOOL_KEYS: &[&str] = &[
        "today_checked_in",
        "today_checked",
        "is_checked_today",
        "checked_today",
        "signed_today",
        "today_checkin",
        "checked",
        "has_checked",
    ];
    for k in BOOL_KEYS {
        if let Some(v) = d.get(*k) {
            if let Some(b) = v.as_bool() {
                return Some(b);
            }
            if let Some(s) = v.as_str() {
                if let Ok(b) = s.trim().parse::<bool>() {
                    return Some(b);
                }
            }
        }
    }
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    const DATE_KEYS: &[&str] = &[
        "last_checkin_date",
        "last_checkin_time",
        "last_time",
        "last_checkin_at",
        "checkin_date",
        "last_checkin",
    ];
    for k in DATE_KEYS {
        if let Some(s) = d.get(*k).and_then(Value::as_str) {
            let s = s.trim();
            if s.starts_with(today.as_str()) {
                return Some(true);
            }
            // 含 10 位日期且不是今天 → 今天没签（明确 false，别留 None 让前端误判）
            if s.len() >= 10 && s[..10].chars().all(|c| c.is_ascii_digit() || c == '-') {
                return Some(false);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account(remain: &[(&str, serde_json::Value)]) -> Value {
        let accounts: Vec<Value> = remain
            .iter()
            .map(|(code, v)| json!({"PackageCode": code, "CycleCapacityRemainPrecise": v}))
            .collect();
        json!({"code": 0, "data": {"Response": {"Data": {"Accounts": accounts}}}})
    }

    #[test]
    fn sums_remaining_credits_across_packages() {
        let body = account(&[("a", json!("500")), ("b", json!("805.14000097"))]);
        // 小数按两位四舍五入，与官方前端展示一致
        assert_eq!(sum_credits(&body), Some(1305.14));
    }

    #[test]
    fn prefers_cycle_remaining_over_capacity_remaining() {
        // 月度包用完后 CapacityRemain 仍是满额 500，只有 CycleCapacityRemain 归零
        let body = json!({"data": {"Response": {"Data": {"Accounts": [
            {"CycleCapacityRemainPrecise": "0", "CapacityRemainPrecise": "500"}
        ]}}}});
        assert_eq!(sum_credits(&body), Some(0.0));
    }

    /// 端到端回归：**真实响应形状**（`Capacity*` 恒 0、`Cycle*` 在动）走完
    /// 「解析 → 台账 → 固化」三步，必须产出一条**有数**的时条目。
    ///
    /// 这条覆盖的正是「积分简报一条都出不来」的完整链路：任何一环重新读回
    /// `Capacity*`（或把增量按 `max(0)` 抹掉），这里都会立刻红。
    #[test]
    fn a_realistic_payload_ends_up_as_a_sealed_hour_entry() {
        // 同一个包，两次采样之间又消耗了 3.44：只有 Cycle* 那一组会变
        let payload = |cyc_used: &str, cyc_remain: &str| {
            json!({"data": {"Response": {"Data": {"Accounts": [
                {"PackageCode": "TCACA_code_008_x", "ResourceId": "codebuddy-k750126j5pbrt6f",
                 "PackageName": "CodeBuddy个人体验版",
                 "CapacitySizePrecise": "500", "CapacityUsedPrecise": "0", "CapacityRemainPrecise": "500",
                 "CycleCapacitySizePrecise": "500", "CycleCapacityUsedPrecise": cyc_used,
                 "CycleCapacityRemainPrecise": cyc_remain,
                 "CycleStartTime": "2026-09-01 00:00:00"}
            ]}}}})
        };
        let accounts = [Account {
            id: "a1".into(),
            name: "甲".into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            last: None,
            checked_today: None,
        }];

        let mut led = crate::ledger::Ledger::default();
        for (body, at) in [
            (payload("257.36999973", "242.63000027"), "2026-09-16 15:00:00"),
            (payload("260.80999973", "239.19000027"), "2026-09-16 15:55:00"),
        ] {
            crate::ledger::observe(
                led.accts.entry("a1".into()).or_default(),
                &parse_packages(&body),
                sum_credits(&body),
                None,
                at,
                crate::ledger::Mode::Normal,
            );
        }

        // 1) 台账里攒出了 15 点这一小时的桶，且就是那 3.44
        let hours = led.accts["a1"]
            .hours_used
            .get("2026-09-16")
            .copied()
            .unwrap_or([0.0; 24]);
        assert_eq!(
            crate::ledger::round2(hours[15]),
            3.44,
            "增量没记进桶 ⇒ 简报必然还是空的"
        );

        // 2) 16:00 固化时必须产出一条有数的时条目（余额读数与条目同小时 ⇒ 一并带上）
        let e = crate::briefing::build_hour(&led.accts, &accounts, "2026-09-16", 15, "2026-09-16 16:00:00")
            .expect("这一小时有动静，必须固化成条目");
        assert_eq!(e.consumed, 3.44);
        assert_eq!(e.accounts.len(), 1);
        assert_eq!(e.accounts[0].consumed, 3.44);
        assert_eq!(e.balance, Some(239.19), "读数落在同一小时里，应当带上");
    }

    #[test]
    fn returns_none_when_no_package_has_a_number() {
        assert_eq!(sum_credits(&json!({"data": {"Response": {"Data": {"Accounts": []}}}})), None);
        assert_eq!(sum_credits(&json!({"data": {"Response": {"Data": {"Accounts": [
            {"CycleCapacityRemain": null}
        ]}}}})), None);
        // 缺 data 也不能 panic
        assert_eq!(sum_credits(&json!({"code": 0})), None);
    }

    #[test]
    fn status_fallback_reads_total_credits() {
        assert_eq!(extract_total_credits(&json!({"total_credits": 12})), Some(12.0));
        assert_eq!(extract_total_credits(&json!({"total_credit": "3.5"})), Some(3.5));
        assert_eq!(extract_total_credits(&json!({})), None);
    }

    /// 回归：同一批「运营裂变包」**共用一个 PackageCode**（实测 28 个包同码），
    /// 逐包明细只能靠 `ResourceId` 区分。曾打算用 PackageCode 当键 —— 那样这些包会
    /// 被合并成一条，每天签到新发的 100 积分只有第一天算得上「新增」，之后永远是 0。
    #[test]
    fn packages_are_keyed_by_resource_id_not_the_shared_package_code() {
        let body = json!({"data": {"Response": {"Data": {"Accounts": [
            {"PackageCode": "TCACA_code_007_x", "ResourceId": "r-1",
             "PackageName": "国内运营裂变包", "CapacitySizePrecise": "100",
             "CapacityUsedPrecise": "100", "CycleStartTime": "2026-08-20 15:04:14"},
            {"PackageCode": "TCACA_code_007_x", "ResourceId": "r-2",
             "PackageName": "国内运营裂变包", "CapacitySizePrecise": "100",
             "CapacityUsedPrecise": "0", "CycleStartTime": "2026-08-21 09:30:09"}
        ]}}}});
        let pkgs = parse_packages(&body);
        assert_eq!(pkgs.len(), 2, "同 PackageCode 的两个包必须各自成条：{pkgs:?}");
        assert_ne!(pkgs[0].key, pkgs[1].key);
        assert_eq!(pkgs[0].cycle_start, "2026-08-20 15:04:14");
        assert_eq!((pkgs[0].size, pkgs[0].used), (100.0, 100.0));
        assert_eq!(pkgs[0].name, "国内运营裂变包");
    }

    /// 回归：数值必须取 `Cycle*` 那一组。
    ///
    /// **实测**只有这组是活的：同一账号在确实服务流量的 120 秒里，
    /// `CapacityUsedPrecise` 增量 `+0.0000`，`CycleCapacityUsedPrecise` 增量 `+1.4400`。
    /// 而 `Capacity*` 组恒定等于 `0 / size / size`（接口不维护它）。
    ///
    /// 曾经取的是 `Capacity*`，症状不是报错，而是**简报一条都出不来**：
    /// 每次采样算出的增量都是 0，小时桶永远是空的，固化自然无候选。
    /// 之所以会被误判，是因为包用满时两组数值天然相等（`Used == Size`），
    /// 「实测取值相同」被读成了「同义」。这条断言钉住字段组的选择。
    #[test]
    fn package_numbers_come_from_the_cycle_family_not_the_dead_capacity_family() {
        let body = json!({"data": {"Response": {"Data": {"Accounts": [
            {"PackageCode": "c", "ResourceId": "r-1", "PackageName": "CodeBuddy个人体验版",
             "CapacitySizePrecise": "500", "CapacityUsedPrecise": "0",
             "CapacityRemainPrecise": "500",
             "CycleCapacitySizePrecise": "500", "CycleCapacityUsedPrecise": "257.36999973",
             "CycleCapacityRemainPrecise": "242.63000027",
             "CycleStartTime": "2026-09-01 00:00:00"}
        ]}}}});
        let pkgs = parse_packages(&body);
        assert_eq!(pkgs.len(), 1);
        assert_eq!((pkgs[0].size, pkgs[0].used), (500.0, 257.36999973));
        assert_ne!(pkgs[0].used, 0.0, "读成 Capacity* 就会恒为 0，台账永远算不出增量");
    }

    #[test]
    fn package_parsing_survives_missing_or_coerced_fields() {
        // 没有 ResourceId 时退化到 PackageCode + 周期起点
        let fallback = json!({"data": {"accounts": [
            {"PackageCode": "c", "CapacitySize": 50, "CycleStartTime": "d1"}
        ]}});
        let pkgs = parse_packages(&fallback);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].key, "c@d1");
        assert_eq!(pkgs[0].size, 50.0);
        // 缺 used → 0（刚发的包还没被用过）
        assert_eq!(pkgs[0].used, 0.0);
        // 缺 size → 没有授予量可记，跳过该包
        assert!(parse_packages(&json!({"data": {"accounts": [{"ResourceId": "r"}]}})).is_empty());
        // 既无 ResourceId 也无 PackageCode → 不记账（宁可漏，不要错账）
        assert!(parse_packages(&json!({"data": {"accounts": [{"CapacitySize": 1}]}})).is_empty());
        // 完全没有 data 也不能 panic
        assert!(parse_packages(&json!({"code": 0})).is_empty());
    }

    #[test]
    fn earliest_expiry_ignores_empty_packages_and_picks_min() {
        let body = json!({"data": {"Response": {"Data": {"Accounts": [
            // 已用完：不参与
            {"CycleCapacityRemainPrecise": "0", "CycleEndTime": "2026-09-09 16:54:23"},
            // 还有余量：两者取更早的
            {"CycleCapacityRemainPrecise": "100", "CycleEndTime": "2026-09-21 09:30:08"},
            {"CycleCapacityRemainPrecise": "805", "CycleEndTime": "2026-09-20 23:48:22"},
            // 没有 CycleEndTime：跳过
            {"CycleCapacityRemainPrecise": "50"}
        ]}}}});
        let want = parse_cycle_end("2026-09-20 23:48:22").unwrap();
        assert_eq!(earliest_cycle_end(&body), Some(want));
        // 全部用完 → None
        let empty = json!({"data": {"Response": {"Data": {"Accounts": [
            {"CycleCapacityRemainPrecise": "0", "CycleEndTime": "2026-09-09 16:54:23"}
        ]}}}});
        assert_eq!(earliest_cycle_end(&empty), None);
        assert_eq!(earliest_cycle_end(&json!({"code": 0})), None);
    }

    #[test]
    fn cycle_end_parse_rejects_garbage() {
        assert!(parse_cycle_end("2026-09-20 23:48:22").is_some());
        assert_eq!(parse_cycle_end(""), None);
        assert_eq!(parse_cycle_end("not-a-date"), None);
        assert_eq!(parse_cycle_end("2026-09-20"), None);
    }

    /// 真实接口冒烟：确认剩余积分接口仍能解析出数值（端点是外部契约，改版会静默失效）。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_credits_endpoint() {
        let list = crate::auth_file::discover_local_accounts();
        let a = list.first().expect("本机应存在 WorkBuddy 登录信息");
        let host = a
            .host
            .clone()
            .unwrap_or_else(|| "https://www.workbuddy.cn".to_string());
        let client = crate::http::api_client();
        let credits = fetch_credits(&client, &host, &a.token).await;
        println!("[{host}] 剩余积分={credits:?}");
        assert!(credits.is_some(), "应能解析出剩余积分");
    }
}
