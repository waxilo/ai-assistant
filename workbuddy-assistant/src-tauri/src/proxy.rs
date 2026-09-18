//! 本地反代（默认 `127.0.0.1:8787`）：**WorkBuddy 专用**的智能接管通道。
//!
//! 开关只有一个：开启 = 监听本机端口 + 把 WorkBuddy 的端点指向这里；
//! 关闭 = 停止监听 + 摘掉端点。装卸与安全由 `stealth` 模块负责。
//!
//! 因为只服务本机的 WorkBuddy（监听 `127.0.0.1`，来源只可能是本机进程），
//! **没有鉴权 Key**——被接管的 WorkBuddy 也不会带任何额外请求头。
//!
//! 路由逻辑：
//!
//! 1. **会话粘滞**：带 `x-conversation-id` 的请求复用上次选中的账号 —— 一次对话中途
//!    换账号会丢上下文，必须粘住。新会话（粘滞过期或首次）才重新选。
//! 2. **选账号**：谁的「还有余量的资源包」最早过期就用谁——把快过期的积分先消耗掉；
//!    查不到过期时间的账号排最后，剩余积分为 0 的账号直接跳过（除非全员为 0）。
//! 3. **限流无感切换**：免费模型（从网关 `/v2/enterprises/personal/models` 动态拉取
//!    积分倍率，倍率为 0 即免费；1h 缓存，失败兜底 hy3）触发限流（429）时，把
//!    **「该账号 × 该模型」**冷却到上游给出的重置时刻，换下一个账号重发同一请求
//!    （上限 2 次切换）；冷却中的「账号 × 模型」在选号时优先跳过。付费模型的 429
//!    原样透传。冷却的粒度与时间节点见 `RateKey` / `limit_until_ms`。
//! 4. **续签兜底**：选中的账号若凭证临近过期（<48h）会先自动续签。
//! 5. **转发**：路径与查询串原样保留，替换 `Authorization` 为选中账号的 token，
//!    去掉逐跳头（Host / Content-Length 等）后透传其余请求头。
//!
//! **响应一律用 chunked 流式下发。** 对话是 SSE（`text/event-stream`），实测若缓冲成
//! 一次性 body，CLI 会报 `Empty stream` 并拿不到任何输出。
//!
//! **accept 出来的连接必须显式复位成阻塞模式**：监听 socket 为了轮询配置开关必须
//! 非阻塞，而 Windows 会把这个非阻塞状态**传染**给 accept 出来的连接（Linux 不会）。
//! 不复位时，只要请求字节还没到齐就被判成「请求非法」→ 400 bad request。详见 `configure_conn`。
//!
//! 实现：`std::net::TcpListener` 手写 HTTP/1.1 解析（本机自用足够），
//! 上游请求用现有 async reqwest + `block_on`。监督线程每 150ms 轮询一次设置，
//! 关闭开关或改端口即自动解绑/重绑，无需重启应用。

use crate::accounts;
use crate::checkin::fetch_resource_view;
use crate::commands;
use crate::ledger;
use crate::stealth;
use chrono;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// WorkBuddy 对话请求带的会话标识，用作粘滞键
pub const CONV_HEADER: &str = "X-Conversation-Id";
/// 积分快照缓存时长：路由决策不必每次都打资源接口
const SNAPSHOT_TTL: Duration = Duration::from_secs(600);
/// 同一会话多久没新请求就释放粘滞（换回按积分重新选）
const STICKY_TTL: Duration = Duration::from_secs(30 * 60);
/// accept 空轮询间隔（非阻塞监听）：它直接等于「请求到达 → 被 accept」的额外延迟
const ACCEPT_POLL: Duration = Duration::from_millis(20);
/// 配置（接管开关 / 端口）轮询间隔。比 accept 轮询慢得多：每轮 accept 都去读一次
/// settings.json 纯属磁盘浪费，而开关变更晚 0.5s 生效完全无感
const CONFIG_POLL: Duration = Duration::from_millis(500);
/// 客户端请求头读取时限（读空闲）：连上了却迟迟不发完整请求就放弃。
/// 注意它只在**阻塞** socket 上生效（`SO_RCVTIMEO` 对非阻塞 socket 无效）——见 `configure_conn`
const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 下游写超时：SSE 长对话可能持续数分钟，写超时要给得足够宽
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(600);
/// 上游连接超时（建连阶段）
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 上游单次读空闲超时：SSE 长对话会持续数分钟，**不能用总超时**——
/// reqwest 的 `timeout()` 覆盖整个响应体读取，会掐断活着的流；
/// `read_timeout()` 只管「多久没收到新数据」，才是流的正确保护方式
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 请求头 / 请求体上限
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .read_timeout(UPSTREAM_IDLE_TIMEOUT)
        // 跟随重定向会把「路由拼错」的 302 变成一页 HTML（CLI 端只见空流，无从诊断）。
        // 不跟：错误的 302 原样回到 CLI 和路由日志，一眼可见。
        .redirect(reqwest::redirect::Policy::none())
        // 官方域直连可达；若继承 shell 的 HTTP_PROXY 会把上游请求发去无关代理
        .no_proxy()
        // 转发用的 UA 与账号接口一致：**不给上游注入额外身份头**（那些头由 CLI 自己带），
        // 只保证「同一个应用发出的请求，UA 不因路径不同而变」
        .user_agent(crate::http::UA)
        .build()
        .expect("构建 HTTP 客户端失败")
});

/// 接管内部「主动查询」用的账号接口客户端：与签到 / 刷新**同一套身份**，并强制直连。
///
/// 不能用 [`CLIENT`]：那个是**透传**用的，只统一 UA、**绝不向请求注入身份头**
/// （CLI 自己带的头必须原样过去）。而拉模型清单（`fetch_models_value`）与路由决策时
/// 重拉积分快照（`choose_account`）是我们自己主动打腾讯的计费接口，需要完整的账号接口
/// 头条——否则同一个接口会在「代理主动查」与「签到 / 刷新」两条路径上收到两套不同的头，
/// 正是这个项目一直在消除的那种不一致。
static API_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(crate::http::api_client_direct);

/// 会话粘滞键：**会话 × 模型**。
///
/// 带上模型是为了跟限流冷却的粒度对齐（见 `RateKey`）：粘滞的意义是「别在同一次对话
/// 中途换账号」，而换号可能是被「某个模型的限流」逼出来的——辅助小模型（如 0 积分的
/// hy3）吃 429 换了号，不该把主模型的后续请求也一起搬走。各模型各自粘，互不牵连。
/// 模型未知（非对话请求）用空串占位，等价于原来按会话粘。
fn sticky_key(conv: &str, model: Option<&str>) -> (String, String) {
    (conv.to_string(), model.unwrap_or_default().to_string())
}

/// 会话粘滞：(会话, 模型) → (最后命中时刻, 账号 id)
fn sticky() -> &'static Mutex<HashMap<(String, String), (Instant, String)>> {
    static STICKY: OnceLock<Mutex<HashMap<(String, String), (Instant, String)>>> = OnceLock::new();
    STICKY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 同一次客户端请求里，最多换几个账号重试（首次 + 2 次切换）
const FAILOVER_MAX_TRIES: usize = 3;

/// 上游 429 没给重置时刻时的兜底冷却时长。
/// 实测网关总会给（见 `limit_until_ms`），这条只防上游改文案格式。
const RATE_LIMIT_FALLBACK: Duration = Duration::from_secs(10 * 60);
/// 冷却时长下限：解析出的时刻若已过去（时钟偏差、文案写的是历史时间）也要真冷却一小会儿，
/// 否则下一个请求立刻撞回同一个 429
const RATE_LIMIT_MIN: Duration = Duration::from_secs(30);
/// 冷却时长上限：解析结果再离谱也不能把账号锁死超过一天
const RATE_LIMIT_MAX: Duration = Duration::from_secs(24 * 60 * 60);

/// 限流冷却的存储键：**账号 × 模型**。
///
/// 键必须带模型。实测（2026-09-14）：同一账号 `hy3` 吃 429 的同一时刻，主模型
/// `deepseek-v4.1-flash` 依然 200，上游文案也明说「您也可以切换其他模型继续使用」——
/// 限流本来就是按「账号 × 模型」算的。按账号整体冷却会把它本来还能服务的模型一起赶走，
/// 白白浪费一个额度充足的账号。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct RateKey {
    account_id: String,
    model: String,
}

/// 时间节点的来源：决定日志怎么写、要不要提示「上游没给」
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LimitSource {
    /// 响应体文案里的「将在 … 重置」（实测网关走这条）
    Body,
    /// `Retry-After` 响应头
    RetryAfter,
    /// `X-RateLimit-Reset` 一类响应头
    ResetHeader,
    /// 上游没给 → 兜底时长
    Fallback,
}

impl LimitSource {
    /// 这个时刻是不是上游真给的（否则是兜底算的，日志要说明）
    fn from_upstream(self) -> bool {
        self != LimitSource::Fallback
    }
}

/// 一条冷却记录
#[derive(Clone, Copy, Debug)]
struct CooldownEntry {
    /// 解禁时刻（unix 毫秒）
    until_ms: i64,
    /// 该时刻的来源
    source: LimitSource,
}

/// 限流冷却表：(账号 × 模型) → 解禁时刻
fn cooldown() -> &'static Mutex<HashMap<RateKey, CooldownEntry>> {
    static COOLDOWN: OnceLock<Mutex<HashMap<RateKey, CooldownEntry>>> = OnceLock::new();
    COOLDOWN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 当前墙钟毫秒。冷却用**墙钟**而非 `Instant`：上游给的是绝对时刻，
/// 只有存绝对时刻才能把「几点解禁 / 还剩多久」原样展示出来。
fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// 「账号 × 模型」是否仍在限流窗口内。顺手清掉已过期的记录，免得长年常驻攒垃圾。
///
/// 模型未知（非对话请求）时无从匹配，一律按「未冷却」——限流本来就是按模型算的。
fn cooling(account_id: &str, model: Option<&str>) -> bool {
    let Some(model) = model else { return false };
    let Ok(mut m) = cooldown().lock() else {
        return false;
    };
    let now = now_ms();
    m.retain(|_, e| e.until_ms > now);
    m.contains_key(&RateKey {
        account_id: account_id.to_string(),
        model: model.to_string(),
    })
}

/// 记一条冷却。解禁时刻先夹到 `[now+MIN, now+MAX]`：时钟偏差与离谱文案既不会把
/// 账号锁死一天以上，也不会让冷却等于没锁。
fn set_cooldown(
    account_id: &str,
    model: &str,
    until_ms: i64,
    source: LimitSource,
) -> CooldownEntry {
    let now = now_ms();
    let entry = CooldownEntry {
        until_ms: until_ms.clamp(
            now + RATE_LIMIT_MIN.as_millis() as i64,
            now + RATE_LIMIT_MAX.as_millis() as i64,
        ),
        source,
    };
    if let Ok(mut m) = cooldown().lock() {
        m.insert(
            RateKey {
                account_id: account_id.to_string(),
                model: model.to_string(),
            },
            entry,
        );
    }
    entry
}

/// 日志里那半句「冷却 …」：带解禁时刻与来源的可读说明
fn cooldown_note(entry: CooldownEntry) -> String {
    let mins = ((entry.until_ms - now_ms()).max(0) as f64 / 60_000.0).ceil() as i64;
    if entry.source.from_upstream() {
        format!("至 {}（约 {mins} 分钟）", until_text(entry.until_ms))
    } else {
        format!("（上游未给重置时刻，按兜底 {mins} 分钟）")
    }
}

/// 解禁时刻的展示串：当天只给 `HH:MM`，跨天才补日期
fn until_text(until_ms: i64) -> String {
    let Some(t) = chrono::DateTime::from_timestamp_millis(until_ms) else {
        return "未知".into();
    };
    let local = t.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m-%d %H:%M").to_string()
    }
}

/// 解析 429 里的「重置时刻」，返回 (unix 毫秒, 来源)。
///
/// # 上游实测长什么样（2026-09-14，`Server: APISIX/3.9.1`）
///
/// 429 **不带任何 `Retry-After` 头**，时间节点只写在响应体的中文文案里：
///
/// ```text
/// {"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-14 19:35:25 UTC+8 重置，
///  您也可以切换其他模型继续使用。","requestId":"…"}
/// ```
///
/// 同一分钟内连发两次探测，返回的重置时刻**逐字相同**——是个绝对时刻，不是
/// 「now + N 秒」的滚动值，所以可以直接当封禁到期时间去比对。解析顺序：
///
/// 1. `Retry-After`（秒数或 HTTP-date）
/// 2. `X-RateLimit-Reset-After` / `X-RateLimit-Reset`（秒差 / unix 秒 / unix 毫秒）
/// 3. 响应体文案里的 `YYYY-MM-DD HH:MM:SS`（可带 `UTC+8` / `+08:00` 偏移，缺省按本机时区）
///
/// 都读不到返回 `None`，由调用方落到兜底时长（日志会写明「上游未给」）。
fn limit_until_ms(
    headers: &[(String, String)],
    body: &[u8],
    now: i64,
) -> Option<(i64, LimitSource)> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    if let Some(ms) = header("retry-after").and_then(|v| parse_retry_after(v, now)) {
        return Some((ms, LimitSource::RetryAfter));
    }
    for name in ["x-ratelimit-reset-after", "x-ratelimit-reset"] {
        if let Some(ms) = header(name).and_then(|v| parse_reset_header(v, now)) {
            return Some((ms, LimitSource::ResetHeader));
        }
    }
    parse_reset_text(&String::from_utf8_lossy(body)).map(|ms| (ms, LimitSource::Body))
}

/// `Retry-After`：RFC 允许「秒数」或「HTTP-date」两种写法
fn parse_retry_after(v: &str, now: i64) -> Option<i64> {
    if let Ok(secs) = v.parse::<i64>() {
        return (secs >= 0).then(|| now + secs * 1000);
    }
    chrono::DateTime::parse_from_rfc2822(v)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// `X-RateLimit-Reset` 系：各网关语义不统一（秒差 / unix 秒 / unix 毫秒），
/// 按数量级判——> 1e12 当毫秒、> 1e9 当 unix 秒、否则当「还剩多少秒」。
fn parse_reset_header(v: &str, now: i64) -> Option<i64> {
    let n: f64 = v.trim().parse().ok()?;
    if !(n > 0.0) {
        return None;
    }
    Some(if n > 1e12 {
        n as i64
    } else if n > 1e9 {
        (n * 1000.0) as i64
    } else {
        now + (n * 1000.0) as i64
    })
}

/// 文案里的时间戳本体 `YYYY-MM-DD HH:MM:SS`
static RESET_AT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2}):(\d{2})").unwrap()
});
/// 紧跟在时间戳之后的时区偏移：`UTC+8` / `GMT-05:00` / `+08:00`。
/// 匹配不上就按本机时区解释（网关文案给的是 `UTC+8`，与国内机器一致）。
static RESET_TZ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:UTC|GMT)?\s*([+-])(\d{1,2})(?::?(\d{2}))?").unwrap());

/// 解析响应体文案里的重置时刻
fn parse_reset_text(text: &str) -> Option<i64> {
    let caps = RESET_AT_RE.captures(text)?;
    let num = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    let naive = chrono::NaiveDate::from_ymd_opt(num(1)? as i32, num(2)?, num(3)?)?
        .and_hms_opt(num(4)?, num(5)?, num(6)?)?;
    // 时间戳后面紧跟的偏移量（`UTC+8`），没写就是本机时区
    let offset_secs = caps
        .get(0)
        .and_then(|m| RESET_TZ_RE.captures(&text[m.end()..]))
        .and_then(|tz| {
            let sign = if &tz[1] == "-" { -1 } else { 1 };
            let h: i64 = tz[2].parse().ok()?;
            let m: i64 = tz.get(3).map_or(Some(0), |x| x.as_str().parse().ok())?;
            Some(sign * (h * 3600 + m * 60))
        });
    match offset_secs {
        // 文案自带偏移：先按 UTC 解释，再减掉偏移得到绝对时刻
        Some(off) => Some(naive.and_utc().timestamp_millis() - off * 1000),
        None => naive
            .and_local_timezone(chrono::Local)
            .single()
            .map(|d| d.timestamp_millis()),
    }
}

/// 候选软过滤（纯函数，便于单测）：优先剔除冷却中的账号；
/// 若剔完为空（全员都在冷却）则原样返回——让上游裁决也比代理直接 503 有信息量。
fn available_candidates<T: Clone>(candidates: &[T], is_cooling: impl Fn(&T) -> bool) -> Vec<T> {
    let usable: Vec<T> = candidates.iter().filter(|a| !is_cooling(a)).cloned().collect();
    if usable.is_empty() {
        candidates.to_vec()
    } else {
        usable
    }
}

/// 路由排序键：最早过期者优先 → 查不到过期时间的靠后 → 剩余积分多者略优先。
///
/// 入参直接用台账的 [`ledger::CreditFact`]：路由的排序依据与账户展示的
/// 「剩余 / 过期时间」是**同一个类型、同一份来源**，不再各立一份画像结构。
fn score(info: &ledger::CreditFact) -> (i64, i64) {
    (
        info.earliest_expiry_ms.unwrap_or(i64::MAX),
        -(info.credits.unwrap_or(0.0) * 100.0) as i64,
    )
}

/// 从候选里选出该用的账号下标。`infos` 与 `ids` 一一对应。
fn pick_index(ids: &[String], infos: &[ledger::CreditFact]) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score: Option<(i64, i64)> = None;
    for (i, info) in infos.iter().enumerate() {
        // 剩余积分为 0 的账号直接跳过（除非全员为 0 / 全未知）
        if info.credits == Some(0.0) {
            continue;
        }
        let s = score(info);
        if best_score.map_or(true, |cur| s < cur) {
            best_score = Some(s);
            best = Some(i);
        }
    }
    // 全员为 0 时退化为取第一个（让上游自己报错，比代理直接 503 更有信息量）
    best.or(if ids.is_empty() { None } else { Some(0) })
}

/// 监督线程：按设置启停 / 换端口重绑，并负责接管端点的装卸与心跳。
///
/// 必须先成功监听，再安装端点。否则端口被占时会把 WorkBuddy 指向无人监听的地址。
pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        if let (Ok(dir), Some(home)) = (commands::try_data_dir(&app), dirs::home_dir()) {
            stealth::sweep(&home, &dir);
        }

        let mut installed_port: Option<u16> = None;
        let mut disabled_cleaned = false;
        loop {
            let Ok(dir) = commands::try_data_dir(&app) else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let settings = accounts::load_settings(&dir);
            if !settings.proxy_enabled {
                if !disabled_cleaned || installed_port.take().is_some() {
                    if let Some(home) = dirs::home_dir() {
                        if let Err(e) = stealth::uninstall(&home, &dir) {
                            eprintln!("[proxy] 摘除接管端点失败：{e}");
                        }
                    }
                    disabled_cleaned = true;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            disabled_cleaned = false;
            let port = settings.proxy_port;
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => {
                    let Some(home) = dirs::home_dir() else {
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    };
                    if let Err(e) = stealth::install(&home, &dir, port) {
                        eprintln!("[proxy] 安装接管端点失败：{e}");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    installed_port = Some(port);
                    // 监听必须非阻塞，才能在 accept 之余顺带轮询配置；
                    // 但 accept 出来的连接会被 Windows 传染非阻塞，必须逐连接复位——见 configure_conn
                    let _ = listener.set_nonblocking(true);
                    let mut last_beat = Instant::now();
                    let mut last_cfg = Instant::now();
                    loop {
                        if last_cfg.elapsed() >= CONFIG_POLL {
                            let current = accounts::load_settings(&dir);
                            if !current.proxy_enabled || current.proxy_port != port {
                                break;
                            }
                            last_cfg = Instant::now();
                        }
                        if last_beat.elapsed() >= stealth::HEARTBEAT_INTERVAL {
                            stealth::heartbeat(&dir, port);
                            last_beat = Instant::now();
                        }
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let app2 = app.clone();
                                std::thread::spawn(move || handle_conn(stream, app2));
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(ACCEPT_POLL);
                            }
                            Err(_) => std::thread::sleep(ACCEPT_POLL),
                        }
                    }
                }
                Err(e) => {
                    if installed_port.take().is_some() {
                        if let Some(home) = dirs::home_dir() {
                            let _ = stealth::uninstall(&home, &dir);
                        }
                    }
                    eprintln!("[proxy] 无法监听 127.0.0.1:{port}：{e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// HTTP 解析（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// 解析出的请求头部分
#[derive(Debug, PartialEq)]
struct Request {
    method: String,
    /// 含查询串的路径，如 `/v2/xxx?a=1`
    target: String,
    /// 全部请求头（名字保留原样，值 trim 过）
    headers: Vec<(String, String)>,
    /// 正文字节数（按 Content-Length）
    body_len: usize,
    /// 请求头（含 `\r\n\r\n`）之后的起始偏移
    head_end: usize,
}

/// 从缓冲里解析请求行 + 请求头。返回 None 表示数据不完整或非法。
fn parse_request(buf: &[u8]) -> Option<Request> {
    let end = find_subslice(buf, b"\r\n\r\n")?;
    if end + 4 > MAX_HEAD + 4 {
        return None;
    }
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    if parts.next().is_none() {
        return None;
    }

    let mut headers = Vec::new();
    let mut body_len = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("content-length") {
            body_len = value.parse().unwrap_or(0);
        }
        headers.push((name.trim().to_string(), value));
    }
    Some(Request {
        method,
        target,
        headers,
        body_len,
        head_end: end + 4,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn header_value<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 不该透传给上游的请求头
fn hop_by_hop(name: &str) -> bool {
    [
        "host",
        "authorization",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "accept-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

// ---------------------------------------------------------------------------
// 连接处理
// ---------------------------------------------------------------------------

/// 把 accept 出来的连接复位成「阻塞 + 超时」模式。
///
/// # 为什么必须显式复位（不是洁癖，是线上事故）
///
/// 监听 socket 为了能在 accept 之余轮询配置开关，必须是**非阻塞**的；而
/// **Windows 上 `accept()` 返回的 socket 会继承监听 socket 的非阻塞状态**
/// （Linux 不继承，所以这个坑在 Linux 上永远测不出来）。于是每个连接天生非阻塞：
///
/// - 只要第一次 `read()` 时请求字节还没到齐，就立刻返回 `WouldBlock`（raw os error 10035），
///   被读循环当成「请求非法」→ 回 400 bad request。触发完全取决于客户端**先连后发**的时序：
///   连上就发 = 正常；隔 200ms 再发 = 稳定 400。Node/undici 把请求头与请求体分两次 write、
///   长 body 分多个 TCP 段到达，都正好落在这个窗口里。
/// - 顺带 `SO_RCVTIMEO` 对非阻塞 socket 无效，`set_read_timeout` 形同虚设 ——
///   slow-header 熔断实际并不存在。
///
/// 复位成阻塞后，`read()` 会老实等到数据到达或读超时，两个问题一起消失。
fn configure_conn(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(HEAD_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
}

/// 读请求的三种结局。
///
/// 必须分开：「收到 0 字节」「请求语法错」「连接一直没发数据」是完全不同的病，
/// 混成一个 `None` 正是这次 400 事故查不出原因的直接原因。
enum HeadRead {
    /// 完整拿到一个请求（连带原始缓冲，供后续切出 body）
    Ready(Request, Vec<u8>),
    /// 请求读完之前对端就断开 / 读失败
    Broken(Vec<u8>),
    /// 读空闲超时；或 socket 仍是非阻塞（一读就 `WouldBlock`）
    Stalled(Vec<u8>, std::io::ErrorKind),
}

/// 读请求头 + 正文，返回已收到的字节供调用方落盘诊断。
fn read_head(stream: &mut TcpStream) -> HeadRead {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return HeadRead::Broken(buf), // 对端关闭
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(req) = parse_request(&buf) {
                    if buf.len() >= req.head_end() + req.body_len {
                        return HeadRead::Ready(req, buf);
                    }
                }
                if buf.len() > MAX_HEAD + MAX_BODY {
                    return HeadRead::Broken(buf);
                }
            }
            // 阻塞模式下的读超时：Windows 报 `TimedOut`，Linux 报 `WouldBlock`（EAGAIN）。
            // 两者语义相同（这个读窗口内没有新数据），合并处理，免得平台差异再引出误判。
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return HeadRead::Stalled(buf, e.kind())
            }
            Err(_) => return HeadRead::Broken(buf),
        }
    }
}

/// 诊断用：把已收到的字节截成可读前缀（最多 512 字节）
fn head_prefix(buf: &[u8]) -> String {
    String::from_utf8_lossy(&buf[..buf.len().min(512)]).to_string()
}

fn handle_conn(mut stream: TcpStream, app: tauri::AppHandle) {
    // 数据目录：选账号 / 写接管日志都要用；取不到直接 500，不再读请求
    let Ok(dir) = commands::try_data_dir(&app) else {
        respond(&mut stream, 500, "text/plain", b"internal error", &[]);
        return;
    };

    // 0. 先复位成阻塞再读。漏掉这一步的代价见 `configure_conn` 的文档。
    if let Err(e) = configure_conn(&stream) {
        let _ = stealth::journal_append(&dir, "proxy_conn_setup_failed", &format!("{e}"));
        return;
    }

    // 1. 读完请求头（+ body）
    let (req, buf) = match read_head(&mut stream) {
        HeadRead::Ready(req, buf) => (req, buf),
        HeadRead::Broken(buf) => {
            // 读到一半断开。线上最常见的是 **0 字节**：客户端连上但还没发出请求就断开——
            // 这绝不等于「请求非法」，所以把字节数写进日志，让这种事一眼可辨。
            let _ = stealth::journal_append(
                &dir,
                "proxy_bad_request",
                &format!(
                    "请求未读完或不合法，回 400（已收 {} 字节）：\n{}",
                    buf.len(),
                    head_prefix(&buf)
                ),
            );
            respond(&mut stream, 400, "text/plain", b"bad request", &[]);
            return;
        }
        HeadRead::Stalled(buf, kind) => {
            // 连上了但一直没把请求发完。与 400 严格分开：400 = 请求语法错，408 = 没等到请求。
            // 若这里出现 `WouldBlock` 而字节数为 0，说明连接没被复位成阻塞——就是本文件最上面那个坑。
            let _ = stealth::journal_append(
                &dir,
                "proxy_head_stalled",
                &format!(
                    "读请求卡住（{kind:?}），回 408（已收 {} 字节）：\n{}",
                    buf.len(),
                    head_prefix(&buf)
                ),
            );
            respond(&mut stream, 408, "text/plain", b"request timeout", &[]);
            return;
        }
    };

    // 2. 选账号：同一会话粘住同一个账号，新会话才按积分重新选
    //
    // 无鉴权：监听 127.0.0.1，来源只可能是本机进程（WorkBuddy 或调试用的 curl）。
    let body_start = req.head_end;
    let body = buf.get(body_start..body_start + req.body_len).unwrap_or(&[]);
    let conv = header_value(&req, CONV_HEADER)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let settings = accounts::load_settings(&dir);
    let host = settings.default_base_url;
    let bare = normalize_target(&req.target);
    let path = upstream_path(bare).to_string();
    let is_chat = bare == "/chat/completions";
    let model = if is_chat { body_model(body) } else { None };
    let url = upstream_url(&host, &path);

    // 3. 选账号并透传；0 积分免费模型限流（429）发生在流式输出开始前，响应头还没写给
    //    下游，正好有重试窗口：把「该账号 × 该模型」冷却到上游给出的重置时刻，换下一个
    //    账号重发同一请求，对 CLI 完全无感。付费模型的 429 与积分余额相关，原样透传不重试。
    //    重试次数有上限，用尽后 429 原样透传。
    //    设置里关掉「限流时切换备用账号」后，这条路径整个不生效：429 直接透传，
    //    该会话自始至终只用一个账号（见下面的 rate_limited 分支）。
    let mut ban: Vec<String> = Vec::new();
    loop {
        let Some(account) = tauri::async_runtime::block_on(choose_account(
            &dir,
            conv.as_deref(),
            &ban,
            model.as_deref(),
        )) else {
            respond(&mut stream, 503, "text/plain", b"no account available", &[]);
            return;
        };
        if ban.is_empty() && is_chat {
            // 内部证据事件：仅供网络救急判定「端点被用过」，时间线展示层会过滤。
            stealth::journal_append(
                &dir,
                "proxy_request",
                &format!(
                    "长驻 CLI host 已使用接管端点（扣费账号：{}，模型：{}）",
                    account.name,
                    model.as_deref().unwrap_or("未知")
                ),
            );
        }

        // 透传
        let upstream = tauri::async_runtime::block_on(async {
            let mut r = CLIENT
                .request(
                    reqwest::Method::from_bytes(req.method.as_bytes())
                        .unwrap_or(reqwest::Method::GET),
                    &url,
                )
                .bearer_auth(&account.token);
            for (k, v) in &req.headers {
                if !hop_by_hop(k) {
                    r = r.header(k.as_str(), v.as_str());
                }
            }
            if !body.is_empty() {
                r = r.body(body.to_vec());
            }
            r.send().await
        });

        // 免费模型集（动态拉取、1h 缓存；拿不到用内置兜底）
        let free_set = if is_chat {
            ensure_free_models(&host, &account.token)
        } else {
            HashSet::new()
        };

        match upstream {
            Ok(resp) => {
                // 是不是「该走限流无感切换」的响应：会话内聊天 + 命中启用切换的模型 + 上游 429。
                // 先算一次，下面两条路径共用——防御优先那条也得先认出这是限流。
                let rate_limited = is_chat
                    && resp.status() == 429
                    && is_rate_limited_model(
                        model.as_deref(),
                        &free_set,
                        &settings.rate_limit_models,
                    );

                // 防御优先（`failover_on_rate_limit = false`）：429 原样透传，并且**连冷却都不记**。
                // 记了冷却就等于放行「本会话的下一个请求换到别的账号」——那正是要避免的
                // 「同一个会话出现两个凭证」。代价是这个会话要等上游自己解除限流。
                if rate_limited && !settings.failover_on_rate_limit {
                    let limited = tauri::async_runtime::block_on(read_rate_limited(resp));
                    stealth::journal_append(
                        &dir,
                        "failover",
                        &format!(
                            "账号「{}」的模型「{}」触发限流（429）：已按「会话内不换号」原样透传，该会话不会被切到其它账号",
                            account.name,
                            model.as_deref().unwrap_or("未知")
                        ),
                    );
                    forward_rate_limited(&mut stream, &limited, &account, &host);
                    return;
                }

                if rate_limited {
                    // 重置时刻只写在响应体的文案里（网关不给 Retry-After），所以必须把体整个
                    // 读下来。代价是这段响应没法再流式透传：没有备用账号那条路径要自己把字节写回下游。
                    let limited = tauri::async_runtime::block_on(read_rate_limited(resp));
                    let (until_ms, source) = limit_until_ms(&limited.headers, &limited.body, now_ms())
                        .unwrap_or((
                            now_ms() + RATE_LIMIT_FALLBACK.as_millis() as i64,
                            LimitSource::Fallback,
                        ));
                    let model_name = model.as_deref().unwrap_or("未知");
                    // 封禁粒度 = 账号 × 模型：上游就是这么算的，这个账号的**其它模型**照用
                    let note = cooldown_note(set_cooldown(&account.id, model_name, until_ms, source));
                    // 粘滞不必解绑：冷却表已保证该「账号 × 模型」在有效期内不会被选中；
                    // 而粘滞按「会话 × 模型」分开记，这次冷却不会牵连同会话的其它模型。
                    if ban.len() + 1 < FAILOVER_MAX_TRIES {
                        ban.push(account.id.clone());
                        stealth::journal_append(
                            &dir,
                            "failover",
                            &format!(
                                "账号「{}」的模型「{model_name}」触发限流（429），该账号 × 该模型冷却{note}，已无感切换备用账号继续服务",
                                account.name
                            ),
                        );
                        continue;
                    }
                    stealth::journal_append(
                        &dir,
                        "failover",
                        &format!(
                            "账号「{}」的模型「{model_name}」触发限流（429），该账号 × 该模型冷却{note}，已无更多备用账号，限流响应原样透传",
                            account.name
                        ),
                    );
                    forward_rate_limited(&mut stream, &limited, &account, &host);
                    return;
                }

                stream_response(&mut stream, resp, &dir, &account, &host, &path);
                return;
            }
            Err(e) => {
                let msg = format!("upstream error: {e}");
                stealth::journal_append(&dir, "proxy_upstream_error", &msg);
                respond(&mut stream, 502, "text/plain", msg.as_bytes(), &[]);
                return;
            }
        }
    }
}

fn upstream_url(host: &str, path: &str) -> String {
    format!("{}{}", host.trim_end_matches('/'), path)
}

/// 请求目标可能是相对路径 `/chat/completions`，也可能是代理风格的绝对 URL。
/// 上游只认相对路径，这里统一剥掉协议与主机部分。
fn normalize_target(target: &str) -> &str {
    match target.find("://") {
        Some(i) => {
            let rest = &target[i + 3..];
            match rest.find('/') {
                Some(p) => &rest[p..],
                None => "/",
            }
        }
        None => target,
    }
}

/// 补上 CLI 在端点覆盖模式下丢掉的 `/v2` 前缀。
///
/// # 为什么必须由代理来补
///
/// CLI 直连官方网关时，请求的是 `https://copilot.tencent.com/v2/chat/completions`
/// （`/v2` 由 CLI 自己拼上）；而一旦设置 `CODEBUDDY_BASE_URL`，它请求的路径就变成
/// 裸的 `/chat/completions`。网关上这个裸路径**不存在**——APISIX 会 302 跳到官网，
/// CLI 收到一页 HTML、解析出 0 个 SSE 数据事件，报
/// `Empty stream: upstream gateway sent only placeholder chunks (chunks=0, bytes=0)`。
/// 所以转发前必须改写成网关真实路由 `/v2/chat/completions`。
fn upstream_path(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = path.strip_prefix("/chat/completions") {
        // 精确匹配裸路径（可带查询串），避免误伤 /chat/completions-foo 之类的路径
        if rest.is_empty() || rest.starts_with('?') {
            return format!("/v2/chat/completions{rest}").into();
        }
    }
    path.into()
}

/// 从对话请求体里取 `model` 字段（解析失败返回 None，不阻断转发）。
fn body_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str().map(str::to_string)))
}

/// 免费模型判定：不再写死模型名，从网关 `GET /v2/enterprises/personal/models`
/// 动态拉取每个模型的积分倍率（`credits` 字段，如 "x0.00 credits"/"x0.05"），
/// 倍率为 0 即免费。缓存 1 小时；拉取失败退回内置兜底（官方目录里 hy3 为
/// x0.00，而 hy3-x 是 x0.05 **不免费**——所以绝不能用 `hy3` 前缀匹配）。
const FREE_MODELS_TTL: Duration = Duration::from_secs(3600);
const FALLBACK_FREE_MODELS: [&str; 1] = ["hy3"];

/// 解析倍率字符串："x0.00 credits" / "x0.05" / "x0.79 credits" → 数字
fn parse_multiplier(s: &str) -> Option<f64> {
    s.trim()
        .trim_start_matches('x')
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// 从 models 接口响应里提取免费模型 id 集（纯函数，便于单测）
fn free_ids_from_value(v: &serde_json::Value) -> Option<HashSet<String>> {
    let arr = v.get("data")?.get("models")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                let mult = m.get("credits")?.as_str().and_then(parse_multiplier)?;
                (mult == 0.0).then_some(id)
            })
            .collect(),
    )
}

/// 免费模型集缓存：(拉取成功时刻, 模型 id 集)
fn free_models_cache() -> &'static Mutex<Option<(Instant, HashSet<String>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, HashSet<String>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 惰性获取免费模型集：缓存有效直接用；过期则用当前账号 token 拉一次；
/// 拉取失败用内置兜底（并保留旧缓存，避免每次请求都重试打接口）。
fn ensure_free_models(host: &str, token: &str) -> HashSet<String> {
    if let Ok(guard) = free_models_cache().lock() {
        if let Some((at, set)) = guard.as_ref() {
            if at.elapsed() < FREE_MODELS_TTL {
                return set.clone();
            }
        }
    }
    let fresh = tauri::async_runtime::block_on(fetch_free_models(host, token));
    match fresh {
        Some(set) if !set.is_empty() => {
            if let Ok(mut g) = free_models_cache().lock() {
                *g = Some((Instant::now(), set.clone()));
            }
            set
        }
        _ => FALLBACK_FREE_MODELS.iter().map(|s| s.to_string()).collect(),
    }
}

/// 免费判定：精确匹配动态集合
fn is_free_model(model: Option<&str>, free: &HashSet<String>) -> bool {
    model.is_some_and(|m| free.contains(m))
}

/// 限流无感切换是否对该模型生效：免费模型（动态集合）恒生效，外加用户在设置里
/// 勾选的付费模型。`rate_limit_models` 来自 `Settings`，0 积分模型无需勾选即自动覆盖。
fn is_rate_limited_model(
    model: Option<&str>,
    free: &HashSet<String>,
    enabled: &[String],
) -> bool {
    is_free_model(model, free) || model.is_some_and(|m| enabled.iter().any(|e| e == m))
}

/// 单个模型的描述（供 UI 勾选限流切换范围）
#[derive(serde::Serialize, Clone)]
pub struct ModelInfo {
    /// 模型 id（如 hy3 / hy3-x / deepseek-v3 …）
    pub id: String,
    /// 是否 0 积分免费模型（恒生效、UI 锁定勾选）
    pub free: bool,
    /// 积分倍率原始串（如 "x0.00" / "x0.05"），仅展示用
    pub multiplier: String,
}

/// 「限流切换」支持的模型（供接管页勾选 + 手动刷新）
#[derive(serde::Serialize)]
pub struct FreeModelsReport {
    /// 全模型列表（含免费与付费），免费排前、其余按 id 排序
    pub models: Vec<ModelInfo>,
    /// "fetched" = 刚从网关拉取；"cache" = 1 小时缓存内；"fallback" = 拉取失败用内置兜底
    pub source: String,
}

/// 拉取整份 models 接口响应（一次 HTTP，路由用的免费集合与 UI 用的全模型都从它派生）
async fn fetch_models_value(host: &str, token: &str) -> Option<serde_json::Value> {
    let url = format!(
        "{}/v2/enterprises/personal/models",
        host.trim_end_matches('/')
    );
    let resp = API_CLIENT.get(&url).bearer_auth(token).send().await.ok()?;
    resp.json().await.ok()
}

/// 免费模型集（动态拉取、1h 缓存；拿不到用内置兜底）：供路由判定限流切换
async fn fetch_free_models(host: &str, token: &str) -> Option<HashSet<String>> {
    fetch_models_value(host, token).await.and_then(|v| free_ids_from_value(&v))
}

/// 全模型描述列表（免费排前、其余按 id 排序），供 UI 勾选限流切换范围
fn model_info_from_value(v: &serde_json::Value) -> Vec<ModelInfo> {
    let Some(arr) = v
        .get("data")
        .and_then(|d| d.get("models"))
        .and_then(|m| m.as_array())
    else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let multiplier = m
                .get("credits")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let free = parse_multiplier(&multiplier).is_some_and(|x| x == 0.0);
            Some(ModelInfo {
                id,
                free,
                multiplier,
            })
        })
        .collect();
    out.sort_by(|a, b| match (a.free, b.free) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.id.cmp(&b.id),
    });
    out
}

/// UI 用的全模型缓存：(拉取成功时刻, 模型列表)
fn all_models_cache() -> &'static Mutex<Option<(Instant, Vec<ModelInfo>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<ModelInfo>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 免费模型列表（限流切换的生效范围）：优先读全模型缓存；`refresh=true` 或缓存过期时
/// 用任一账号的 token 从网关重新拉取（倍率 x0.00 的模型恒生效，付费模型需用户勾选）。
/// 接管页勾选 + 手动刷新。同时顺手刷新路由用的免费集合缓存。
#[tauri::command]
pub async fn free_models(
    app: tauri::AppHandle,
    refresh: Option<bool>,
) -> Result<FreeModelsReport, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    if !refresh.unwrap_or(false) {
        if let Ok(guard) = all_models_cache().lock() {
            if let Some((at, list)) = guard.as_ref() {
                if at.elapsed() < FREE_MODELS_TTL {
                    return Ok(FreeModelsReport {
                        models: list.clone(),
                        source: "cache".into(),
                    });
                }
            }
        }
    }
    let settings = accounts::load_settings(&dir);
    // 拉模型列表也要先整池同步：这一路要拿一个真 token 去请求，而绑了池之后
    // 本地那份 token 可能早被别的机器轮换掉了（用旧的会直接 401）。
    crate::commands::sync_pool_if_bound(&dir).await;
    let mut account = accounts::load_accounts(&dir)
        .into_iter()
        .find(|a| !a.token.is_empty())
        .ok_or_else(|| "暂无账号，无法拉取模型列表".to_string())?;
    // token 临近过期就先续签（单机路径；不落盘也无妨：落盘版只在路由时做，
    // 这里仅求拉取成功）
    let _ = commands::ensure_fresh_token(&mut account).await;
    match fetch_models_value(&settings.default_base_url, &account.token).await {
        Some(v) => {
            let list = model_info_from_value(&v);
            // 同步刷新路由用的免费集合缓存（倍率 x0.00 的 id）
            if let Some(set) = free_ids_from_value(&v) {
                if let Ok(mut g) = free_models_cache().lock() {
                    *g = Some((Instant::now(), set));
                }
            }
            if let Ok(mut g) = all_models_cache().lock() {
                *g = Some((Instant::now(), list.clone()));
            }
            Ok(FreeModelsReport {
                models: list,
                source: "fetched".into(),
            })
        }
        _ => Ok(FreeModelsReport {
            models: FALLBACK_FREE_MODELS
                .iter()
                .map(|s| ModelInfo {
                    id: s.to_string(),
                    free: true,
                    multiplier: "x0.00".into(),
                })
                .collect(),
            source: "fallback".into(),
        }),
    }
}

/// 不该回给客户端的响应头：逐跳头、reqwest 已代劳解压后失效的，
/// 以及**由 `write_head` 用同一份值显式写出的** `content-type`——
/// 放行它只会在下游多出一个重复头。
fn response_hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "content-length",
        "transfer-encoding",
        "content-encoding",
        "content-type",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// 状态行里的原因短语。只列本代理会发出的状态码，其余兜底 OK
/// （客户端的判断依据是数字，短语纯粹给人看的）。
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// 写响应头。**一律用 chunked** —— 下游（CLI / 桌面端）按 SSE 解析，
/// 缓冲成一次性 body 会让它报 `Empty stream` 并丢掉全部输出。
fn write_head(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let reason = reason_phrase(status);
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Transfer-Encoding: chunked\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n"
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// 写一个 chunk 并**立刻 flush**：SSE 的实时性全靠这个
fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    stream.write_all(format!("{:X}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// 终止 chunked 流
fn write_chunk_end(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// 边收边转：上游出一个 chunk 就往下游写一个。
///
/// 中途出错只能断开 —— chunked 没有「出错补报」机制，但对端看到流被截断
/// 至少比拿到一个空响应要好。
fn stream_response(
    stream: &mut TcpStream,
    mut resp: reqwest::Response,
    dir: &std::path::Path,
    account: &accounts::Account,
    host: &str,
    path: &str,
) {
    let status = resp.status().as_u16();
    // 诊断用：上游返回 4xx/5xx 时落盘，便于区分「代理自己回的 400」与「上游 400 透传」
    if status >= 400 {
        let _ = stealth::journal_append(
            dir,
            "proxy_upstream_status",
            &format!("上游返回 {status}：{host}{path}"),
        );
    }
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let mut headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !response_hop_by_hop(k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    // 只回 ASCII：账号名可能是中文，直接放进响应头会破坏报文
    headers.push(("X-Proxy-Account-Id".into(), account.id.clone()));
    headers.push(("X-Proxy-Host".into(), host.to_string()));

    if write_head(stream, status, &ctype, &headers).is_err() {
        return;
    }

    let mut bytes = 0usize;
    let mut read_error: Option<String> = None;
    tauri::async_runtime::block_on(async {
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if write_chunk(stream, &chunk).is_err() {
                        break; // 下游断了，不是上游的错
                    }
                    bytes += chunk.len();
                }
                Ok(None) => break,
                // 上游读失败绝不能静默：吞掉的话 CLI 只会看到一个「干净」的空流，
                // 报 Empty stream 却查不到原因。这里留痕到接管日志。
                Err(e) => {
                    read_error = Some(format!("上游读流失败（已转发 {bytes} 字节）：{e}"));
                    break;
                }
            }
        }
    });
    if let Some(msg) = read_error {
        stealth::journal_append(dir, "proxy_stream_error", &format!("[{path}] {msg}"));
    }
    let _ = write_chunk_end(stream);
}

/// 读下来的上游 429：状态 / 内容类型 / 响应头 / 完整体。
///
/// 为什么非得整个读下来：重置时刻只写在 body 文案里（网关连 `Retry-After` 都不给）。
/// 而一旦读走，这段响应就无法再流式透传——转发路径要自己把字节写回下游，
/// 见 `forward_rate_limited`。
struct RateLimited {
    status: u16,
    ctype: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 把上游 429 收全（体很小，网关只回一段错误 JSON）
async fn read_rate_limited(resp: reqwest::Response) -> RateLimited {
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !response_hop_by_hop(k.as_str()))
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    RateLimited {
        status,
        ctype,
        headers,
        body,
    }
}

/// 把读下来的 429 原样写回下游（替代流式透传）。
///
/// 用 `Content-Length` 而不是 chunked，与网关自己发 429 的形态一致——
/// 一段错误 JSON 不必套成 SSE 帧，客户端也少一层解析。
fn forward_rate_limited(
    stream: &mut TcpStream,
    rl: &RateLimited,
    account: &accounts::Account,
    host: &str,
) {
    let mut extra: Vec<(&str, &str)> = rl
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    // 与 stream_response 保持一致：让客户端看得出这次是谁接的
    extra.push(("X-Proxy-Account-Id", &account.id));
    extra.push(("X-Proxy-Host", host));
    respond(stream, rl.status, &rl.ctype, &rl.body, &extra);
}

impl Request {
    /// 头结束（含 `\r\n\r\n`）之后的起始偏移
    fn head_end(&self) -> usize {
        self.head_end
    }
}

/// 粘滞是否命中：命中返回账号 id，顺手清掉过期项。
fn sticky_hit(conv: &str, model: Option<&str>) -> Option<String> {
    let mut map = sticky().lock().ok()?;
    map.retain(|_, (at, _)| at.elapsed() < STICKY_TTL);
    let (at, id) = map.get_mut(&sticky_key(conv, model))?;
    *at = Instant::now();
    Some(id.clone())
}

/// 写入/刷新会话粘滞。返回 true 表示该「会话 × 模型」**换到了新账号**
/// （首次上代理或被切换），调用方据此写「开始使用账号」事件；同一组合的后续
/// 请求返回 false，不刷屏。
fn sticky_put(conv: &str, model: Option<&str>, account_id: String) -> bool {
    if let Ok(mut map) = sticky().lock() {
        let key = sticky_key(conv, model);
        let changed = map.get(&key).map(|(_, id)| id != &account_id).unwrap_or(true);
        map.insert(key, (Instant::now(), account_id));
        return changed;
    }
    false
}

/// 台账里的积分读数是否过期（决定新会话要不要重新打资源接口）。
///
/// 判据是读数时刻（`credits.at`）—— 读数本身存在积分台账里，与账户管理页显示的是同一个数。
fn snapshot_stale(at: Option<&str>) -> bool {
    let Some(at) = at else { return true };
    match chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|t| t.and_local_timezone(chrono::Local).single())
    {
        Some(t) => {
            chrono::Local::now().timestamp() - t.timestamp() >= SNAPSHOT_TTL.as_secs() as i64
        }
        None => true,
    }
}

/// 扣费候选集过滤（纯函数，便于单测）：设置里勾了谁，谁才有资格被扣费。
///
/// - 勾选列表为空 = 不限制，全部账号都可作为备选；
/// - 勾选的 id 在账号列表里一个都找不到（比如账号已删光）→ 退回全部，
///   宁可多扣也不能让接管直接瘫掉；用户在界面上能看到「备选为空」的提示。
fn billing_candidates(accounts: &[crate::accounts::Account], selected: &[String]) -> Vec<crate::accounts::Account> {
    if selected.is_empty() {
        return accounts.to_vec();
    }
    let picked: Vec<_> = accounts
        .iter()
        .filter(|a| selected.iter().any(|s| s == &a.id))
        .cloned()
        .collect();
    if picked.is_empty() {
        accounts.to_vec()
    } else {
        picked
    }
}

/// 选出一个该用的账号。
///
/// `model` 是本轮请求要用的模型：限流冷却（和因此产生的粘滞）都是按
/// **账号 × 模型** 算的，选号必须知道模型是谁。
///
/// 候选集 = 设置里勾选的扣费账号（未勾选的不允许扣费；全不勾 = 全部可用），再做两层过滤：
/// - **禁用（严格）**：`ban` 里的账号是本轮请求已试败的限流账号，直接剔除；剔完为空返回 None；
/// - **冷却（软）**：对本模型仍在限流窗口内的账号优先跳过，全员冷却则照常用。
///
/// 候选集内的优先级从高到低：
/// 1. **会话粘滞**——一次对话中途换账号会丢上下文；粘滞账号若已被移出候选集/在冷却则视为未命中；
/// 2. **智能轮换**——快照过期就重新拉，按「最旧积分」挑。
///
/// 会话首次落到某个账号（或被切换到新账号）时写一条 `route_start` 事件。
/// 单机模式下，选中的账号若触发了续签会就地保存账号列表；绑了池则不会 ——
/// 那一份凭证在上面的整池同步里已经落过盘了。
async fn choose_account(
    dir: &PathBuf,
    conv: Option<&str>,
    ban: &[String],
    model: Option<&str>,
) -> Option<crate::accounts::Account> {
    let settings = accounts::load_settings(dir);
    // ⚠️ 整池同步必须在**读账号之前**：它会把云端那一份并进 accounts.json，
    // 而闸带回来的才是最新凭证。顺序颠倒 = 整轮请求都在用已作废的 token。
    // 它是热路径，但节流（`broker::SYNC_TTL_MS`）让绝大多数调用只是读一下内存。
    crate::commands::sync_pool_if_bound(dir).await;
    let all = accounts::load_accounts(dir);
    if all.is_empty() {
        return None;
    }
    // 这里**不给账号配积分读数**：路由排序直接用台账本身（见下方 `store.fact_of`），
    // 在这儿再配一份投影就是第二个会漂移的口径 —— 而漂移正是这次要消掉的东西。
    let candidates = billing_candidates(&all, &settings.billing_account_ids);
    // 限流重试时已试败的账号严格剔除：再试一次只会再吃一个 429
    let accounts: Vec<_> = candidates
        .into_iter()
        .filter(|a| !ban.iter().any(|b| b == &a.id))
        .collect();
    if accounts.is_empty() {
        return None;
    }
    let usable = available_candidates(&accounts, |a| cooling(&a.id, model));

    // 1) 已在进行的会话：继续用同一个账号（除非它已被移出可用集）
    if let Some(conv) = conv {
        if let Some(id) = sticky_hit(conv, model) {
            if let Some(a) = usable.iter().find(|a| a.id == id) {
                return Some(a.clone());
            }
        }
    }

    // 2) 新会话：积分事实只看台账（账户管理页显示的就是它）。
    //    读数缺失/过期就用同一次拉取刷新 —— 拉回来的东西**写进台账**，
    //    不再往 accounts.json 里塞第二份会各自漂移的副本。
    let ids: Vec<String> = usable.iter().map(|a| a.id.clone()).collect();
    // 采集（打接口）在锁外，记账一次性完成 —— 理由见 `commands::fetch_samples`。
    // 这一步是「顺手补拉过期读数」：拉回来的东西写进唯一那本台账，
    // 不再往 accounts.json 里塞第二份会各自漂移的副本。
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    // 快照（含资源包过期时间）**统一从全局台账读**：账户展示的「剩余 / 过期时间」、
    // 简报页的「当前剩余」、这里的路由排序，问的都是同一份。
    // 不再看 `Account` 上那份投影副本 —— 两份读数一旦各自演化，就会回到
    // 「按旧快照路由、界面显示新数」的老问题。
    let store = ledger::store(dir);
    let snaps: Vec<Option<ledger::CreditFact>> =
        usable.iter().map(|a| store.fact_of(&a.id)).collect();
    let mut readings: Vec<ledger::Reading> = Vec::new();
    for (i, acct) in usable.iter().enumerate() {
        // 快照还新鲜就不打这一趟接口（`None` = 从没读到过，同样要拉）
        if !snapshot_stale(snaps[i].as_ref().map(|f| f.at.as_str())) {
            continue;
        }
        let host = commands::account_host(acct);
        let view = fetch_resource_view(&API_CLIENT, &host, &acct.token).await;
        readings.push(ledger::Reading {
            id: acct.id.clone(),
            packages: view.packages,
            credits: view.credits,
            expiry_ms: view.earliest_expiry_ms,
        });
    }
    if !readings.is_empty() {
        // 落盘失败只意味着「下次新会话还得再拉一次」，不影响本次路由 ——
        // 内存里的值是对的，下一次任意写入都会再落一遍。
        if let Err(e) = store.apply(&readings, &now, ledger::Mode::Normal) {
            eprintln!("积分台账落盘失败：{e}");
        }
    }
    // 排序依据一律取自**唯一那本台账**（刚补拉过的账号已经是新值）；
    // 从没读到过的账号用 `CreditFact::default()`（两栏都是 `None`），`pick_index`
    // 会把它排到最后。顺序必须与 `ids` 严格一一对应，所以不能 `filter_map` 掉。
    let infos: Vec<ledger::CreditFact> = usable
        .iter()
        .map(|a| store.fact_of(&a.id).unwrap_or_default())
        .collect();

    let idx = pick_index(&ids, &infos)?;
    let mut account = usable[idx].clone();
    // 单机模式下选中的账号临期就先续签（绑了池的恒为「没动」：那一份在上面整池同步好了）。
    // 失败不阻断，仍用旧 token 试
    if commands::ensure_fresh_token(&mut account).await.unwrap_or(false) {
        // 回填**全量**账号列表落盘：只存候选子集会把未勾选的账号从磁盘上删掉
        let mut merged = all;
        if let Some(a) = merged.iter_mut().find(|a| a.id == account.id) {
            *a = account.clone();
        }
        let _ = accounts::save_accounts(dir, &merged);
    }
    if let Some(conv) = conv {
        if sticky_put(conv, model, account.id.clone()) {
            // 该会话第一次走上代理，或被切到了新账号 —— 记一条「开始使用」事件
            stealth::journal_append(
                dir,
                "route_start",
                &format!(
                    "开始使用账号「{}」服务会话 {}（当前扣费备选 {} 个）",
                    account.name,
                    &conv.chars().take(8).collect::<String>(),
                    usable.len()
                ),
            );
        }
    }
    Some(account)
}

/// 写一个最简 HTTP 响应并关闭连接。
fn respond(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) {
    let reason = reason_phrase(status);
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head_and_body_length() {
        let raw = b"POST /v2/billing/meter/daily-checkin?a=1 HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nX-Request-Id: r1\r\n\r\n{}";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/v2/billing/meter/daily-checkin?a=1");
        assert_eq!(req.body_len, 2);
        assert_eq!(req.head_end, raw.len() - 2);
        assert_eq!(header_value(&req, "x-request-id"), Some("r1"));
        assert_eq!(header_value(&req, "X-REQUEST-ID"), Some("r1"));
    }

    #[test]
    fn rejects_incomplete_or_garbage_input() {
        assert_eq!(parse_request(b"GET /x HTTP/1.1\r\n"), None, "头未读完");
        assert_eq!(parse_request(b"garbage"), None);
        // 请求行必须有三段
        assert_eq!(parse_request(b"GET /x\r\n\r\n"), None);
    }

    #[test]
    fn hop_by_hop_filters_credentials_and_length() {
        assert!(hop_by_hop("Authorization"));
        assert!(hop_by_hop("content-length"));
        assert!(!hop_by_hop("Content-Type"));
        assert!(!hop_by_hop("Accept"));
    }

    #[test]
    fn model_requests_use_the_configured_gateway() {
        assert_eq!(
            upstream_url("https://copilot.tencent.com/", "/chat/completions"),
            "https://copilot.tencent.com/chat/completions"
        );
    }

    #[test]
    fn normalizes_absolute_and_relative_targets() {
        // WorkBuddy 实测发的是相对路径
        assert_eq!(normalize_target("/chat/completions"), "/chat/completions");
        // 代理风格（绝对 URL）要把协议与主机剥掉，只留路径 + 查询串
        assert_eq!(
            normalize_target("http://copilot.tencent.com/v2/x?a=1"),
            "/v2/x?a=1"
        );
        assert_eq!(normalize_target("https://host"), "/");
    }

    #[test]
    fn rewrites_bare_chat_path_to_v2() {
        // CLI 在端点覆盖模式下发的裸路径：必须补 /v2，否则网关 302 → CLI 报 Empty stream
        assert_eq!(upstream_path("/chat/completions"), "/v2/chat/completions");
        assert_eq!(
            upstream_path("/chat/completions?a=b"),
            "/v2/chat/completions?a=b"
        );
    }

    #[test]
    fn billing_candidates_restricts_to_selected_accounts() {
        let mk = |id: &str| crate::accounts::Account {
            id: id.into(),
            name: id.into(),
            phone: None,
            token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            checked_today: None,
            last: None,
        };
        let all = vec![mk("a"), mk("b"), mk("c")];
        // 未勾选 = 全部可用
        assert_eq!(billing_candidates(&all, &[]).len(), 3);
        // 勾了 b → 只有 b 有资格被扣费
        let picked = billing_candidates(&all, &["b".to_string()]);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, "b");
        // 勾选的账号全部不存在（如已被删除）→ 退回全部，接管不瘫
        assert_eq!(billing_candidates(&all, &["zzz".to_string()]).len(), 3);
    }

    #[test]
    fn available_candidates_skips_cooling_unless_all_cooling() {
        let mk = |id: &str| crate::accounts::Account {
            id: id.into(),
            name: id.into(),
            phone: None,
            token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            checked_today: None,
            last: None,
        };
        let all = vec![mk("a"), mk("b"), mk("c")];
        let ids = |v: &[crate::accounts::Account]| -> Vec<String> {
            v.iter().map(|a| a.id.clone()).collect()
        };
        // 无人冷却 → 原样返回
        assert_eq!(ids(&available_candidates(&all, |a| a.id == "x")).len(), 3);
        // b 在冷却 → 跳过 b
        let got = ids(&available_candidates(&all, |a| a.id == "b"));
        assert_eq!(got, vec!["a".to_string(), "c".to_string()]);
        // 全员冷却 → 软过滤退回全部：让上游裁决也比代理直接 503 有信息量
        assert_eq!(ids(&available_candidates(&all, |_| true)).len(), 3);
    }

    #[test]
    fn failover_only_for_free_models() {
        // 请求体缺 model / 非法 JSON → 视为未知，不触发切换
        assert_eq!(body_model(b"{}"), None);
        assert_eq!(body_model(b"not json"), None);
        assert_eq!(body_model(br#"{"model":"hy3"}"#).as_deref(), Some("hy3"));

        // 倍率解析：格式不统一（带/不带 "credits" 后缀）都要兼容
        assert_eq!(parse_multiplier("x0.00 credits"), Some(0.0));
        assert_eq!(parse_multiplier("x0.05"), Some(0.05));
        assert_eq!(parse_multiplier(" x0.79 credits "), Some(0.79));
        assert_eq!(parse_multiplier("credits"), None);

        // 从接口响应提取免费模型集：倍率 0 才算，缺 credits 字段的不算
        let sample = serde_json::json!({"data":{"models":[
            {"id":"hy3","credits":"x0.00 credits"},
            {"id":"hy3-x","credits":"x0.05"},
            {"id":"auto"},
            {"id":"glm-5.1","credits":"x0.79 credits"}
        ]}});
        let set = free_ids_from_value(&sample).unwrap();
        assert!(set.contains("hy3"));
        assert!(!set.contains("hy3-x"), "hy3-x 倍率 x0.05，不是免费模型");
        assert!(!set.contains("auto"));
        assert!(!set.contains("glm-5.1"));

        // 判定走精确匹配
        assert!(is_free_model(Some("hy3"), &set));
        assert!(!is_free_model(Some("hy3-x"), &set));
        assert!(!is_free_model(None, &set));
    }

    #[test]
    fn leaves_non_chat_paths_untouched() {
        // 已带 /v2 的、以及其它任何路径都不动
        assert_eq!(upstream_path("/v2/chat/completions"), "/v2/chat/completions");
        assert_eq!(upstream_path("/v1/models"), "/v1/models");
        assert_eq!(upstream_path("/"), "/");
        // 前缀相同但不是同一个路径，不能误伤
        assert_eq!(
            upstream_path("/chat/completions-extra"),
            "/chat/completions-extra"
        );
    }

    #[test]
    fn strips_hop_and_decompressed_headers_from_responses() {
        assert!(response_hop_by_hop("Content-Length"));
        assert!(response_hop_by_hop("transfer-encoding"));
        // reqwest 已代劳解压，这个头留着会让对端以为内容还是 gzip
        assert!(response_hop_by_hop("content-encoding"));
        // 内容类型由 write_head 用同一个值显式写出，放行它下游会出现重复头
        assert!(response_hop_by_hop("Content-Type"));
        assert!(!response_hop_by_hop("X-Request-Id"));
    }

    #[test]
    fn sticky_session_reuses_the_same_account() {
        // 一次对话中途换账号会丢上下文，必须粘住
        let conv = "conv-abc";
        let m = Some("hy3");
        assert!(sticky_hit(conv, m).is_none(), "首次访问不该命中");
        sticky_put(conv, m, "acct-1".into());
        assert_eq!(sticky_hit(conv, m).as_deref(), Some("acct-1"));
        sticky_put(conv, m, "acct-2".into());
        assert_eq!(
            sticky_hit(conv, m).as_deref(),
            Some("acct-2"),
            "同一会话被改写后应跟随最新值"
        );
        // 别的会话互不干扰
        assert!(sticky_hit("conv-other", m).is_none());
    }

    /// 回归：粘滞与限流冷却同为「账号 × 模型」粒度。以前的实现只有会话一个维度，
    /// 于是辅助小模型（0 积分的 hy3）吃一次 429 换号，就把整段对话连同主模型一起搬走。
    #[test]
    fn sticky_is_scoped_per_model() {
        let conv = "conv-multi";
        let main = Some("deepseek-v4.1-flash");
        let side = Some("hy3");
        sticky_put(conv, main, "acct-main".into());
        sticky_put(conv, side, "acct-side".into());
        assert_eq!(sticky_hit(conv, main).as_deref(), Some("acct-main"));
        assert_eq!(sticky_hit(conv, side).as_deref(), Some("acct-side"));
        // 非对话请求（模型未知）用空串占位，也是一个独立维度
        assert!(sticky_hit(conv, None).is_none());
        sticky_put(conv, None, "acct-other".into());
        assert_eq!(sticky_hit(conv, None).as_deref(), Some("acct-other"));
        assert_eq!(
            sticky_hit(conv, side).as_deref(),
            Some("acct-side"),
            "占位维度不该覆盖同一个会话里具体模型的粘滞"
        );
    }

    #[test]
    fn sticky_entry_expires_after_ttl() {
        let conv = "conv-expire";
        let m = Some("hy3");
        sticky_put(conv, m, "acct-1".into());
        // 把最后命中时刻拨回 TTL 之前
        if let Ok(mut map) = sticky().lock() {
            if let Some((at, _)) = map.get_mut(&sticky_key(conv, m)) {
                *at = Instant::now() - STICKY_TTL - Duration::from_secs(1);
            }
        }
        assert!(
            sticky_hit(conv, m).is_none(),
            "超过 TTL 的粘滞必须释放，好让新会话重新按积分选号"
        );
    }

    // ---- 限流冷却：重置时刻解析 + 「账号 × 模型」粒度 ----

    /// 线上真实抓到的 429 报文（2026-09-14 18:06，账号 waxiloao 用 hy3 触发，
    /// 两个免费探测都打到它）。网关 `Server: APISIX/3.9.1` **不给 `Retry-After` 头**，
    /// 重置时刻只写在这段中文文案里。
    const REAL_429_BODY: &str = r#"{"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-14 19:35:25 UTC+8 重置，您也可以切换其他模型继续使用。","requestId":"64bfbf60-0dcc-4480-8a41-6441ebe672c5"}"#;

    #[test]
    fn reads_reset_instant_out_of_the_gateway_message() {
        let (ms, source) = limit_until_ms(&[], REAL_429_BODY.as_bytes(), 0).unwrap();
        assert_eq!(source, LimitSource::Body);
        // 用固定 +08:00 还原，断言与文案逐字一致（不依赖跑测试的机器时区）
        let east8 = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let got = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&east8)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(got, "2026-09-14 19:35:25");

        // 文案给的是**绝对时刻**而不是「now + N 秒」：换一个 now 必须解出同一个瞬间，
        // 这正是「按时间节点封禁」能成立的前提
        assert_eq!(
            limit_until_ms(&[], REAL_429_BODY.as_bytes(), 1_700_000_000_000).map(|x| x.0),
            Some(ms)
        );

        // 文案不带时区偏移 → 按本机时区解释
        let (local_ms, _) = limit_until_ms(&[], "将在 2026-09-14 19:35:25 重置".as_bytes(), 0)
            .expect("无偏移的文案也该能解析");
        let local = chrono::DateTime::from_timestamp_millis(local_ms)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(local.format("%H:%M:%S").to_string(), "19:35:25");
    }

    #[test]
    fn reset_instant_falls_back_to_headers_then_gives_up() {
        let h = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
        // Retry-After：秒数
        assert_eq!(
            limit_until_ms(&h("Retry-After", "42"), b"", 1000),
            Some((1000 + 42_000, LimitSource::RetryAfter))
        );
        // Retry-After：HTTP-date
        let (ms, src) =
            limit_until_ms(&h("retry-after", "Mon, 14 Sep 2026 11:35:25 GMT"), b"", 0).unwrap();
        assert_eq!(src, LimitSource::RetryAfter);
        assert_eq!(
            chrono::DateTime::from_timestamp_millis(ms).unwrap().timestamp(),
            1_789_385_725
        );
        // X-RateLimit-Reset：unix 秒 / unix 毫秒 / 秒差 三种数量级都要认
        assert_eq!(
            limit_until_ms(&h("X-RateLimit-Reset", "1789385725"), b"", 0),
            Some((1_789_385_725_000, LimitSource::ResetHeader))
        );
        assert_eq!(
            limit_until_ms(&h("X-RateLimit-Reset", "1789385725000"), b"", 0),
            Some((1_789_385_725_000, LimitSource::ResetHeader))
        );
        assert_eq!(
            limit_until_ms(&h("X-RateLimit-Reset-After", "30"), b"", 1000),
            Some((31_000, LimitSource::ResetHeader))
        );
        // 头读不出来就落到 body
        assert_eq!(
            limit_until_ms(&h("Retry-After", "soon"), REAL_429_BODY.as_bytes(), 0).map(|x| x.1),
            Some(LimitSource::Body)
        );
        // 都不行就是 None：调用方退到兜底时长，宁可保守也不能瞎猜一个时刻
        assert!(limit_until_ms(&h("Retry-After", "-1"), b"{}", 0).is_none());
        assert!(limit_until_ms(&[], b"<html>429 Too Many Requests</html>", 0).is_none());
    }

    #[test]
    fn cooldown_is_scoped_to_account_times_model_and_clamped() {
        if let Ok(mut m) = cooldown().lock() {
            m.clear();
        }
        let now = now_ms();
        // 容差 2s：判定用的 now 一定不早于上面这个 now，夹取基准也会随之右移
        let max_allowed = now + RATE_LIMIT_MAX.as_millis() as i64 + 2000;
        let min_allowed = now + RATE_LIMIT_MIN.as_millis() as i64 - 2000;
        // 离谱地远（时钟偏差 / 文案写错）→ 夹到上限，不能把账号锁死
        assert!(
            set_cooldown("acct", "hy3", i64::MAX, LimitSource::Body).until_ms <= max_allowed
        );
        // 已经过去的时刻 → 至少保留下限，否则下一个请求立刻撞回同一个 429
        assert!(set_cooldown("acct2", "hy3", 0, LimitSource::Body).until_ms >= min_allowed);

        // 核心粒度：只封「这个账号 × 这个模型」
        set_cooldown("acct3", "hy3", now + 3_600_000, LimitSource::Body);
        assert!(cooling("acct3", Some("hy3")));
        assert!(
            !cooling("acct3", Some("deepseek-v4.1-flash")),
            "同账号的其它模型不该被牵连——实测 hy3 吃 429 时主模型依然 200"
        );
        assert!(!cooling("acct-other", Some("hy3")), "别的账号不受影响");
        assert!(!cooling("acct3", None), "模型未知（非对话请求）不参与冷却判定");

        // 过期记录顺手清掉，常年常驻不会攒垃圾
        if let Ok(mut m) = cooldown().lock() {
            m.clear();
            m.insert(
                RateKey {
                    account_id: "stale".into(),
                    model: "hy3".into(),
                },
                CooldownEntry {
                    until_ms: now_ms() - 1,
                    source: LimitSource::Body,
                },
            );
        }
        assert!(!cooling("stale", Some("hy3")));
        assert!(
            cooldown().lock().map(|m| m.is_empty()).unwrap_or(false),
            "过期条目应被顺手清理"
        );

        // 日志文案：上游给了要写解禁时刻，没给要写明是兜底
        let given = cooldown_note(set_cooldown("a", "m", now + 90 * 60_000, LimitSource::Body));
        assert!(given.contains("约 90 分钟"), "{given}");
        let fallback = cooldown_note(CooldownEntry {
            until_ms: now_ms() + RATE_LIMIT_FALLBACK.as_millis() as i64,
            source: LimitSource::Fallback,
        });
        assert!(fallback.contains("上游未给"), "{fallback}");
    }

    /// 回归：线上 400 事故的根因 —— 监听 socket 非阻塞时，**Windows 会把非阻塞状态
    /// 传染给 accept 出来的连接**（Linux 不会）。不复位的话，客户端「先连上、稍后再发」
    /// 就会被读循环判成非法请求（实测：连上就发 = 正常，隔 200/300ms 再发 = 稳定 400）。
    ///
    /// 这里用真实 socket 复现该时序：accept 后一个字都没收到，等 300ms 才发完整请求，
    /// 断言仍能正确解析。缺了 `configure_conn` 的复位，本用例在 Windows 上必失败。
    #[test]
    fn late_arriving_request_is_read_after_conn_setup() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // 与代理的 accept 循环保持一致：监听必须非阻塞才能顺带轮询配置
        listener.set_nonblocking(true).unwrap();

        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        configure_conn(&s).expect("复位阻塞模式失败");
                        return read_head(&mut s);
                    }
                    Err(e) => {
                        assert!(Instant::now() < deadline, "等 accept 超时：{e}");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // 关键：连上之后先不发，把「数据晚于 accept 到达」这个时序做出来
        std::thread::sleep(Duration::from_millis(300));
        let raw =
            b"POST /v2/billing/meter/daily-checkin HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}";
        client.write_all(raw).unwrap();
        client.flush().unwrap();

        match server.join().unwrap() {
            HeadRead::Ready(req, buf) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.target, "/v2/billing/meter/daily-checkin");
                assert_eq!(buf.len(), raw.len(), "整个请求都该收到");
            }
            HeadRead::Broken(buf) => panic!("请求被误判为断开（已收 {} 字节）", buf.len()),
            HeadRead::Stalled(buf, kind) => panic!(
                "请求被误判为卡住（{kind:?}，已收 {} 字节）——连接没复位成阻塞？",
                buf.len()
            ),
        }
    }

    /// 流式最容易写错的就是分块长度帧与终止帧，这里用一对真实 socket 端到端校验字节。
    #[test]
    fn chunked_encoding_frames_and_terminates_correctly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut server = listener.accept().unwrap().0;

        write_head(
            &mut server,
            200,
            "text/event-stream",
            &[("X-Proxy-Account-Id".to_string(), "a1".to_string())],
        )
        .unwrap();
        // SSE 的一个事件帧，长度 13 → 十六进制 D
        write_chunk(&mut server, b"data: hello\n\n").unwrap();
        write_chunk(&mut server, b"data: [DONE]\n\n").unwrap();
        write_chunk_end(&mut server).unwrap();
        drop(server); // 关掉写端，让客户端 read 到 EOF

        let mut out = String::new();
        let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = client.read_to_string(&mut out);

        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "状态行：{out:?}");
        assert!(out.contains("Transfer-Encoding: chunked"));
        assert!(out.contains("Content-Type: text/event-stream"));
        assert!(out.contains("X-Proxy-Account-Id: a1"), "自定义头要带上");
        // 长度必须是十六进制且不含前导 0x
        assert!(out.contains("\r\nD\r\ndata: hello\n\n\r\n"), "分块长度帧：{out:?}");
        assert!(out.ends_with("0\r\n\r\n"), "必须以终止帧收尾：{out:?}");
        // SSE 内容必须原样透传，不能被改写或缓冲
        assert!(out.contains("data: [DONE]"));
    }

    /// 端到端：起一个本地 SSE 上游 → 用真实 reqwest 请求 → 走 `stream_response` 写到下游。
    ///
    /// 这是「边收边转」那条胶水的唯一自动化覆盖点：上游分块、我们解块再重新分块，
    /// 哪一步写错都会在这里露出来。不碰任何全局配置，也不消耗真实配额。
    #[test]
    fn streams_an_sse_upstream_end_to_end() {
        // 1) 本地 mock 上游：一次性返回一个 SSE 流（chunked）
        let up = TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = up.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = up.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf); // 读掉请求头，读多少算多少
                let body = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
                let head = "HTTP/1.1 200 OK\r\n\
                            Content-Type: text/event-stream\r\n\
                            Transfer-Encoding: chunked\r\n\r\n";
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(format!("{:X}\r\n{body}\r\n", body.len()).as_bytes());
                let _ = s.write_all(b"0\r\n\r\n");
                let _ = s.flush();
            }
        });

        let resp = tauri::async_runtime::block_on(async {
            CLIENT
                .get(format!("http://127.0.0.1:{up_port}/chat/completions"))
                .send()
                .await
        })
        .expect("请求 mock 上游失败");

        // 2) 下游：一对真实 socket
        let down = TcpListener::bind("127.0.0.1:0").unwrap();
        let down_port = down.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", down_port)).unwrap();
        let mut server = down.accept().unwrap().0;

        let acct = accounts::Account {
            id: "acct-e2e".into(),
            name: "端到端".into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            checked_today: None,
            last: None,
        };
        stream_response(
            &mut server,
            resp,
            std::path::Path::new("/tmp"),
            &acct,
            "mock.host",
            "/chat/completions",
        );
        drop(server); // 关写端，让客户端读到 EOF

        let mut out = String::new();
        let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = client.read_to_string(&mut out);

        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "状态行：{out:?}");
        assert!(out.contains("Transfer-Encoding: chunked"), "必须流式下发");
        assert!(
            out.contains("Content-Type: text/event-stream"),
            "内容类型要透传：{out:?}"
        );
        assert!(out.contains("X-Proxy-Account-Id: acct-e2e"), "要带上选中账号");
        // SSE 数据必须原样到达下游，不能被吞掉或改写成一次性 body
        assert!(out.contains("data: {\"a\":1}"), "SSE 帧要透传：{out:?}");
        assert!(out.contains("data: [DONE]"));
        assert!(out.ends_with("0\r\n\r\n"), "必须以终止帧收尾：{out:?}");
    }

    /// 端到端：mock 上游回一个**真实形态**的 429（APISIX 网关、时间节点只写在文案里），
    /// 走通「读全 429 → 解析重置时刻 → 原样写回下游」这条链路。
    ///
    /// 这是本次改动最容易写错的接缝：429 一旦读进内存就没法再流式透传，
    /// 回写必须自己把状态码、报文、代理头都写对，且字节要与上游一字不差。
    #[test]
    fn rate_limited_response_is_read_parsed_and_forwarded() {
        // 1) mock 上游：真实 429 报文
        let up = TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = up.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = up.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf); // 读掉请求头，读多少算多少
                let head = format!(
                    "HTTP/1.1 429 Too Many Requests\r\n\
                     Content-Type: application/json; charset=utf-8\r\n\
                     Server: APISIX/3.9.1\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    REAL_429_BODY.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(REAL_429_BODY.as_bytes());
                let _ = s.flush();
            }
        });

        let resp = tauri::async_runtime::block_on(async {
            CLIENT
                .post(format!("http://127.0.0.1:{up_port}/v2/chat/completions"))
                .send()
                .await
        })
        .expect("请求 mock 上游失败");
        assert_eq!(resp.status().as_u16(), 429);

        // 2) 读全 → 解析
        let limited = tauri::async_runtime::block_on(read_rate_limited(resp));
        assert_eq!(limited.status, 429);
        assert!(
            limited.ctype.starts_with("application/json"),
            "内容类型要留住：{}",
            limited.ctype
        );
        assert!(
            !limited
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type")),
            "内容类型由 write_head 显式写出，透传会让下游出现重复头"
        );
        assert_eq!(
            limited.body,
            REAL_429_BODY.as_bytes(),
            "回写用的字节必须与上游一字不差"
        );
        assert_eq!(
            limit_until_ms(&limited.headers, &limited.body, now_ms()).map(|x| x.1),
            Some(LimitSource::Body)
        );

        // 3) 原样写回下游
        let down = TcpListener::bind("127.0.0.1:0").unwrap();
        let down_port = down.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", down_port)).unwrap();
        let mut server = down.accept().unwrap().0;
        let acct = accounts::Account {
            id: "acct-e2e".into(),
            name: "端到端".into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            checked_today: None,
            last: None,
        };
        forward_rate_limited(&mut server, &limited, &acct, "mock.host");
        drop(server); // 关写端，让客户端读到 EOF

        let mut out = String::new();
        let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = client.read_to_string(&mut out);

        assert!(
            out.starts_with("HTTP/1.1 429 Too Many Requests\r\n"),
            "状态行要带上正确的原因短语：{out:?}"
        );
        assert!(
            out.contains(&format!("Content-Length: {}", REAL_429_BODY.len())),
            "用 Content-Length 而非 chunked，与网关自己的 429 同形：{out:?}"
        );
        assert!(out.contains("X-Proxy-Account-Id: acct-e2e"), "要带上选中账号");
        assert!(
            !out.to_ascii_lowercase().matches("content-type:").count().gt(&1),
            "内容类型只能出现一次：{out:?}"
        );
        // 面向用户的那半句（含重置时刻）必须完整到达客户端
        assert!(out.contains("19:35:25 UTC+8"), "重置文案要透传：{out:?}");
        assert!(out.contains("切换其他模型继续使用"), "{out:?}");
    }

    /// 测试用：只填「过期时间 + 剩余积分」两栏，其余留空
    /// （`at` 是读数时刻，路由排序不看它）。
    fn credit_fact(expiry_ms: Option<i64>, credits: Option<f64>) -> ledger::CreditFact {
        ledger::CreditFact {
            credits,
            at: String::new(),
            earliest_expiry_ms: expiry_ms,
            packages: vec![],
        }
    }

    #[test]
    fn routing_prefers_earliest_expiry_then_most_credits() {
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let infos = vec![
            credit_fact(Some(2000), Some(100.0)),
            credit_fact(Some(1000), Some(10.0)), // 最早过期 → 胜出
            credit_fact(None, Some(99999.0)),    // 未知 → 靠后
            credit_fact(Some(500), Some(0.0)),   // 积分为 0 → 跳过
        ];
        assert_eq!(pick_index(&ids, &infos), Some(1));

        // 过期时间相同 → 剩余积分多者优先
        let ids2 = vec!["a".into(), "b".into()];
        let infos2 = vec![
            credit_fact(Some(1000), Some(10.0)),
            credit_fact(Some(1000), Some(500.0)),
        ];
        assert_eq!(pick_index(&ids2, &infos2), Some(1));
    }

    #[test]
    fn routing_falls_back_to_first_when_everyone_is_empty() {
        // 未知积分（可能还有余量）应优先于已知为 0 的账号
        let ids = vec!["a".into(), "b".into()];
        let infos = vec![
            credit_fact(Some(100), Some(0.0)),
            ledger::CreditFact::default(),
        ];
        assert_eq!(pick_index(&ids, &infos), Some(1));
        // 全员已知为 0 → 谁都不入选，退化为第一个（让上游报错，比代理 503 更有信息量）
        let ids2 = vec!["a".into(), "b".into()];
        let infos2 = vec![
            credit_fact(Some(100), Some(0.0)),
            credit_fact(Some(200), Some(0.0)),
        ];
        assert_eq!(pick_index(&ids2, &infos2), Some(0));
        assert_eq!(pick_index(&[], &[]), None, "没有账号就没有下标");
    }
}
