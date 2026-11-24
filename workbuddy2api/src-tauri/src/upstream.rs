//! 上游对接：地址改写、转发客户端、OpenAI 风格错误体。
//!
//! 上游只有一个真实对话路由：`POST {host}/v2/chat/completions`（流式，OpenAI 兼容）。
//! 实测结论（2026-11-24）：
//!
//! - `stream:false` → `400 code:11101 "Non-stream chat request is currently not supported"`
//!   ⇒ 非流式只能由网关聚合，见 `gateway::chat`。
//! - `/v2/responses`、`/v1/models`、`/v2/models` 全部 404 ⇒ 这两条路由只能本地实现。
//! - `tool_choice` 只接受字符串，传对象会 400 ⇒ 对象形式一律降级成 `auto`。

use crate::httpsrv::Request;
use std::sync::LazyLock;
use std::time::Duration;

/// 上游连接超时（建连阶段）
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 上游单次读空闲超时：SSE 长对话会持续数分钟，**不能用总超时**——
/// reqwest 的 `timeout()` 覆盖整个响应体读取，会掐断活着的流；
/// `read_timeout()` 只管「多久没收到新数据」，才是流的正确保护方式
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// 网关对话转发用的客户端。
///
/// 与助手的接管反代同一套配置：不跟重定向（错误 302 会被变成一页 HTML，
/// 客户端只见空流、无从诊断）、强制直连（不继承 shell 的 HTTP_PROXY）。
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .read_timeout(UPSTREAM_IDLE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(crate::http::UA)
        .build()
        .expect("构建上游客户端失败")
});

pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// 网关对外声明的模型清单来源（上游私有结构，不是 OpenAI 格式）
pub fn models_url(host: &str) -> String {
    format!(
        "{}/v2/enterprises/personal/models",
        host.trim_end_matches('/')
    )
}

/// 真正打上游的对话地址
pub fn chat_url(host: &str) -> String {
    format!("{}/v2/chat/completions", host.trim_end_matches('/'))
}

/// 由网关**本地处理**的路由
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LocalRoute {
    Chat,
    /// Responses API（Codex 用）：本地翻译成 Chat Completions 再打上游
    Responses,
    Models,
    Health,
    NotFound,
}

/// 路由判定。
///
/// 同时接受 `/v1/...`（OpenAI 惯例）与 `/v2/...`（老客户端直接照抄上游路径）。
/// `/v1/responses` 是**本地实现**的：上游没有这个路由（实测 404），
/// 必须由 `gateway::responses` 双向翻译。
pub fn local_route(method: &str, path: &str) -> LocalRoute {
    let path = path.trim_end_matches('/');
    match (method, path) {
        ("POST", "/v1/chat/completions") => LocalRoute::Chat,
        ("POST", "/chat/completions") => LocalRoute::Chat,
        ("POST", "/v2/chat/completions") => LocalRoute::Chat,
        ("POST", "/v1/responses") => LocalRoute::Responses,
        ("POST", "/responses") => LocalRoute::Responses,
        ("GET", "/v1/models") => LocalRoute::Models,
        ("GET", "/models") => LocalRoute::Models,
        ("GET", "/healthz") => LocalRoute::Health,
        _ => LocalRoute::NotFound,
    }
}

/// 客户端指纹头：一律不外泄给上游。
///
/// `user-agent` 最要紧——`reqwest` 的同名头以**后写的为准**，所以把下游客户端的
/// `user-agent` 透传过去，等于把我们自己设的 [`crate::http::UA`] 覆盖掉。而下游发的是
/// `OpenAI/Python 1.x`、`codex_cli_rs/0.1.0` 这类一眼不像官方客户端的串。上游对 UA
/// 有白名单校验，送错轻则被记成可疑设备，重则 `403 code 11140 "request illegal"`。
///
/// `x-stainless-*` 是 OpenAI 官方 SDK 的调试头（重试次数、超时、语言、包版本），
/// 带上等于主动交代「这不是官方客户端发的」，同样没有理由外泄。
///
/// `referer` / `origin` / `x-title` 属浏览器与其它厂商 SDK 的指纹，上游用不上。
fn is_client_fingerprint(name: &str) -> bool {
    name.eq_ignore_ascii_case("user-agent")
        || name.eq_ignore_ascii_case("referer")
        || name.eq_ignore_ascii_case("origin")
        || name.eq_ignore_ascii_case("x-title")
        // 网关自己的凭据
        || name.eq_ignore_ascii_case("x-gateway-key")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.to_ascii_lowercase().starts_with("x-stainless-")
}

/// 把下游请求头里该透传的那些挑出来（去掉逐跳头、我们自己的凭据、客户端指纹）。
pub fn forward_headers(req: &Request) -> Vec<(String, String)> {
    req.headers
        .iter()
        .filter(|(k, _)| !crate::httpsrv::hop_by_hop(k))
        .filter(|(k, _)| !is_client_fingerprint(k))
        .cloned()
        .collect()
}

/// OpenAI 风格的错误体。客户端（含 Codex）都按这个结构取 message，
/// 换成纯文本会显示成 "unexpected status 401" 而看不到原因。
pub fn error_body(message: &str, kind: &str) -> String {
    serde_json::json!({
        "error": {
            "message": message,
            "type": kind,
            "code": serde_json::Value::Null,
        }
    })
    .to_string()
}

/// 把上游的错误原文包成 OpenAI 错误体（保持上游那句人话）。
///
/// 上游的形态是 `{"code":11128,"msg":"Illegal ...","displayTips":{"zh":"..."}}`。
/// Chat 客户端能直接读懂这个结构（`upstream_error_payload` 原样透传），
/// 但 Codex 走 Responses，它只看 `error.message`，拿到裸 `code/msg` 会显示
/// "unexpected status 400" —— 用户等于什么都看不到。
///
/// 所以这里优先取 `msg`，其次 `displayTips.zh`（上游有时把真正的说明放这里），
/// 最后退回原文。
pub fn error_from_upstream(text: &str, fallback: &str) -> String {
    let msg = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("msg")
                .and_then(|m| m.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("displayTips")?
                        .get("zh")?
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_else(|| {
            let t = text.trim();
            if t.is_empty() {
                fallback.to_string()
            } else {
                t.to_string()
            }
        });
    error_body(&msg, "upstream_error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_openai_and_legacy_paths_to_chat() {
        assert_eq!(local_route("POST", "/v1/chat/completions"), LocalRoute::Chat);
        assert_eq!(
            local_route("POST", "/v2/chat/completions"),
            LocalRoute::Chat,
            "老客户端会照抄上游路径"
        );
        assert_eq!(local_route("POST", "/v1/chat/completions/"), LocalRoute::Chat);
    }

    #[test]
    fn models_and_health_are_local() {
        assert_eq!(local_route("GET", "/v1/models"), LocalRoute::Models);
        assert_eq!(local_route("GET", "/healthz"), LocalRoute::Health);
    }

    #[test]
    fn responses_route_is_local_not_passthrough() {
        // Codex 走的就是这条；上游 `/v2/responses` 是 404，必须本地翻译
        assert_eq!(local_route("POST", "/v1/responses"), LocalRoute::Responses);
        assert_eq!(local_route("POST", "/responses"), LocalRoute::Responses);
        assert_eq!(local_route("POST", "/v1/responses/"), LocalRoute::Responses);
        // GET 不是合法用法（Responses 只支持 POST）
        assert_eq!(local_route("GET", "/v1/responses"), LocalRoute::NotFound);
    }

    #[test]
    fn unknown_paths_do_not_fall_through_to_upstream() {
        // 上游有几十个私有路由（签到、计费…），网关**一个都不能**透传：
        // 那等于把账号池的其它权限一起暴露出去
        assert_eq!(local_route("POST", "/v2/billing/meter/daily-checkin"), LocalRoute::NotFound);
        assert_eq!(local_route("GET", "/v2/enterprises/personal/models"), LocalRoute::NotFound);
        assert_eq!(local_route("POST", "/v2/responses"), LocalRoute::NotFound);
    }

    #[test]
    fn error_body_is_openai_shaped() {
        let v: serde_json::Value =
            serde_json::from_str(&error_body("缺少 API Key", "invalid_request_error")).unwrap();
        assert_eq!(v["error"]["message"], "缺少 API Key");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    /// 上游真实错误原文（2026-11-24 实测 `role:"developer"` 的响应）
    #[test]
    fn upstream_error_text_is_wrapped_for_codex() {
        let raw = r#"{"code":11128,"msg":"Illegal API invocation from an unapproved channel","displayTips":{"zh":"请求可能不是官方客户端发出的"}}"#;
        let v: serde_json::Value = serde_json::from_str(&error_from_upstream(raw, "x")).unwrap();
        assert_eq!(v["error"]["message"], "Illegal API invocation from an unapproved channel");
        assert_eq!(v["error"]["type"], "upstream_error");
    }

    #[test]
    fn upstream_error_falls_back_to_display_tips_then_raw() {
        let tips = r#"{"code":11140,"displayTips":{"zh":"账号存在异常"}}"#;
        let v: serde_json::Value = serde_json::from_str(&error_from_upstream(tips, "x")).unwrap();
        assert_eq!(v["error"]["message"], "账号存在异常");

        // 完全不是 JSON：把原文塞进 message，至少用户能看见
        let v: serde_json::Value =
            serde_json::from_str(&error_from_upstream("<html>502</html>", "x")).unwrap();
        assert_eq!(v["error"]["message"], "<html>502</html>");

        // 空 body：用调用方给的兜底文案
        let v: serde_json::Value = serde_json::from_str(&error_from_upstream("", "上游没说话")).unwrap();
        assert_eq!(v["error"]["message"], "上游没说话");
    }

    fn req_with(headers: &[(&str, &str)]) -> Request {
        Request {
            method: "POST".into(),
            target: "/v1/chat/completions".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body_len: 0,
            head_end: 0,
        }
    }

    /// 实测背景：`reqwest` 的同名头以后写为准，透传下游 `user-agent` 会覆盖我们自己的。
    #[test]
    fn never_forwards_client_fingerprints() {
        let r = req_with(&[
            ("User-Agent", "OpenAI/Python 1.55.0"),
            ("x-stainless-retry-count", "1"),
            ("X-Stainless-Lang", "python"),
            ("Referer", "https://evil.example"),
            ("Origin", "https://evil.example"),
            ("X-Title", "my-app"),
            ("x-api-key", "sk-wb-ours"),
            ("x-gateway-key", "sk-wb-ours"),
        ]);
        let out = forward_headers(&r);
        assert!(
            out.is_empty(),
            "这些都是客户端指纹或我们自己的凭据，一个都不能外泄：{out:?}"
        );
    }

    #[test]
    fn keeps_headers_the_upstream_actually_needs() {
        let r = req_with(&[
            ("Content-Type", "application/json"),
            ("Accept", "text/event-stream"),
            ("user-agent", "whatever"),
        ]);
        let out = forward_headers(&r);
        let names: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"Content-Type"));
        assert!(names.contains(&"Accept"));
        assert!(!names.contains(&"user-agent"));
    }
}
