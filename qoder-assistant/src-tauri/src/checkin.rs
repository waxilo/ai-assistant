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
use crate::region::Region;
use crate::usage::ResourceView;

/// 一次拉取「剩余积分 + 最早过期时间 + 逐包明细」—— **额度读数的唯一入口**。
///
/// `commands`（刷新 / 采样 / 批量签到后的补读）与 `proxy`（路由前补拉）都调它，
/// 再交给 `ledger` 落同一份盘。
///
/// **为什么要收 `region`**：上一版这里收的是一个**恒被忽略**的 `host` 参数
/// （CodeBuddy 时代「按 token 的 `iss` 猜域」的残留）。现在域真的有两个了，
/// 但它**依然不该由调用方猜** —— 传进来的必须是 [`Account::region`]，
/// 即账号自己登记的区域。理由是同一个：猜错的代价是静默 401，
/// 而唯一知道答案的地方就是账号记录本身。
pub async fn fetch_resource_view(region: Region, token: &str) -> ResourceView {
    crate::usage::fetch_usage(region, token).await
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
    let view = qoder_api::fetch_campaigns(account.region, &account.token).await?;
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
    /// 今天已经领过。
    ///
    /// **仍带上那条活动**：它的领取接口是幂等的，回放一次能取回当初那张发放凭据
    /// （`grantId` + `expiresAt`）—— 那笔积分的到期时间只有在那里有。
    Done(&'a Campaign),
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
        Some(c) if c.claim_status == "CLAIMED" => Plan::Done(c),
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
    let Some(view) = qoder_api::fetch_campaigns(account.region, &account.token).await else {
        return CheckinRecord {
            message: "活动接口不可用（网络或登录态异常）".into(),
            ..blank(&at)
        };
    };
    match plan(&view) {
        Plan::Claim(c) => claim(account, c, &at).await,
        Plan::Done(c) => {
            // 今天已领 —— **回放一次领取**，唯一目的是取回发放凭据里的到期时间。
            //
            // 为什么值得多打这一下：活动接口只声明「领取后 30 天有效」这条**规则**，
            // 不告诉你当初是哪一天领的；而到期时间只在发放凭据（claim 响应）里。
            // 少了它，「积分过期」列对免费号只能显示「不过期 / 未知」。
            //
            // 安全性依据（实测，2026-09-19）：对已领活动重打返回 `200` +
            // `replayed: true`，`grantId` 与 `expiresAt` 与当初领取时逐字相同，
            // **不产生重复发放**。所以它在这条路径上等价于一次只读查询，
            // 且每天每账号至多发生一次（本函数由签到触发，而签到一天一次）。
            //
            // 失败一律不致命：拿不到到期时间只是少一个展示字段，不能把
            // 「今天已领」这个既成事实报成失败。
            let receipt = qoder_api::claim_campaign(account.region, &account.token, &c.id)
                .await
                .ok();
            CheckinRecord {
                success: true,
                already: true,
                message: "今日已领取".into(),
                campaign_key: Some(c.key.clone()),
                expires_at: receipt.and_then(|r| r.expires_at),
                ..blank(&at)
            }
        }
        Plan::Inactive(why) => CheckinRecord {
            inactive: true,
            message: why,
            ..blank(&at)
        },
    }
}

/// 真正的那一下 `POST …/{campaignId}/claim`，外加一次余额补读。
///
/// 除了「领到了多少」，它还要把发放凭据里的 **`expiresAt`** 带回记录里去 ——
/// 那是这**一笔**积分真正的到期时刻，也是「积分过期」列对免费号唯一能显示日期的来源。
async fn claim(account: &Account, c: &Campaign, at: &str) -> CheckinRecord {
    // 活动声明的数量（仅作兜底）：发放凭据里的 `benefit.amount` 是**实际发下来多少**，以它为准。
    let declared = c.benefit.as_ref().map(|b| b.amount);
    let mut rec = match qoder_api::claim_campaign(account.region, &account.token, &c.id).await {
        Ok(receipt) => {
            let amount = receipt.amount.or(declared);
            CheckinRecord {
                success: true,
                message: match amount {
                    Some(a) => format!("已领取 {} Credits", trim_num(a)),
                    None => "已领取".to_string(),
                },
                credit: amount,
                campaign_key: Some(c.key.clone()),
                expires_at: receipt.expires_at,
                ..blank(at)
            }
        }
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
        rec.balance = fetch_resource_view(account.region, &account.token).await.credits;
    }
    rec
}

/// 复查某条活动是否已经变成 `CLAIMED`（只读；查询失败即 `false`）。
async fn claimed_afterwards(account: &Account, campaign_id: &str) -> bool {
    qoder_api::fetch_campaigns(account.region, &account.token)
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
        expires_at: None,
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
        // 必须**带上那条活动**：签到要拿它回放一次领取，才能取回发放凭据里的到期时间。
        // 少了这个字段，「已领」这条路径就再也拿不到到期时间了（免费号尤其明显）。
        match plan(&v) {
            Plan::Done(c) => assert_eq!(c.id, "c1"),
            _ => panic!("已领的每日活动应当判成 Done"),
        }
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
            other => panic!("不该被当成已领：{}", matches!(other, Plan::Done(_))),
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
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        // 这里直接打接口（而不是 `query_campaigns`）：本机登录文件给的是 `LocalAccount`，
        // 与落盘的 `Account` 不是一个类型，冒烟测试没有理由为了一个 token 去造后者
        let v = qoder_api::fetch_campaigns(a.region, &a.token)
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

    /// 真实接口冒烟：额度读数（重构后 `fetch_resource_view` 收的是**账号自己的区域**，
    /// 用来确认本机账号的区域登记与真实域对得上）。
    /// 运行：`cargo test --lib -- --ignored --nocapture smoke_real_credits_endpoint`
    #[tokio::test]
    #[ignore = "真实网络调用，需本机已登录 Qoder"]
    async fn smoke_real_credits_endpoint() {
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        let v = fetch_resource_view(a.region, &a.token).await;
        println!(
            "剩余={:?} 最早到期={:?} 包数={}",
            v.credits,
            v.earliest_expiry_ms,
            v.packages.len()
        );
        assert!(v.credits.is_some(), "应能解析出剩余积分");
    }

    /// 真实接口冒烟：**发放凭据**里的到期时间（`replayed` 那条路径）。
    ///
    /// ⚠️ 会发一次 `POST …/{campaignId}/claim`，但对象是**当天已领取**的那条活动 ——
    /// 实测它返回 `200` + `replayed: true`，`grantId` / `expiresAt` 与当初领取时逐字相同，
    /// **不产生重复发放**（这也是「回放可以当只读查询用」这条假设的唯一现场验证）。
    /// 今天还没领时它**不代领**（那是签到的职责），只报告后跳过。
    ///
    /// 运行：`cargo test --lib -- --ignored --nocapture smoke_real_grant_receipt`
    #[tokio::test]
    #[ignore = "真实网络调用，需本机已登录 Qoder；会发一次幂等 claim 回放"]
    async fn smoke_real_grant_receipt() {
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        let v = qoder_api::fetch_campaigns(a.region, &a.token)
            .await
            .expect("活动接口应可用");
        let Some(c) = v.daily_claim() else {
            println!("当天没有每日活动，跳过");
            return;
        };
        if c.claim_status != "CLAIMED" {
            println!(
                "今天还没领（claimStatus={}），本冒烟只验证回放，跳过",
                c.claim_status
            );
            return;
        }
        let r = qoder_api::claim_campaign(a.region, &a.token, &c.id)
            .await
            .expect("已领的活动应当能回放");
        println!(
            "replayed={} status={} grant={} amount={:?}",
            r.replayed, r.status, r.grant_id, r.amount
        );
        println!("claimedAt={:?} expiresAt(ms)={:?}", r.claimed_at, r.expires_at);
        assert!(r.replayed, "对已领活动重打应当是回放，不是新发放");
        let exp = r
            .expires_at
            .expect("回放响应必须带 expiresAt —— 这正是本次改造要取的那个数");
        assert!(
            exp > crate::timeutil::now_ms(),
            "到期时间必须在未来：{exp}"
        );
    }

    /// 真实链路冒烟：**免费号的「积分过期」列终于有日期**。
    ///
    /// 走完整的那条链，一步不省：真实 usage 采样（免费号那份里根本没有到期时间）→
    /// 真实发放凭据 → 台账（`observe` + `note_grant_expiry`）→ 投影给界面的
    /// `CreditFact`。断言最后那一份里，附加额度包带着凭据给的日期，
    /// 而不是改造前的「不过期」或「未知」。
    ///
    /// 运行：`cargo test --lib -- --ignored --nocapture smoke_real_free_account_expiry`
    #[tokio::test]
    #[ignore = "真实网络调用，需本机已登录 Qoder；会发一次幂等 claim 回放"]
    async fn smoke_real_free_account_expiry_lands_in_the_ledger() {
        use crate::ledger;
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");

        // 1) 采样：免费号这一步拿不到任何真实到期时间
        let view = fetch_resource_view(a.region, &a.token).await;
        let mut led = ledger::Ledger::default();
        ledger::observe(
            led.accts.entry("smoke".into()).or_default(),
            &view.packages,
            view.credits,
            view.earliest_expiry_ms,
            "2026-09-19 13:16:49",
            ledger::Mode::Baseline,
        );
        println!("采样后 earliest={:?}", led.accts["smoke"].earliest_expiry_ms);

        // 2) 发放凭据：这一笔积分自己的到期时间
        let campaigns = qoder_api::fetch_campaigns(a.region, &a.token)
            .await
            .expect("活动接口应可用");
        let c = campaigns.daily_claim().expect("当天应有每日活动");
        if c.claim_status != "CLAIMED" {
            println!("今天还没领（claimStatus={}），跳过", c.claim_status);
            return;
        }
        let receipt = qoder_api::claim_campaign(a.region, &a.token, &c.id)
            .await
            .expect("已领的活动应当能回放");
        let exp = receipt.expires_at.expect("回放必须带 expiresAt");
        ledger::note_grant_expiry(
            led.accts.get_mut("smoke").unwrap(),
            crate::usage::KEY_ADDON,
            exp,
        );

        // 3) 投影给界面的那一份
        let f = ledger::fact(&led, "smoke").expect("采到过读数就该有投影");
        for p in &f.packages {
            println!(
                "包：{} remaining={} expiry={:?} never={}",
                p.name, p.remaining, p.expiry_ms, p.never_expires
            );
        }
        assert!(
            f.packages
                .iter()
                .any(|p| p.expiry_ms == Some(exp) && !p.never_expires),
            "附加额度包应当带上凭据给的日期（这一条正是改造前缺的那个数）"
        );
        assert_eq!(
            f.earliest_expiry_ms,
            Some(exp),
            "接管路由排序用的最早到期也要把它算进去"
        );
    }
}
