//! 签到线：Qoder 的签到 = 领取「活动权益」里那条每日 `CLAIM_BENEFIT`。
//!
//! # 为什么整个文件重写了
//!
//! 上一版是从 CodeBuddy 模板带过来的：`/billing/meter/daily-checkin`、
//! `/v2/billing/meter/checkin-status`、用 JWT 的 `iss` 推 host、
//! `PackageCode/ResourceId` 资源包解析 —— 这些在 Qoder 上**一个都不成立**。
//!
//! Qoder 真正的形态是「每日一条活动」：`GET /sash/api/v1/me/campaigns` 看状态，
//! `POST …/{campaignId}/claim` 领取，每天 10:00（UTC+8）刷新、每次 100 Credits、
//! 领取后 30 天有效。全部结论都有真实请求佐证，见
//! `basedata/20260918_Qoder活动权益接口逆向.md`（`basedata/20260918_Qoder缺失接口逆向.md`
//! 第 4 节里「Qoder 没有签到」的旧结论是错的，已在原处更正）。
//!
//! 因此这里**不再有「候选 host × 候选 path」的枚举**：Qoder 只有一个域、接口也只有一个。
//! 留一整套枚举兜底只会把「今天没领到」伪装成「接口漂移」，掩盖真正的原因
//! ——而且固定顺序、零间隔地枚举端点，本身就是扫描器的形态。
//!
//! # 额度读数的唯一入口仍在这里
//!
//! [`fetch_resource_view`] 是 `commands` / `proxy` 共用的额度读数入口，内部走
//! [`crate::usage::fetch_usage`]。它与签到是两件事，只是历史上长在同一个文件里；
//! 这次重构只把它身上那个**恒被忽略的 `host` 参数**摘掉了。

use crate::accounts::{Account, CheckinRecord};
use crate::qoder_api::{self, Campaign, CampaignView};
use crate::usage::ResourceView;

/// 一次拉取「剩余积分 + 最早过期时间 + 逐包明细」—— **额度读数的唯一入口**。
///
/// `commands`（刷新 / 采样 / 批量签到后的补读）与 `proxy`（路由前补拉）都调它，
/// 再交给 `ledger` 落同一份盘。
///
/// **为什么不再收 `host`**：那个参数是 CodeBuddy 时代「按 token 的 `iss` 猜域」的残留
/// （旧版 `candidate_hosts`），在 Qoder 上三个调用方传进来的值**一个都没被用过**。
/// 一个恒被忽略的参数比没有参数更糟：它让下一个人以为「这里可以选域」，
/// 而实际上打到哪个域只由 [`crate::qoder_api::OPENAPI_BASE`] 决定。
pub async fn fetch_resource_view(token: &str) -> ResourceView {
    crate::usage::fetch_usage(token).await
}

/// 只读查询「今天领没领」。
///
/// 判据就是活动自己的 `claimStatus` —— 不再有第二套兜底字段。旧版列了
/// `today_checked_in` / `last_checkin_date` 等一串猜测键名，那是「不知道后端长什么样」
/// 时的写法；现在后端长什么样是确定的。
///
/// `CLAIMED` → 已领、`CLAIMABLE` → 未领，**其余状态回 `None`**：
/// 「今天没有这条活动」「活动已过期」「查询失败」都不等于「没领」，
/// 编一个布尔值出去只会让界面显示一个假状态。
pub async fn query_checked_today(account: &Account) -> Option<bool> {
    let view = qoder_api::fetch_campaigns(&account.token).await?;
    checked_from_status(&view.daily_claim()?.claim_status)
}

/// `claimStatus` → 「今天已领?」。抽成纯函数是为了把三个分支钉在单测里。
fn checked_from_status(status: &str) -> Option<bool> {
    match status {
        "CLAIMED" => Some(true),
        "CLAIMABLE" => Some(false),
        // 其余（过期、不可领、服务端新加的状态）一律「不知道」，绝不猜
        _ => None,
    }
}

/// 判定树的结论。
enum Plan<'a> {
    /// 现在可领 → 领它
    Claim(&'a Campaign),
    /// 今天已经领过
    Done,
    /// 当前没有可领的活动（`String` 是给人看的原因）
    Inactive(String),
}

/// 纯判定：给定活动状态，决定「去领哪一条」或「结论已经是什么」。
///
/// 抽成纯函数是为了让整棵判定树**脱网可测**：这里的每个分支都对应一种真实响应 ——
/// 可领、已领、活动还没生成、活动已过期、只有别的运营活动（`VIEW_DETAILS`）。
///
/// 注意顶层 `claimable` 与条目的 `claimStatus` 是同一件事的两种表达，这里**只认条目本身**：
/// 顶层标志是给桌面端「要不要自动弹窗」用的，在有别的活动时也会是 `false`，
/// 拿它当「今天领没领」会误判。
fn plan(view: &CampaignView) -> Plan<'_> {
    if let Some(c) = view.claimable_campaign() {
        return Plan::Claim(c);
    }
    match view.daily_claim() {
        Some(c) if c.claim_status == "CLAIMED" => Plan::Done,
        Some(c) => Plan::Inactive(format!("今日活动暂不可领（claimStatus={}）", c.claim_status)),
        // 当日那条根本不在了：最常见的解释是还没到刷新点（10:00 UTC+8 生成，可领 24 小时）
        None => Plan::Inactive("当前没有可领取的活动（每日 10:00 UTC+8 刷新后才会出现）".into()),
    }
}

/// 对一个账号执行签到（= 领取当天的活动权益）。
///
/// ⚠️ **这是写操作**：会真的把额度领到账号上，也可能因重复而拿到 409。
/// 所以它**只尝试一次、从不自动重试** —— 「太频繁（429）」和「已领（409）」反复重打
/// 既是风控信号，也没有任何意义（想再确认状态，只读接口就够了）。
///
/// 四种落定状态（对应 [`CheckinRecord`] 的语义）：
/// - `success && already`：今天已经领过
/// - `success`：本次领取成功，`credit` 就是领到的额度
/// - `inactive`：当前没有可领的活动
/// - 都不是：真失败（网络 / 鉴权 / 服务端拒绝），`message` 里带原因
pub async fn do_checkin(account: &Account) -> CheckinRecord {
    let at = now();
    let Some(view) = qoder_api::fetch_campaigns(&account.token).await else {
        return CheckinRecord {
            message: "活动接口不可用（网络或登录态异常）".into(),
            ..blank(&at)
        };
    };
    match plan(&view) {
        Plan::Done => CheckinRecord {
            success: true,
            already: true,
            message: "今日已领取".into(),
            ..blank(&at)
        },
        Plan::Inactive(why) => CheckinRecord {
            inactive: true,
            message: why,
            ..blank(&at)
        },
        Plan::Claim(c) => claim(account, c, &at).await,
    }
}

/// 真正的那一下 `POST …/{campaignId}/claim`，外加一次余额补读。
async fn claim(account: &Account, c: &Campaign, at: &str) -> CheckinRecord {
    let amount = c.benefit.as_ref().map(|b| b.amount);
    let mut rec = match qoder_api::claim_campaign(&account.token, &c.id).await {
        Ok(()) => CheckinRecord {
            success: true,
            message: match amount {
                Some(a) => format!("已领取 {} Credits", trim_num(a)),
                None => "已领取".to_string(),
            },
            credit: amount,
            campaign_key: Some(c.key.clone()),
            ..blank(at)
        },
        Err(e) => {
            // 失败之后**复看一次状态**：409 常常意味着「另一个客户端刚把它领走了」，
            // 那一刻这条活动已经变成 CLAIMED。用事实判断「是不是已领」，
            // 比按错误码猜「409 一定是已领」可靠 —— 后者会把真正的「不可领取」也报成已领。
            if claimed_afterwards(account, &c.id).await {
                CheckinRecord {
                    success: true,
                    already: true,
                    message: "今日已领取（领取时已被认领）".into(),
                    campaign_key: Some(c.key.clone()),
                    ..blank(at)
                }
            } else {
                CheckinRecord {
                    message: format!("领取失败：{e}"),
                    campaign_key: Some(c.key.clone()),
                    ..blank(at)
                }
            }
        }
    };
    // 领取成功 → 立刻补一次额度读数，让台账马上能看到这一笔。
    // best-effort：读不到就留空，界面显示「—」而不是谎报 0。
    if rec.success {
        rec.balance = fetch_resource_view(&account.token).await.credits;
    }
    rec
}

/// 复查某条活动是否已经变成 `CLAIMED`（只读；查询失败即 `false`）。
async fn claimed_afterwards(account: &Account, campaign_id: &str) -> bool {
    qoder_api::fetch_campaigns(&account.token)
        .await
        .and_then(|v| v.campaigns.into_iter().find(|c| c.id == campaign_id))
        .is_some_and(|c| c.claim_status == "CLAIMED")
}

/// 一条空记录（`at` 已填）—— 让上面每个分支只写自己那几个字段。
fn blank(at: &str) -> CheckinRecord {
    CheckinRecord {
        success: false,
        already: false,
        inactive: false,
        message: String::new(),
        credit: None,
        balance: None,
        campaign_key: None,
        at: at.to_string(),
    }
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 整数额度不带小数点（`100` 而不是 `100.00`），非整数保留两位。
fn trim_num(n: f64) -> String {
    if (n - n.round()).abs() < f64::EPSILON {
        format!("{}", n.round() as i64)
    } else {
        format!("{n:.2}")
    }
}

#[cfg(test)]
mod tests {
    use crate::qoder_api::CampaignBenefit;

    use super::*;

    /// 造一条活动：`CLAIM_BENEFIT` + CREDITS 权益（即「每日一领」那条）
    fn daily(id: &str, key: &str, status: &str) -> Campaign {
        Campaign {
            id: id.into(),
            key: key.into(),
            action_type: "CLAIM_BENEFIT".into(),
            claim_status: status.into(),
            start_at: 1_789_696_500,
            end_at: 1_789_783_140,
            benefit: Some(CampaignBenefit {
                kind: "CREDITS".into(),
                amount: 100.0,
                validity_mode: "RELATIVE_DAYS".into(),
                validity_days: 30,
                fixed_end: String::new(),
            }),
            title: "每天领 100 Credits".into(),
            description: String::new(),
            button_text: String::new(),
            detail_url: String::new(),
        }
    }

    /// 造一条只跳详情的运营活动（没有 benefit）
    fn details(id: &str, status: &str) -> Campaign {
        Campaign {
            id: id.into(),
            key: "act-20260901-493".into(),
            action_type: "VIEW_DETAILS".into(),
            claim_status: status.into(),
            start_at: 0,
            end_at: 0,
            benefit: None,
            title: "9月限时福利".into(),
            description: String::new(),
            button_text: String::new(),
            detail_url: String::new(),
        }
    }

    fn view(campaigns: Vec<Campaign>) -> CampaignView {
        CampaignView {
            show_campaign: true,
            claimable: campaigns.iter().any(|c| c.is_claimable()),
            campaign_url: "https://openapi.qoder.sh/growth-page/activity-iframe".into(),
            campaigns,
        }
    }

    #[test]
    fn checked_status_maps_only_the_two_known_states() {
        assert_eq!(checked_from_status("CLAIMED"), Some(true));
        assert_eq!(checked_from_status("CLAIMABLE"), Some(false));
        // 没见过 / 已过期 / 不可领 → 不知道，绝不猜
        assert_eq!(checked_from_status("EXPIRED"), None);
        assert_eq!(checked_from_status("NOT_ELIGIBLE"), None);
        assert_eq!(checked_from_status(""), None);
    }

    #[test]
    fn plan_claims_the_claimable_campaign() {
        let v = view(vec![details("d1", "CLAIMED"), daily("c1", "act-1", "CLAIMABLE")]);
        match plan(&v) {
            Plan::Claim(c) => assert_eq!(c.id, "c1"),
            _ => panic!("应挑出可领的那条"),
        }
    }

    #[test]
    fn plan_reports_already_claimed_as_done() {
        let v = view(vec![daily("c1", "act-1", "CLAIMED")]);
        assert!(matches!(plan(&v), Plan::Done));
    }

    /// 回归：**「今天没有活动」不等于「已领」**。
    ///
    /// 这是本文件最容易犯、且犯了不报错的错：把「没有可领的活动」当成已签，
    /// 状态列会一直显示「今日已签到」，而实际上一次都没领到 —— 断签了也看不出来。
    /// 同理，「只有别的运营活动」也不是已领。
    #[test]
    fn plan_reports_no_campaign_as_inactive_never_done() {
        let empty = view(vec![]);
        assert!(matches!(plan(&empty), Plan::Inactive(_)));

        // 只有 VIEW_DETAILS 那条（真实响应里就有这种）
        let only_details = view(vec![details("d1", "CLAIMED")]);
        match plan(&only_details) {
            Plan::Inactive(why) => assert!(why.contains("10:00"), "应把刷新点告诉用户：{why}"),
            other => panic!("不该被当成已领：{}", matches!(other, Plan::Done)),
        }

        // 当日那条存在但状态既不是 CLAIMABLE 也不是 CLAIMED
        let odd = view(vec![daily("c1", "act-1", "EXPIRED")]);
        match plan(&odd) {
            Plan::Inactive(why) => assert!(why.contains("EXPIRED"), "要把真实状态带出来：{why}"),
            _ => panic!("未知状态不该被当成已领"),
        }
    }

    /// 可领那一支优先于「已领」的判定：同时存在已领的日签与另一条可领活动时，
    /// 应当去领可领的那条，而不是看日签已领就收工。
    #[test]
    fn plan_prefers_claimable_over_the_already_claimed_daily() {
        let v = view(vec![
            daily("c1", "act-1", "CLAIMED"),
            daily("c2", "act-2", "CLAIMABLE"),
        ]);
        match plan(&v) {
            Plan::Claim(c) => assert_eq!(c.id, "c2"),
            _ => panic!("有可领的就该去领"),
        }
    }

    #[test]
    fn blank_record_is_an_empty_failure_not_a_silent_success() {
        let r = blank("2026-09-18 10:00:00");
        assert!(!r.success && !r.already && !r.inactive);
        assert!(r.credit.is_none() && r.balance.is_none() && r.campaign_key.is_none());
        assert_eq!(r.at, "2026-09-18 10:00:00");
    }

    #[test]
    fn trim_num_keeps_integers_clean() {
        assert_eq!(trim_num(100.0), "100");
        assert_eq!(trim_num(100.5), "100.50");
    }

    /// 真实接口冒烟：确认活动接口能解析、并打印当前状态（**只读，绝不发 claim**）。
    /// 运行：`cargo test --lib -- --ignored --nocapture smoke_real_campaigns_endpoint`
    #[tokio::test]
    #[ignore = "真实网络调用，需本机已登录 Qoder"]
    async fn smoke_real_campaigns_endpoint() {
        let list = crate::auth_file::discover_local_accounts();
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        // 这里直接打接口（而不是 `query_campaigns`）：本机登录文件给的是 `LocalAccount`，
        // 与落盘的 `Account` 不是一个类型，冒烟测试没有理由为了一个 token 去造后者
        let v = qoder_api::fetch_campaigns(&a.token)
            .await
            .expect("活动接口应可用");
        println!(
            "showCampaign={} claimable={} 今日已领={:?}",
            v.show_campaign,
            v.claimable,
            checked_from_status(&v.daily_claim().map(|c| c.claim_status.clone()).unwrap_or_default())
        );
        for c in &v.campaigns {
            println!("  - {} key={} action={} status={}", c.id, c.key, c.action_type, c.claim_status);
        }
    }

    /// 真实接口冒烟：额度读数（重构后 `fetch_resource_view` 少了一个参数，确认仍可用）。
    /// 运行：`cargo test --lib -- --ignored --nocapture smoke_real_credits_endpoint`
    #[tokio::test]
    #[ignore = "真实网络调用，需本机已登录 Qoder"]
    async fn smoke_real_credits_endpoint() {
        let list = crate::auth_file::discover_local_accounts();
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        let v = fetch_resource_view(&a.token).await;
        println!(
            "剩余={:?} 最早到期={:?} 包数={}",
            v.credits,
            v.earliest_expiry_ms,
            v.packages.len()
        );
        assert!(v.credits.is_some(), "应能解析出剩余积分");
    }
}
