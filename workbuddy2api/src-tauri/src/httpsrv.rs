//! HTTP/1.1 服务器原语：请求解析、连接复位、chunked 输出。
//!
//! 为什么手写而不用现成框架：本机 `crates.io` 走代理会 502，`cargo` 必须 `--offline`，
//! **加不了新依赖**。而 Tauri 只暴露 `async_runtime`，不暴露底层 hyper。
//! 这套原语在 `workbuddy-assistant` 的接管反代里已经跑了几十万次请求，直接搬过来用。
//!
//! 两个平台坑（都踩过，别再踩）：
//!
//! 1. **accept 出来的连接必须显式复位成阻塞**：监听 socket 为了顺带轮询配置是非阻塞的，
//!    而 Windows 会把非阻塞状态**传染**给 accept 出来的连接（Linux 不传染）。
//!    不复位时，只要请求字节没一次到齐就判成非法 → 400。详见 [`configure_conn`]。
//! 2. **响应一律 chunked**：SSE 被缓冲成一次性 body 时，客户端只会看到空流。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 请求头上限
pub const MAX_HEAD: usize = 64 * 1024;
/// 请求体上限（Codex 的长上下文实测 186KB，这里留了两个数量级余量）
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// 请求头读取时限（读空闲）：连上了却迟迟不发完整请求就放弃
const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 下游写超时：SSE 长对话可能持续数分钟，要给得足够宽
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(600);

/// 解析出的请求头部分
#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    /// 含查询串的路径，如 `/v1/chat/completions?x=1`
    pub target: String,
    /// 全部请求头（名字保留原样，值 trim 过）
    pub headers: Vec<(String, String)>,
    /// 正文字节数（按 Content-Length）
    pub body_len: usize,
    /// 请求头（含 `\r\n\r\n`）之后的起始偏移
    pub head_end: usize,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 从读到的原始缓冲里切出请求体
    pub fn body<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        buf.get(self.head_end..self.head_end + self.body_len)
            .unwrap_or(&[])
    }

    /// 只取路径部分（去掉查询串）
    pub fn path(&self) -> &str {
        self.target
            .split('?')
            .next()
            .unwrap_or(self.target.as_str())
    }
}

/// 从缓冲里解析请求行 + 请求头。返回 None 表示数据不完整或非法。
pub fn parse_request(buf: &[u8]) -> Option<Request> {
    let end = find_subslice(buf, b"\r\n\r\n")?;
    if end > MAX_HEAD {
        return None;
    }
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    // 请求行必须是三段（方法 / 目标 / 版本），少一段就是坏请求
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

pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 不该透传给上游的请求头。
///
/// `authorization` 必删：转发时换成选中账号的 token，原值（我们自己的 Key / 官方票）不能外泄。
pub fn hop_by_hop(name: &str) -> bool {
    [
        "host",
        "authorization",
        "x-api-key",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "accept-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// 不该回给客户端的响应头：逐跳头、reqwest 已代劳解压后失效的，
/// 以及**由 [`write_head`] 用同一份值显式写出的** `content-type`。
pub fn response_hop_by_hop(name: &str) -> bool {
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

/// 把 accept 出来的连接复位成「阻塞 + 超时」模式。
///
/// 见模块文档第 1 条。复位后 `read()` 会老实等到数据到达或读超时，
/// 「先连后发」时序导致的假 400 与「读超时形同虚设」两个问题一起消失。
pub fn configure_conn(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(HEAD_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
}

/// 读请求的三种结局。
///
/// 必须分开：「收到 0 字节」「请求语法错」「连接一直没发数据」是完全不同的病，
/// 混成一个 `None` 会让线上排查只能靠猜。
///
/// 后两种携带的原始字节与错误类型供诊断用（需要时打日志），
/// 网关目前只区分状态码，因此标了 `allow(dead_code)`。
#[allow(dead_code)]
pub enum HeadRead {
    /// 完整拿到一个请求（连带原始缓冲，供后续切出 body）
    Ready(Request, Vec<u8>),
    /// 请求读完之前对端就断开 / 读失败
    Broken(Vec<u8>),
    /// 读空闲超时；或 socket 仍是非阻塞（一读就 `WouldBlock`）
    Stalled(Vec<u8>, std::io::ErrorKind),
}

/// 读请求头 + 正文，返回已收到的字节供调用方落盘诊断。
pub fn read_head(stream: &mut TcpStream) -> HeadRead {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return HeadRead::Broken(buf), // 对端关闭
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(req) = parse_request(&buf) {
                    if buf.len() >= req.head_end + req.body_len {
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

/// 状态行里的原因短语。客户端的判断依据是数字，短语纯粹给人看的。
pub fn reason_phrase(status: u16) -> &'static str {
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

/// 写响应头。**一律 chunked**：调用方随后用 [`write_chunk`] / [`write_chunk_end`] 收尾。
pub fn write_head(
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
pub fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    stream.write_all(format!("{:X}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// 终止 chunked 流
pub fn write_chunk_end(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// 写一个最简（带 Content-Length 的）HTTP 响应并关闭连接。
///
/// 用于错误体与不流式的 JSON：一段小 body 不必套成 SSE 帧，客户端也少一层解析。
pub fn respond(
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

    fn req(raw: &[u8]) -> Request {
        parse_request(raw).expect("应能解析")
    }

    #[test]
    fn parses_request_head_and_body_length() {
        let raw = b"POST /v1/chat/completions?a=1 HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nX-Request-Id: r1\r\n\r\n{}";
        let r = req(raw);
        assert_eq!(r.method, "POST");
        assert_eq!(r.target, "/v1/chat/completions?a=1");
        assert_eq!(r.path(), "/v1/chat/completions");
        assert_eq!(r.body_len, 2);
        assert_eq!(r.head_end, raw.len() - 2);
        assert_eq!(r.header("x-request-id"), Some("r1"));
        assert_eq!(r.header("X-REQUEST-ID"), Some("r1"));
        assert_eq!(r.body(raw), b"{}");
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
        assert!(hop_by_hop("x-api-key"), "我们自己的 Key 绝不能转发给上游");
        assert!(hop_by_hop("content-length"));
        assert!(!hop_by_hop("Content-Type"));
        assert!(!hop_by_hop("Accept"));
    }

    #[test]
    fn path_strips_query_only() {
        assert_eq!(req(b"GET /v1/models HTTP/1.1\r\n\r\n").path(), "/v1/models");
        assert_eq!(req(b"GET /a?b=c HTTP/1.1\r\n\r\n").path(), "/a");
    }
}
