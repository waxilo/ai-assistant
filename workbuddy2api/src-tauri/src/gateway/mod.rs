//! 对外 API 网关：独立端口、带 Key、OpenAI 兼容。
//!
//! # 为什么必须是独立端口（不是复用接管反代的 8787）
//!
//! 接管反代装扮成官方网关给本机 WorkBuddy CLI 用，**不能加 Key**（CLI 带的是官方票，
//! 不是我们的 Key）；而这个端口是我们自己的 API，**必须鉴权**。
//! 两种信任模型混在一个端口里，迟早会为了其中一边牺牲另一边。
//!
//! # 首版（P0）提供什么
//!
//! | 路由 | 行为 |
//! | --- | --- |
//! | `POST /v1/chat/completions` | 流式透传；非流式由网关聚合（上游不支持 `stream:false`） |
//! | `POST /v1/responses` | Codex 用：请求翻成 Chat Completions，响应翻回事件流 |
//! | `GET /v1/models` | 上游私有清单 → OpenAI 结构，1h 缓存 |
//! | `GET /healthz` | 连通性探测，不鉴权 |
//!
//! **只监听 `127.0.0.1`，且每个请求都必须带 Key**：这个端口是我们自己的 API，
//! 一旦对外或者免鉴权，同机/同网段的任何程序都能烧掉整个账号池。
//!
//! # 上游错误怎么处置
//!
//! 早期版本只认 429，其余状态码一律原样透传。那是错的：某号余额耗尽（402）或
//! 对该模型无权限（11102）时冷却表没记，下一轮排序照样选中它 —— 每次请求都撞
//! 同一个号同一个错，直到人手关掉那个号。
//!
//! 现在按 [`routing::ErrKind`] 分 12 类，每类各自的冷却档位与「要不要换号」
//! 见 `routing` 模块文档。这里只负责三件 IO 事：[`read_error`] 读全文、
//! [`apply_policy`] 落冷却、调用方按 `worth_retry` 决定换号还是透传。
//!
//! 两条铁律：
//!
//! - **请求本身的问题（11101 参数错、11115 prompt 超长、内容拦截）绝不换号**：
//!   同一个请求体换任何号都得到同样的错，换号只是把一次失败放大成三次。
//! - **透传时用上游原文**：客户端看到的错误必须是上游说的那个，不是我们加工的。

mod auth;
mod chat;
pub(crate) mod models;
mod responses;

use crate::accounts::{self, Settings};
use crate::httpsrv::{self, HeadRead};
use crate::routing;
use crate::upstream;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// accept 空轮询间隔：直接等于「请求到达 → 被 accept」的额外延迟
const ACCEPT_POLL: Duration = Duration::from_millis(20);
/// 配置轮询间隔：开关 / 端口 / 监听范围变更最多晚这么久生效
const CONFIG_POLL: Duration = Duration::from_millis(500);
/// 绑定失败后的重试间隔
const BIND_RETRY: Duration = Duration::from_secs(3);

/// 网关运行状态（给界面看：到底在不在监听、上次为什么没起来）
#[derive(Clone, Default, serde::Serialize)]
pub struct GatewayState {
    pub listening: bool,
    pub port: u16,
    /// 最近一次绑定失败的原因；成功绑定后清空
    pub error: Option<String>,
}

fn state() -> &'static Mutex<GatewayState> {
    static STATE: OnceLock<Mutex<GatewayState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(GatewayState::default()))
}

fn set_state(listening: bool, port: u16, error: Option<String>) {
    if let Ok(mut s) = state().lock() {
        *s = GatewayState {
            listening,
            port,
            error,
        };
    }
}

/// 当前状态快照
pub fn status() -> GatewayState {
    state().lock().map(|s| s.clone()).unwrap_or_default()
}

/// 监督线程：按设置启停 / 换端口重绑。
///
/// 与接管反代同构：**先监听成功再对外宣称可用**，端口被占时把原因留在状态里
/// 而不是静默失败（界面会显示「端口被占用」而不是一个假的"已开启"）。
pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        loop {
            let Ok(dir) = crate::commands::try_data_dir(&app) else {
                std::thread::sleep(BIND_RETRY);
                continue;
            };
            let settings = accounts::load_settings(&dir);
            if !settings.gateway_enabled {
                // 关掉时把状态复位：界面不该继续显示「监听中」
                if status().listening {
                    set_state(false, 0, None);
                    crate::tray::set_gateway_active(&app, false, 0);
                }
                std::thread::sleep(CONFIG_POLL);
                continue;
            }

            let port = settings.gateway_port;
            // 永远只绑回环：这个端口烧的是账号池的积分，不该对局域网开口
            let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
            match TcpListener::bind((ip, port)) {
                Ok(listener) => {
                    set_state(true, port, None);
                    crate::tray::set_gateway_active(&app, true, port);
                    // 监听必须非阻塞，才能在 accept 之余顺带轮询配置；
                    // accept 出来的连接会被 Windows 传染非阻塞，逐连接复位——见 httpsrv::configure_conn
                    let _ = listener.set_nonblocking(true);
                    let mut last_cfg = Instant::now();
                    // 已绑定期间的「活设置」：端口变化要重绑（break），
                    // 但 Key / 扣费账号这些**不需要重绑**，就地替换即可。
                    let mut live = settings.clone();
                    loop {
                        if last_cfg.elapsed() >= CONFIG_POLL {
                            let cur = accounts::load_settings(&dir);
                            if !cur.gateway_enabled || cur.gateway_port != port {
                                break;
                            }
                            live = cur;
                            last_cfg = Instant::now();
                        }
                        match listener.accept() {
                            Ok((stream, peer)) => {
                                let dir2 = dir.clone();
                                let st2 = live.clone();
                                std::thread::spawn(move || handle_conn(stream, peer, dir2, st2));
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(ACCEPT_POLL);
                            }
                            Err(_) => std::thread::sleep(ACCEPT_POLL),
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("无法监听 {ip}:{port}：{e}");
                    eprintln!("[gateway] {msg}");
                    set_state(false, port, Some(msg));
                    crate::tray::set_gateway_active(&app, false, port);
                    std::thread::sleep(BIND_RETRY);
                }
            }
        }
    });
}

/// 单个连接：解析 → 鉴权 → 分发。
///
/// `settings` 由调用方传入而不是在这里读盘：网关每 500ms 已轮询过一次，
/// 这里再读一遍等于每个请求多一次磁盘 IO；也让端到端测试可以注入设置而不写盘。
fn handle_conn(mut stream: TcpStream, peer: SocketAddr, dir: std::path::PathBuf, settings: Settings) {
    // 0. 先复位成阻塞再读。漏掉这一步的代价见 httpsrv::configure_conn 的文档。
    if httpsrv::configure_conn(&stream).is_err() {
        return;
    }

    let (req, buf) = match httpsrv::read_head(&mut stream) {
        HeadRead::Ready(req, buf) => (req, buf),
        HeadRead::Broken(buf) => {
            httpsrv::respond(&mut stream, 400, "application/json", bad("请求未读完", "invalid_request_error").as_bytes(), &[]);
            let _ = buf;
            return;
        }
        HeadRead::Stalled(_, _) => {
            httpsrv::respond(&mut stream, 408, "application/json", bad("读取请求超时", "invalid_request_error").as_bytes(), &[]);
            return;
        }
    };

    let route = upstream::local_route(&req.method, req.path());

    // 健康检查不鉴权：客户端与脚本要能在没有 Key 的情况下判断「端口活着」
    if route == upstream::LocalRoute::Health {
        httpsrv::respond(&mut stream, 200, "text/plain", b"ok", &[]);
        return;
    }

    if let Err(msg) = authorize(&settings, &req) {
        httpsrv::respond(&mut stream, 401, "application/json", bad(&msg, "authentication_error").as_bytes(), &[]);
        return;
    }

    let body = req.body(&buf).to_vec();
    match route {
        upstream::LocalRoute::Chat => serve(Wire::Chat, &mut stream, &dir, &req, &body, &settings, peer),
        upstream::LocalRoute::Responses => {
            serve(Wire::Responses, &mut stream, &dir, &req, &body, &settings, peer)
        }
        upstream::LocalRoute::Models => serve_models(&mut stream, &dir, &settings),
        upstream::LocalRoute::Health => unreachable!(),
        upstream::LocalRoute::NotFound => {
            httpsrv::respond(
                &mut stream,
                404,
                "application/json",
                bad(
                    "未知路由。本网关只提供 /v1/chat/completions、/v1/responses、/v1/models、/healthz",
                    "invalid_request_error",
                )
                .as_bytes(),
                &[],
            );
        }
    }
}

/// 鉴权：**每个请求都必须带 Key**（本机来源也不例外）。
///
/// 曾经有过「本机免 Key」开关，已经删掉：`127.0.0.1` 上跑着什么程序用户未必全清楚，
/// 而这条通道烧的是整个账号池的积分。丢一次 Key 只是点一下「重新生成」的事，
/// 放开鉴权省下的那点复制粘贴不值这个风险。
fn authorize(settings: &Settings, req: &httpsrv::Request) -> Result<(), String> {
    let Some(given) = auth::extract_key(req) else {
        return Err("缺少 API Key：请在 Authorization 头里带 `Bearer <key>`".into());
    };
    if settings.gateway_api_key.is_empty() {
        return Err("网关尚未生成 API Key，请在「API 接入」页点一次「重新生成」".into());
    }
    if auth::key_matches(&settings.gateway_api_key, given) {
        Ok(())
    } else {
        Err("API Key 无效".into())
    }
}

fn bad(message: &str, kind: &str) -> String {
    upstream::error_body(message, kind)
}

/// 下游说哪种方言。
///
/// 两条线共用同一套选号、冷却、错误处置；只有「上游请求体怎么造」和
/// 「响应怎么回给客户端」不同。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// `POST /v1/chat/completions`
    Chat,
    /// `POST /v1/responses`（Codex 用，需要双向翻译）
    Responses,
}

fn serve(
    wire: Wire,
    stream: &mut TcpStream,
    dir: &Path,
    req: &httpsrv::Request,
    body: &[u8],
    settings: &Settings,
    peer: SocketAddr,
) {
    let model = chat::body_model(body);
    let want_stream = chat::body_wants_stream(body);
    let up_body = match wire {
        Wire::Chat => chat::upstream_body(body),
        Wire::Responses => responses::to_chat(body),
    };
    let Some(up_body) = up_body else {
        httpsrv::respond(stream, 400, "application/json", bad("请求体不是合法 JSON 对象", "invalid_request_error").as_bytes(), &[]);
        return;
    };
    let host = settings.default_base_url.clone();
    let mut ban: Vec<String> = Vec::new();

    loop {
        let Some(account) =
            tauri::async_runtime::block_on(routing::choose_account(dir, &ban, model.as_deref()))
        else {
            httpsrv::respond(stream, 503, "application/json", bad("没有可用账号（未导入或全部不可扣费）", "server_error").as_bytes(), &[]);
            return;
        };

        let started = Instant::now();
        let resp = tauri::async_runtime::block_on(async {
            let mut r = upstream::client()
                .post(upstream::chat_url(&host))
                .bearer_auth(&account.token);
            for (k, v) in upstream::forward_headers(req) {
                r = r.header(k.as_str(), v.as_str());
            }
            r.body(up_body.clone()).send().await
        });

        let r = match resp {
            Ok(r) => r,
            Err(e) => {
                api_log(dir, &account.name, model.as_deref(), peer, false, &format!("上游请求失败：{e}"), started.elapsed());
                httpsrv::respond(stream, 502, "application/json", bad(&format!("上游请求失败：{e}"), "server_error").as_bytes(), &[]);
                return;
            }
        };

        // 成功：清熔断计数与该模型的负缓存，再交给 relay
        if r.status().as_u16() < 400 {
            routing::note_success(&account.id, model.as_deref());
            match wire {
                Wire::Chat => {
                    relay(stream, r, want_stream, dir, &account, model.as_deref(), peer, started)
                }
                Wire::Responses => {
                    relay_responses(stream, r, want_stream, dir, &account, model.as_deref(), peer, started)
                }
            }
            return;
        }

        // 失败：错误体很小，整段读下来再分类——上游把语义写在 body 的 `code` 里，
        // 光看 HTTP 状态判不出「模型级限流」和「账号级限流」，也认不出 11102。
        let (status, headers, text) = read_error(r);
        let kind = routing::classify(status, text.as_bytes());
        let note = apply_policy(&account, model.as_deref(), kind, &headers, text.as_bytes());

        if kind.worth_retry() && ban.len() + 1 < routing::FAILOVER_MAX_TRIES {
            ban.push(account.id.clone());
            api_log(
                dir,
                &account.name,
                model.as_deref(),
                peer,
                false,
                &format!("{}（{note}），换号重发", kind.label()),
                started.elapsed(),
            );
            continue;
        }
        api_log(
            dir,
            &account.name,
            model.as_deref(),
            peer,
            false,
            &format!("{}（{note}），回给客户端", kind.label()),
            started.elapsed(),
        );
        let extra: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let payload = upstream_error_payload(wire, &text, kind.label());
        httpsrv::respond(stream, status, "application/json", payload.as_bytes(), &extra);
        return;
    }
}

/// 上游错误怎么回给下游。
///
/// Chat 客户端（curl、SDK）能直接读懂上游的 `{"code":...,"msg":...}`，原样透传
/// 信息最全；但 Codex 走 Responses，它按 `{"error":{"message":...}}` 取文案，
/// 拿到裸 `{"code":11128}` 只会显示 "unexpected status 400"，用户什么都看不到。
fn upstream_error_payload(wire: Wire, text: &str, fallback: &str) -> String {
    match wire {
        Wire::Chat => text.to_string(),
        Wire::Responses => upstream::error_from_upstream(text, fallback),
    }
}

/// 读一个上游错误响应：`(状态码, 可回传的响应头, 原文)`。
///
/// 错误体很小（几百字节），整段读下来才能看业务码。响应头保留 `Retry-After`
/// 之类对客户端有用的项，逐跳头与 `content-type` 由 `respond` 自己处理。
fn read_error(r: reqwest::Response) -> (u16, Vec<(String, String)>, String) {
    let status = r.status().as_u16();
    let headers: Vec<(String, String)> = r
        .headers()
        .iter()
        .filter(|(k, _)| !httpsrv::response_hop_by_hop(k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    let text = tauri::async_runtime::block_on(r.text()).unwrap_or_default();
    (status, headers, text)
}

/// 按错误分类处置账号，返回一句写进日志的说明。
///
/// 处置规则见 [`routing::ErrKind`] 的文档表。这里只负责「把分类落到冷却表上」，
/// 要不要换号、要不要透传由调用方按 [`routing::ErrKind::worth_retry`] 决定。
fn apply_policy(
    account: &accounts::Account,
    model: Option<&str>,
    kind: routing::ErrKind,
    headers: &[(String, String)],
    body: &[u8],
) -> String {
    use routing::ErrKind;
    let id = &account.id;
    let note = routing::cooldown_note;
    match kind {
        ErrKind::CreditExhausted => note(routing::cool_credit_exhausted(id)),
        ErrKind::SessionDead => note(routing::cool_session_dead(id)),
        ErrKind::Restricted => note(routing::cool_restricted(id)),
        ErrKind::Waf => note(routing::cool_waf(id)),
        ErrKind::NotFound => note(routing::cool_not_found(id)),
        ErrKind::RateModel | ErrKind::RateAccount => {
            // 只有「模型级」才按模型记；模型名缺失时降级成账号级（没有更细的粒度可用）
            let scoped = if kind == ErrKind::RateModel { model } else { None };
            match routing::limit_until_ms(headers, body, routing::now_ms()) {
                // 有权威重置时刻：精确冷却到那个墙钟，**绝不做指数堆加**
                Some((until, source)) => match scoped {
                    Some(m) => note(routing::cool_model(id, m, until, source, "模型级限流")),
                    None => note(routing::cool_account(id, until, source, "账号级限流")),
                },
                // 上游没给时刻：有界退避（冷却中不延长）
                None => match scoped {
                    Some(m) => match routing::backoff_model(id, m, routing::SOFT_BASE_MS, "模型级限流（无重置时刻）") {
                        Some(e) => note(e),
                        None => "该模型已在冷却中，不延长".into(),
                    },
                    None => match routing::backoff_account(id, routing::SOFT_BASE_MS, "账号级限流（无重置时刻）") {
                        Some(e) => note(e),
                        None => "该账号已在冷却中，不延长".into(),
                    },
                },
            }
        }
        ErrKind::ModelBlocked => match model {
            Some(m) => note(routing::block_model(id, m, "该号无此模型")),
            // 没有模型上下文时无法按 (账号, 模型) 避让，退化成账号级短冷却
            None => note(routing::cool_not_found(id)),
        },
        ErrKind::Server => match routing::note_failure(id) {
            Some(e) => note(e),
            None => "上游 5xx（未达熔断阈值，只记数不罚号）".into(),
        },
        // 以下三类**不罚号**：账号是健康的，是请求本身或客户端的通道指纹有问题
        ErrKind::ChannelRefused => "通道校验失败（账号健康，不罚号）".into(),
        ErrKind::BadRequest => "请求本身有问题（不罚号也不换号）".into(),
        ErrKind::Other => "未分类错误（不罚号）".into(),
    }
}

/// 把上游响应交给下游：流式原样转发，非流式聚合成一个 JSON。
fn relay(
    stream: &mut TcpStream,
    mut resp: reqwest::Response,
    want_stream: bool,
    dir: &Path,
    account: &accounts::Account,
    model: Option<&str>,
    peer: SocketAddr,
    started: Instant,
) {
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let extra: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !httpsrv::response_hop_by_hop(k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();

    if want_stream {
        if httpsrv::write_head(stream, status, &ctype, &extra).is_err() {
            return;
        }
        let mut bytes = 0usize;
        let mut err: Option<String> = None;
        tauri::async_runtime::block_on(async {
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if httpsrv::write_chunk(stream, &chunk).is_err() {
                            break; // 下游断了，不是上游的错
                        }
                        bytes += chunk.len();
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // 上游读失败绝不能静默：吞掉的话客户端只看到一个空流
                        err = Some(format!("上游读流失败（已转发 {bytes} 字节）：{e}"));
                        break;
                    }
                }
            }
        });
        let _ = httpsrv::write_chunk_end(stream);
        api_log(
            dir,
            &account.name,
            model,
            peer,
            err.is_none() && status < 400,
            &match &err {
                Some(e) => e.clone(),
                None => format!("流式透传 {bytes} 字节"),
            },
            started.elapsed(),
        );
        return;
    }

    // 非流式：上游只给流，这里收全再聚合
    let all = tauri::async_runtime::block_on(resp.bytes())
        .map(|b| b.to_vec())
        .unwrap_or_default();
    if status != 200 {
        api_log(dir, &account.name, model, peer, false, &format!("上游返回 {status}"), started.elapsed());
        httpsrv::respond(stream, status, &ctype, &all, &[]);
        return;
    }
    match chat::aggregate_sse(&all) {
        Some(json) => {
            api_log(dir, &account.name, model, peer, true, "非流式（网关聚合）", started.elapsed());
            httpsrv::respond(stream, 200, "application/json", json.as_bytes(), &[]);
        }
        None => {
            api_log(dir, &account.name, model, peer, false, "上游未返回可聚合的流，原样透传", started.elapsed());
            httpsrv::respond(stream, status, &ctype, &all, &[]);
        }
    }
}

/// Responses 版 relay：把上游 Chat SSE 逐帧翻译成 Responses 事件。
///
/// 与 [`relay`] 的关键差别是**延迟写响应头**：上游偶尔回 200 但 body 是一段
/// 普通 JSON（不是 SSE），此时一帧都翻译不出来。先读到第一帧、确认解析成功，
/// 才写 200 —— 否则客户端会收到一个"成功但空"的响应，比报错更难排查。
fn relay_responses(
    stream: &mut TcpStream,
    mut resp: reqwest::Response,
    want_stream: bool,
    dir: &Path,
    account: &accounts::Account,
    model: Option<&str>,
    peer: SocketAddr,
    started: Instant,
) {
    let mut t = responses::Translate::new(model);
    let mut out = String::new();

    // 非流式：收全翻译完，直接把最终的 response 对象当 JSON 返回
    if !want_stream {
        let all = tauri::async_runtime::block_on(resp.bytes())
            .map(|b| b.to_vec())
            .unwrap_or_default();
        t.feed_bytes(&all, &mut out);
        t.finish(&mut out);
        match t.final_response() {
            Some(r) => {
                let body = r.to_string();
                api_log(dir, &account.name, model, peer, true, "非流式（Responses 本地聚合）", started.elapsed());
                httpsrv::respond(stream, 200, "application/json", body.as_bytes(), &[]);
            }
            None => {
                api_log(dir, &account.name, model, peer, false, "上游未返回可解析的流", started.elapsed());
                httpsrv::respond(
                    stream,
                    502,
                    "application/json",
                    bad("上游返回了不可解析的响应（不是 SSE 流）", "server_error").as_bytes(),
                    &[],
                );
            }
        }
        return;
    }

    let mut head_written = false;
    let mut bytes = 0usize;
    // 前 4KB 原文留着：万一上游 200 但不是 SSE，得把内容告诉用户
    let mut raw: Vec<u8> = Vec::new();
    let mut err: Option<String> = None;

    tauri::async_runtime::block_on(async {
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    bytes += chunk.len();
                    if raw.len() < 4096 {
                        raw.extend_from_slice(&chunk);
                    }
                    t.feed_bytes(&chunk, &mut out);
                    if !head_written && t.opened() {
                        // 不再手动加 cache-control：write_head 已经写了
                        if httpsrv::write_head(stream, 200, "text/event-stream", &[]).is_err() {
                            return; // 下游已断，别再翻译了
                        }
                        head_written = true;
                    }
                    if head_written && !out.is_empty() {
                        if httpsrv::write_chunk(stream, out.as_bytes()).is_err() {
                            return;
                        }
                        out.clear();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    err = Some(format!("上游读流失败（已翻译 {bytes} 字节）：{e}"));
                    break;
                }
            }
        }
    });

    if !head_written {
        let snippet: String = String::from_utf8_lossy(&raw).chars().take(300).collect();
        api_log(
            dir,
            &account.name,
            model,
            peer,
            false,
            &format!("上游 200 但不是 SSE：{snippet}"),
            started.elapsed(),
        );
        httpsrv::respond(
            stream,
            502,
            "application/json",
            bad(&format!("上游没有返回流式内容：{snippet}"), "server_error").as_bytes(),
            &[],
        );
        return;
    }

    // 收尾：客户端已经拿到 200，必须给一个终止事件，否则它会一直等
    t.finish(&mut out);
    if !out.is_empty() {
        let _ = httpsrv::write_chunk(stream, out.as_bytes());
    }
    let _ = httpsrv::write_chunk_end(stream);
    api_log(
        dir,
        &account.name,
        model,
        peer,
        err.is_none(),
        &match &err {
            Some(e) => e.clone(),
            None => format!("Responses 流式翻译 {bytes} 字节"),
        },
        started.elapsed(),
    );
}

fn serve_models(stream: &mut TcpStream, dir: &Path, settings: &Settings) {
    if let Some(body) = models::cached() {
        httpsrv::respond(stream, 200, "application/json", body.as_bytes(), &[]);
        return;
    }
    match tauri::async_runtime::block_on(models::refresh(dir, &settings.default_base_url)) {
        Some(body) => httpsrv::respond(stream, 200, "application/json", body.as_bytes(), &[]),
        None => httpsrv::respond(
            stream,
            502,
            "application/json",
            bad("拉取模型清单失败（没有可用账号或上游不可达）", "server_error").as_bytes(),
            &[],
        ),
    }
}

/// 访问日志：写进统一日志流的 `api` 类别，便于事后对账（哪个账号、哪个模型、多久）。
///
/// **不落请求体**：提示词与代码片段属于用户数据，日志只记元信息。
fn api_log(
    dir: &Path,
    account: &str,
    model: Option<&str>,
    peer: SocketAddr,
    ok: bool,
    msg: &str,
    took: Duration,
) {
    crate::logs::push(
        dir,
        crate::logs::CAT_API,
        account,
        ok,
        format!(
            "{model} ← {peer} · {}ms · {msg}",
            took.as_millis(),
            model = model.unwrap_or("未知模型")
        ),
    );
}

#[cfg(test)]
mod e2e {
    use super::*;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    /// 发一个原始 HTTP/1.1 请求，返回 (状态码, 原始响应文本)
    fn http(port: u16, raw: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("连不上网关");
        s.write_all(raw.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (status, text)
    }

    /// **真实端到端**：真监听、真请求、真上游。默认不跑。
    ///
    /// ```bash
    /// WBA2API_TEST_DIR="$APPDATA/com.waxilo.workbuddy-assistant" \
    ///   cargo test --offline --lib -- --ignored --nocapture end_to_end
    /// ```
    ///
    /// 账号文件会**复制**到临时目录再跑，绝不碰源目录——那里的 refresh token 是一次性的，
    /// 被测试顶掉就等于把用户账号弄失效。
    #[test]
    #[ignore]
    fn end_to_end_health_auth_models_and_chat() {
        let src = PathBuf::from(
            std::env::var("WBA2API_TEST_DIR").expect("请设 WBA2API_TEST_DIR 指向有账号的数据目录"),
        );
        let dir = std::env::temp_dir().join("wba2api_e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(src.join("accounts.json"), dir.join("accounts.json")).expect("复制账号失败");

        let mut settings = Settings::default();
        settings.gateway_enabled = true;
        settings.gateway_api_key = "sk-wb-e2e".into();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir2 = dir.clone();
        let st2 = settings.clone();
        std::thread::spawn(move || {
            // 每个连接只处理一次请求（没有 keep-alive），所以 accept 次数要够用例数
            for _ in 0..8 {
                if let Ok((s, p)) = listener.accept() {
                    handle_conn(s, p, dir2.clone(), st2.clone());
                }
            }
        });

        // 1. 健康检查：不鉴权
        let (status, body) = http(port, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200, "healthz 应免鉴权：{body}");

        // 2. 无 Key 必须 401，且是 OpenAI 错误结构（客户端才显示得出原因）
        let (status, body) = http(
            port,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        );
        assert_eq!(status, 401, "{body}");
        assert!(body.contains("\"error\""), "401 要用 OpenAI 错误体：{body}");

        // 3. 错 Key 也是 401
        let (status, _) = http(
            port,
            "GET /v1/models HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sk-wb-wrong\r\n\r\n",
        );
        assert_eq!(status, 401);

        // 4. 正确 Key + 模型清单（打真上游）
        let (status, body) = http(
            port,
            "GET /v1/models HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sk-wb-e2e\r\n\r\n",
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"object\":\"list\""), "应是 OpenAI 列表结构：{body}");
        assert!(body.contains("hy4flash-free"), "真实清单里应有免费模型：{body}");

        // 5. 非流式对话（上游只会流式，网关负责聚合）
        let payload = r#"{"model":"hy4flash-free","stream":false,"max_tokens":16,"messages":[{"role":"user","content":"say hi"}]}"#;
        let raw = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sk-wb-e2e\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http(port, &raw);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"object\":\"chat.completion\""), "应聚合出完整 completion：{body}");
        assert!(body.contains("\"usage\""), "非流式必须带 usage：{body}");

        // 6. Responses API（Codex 走这条）：上游没有这个路由，全靠本地翻译
        //
        // 不设 max_output_tokens：hy4flash 会先把预算全花在思维链上
        // （实测 16 token 全记成 reasoning_tokens，正文一个字都没有）。
        let payload = r#"{"model":"hy4flash-free","input":"hi","stream":true}"#;
        let raw = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sk-wb-e2e\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http(port, &raw);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("event: response.created"), "缺 created：{body}");
        assert!(body.contains("event: response.output_text.delta"), "缺文本增量：{body}");
        assert!(body.contains("event: response.completed"), "必须给终止事件，否则 Codex 一直等：{body}");
        assert!(body.contains("\"sequence_number\":0"), "序号从 0 开始：{body}");

        // 7. Responses 非流式：网关内部收全，直接回一个 response 对象
        let payload = r#"{"model":"hy4flash-free","input":"hi","stream":false}"#;
        let raw = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sk-wb-e2e\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http(port, &raw);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"object\":\"response\""), "应是 response 对象：{body}");
        assert!(body.contains("\"status\":\"completed\""), "{body}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
