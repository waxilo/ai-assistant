//! 签到失败告警：**只在有账号签到失败时**推送到用户自配的 webhook 地址。
//!
//! 全部成功 / 今日已签 / 活动未开都不发 —— 日常推送只会在手机上刷成噪音，
//! 真正需要人处理的是「哪个账号失败了、为什么」。失败明细见 [`alert_message`]。
//!
//! 对齐参考脚本 `workbuddy-checkin/workbuddy_checkin.py` 的 `notify_webhook`：
//!   * `GET {webhook}?message=<消息内容>`（query 参数，需 URL 编码）
//!   * **必须带浏览器 User-Agent**——notify-hub 前置的 Cloudflare 会按 UA 拦截
//!     裸 `Python-urllib` / 空 UA，直接返回 `403 error code: 1010`
//!   * 网络抖动自动重试，共 3 次（1.5s / 3s 退避）
//!
//! 站点实测（2026-09-12，notify-hub）：
//!   `?message=…`      → `HTTP 201 {"ok":true,"id":N,"delivered":true}` ✅
//!   `?title=…&body=…` → `HTTP 201 {"ok":true,"id":N,"empty":true}`  ❌ 内容为空
//!   `?content=…`      → `HTTP 201 {"ok":true,"id":N,"empty":true}`  ❌ 内容为空
//! 所以只发 `message`，不要自作聪明换参数名。
//!
//! 通知永远只是「尽力而为」：失败只返回错误字符串，绝不影响签到主流程。
//!
//! # 品牌前缀
//!
//! 用户把**三款助手**（WorkBuddy / TraeWork / Qoder）的通知都接到同一个通道上，
//! 于是每条推送都必须能一眼看出是谁发的。前缀在 [`BRAND`] 定义一次、由 [`send`]
//! **统一施加**：调用点只管写正文，不要自己拼前缀，也就不存在「有的带、有的不带」。

use crate::accounts::{Account, AccountView};
use std::time::Duration;

/// 通知品牌前缀。改名只改这一处（`alert_message` 等文案里不要再写应用名，
/// 否则会变成「【WorkBuddy 助手】WorkBuddy 签到失败」这种重复）。
pub const BRAND: &str = "【WorkBuddy 助手】";

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

/// 给通知正文盖上品牌前缀。
///
/// * 幂等：正文已经以 [`BRAND`] 开头时原样返回，免得出现两个前缀；
/// * 空消息不加：否则会推出去一条只剩前缀、没有任何信息的通知。
pub fn branded(message: &str) -> String {
    let m = message.trim_start();
    if m.is_empty() || m.starts_with(BRAND) {
        m.to_string()
    } else {
        format!("{BRAND}{m}")
    }
}

/// 把消息拼进 webhook 的 query（`?message=…`），负责百分号编码。
///
/// 单独抽出来是为了可单测：中文、空格、`&`/`#` 都必须被正确转义，
/// 否则签到结果里的失败原因会截断 URL。
///
/// 注意它**不加品牌前缀**——那是 [`send`] 的职责，这里保持「拼 URL」的纯粹。
pub fn build_url(webhook: &str, message: &str) -> Result<String, String> {
    let raw = webhook.trim();
    if raw.is_empty() {
        return Err("未配置通知 webhook".to_string());
    }
    let mut url = reqwest::Url::parse(raw).map_err(|e| format!("webhook 地址无效：{e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("webhook 仅支持 http(s) 地址".to_string());
    }
    url.query_pairs_mut().append_pair("message", message);
    Ok(url.to_string())
}

/// 发送一条通知（自动带上品牌前缀）。返回可读的成功描述（含响应体片段）或最终失败原因。
pub async fn send(webhook: &str, message: &str) -> Result<String, String> {
    let url = build_url(webhook, &branded(message))?;
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

/// 「本次触发时刻」那一行，附在失败告警正文之后。
///
/// `plan` 是当天计划时刻（随机窗口生效时即摇出来的值），`base` 是用户配置值，
/// `actual` 是真正跑的那一刻。三者常常互不相等：随机窗口让 `plan != base`，
/// 而**补跑**（错过时刻后 30 分钟内打开应用）让 `actual != plan`。
/// 告警里不带这一行，就看不出今天究竟几点跑的，也就对不上「为什么没签上」。
pub fn trigger_line(actual: &str, plan: &str, base: &str) -> String {
    let (actual, plan, base) = (actual.trim(), plan.trim(), base.trim());
    if plan == base {
        // 没开窗口（或摇号恰好停在设定值上）：只有一个时刻，没什么可交代的
        return format!("触发时刻 {actual}");
    }
    if plan == actual {
        // 随机挑定的时刻就是真实触发时刻（最常见的情况）：同一个时分不必写两遍
        return format!("触发时刻 {actual}（设定 {base} 起随机挑定）");
    }
    // 补跑：过了随机时刻才打开应用。三个值互不相等，全列出来才对得上账
    format!("触发时刻 {actual}（今日随机 {plan}，设定 {base}）")
}

/// 这一轮里**真正失败**的账号（`last` 缺失 = 没执行，也算失败）。
///
/// 「活动未开」不在此列：那是还没到刷新点 / 当天没有可领活动，不是错误。
fn failed_accounts(views: &[AccountView]) -> Vec<&Account> {
    views
        .iter()
        .map(|v| &v.account)
        .filter(|a| match &a.last {
            Some(r) => !r.success && !r.already && !r.inactive,
            None => true,
        })
        .collect()
}

/// 一批账号的**失败告警**正文（品牌前缀由 [`send`] 施加）；没有失败时返回 `None`。
///
/// 这就是「只有失败才告警」的唯一出口：调用点拿到 `None` 就什么都不发。
/// 正文带四个计数做上下文，但明细只列失败账号 —— 活动未开是正常状态，
/// 混进明细只会把真正要处理的那几条挤出前 5 行。
pub fn alert_message(views: &[AccountView]) -> Option<String> {
    // 通知只关心账号本身（签到结果 / 名称 / 手机号），积分与它无关
    let accounts: Vec<&Account> = views.iter().map(|v| &v.account).collect();
    let failed = failed_accounts(views);
    if failed.is_empty() {
        return None;
    }
    let total = accounts.len();
    // 「已签」也算 success=true（幂等成功），但计数时必须与「成功」互斥，
    // 否则一个账号会被同时算进两栏（实测：1 个账号显示「成功 1 / 已签 1」）。
    let already = accounts
        .iter()
        .filter(|a| matches!(&a.last, Some(r) if r.already))
        .count();
    let ok = accounts
        .iter()
        .filter(|a| matches!(&a.last, Some(r) if r.success && !r.already))
        .count();
    let inactive = accounts
        .iter()
        .filter(|a| matches!(&a.last, Some(r) if !r.success && !r.already && r.inactive))
        .count();

    let mut s = format!(
        "签到失败 {} 个：成功 {ok} / 已签 {already} / 活动未开 {inactive}（共 {total} 个账号）",
        failed.len()
    );
    s.push_str("\n失败明细：");
    for a in failed.iter().take(5) {
        let who = match a.phone.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("{}（{}）", a.name, p),
            None => a.name.clone(),
        };
        let why = a
            .last
            .as_ref()
            .map(|r| r.message.clone())
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "未执行".to_string());
        s.push_str(&format!("\n· {who}：{why}"));
    }
    if failed.len() > 5 {
        s.push_str(&format!("\n…另有 {} 个失败账号", failed.len() - 5));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::CheckinRecord;

    /// 造一条「账号 + 积分读数」的视图 —— 通知里只会用到账号那一半，
    /// 所以积分栏留空（这些断言与积分无关）。
    fn acct(name: &str, phone: Option<&str>, rec: Option<CheckinRecord>) -> AccountView {
        AccountView {
            account: Account {
                id: name.to_string(),
                name: name.to_string(),
                phone: phone.map(|p| p.to_string()),
                token: "t".into(),
                refresh_token: None,
                shared_token: false,
                expires_at: None,
                rt_expires_at: None,
                base_url: None,
                created_at: String::new(),
                checked_today: None,
                invalidated_at: None,
                renew_blocked_until: None,
                last: rec,
            },
            credits: None,
            origin: "local",
        }
    }

    fn rec(success: bool, already: bool, inactive: bool, msg: &str) -> CheckinRecord {
        CheckinRecord {
            success,
            already,
            inactive,
            message: msg.to_string(),
            credit: None,
            balance: None,
            streak: None,
            host: None,
            at: String::new(),
            code: None,
        }
    }

    #[test]
    fn builds_query_with_message_param_only() {
        let u = build_url("https://hub.example/hook/abc", "签到成功").unwrap();
        // 只应出现 message，不要 title/body（站点不认，会存成空内容）
        assert!(u.contains("message="), "{u}");
        assert!(!u.contains("title="), "{u}");
        assert!(!u.contains("body="), "{u}");
        assert!(u.starts_with("https://hub.example/hook/abc?"), "{u}");
    }

    #[test]
    fn percent_encodes_tricky_characters() {
        // 中文 + 空格 + & + # 都必须被转义，否则 URL 会在这些字符处被截断
        let u = build_url("https://h/x", "失败 1&2 #tag").unwrap();
        assert!(!u.contains(' '), "{u}");
        // `Url::query_pairs_mut` 用表单编码，空格写成 `+`（与 Python 的 urlencode 一致）
        assert!(u.contains("+") || u.contains("%20"), "{u}");
        assert!(u.contains("%26"), "& 应编码：{u}");
        assert!(u.contains("%23"), "# 应编码：{u}");
        // 解码回来应与原文一致
        let parsed = reqwest::Url::parse(&u).unwrap();
        let got: Vec<_> = parsed.query_pairs().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "message");
        assert_eq!(got[0].1, "失败 1&2 #tag");
    }

    #[test]
    fn keeps_existing_query_params() {
        let u = build_url("https://h/x?k=v", "m").unwrap();
        let parsed = reqwest::Url::parse(&u).unwrap();
        let pairs: Vec<_> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(pairs, vec![("k".into(), "v".into()), ("message".into(), "m".into())]);
    }

    #[test]
    fn rejects_empty_and_non_http_webhook() {
        assert!(build_url("", "m").unwrap_err().contains("未配置"));
        assert!(build_url("   ", "m").unwrap_err().contains("未配置"));
        assert!(build_url("ftp://h/x", "m").unwrap_err().contains("http"));
        assert!(build_url("not a url", "m").unwrap_err().contains("无效"));
    }

    /// 三款助手共用同一个通知通道，每条推送都要能一眼看出是谁发的。
    #[test]
    fn brands_every_message_exactly_once() {
        assert_eq!(branded("签到失败"), "【WorkBuddy 助手】签到失败");
        // 幂等：调用点万一自己写了前缀，也不该变成两个
        assert_eq!(
            branded("【WorkBuddy 助手】签到失败"),
            "【WorkBuddy 助手】签到失败"
        );
        // 空消息不硬凑一条只剩前缀、没有信息量的推送
        assert_eq!(branded(""), "");
        assert_eq!(branded("   "), "");
    }

    /// 前缀必须真的落进发出去的 query（顺带覆盖中文编码后的形态）。
    #[test]
    fn brand_reaches_the_webhook_query() {
        let alert = alert_message(&[acct("a", None, None)]).unwrap();
        let u = build_url("https://h/x", &branded(&alert)).unwrap();
        let parsed = reqwest::Url::parse(&u).unwrap();
        let msg = parsed
            .query_pairs()
            .find(|(k, _)| k == "message")
            .map(|(_, v)| v.to_string())
            .unwrap();
        assert!(msg.starts_with("【WorkBuddy 助手】"), "{msg}");
        assert!(msg.contains("签到失败"), "{msg}");
    }

    /// 正文自己带品牌词的话，前缀一加就成了「【X 助手】X 签到失败」——
    /// 文案里不许再出现应用名（前缀是唯一的品牌出口）。
    #[test]
    fn alert_text_does_not_repeat_the_brand() {
        let m = alert_message(&[acct("a", None, Some(rec(false, false, false, "HTTP 401")))])
            .unwrap();
        assert!(!m.contains("WorkBuddy"), "{m}");
        assert!(m.starts_with("签到失败"), "{m}");
    }

    /// 核心契约：**没有失败就一条都不发**。全成功、已签、活动未开都不算失败。
    #[test]
    fn no_failure_means_no_notification() {
        let accounts = vec![
            acct("a", Some("138"), Some(rec(true, false, false, "签到成功"))),
            acct("b", None, Some(rec(false, true, false, "今天已签到"))),
            acct("c", None, Some(rec(false, false, true, "活动未开启"))),
        ];
        assert!(alert_message(&accounts).is_none());
        assert!(alert_message(&[]).is_none());
    }

    /// 「今天已签到」（success 与 already 同为 true）单独出现时同样静音；
    /// 有失败时计数必须互斥，否则 1 个账号会显示成「成功 1 / 已签 1」。
    #[test]
    fn already_claimed_is_silent_and_never_double_counted() {
        let already = acct(
            "waxiloao",
            Some("190****9775"),
            Some(rec(true, true, false, "今天已签到，请明天再来")),
        );
        assert!(alert_message(&[already]).is_none());

        let m = alert_message(&[
            acct("waxiloao", None, Some(rec(true, true, false, "今天已签到"))),
            acct("小号", None, Some(rec(false, false, false, "HTTP 500"))),
        ])
        .unwrap();
        assert!(m.contains("签到失败 1 个：成功 0 / 已签 1 / 活动未开 0"), "{m}");
    }

    #[test]
    fn alert_lists_failures_with_phone_and_reason() {
        let accounts = vec![
            acct("主号", Some("190****9775"), Some(rec(true, false, false, "签到成功"))),
            acct("小号", Some("138****0000"), Some(rec(false, false, false, "HTTP 401 (token expired)"))),
            acct("无记录", None, None),
        ];
        let m = alert_message(&accounts).unwrap();
        assert!(m.contains("签到失败 2 个：成功 1 / 已签 0 / 活动未开 0（共 3 个账号）"), "{m}");
        assert!(m.contains("失败明细"), "{m}");
        assert!(m.contains("小号（138****0000）：HTTP 401 (token expired)"), "{m}");
        // last 为 None 的账号算失败，原因写「未执行」
        assert!(m.contains("无记录：未执行"), "{m}");
    }

    /// 「活动未开」只做计数上下文，不进失败明细：它是正常状态，
    /// 混进来只会把真正要处理的失败挤出前 5 行。
    #[test]
    fn inactive_is_context_not_a_failure_detail() {
        let m = alert_message(&[
            acct("主号", Some("190****9775"), Some(rec(true, false, false, "已领取 100 积分"))),
            acct("小号", Some("138****0000"), Some(rec(false, false, true, "活动未开启"))),
            acct("三号", Some("137****0000"), Some(rec(false, false, false, "HTTP 500"))),
        ])
        .unwrap();
        assert!(m.contains("签到失败 1 个：成功 1 / 已签 0 / 活动未开 1（共 3 个账号）"), "{m}");
        assert!(!m.contains("小号"), "活动未开不该出现在失败明细：{m}");
        assert!(m.contains("三号（137****0000）：HTTP 500"), "{m}");
    }

    /// 计数覆盖全部账号：失败 + 成功 + 已签 + 活动未开 + 未执行 == 总数。
    #[test]
    fn counts_cover_every_account() {
        let accounts = vec![
            acct("a", Some("1"), Some(rec(true, false, false, "签到成功"))),
            acct("b", Some("2"), Some(rec(true, true, false, "今天已签到"))),
            acct("c", Some("3"), Some(rec(false, false, true, "活动未开启"))),
            acct("d", Some("4"), Some(rec(false, false, false, "HTTP 500"))),
            acct("e", Some("5"), None),
        ];
        let m = alert_message(&accounts).unwrap();
        assert!(m.contains("签到失败 2 个：成功 1 / 已签 1 / 活动未开 1（共 5 个账号）"), "{m}");
    }

    /// 随机窗口把「设定」与「今天真正的时刻」拉开了，通知里必须两个都写出来。
    #[test]
    fn trigger_line_spells_out_the_random_time() {
        // 随机挑定、准点跑：说明「为什么不是设的 08:00」就够了
        let line = trigger_line("09:42", "09:42", "08:00");
        assert_eq!(line, "触发时刻 09:42（设定 08:00 起随机挑定）");

        // 补跑：真实时刻、随机时刻、设定时刻三者互不相等，全部列出
        let line = trigger_line("10:05", "09:42", "08:00");
        assert!(line.contains("触发时刻 10:05"), "{line}");
        assert!(line.contains("今日随机 09:42"), "{line}");
        assert!(line.contains("设定 08:00"), "{line}");

        // 没开窗口（或摇号恰好落在设定值上）：只有一个时刻，别硬凑出「随机 08:00」
        assert_eq!(trigger_line("08:00", "08:00", "08:00"), "触发时刻 08:00");
    }

    /// 本机冒烟：真的往 webhook 发一条（会收到真实推送）。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_webhook_delivery() {
        let hook = "http://127.0.0.1:7002/hook/z9sm8jfJNpWwfWsfGW1xlRiFV8t-t6WD";
        let out = send(hook, "【测试】通知链路自检")
            .await
            .unwrap_or_else(|e| panic!("webhook 发送失败: {e}"));
        // 站点成功响应形如 {"ok":true,"id":N,"delivered":true}
        assert!(out.contains("\"ok\":true"), "响应异常：{out}");
        assert!(out.contains("\"delivered\":true"), "未投递：{out}");
        println!("webhook ok → {out}");
    }
}
