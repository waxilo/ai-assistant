//! 网关鉴权。
//!
//! 与助手的接管反代**本质不同**：那个端口装扮成官方网关给本机 CLI 用，不能加 Key；
//! 这个端口是我们自己的 API，**每个请求都必须带 Key**（没有本机豁免），
//! 否则同机/同网段的任何人都能烧掉账号池。
//!
//! 比较用常量时间：`==` 会在第一个不同字节处返回，理论上能按耗时逐字节猜出 Key。
//! 本机场景风险极低，但这条成本只有几行代码，没有理由省。

use crate::httpsrv::Request;

/// 从请求里取 Key：优先 `Authorization: Bearer xxx`，其次 `x-api-key`。
pub fn extract_key(req: &Request) -> Option<&str> {
    if let Some(v) = req.header("authorization") {
        let v = v.trim();
        if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
            let token = v[7..].trim();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    req.header("x-api-key").map(str::trim).filter(|s| !s.is_empty())
}

/// 常量时间比较。长度不同直接 false（长度本身不是秘密）。
pub fn key_matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with(headers: &str) -> Request {
        let raw = format!("POST /v1/chat/completions HTTP/1.1\r\n{headers}\r\n");
        crate::httpsrv::parse_request(raw.as_bytes()).unwrap()
    }

    #[test]
    fn reads_bearer_token() {
        let r = req_with("Authorization: Bearer sk-wb-abc\r\n");
        assert_eq!(extract_key(&r), Some("sk-wb-abc"));
    }

    #[test]
    fn bearer_prefix_is_case_insensitive_and_trimmed() {
        let r = req_with("Authorization: bearer   sk-wb-abc   \r\n");
        assert_eq!(extract_key(&r), Some("sk-wb-abc"));
    }

    #[test]
    fn falls_back_to_x_api_key() {
        let r = req_with("x-api-key: sk-wb-def\r\n");
        assert_eq!(extract_key(&r), Some("sk-wb-def"));
    }

    #[test]
    fn missing_or_malformed_key_is_none() {
        assert_eq!(extract_key(&req_with("Host: x\r\n")), None);
        assert_eq!(extract_key(&req_with("Authorization: sk-wb-abc\r\n")), None);
        assert_eq!(extract_key(&req_with("Authorization: Bearer \r\n")), None);
    }

    #[test]
    fn key_comparison_rejects_prefix_and_length_mismatch() {
        assert!(key_matches("sk-wb-abc", "sk-wb-abc"));
        assert!(!key_matches("sk-wb-abc", "sk-wb-ab"));
        assert!(!key_matches("sk-wb-abc", "sk-wb-abd"));
        assert!(!key_matches("", ""), "空 Key 必须恒不通过");
    }
}
