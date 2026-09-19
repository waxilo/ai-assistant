//! 通知/告警：把签到结果与积分简报推送到用户自配的 webhook（失败只记日志，
//! 绝不影响主流程）。
//!
//! # Webhook 形状
//! `GET {webhook}?title=<url-encoded>&message=<url-encoded>`
//!
//! 标题与正文**分开传**：仅把正文塞进 `message` 时，多数中转（含自建 Notify Hub、
//! qmsg 类）会把整段正文当成标题，于是通知标题变成一长串状态汇总——这就是之前
//! 「标题有问题」的原因。现在显式给出精简 title（如「签到」）。
//!
//! 必须带浏览器 UA（Cloudflare 前置会对脚本类 UA 返回 403）；最多重试 3 次
//! （1.5s / 3s 退避）。
//!
//! # 品牌前缀
//!
//! 用户把**三款助手**（WorkBuddy / TraeWork / Qoder）的通知都接到同一个通道上，
//! 于是每条推送都必须能一眼看出是谁发的。前缀在 [`BRAND`] 定义一次、由 [`send`]
//! 对 **title 与 message 分别**施加：调用点只管写正文，不要自己拼前缀，
//! 也就不存在「有的带、有的不带」。

use std::time::Duration;

/// 通知品牌前缀。改名只改这一处（[`summary_title`] 等文案里不要再写应用名，
/// 否则会变成「【TraeWork 助手】TraeWork 签到成功」这种重复）。
pub const BRAND: &str = "【TraeWork 助手】";

/// 普通浏览器 UA。Cloudflare 按 UA 拦截脚本类客户端，缺了它必然 403。
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                          AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_ATTEMPTS: u32 = 3;

fn excerpt(s: &str) -> String {
    let t = s.trim();
    let cut: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 给一段通知文案盖上品牌前缀。
///
/// * 幂等：已经以 [`BRAND`] 开头时原样返回，免得出现两个前缀；
/// * 空文案不加：否则会推出去一条只剩前缀、没有任何信息量的通知，
///   也会让 [`build_url`] 里「title 为空就不带该参数」的判断失效。
pub fn branded(text: &str) -> String {
    let t = text.trim_start();
    if t.is_empty() || t.starts_with(BRAND) {
        t.to_string()
    } else {
        format!("{BRAND}{t}")
    }
}

/// 把 title / message 拼进 webhook 的 query，负责百分号编码。
/// title 为空时只带 message。
///
/// 注意它**不加品牌前缀**——那是 [`send`] 的职责，这里保持「拼 URL」的纯粹。
pub fn build_url(webhook: &str, title: &str, message: &str) -> Result<String, String> {
    let raw = webhook.trim();
    if raw.is_empty() {
        return Err("未配置 webhook 地址".into());
    }
    let mut url = reqwest::Url::parse(raw).map_err(|e| format!("webhook 地址无效：{e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("webhook 仅支持 http(s) 地址".into());
    }
    {
        let mut q = url.query_pairs_mut();
        if !title.trim().is_empty() {
            q.append_pair("title", title.trim());
        }
        q.append_pair("message", message);
    }
    Ok(url.to_string())
}

/// 发送一条通知（title / message 各自自动带品牌前缀）。
/// 返回可读的成功描述（含响应体片段）或最终失败原因。
pub async fn send(webhook: &str, title: &str, message: &str) -> Result<String, String> {
    let url = build_url(webhook, &branded(title), &branded(message))?;
    let client = reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))?;

    let mut last_err = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let b = excerpt(&body);
                    return Ok(if b.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        format!("HTTP {} {}", status.as_u16(), b)
                    });
                }
                last_err = format!("HTTP {} {}", status.as_u16(), excerpt(&body));
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(1500 * attempt as u64)).await;
        }
    }
    Err(format!("已重试 {MAX_ATTEMPTS} 次仍失败：{last_err}"))
}

/// 通知标题：按结果给一个短标题（品牌前缀由 [`send`] 施加），
/// 避免正文被当成标题。
pub fn summary_title(ok: usize, already: usize, failed: usize) -> String {
    if failed > 0 {
        format!("签到异常（{failed} 个失败）")
    } else if ok > 0 {
        format!("签到成功（{ok} 个）")
    } else if already > 0 {
        "签到：今日已签".to_string()
    } else {
        "签到".to_string()
    }
}

/// 把一次定时签到的计数汇总成一条通知正文（不含标题，品牌前缀由 [`send`] 施加）。
pub fn summary_message(ok: usize, already: usize, failed: &[String]) -> String {
    let total = ok + already + failed.len();
    let mut s = format!(
        "成功 {ok} / 已签 {already} / 失败 {}（共 {total} 个账号）",
        failed.len()
    );
    if !failed.is_empty() {
        s.push_str("\n失败明细：");
        for f in failed.iter().take(5) {
            s.push_str(&format!("\n· {f}"));
        }
        if failed.len() > 5 {
            s.push_str(&format!("\n…另有 {} 个失败账号", failed.len() - 5));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三款助手共用同一个通知通道，每条推送都要能一眼看出是谁发的。
    #[test]
    fn brands_every_text_exactly_once() {
        assert_eq!(branded("签到"), "【TraeWork 助手】签到");
        // 幂等：调用点万一自己写了前缀，也不该变成两个
        assert_eq!(branded("【TraeWork 助手】签到"), "【TraeWork 助手】签到");
        // 空文案不加前缀：否则 title 的非空判断会被一个「只有前缀的标题」骗过
        assert_eq!(branded(""), "");
        assert_eq!(branded("   "), "");
    }

    /// 前缀必须真的落进发出去的 query：title 与 message 各自一份。
    #[test]
    fn brand_reaches_both_title_and_message() {
        let url = build_url(
            "https://h/x",
            &branded(&summary_title(1, 0, 0)),
            &branded(&summary_message(1, 0, &[])),
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        assert_eq!(pairs[0].0, "title");
        assert!(pairs[0].1.starts_with("【TraeWork 助手】"), "{pairs:?}");
        assert_eq!(pairs[1].0, "message");
        assert!(pairs[1].1.starts_with("【TraeWork 助手】"), "{pairs:?}");
    }

    /// 标题/正文里不许再出现应用名，否则前缀一加就成了「【TraeWork 助手】TraeWork 签到成功」。
    #[test]
    fn summary_text_does_not_repeat_the_brand() {
        for t in [
            summary_title(1, 0, 0),
            summary_title(0, 1, 0),
            summary_title(0, 0, 1),
            summary_title(0, 0, 0),
        ] {
            assert!(!t.contains("TraeWork"), "{t}");
        }
        assert!(!summary_message(1, 0, &[]).contains("TraeWork"));
    }

    #[test]
    fn title_reflects_the_worst_case() {
        assert_eq!(summary_title(3, 0, 0), "签到成功（3 个）");
        assert_eq!(summary_title(0, 2, 0), "签到：今日已签");
        // 有失败就报失败，成功数不该把这个信息盖掉
        assert_eq!(summary_title(3, 0, 1), "签到异常（1 个失败）");
    }

    #[test]
    fn empty_title_is_dropped_from_the_query() {
        let url = build_url("https://h/x", "", &branded("签到")).unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert_eq!(pairs[0].0, "message");
    }

    #[test]
    fn keeps_existing_query_params_and_rejects_bad_webhook() {
        let url = build_url("https://h/x?k=v", "签到", "成功 1").unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("k".into(), "v".into()),
                ("title".into(), "签到".into()),
                ("message".into(), "成功 1".into()),
            ]
        );

        assert!(build_url("", "t", "m").unwrap_err().contains("未配置"));
        assert!(build_url("ftp://h/x", "t", "m").unwrap_err().contains("http"));
        assert!(build_url("not a url", "t", "m").unwrap_err().contains("无效"));
    }

    #[test]
    fn summary_lists_failures() {
        let m = summary_message(1, 1, &["小号：HTTP 401".to_string()]);
        assert!(m.contains("成功 1 / 已签 1 / 失败 1（共 3 个账号）"), "{m}");
        assert!(m.contains("· 小号：HTTP 401"), "{m}");
    }

    /// 本机冒烟：真的往 webhook 发一条（会收到真实推送）。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_webhook_delivery() {
        let hook = "https://notify-hub-worker.sloan.dpdns.org/hook/z9sm8jfJNpWwfWsfGW1xlRiFV8t-t6WD";
        let out = send(hook, "通知链路自检", "【测试】通知配置正常")
            .await
            .unwrap_or_else(|e| panic!("webhook 发送失败: {e}"));
        assert!(out.contains("\"ok\":true"), "响应异常：{out}");
        assert!(out.contains("\"delivered\":true"), "未投递：{out}");
        println!("webhook ok → {out}");
    }
}
