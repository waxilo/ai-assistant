//! Qoder 额度总览：用 Qoder 真实的 usage 接口替换旧的 CodeBuddy `get-user-resource`。
//!
//! 本项目是从 workbuddy(clone) 改名而来，额度数据原本从 `/v2/billing/meter/get-user-resource`
//! 拉取（那是 CodeBuddy 体系）。Qoder 用完全不同的一套后端（见
//! `basedata/20260918_Qoder接口逆向报告.md`）：
//!
//! ```text
//! GET https://openapi.qoder.sh/sash/api/v2/me/usage?product=app
//! 头（Bx 头）：Accept: application/json、Authorization: Bearer <token>、
//!              Cosy-ClientType: 10、User-Agent: Qoder
//!              —— 没有 X-Domain / X-Refresh-Token
//! ```
//!
//! 对外只暴露 [`fetch_usage`]：拿到 token 就回一份 [`ResourceView`]，输出结构与旧接口
//! 完全一致，因此 `checkin` / `ledger` / `proxy` / `commands` 里所有消费方都无需改动 ——
//! 只有 `checkin::fetch_resource_view` 换成了这条路（见 `checkin.rs`）。
//!
//! # 两种真实响应形态（2026-09-18 实测）
//!
//! 这个接口的返回**取决于账号类型**，两形态的字段名毫无交集。上一版代码只认后者，
//! 于是在个人版账号上「解析不出任何包 → 返回空」——不报错、只是额度永远显示不出来：
//!
//! ① **个人版**（`displayMode: "qoder"`，本机实跑形态）——**没有**包数组，只有两个额度槽位：
//!
//! ```json
//! { "displayMode": "qoder",
//!   "qoderUsage": {
//!     "userType": "personal_professional_trial", "usageType": "credits",
//!     "totalUsagePercentage": 0, "isQuotaExceeded": false,
//!     "expiresAt": 1790927528327,
//!     "userQuota":  { "total": 300, "used": 0, "remaining": 300, "percentage": 0, "unit": "credits" },
//!     "addOnQuota": { "total": 100, "used": 0, "remaining": 100, "percentage": 0, "unit": "credits" } } }
//! ```
//!
//! ② **团队 / 企业版**——有逐包的 `dedicatedResourcePackages[]`（字段 `id`/`name`/`total`/
//! `used`/`remaining`/`expiresAt`），这才是上一版代码写死要找的东西。
//!
//! # ③ 免费账号的 `expiresAt` 是「永不过期」哨兵，不是日期
//!
//! 2026-09-19 实测（国内版免费号，`userType: "personal_standard"`）：
//!
//! ```json
//! { "qoderUsage": {
//!     "expiresAt": 253402214400000,                        // ← 9999-12-31T00:00:00Z
//!     "userQuota":  { "total": 0,   "used": 0, "remaining": 0 },   // 空槽位：不产出包
//!     "addOnQuota": { "total": 100, "used": 0, "remaining": 100 } } }
//! ```
//!
//! 同一个账号 `GET /api/v2/user/plan` 给的是 `{"plan_tier_name":"Free",
//! "is_paid_plan":false,"end_date":0}` —— **`end_date: 0` 与 `expiresAt: 253402214400000`
//! 是同一个意思的两套写法**：没有期限。
//!
//! 上一版拿它当普通毫秒时间戳用，于是界面上出现「到期 9999-12-31」与
//! 「2922776 天后过期 100」。判定与归一现在只在 [`crate::ledger::normalize_expiry`] 一处，
//! 本模块只负责调用；「永不过期」有独立的表示法，绝不会落进 `expiry_ms`。
//!
//! 归一化规则见 [`parse_view`]：**有逐包数组就用逐包，否则把两个槽位当两个包**。
//!
//! # ④ 两个槽位的到期口径**不一样**（2026-09-19 更正）
//!
//! `expiresAt` 是**整个额度概览**的到期（= 计划周期终点），它与 `userQuota` /
//! `addOnQuota` 同级，旧代码因此把它同时当成两个槽位的到期。对计划额度这是对的；
//! 对附加额度是**推断过头** —— 响应里附加额度根本没有自己的到期字段，而那句「不过期」
//! （免费号上的哨兵）说的是**计划没有期限**。可签到赠送的那 100 Credits，官方自己
//! 明确「领取后 30 天有效」（`benefit.validity = RELATIVE_DAYS/30`）。
//!
//! 于是现在的口径是：计划额度沿用 `expiresAt`；附加额度的 `never_expires` 恒 `false`，
//! 真实到期由**发放凭据**（`POST …/{campaignId}/claim` 响应里的 `expiresAt`）补上，
//! 见 [`crate::ledger::note_grant_expiry`]。凭据到位前如实显示「未知」——
//! 那比「不过期」正确，因为后者会让人以为这批积分永远不会作废。
//!
//! # 与台账（`ledger`）的契约
//!
//! [`PkgView`] 的 `used` 必须是**周期内累计量**（台账靠「周期变了且 used 回退」判定翻周期），
//! 所以每个包的 `cycle_start` 要能标识它所属的周期：
//!
//! - 计划额度：真实响应只给**周期终点**（`qoderUsage.expiresAt`），拿它当周期标识 ——
//!   续期后终点后移，台账据此把旧周期用量归档，与「用周期起点」的判据等价。
//!   （实测它等于 `/api/v2/user/plan` 的 `end_date`，两处口径一致，可互相校验。）
//! - 附加额度：响应里没有任何周期信息。**故意不给周期标识**（留空），
//!   让它的累计量只增不减 —— 宁可漏记一次翻周期（可查），也不要在没有依据时
//!   虚报一次「用量归零」（不可查）。

use crate::ledger::PkgView;
use crate::region::Region;
use serde_json::Value;

/// Qoder usage 的路径（基址由 [`Region::openapi_base`] 按账号区域给出）。
///
/// 带不带 `product=app` 官方客户端都会发，这里显式带上以与官方语义对齐。
const USAGE_PATH: &str = "/sash/api/v2/me/usage";

/// 计划额度槽位的稳定键。
///
/// **不能用「看起来更像名字」的字段当键**：个人版响应里两个槽位都没有 id，
/// 而键必须跨采样稳定（台账按它认包），所以只能用语义槽位来当键。
const KEY_PLAN: &str = "qoder:plan";
/// 附加额度槽位的稳定键（同上）。
///
/// 公开给 crate 内：签到路径拿到**发放凭据**后，要把那笔积分的真实到期时间记到这个包上
/// （`ledger::note_grant_expiry`），而「签到的 100 Credits 落在哪一格」这件事
/// 只有本模块知道 —— 由调用方另写一份字面量，迟早会有一处写错。
pub const KEY_ADDON: &str = "qoder:addon";

/// 响应可能被包一层甚至两层的容器键，自顶向下按此顺序下钻。
///
/// Qoder 自己的响应目前是裸的，但网关/中间层会把它塞进 `data` / `Response` / `Data`
/// —— 这两类写法都在真实抓包里出现过，所以下钻一次就够，多写几层是为容错。
const WRAPPER_KEYS: [&str; 4] = ["qoderUsage", "data", "Response", "Data"];

/// 下钻深度上限：响应体积很小，几层足够；写死上限是为了「既容错又不会无限下钻」。
const MAX_DEPTH: usize = 6;

/// 一次完整的额度读数：汇总值 + 逐包明细。
///
/// 结构刻意与旧 get-user-resource 的那份保持一致（它是积分事实的唯一入口）：
/// `credits` 给账户管理与路由用，`packages` 给台账算消耗与新增，
/// `earliest_expiry_ms` 给路由排序。定义在本模块（而不是 checkin.rs）是为了避免
/// checkin ↔ usage 之间的循环依赖；`checkin.rs` 用 `use crate::usage::ResourceView`。
#[derive(Default, Clone, Debug)]
pub struct ResourceView {
    /// 剩余积分合计（各包 `remaining` 之和）
    pub credits: Option<f64>,
    /// 还有余量的包里最早到期时间（毫秒）
    pub earliest_expiry_ms: Option<i64>,
    /// 逐包明细；响应里解析不到任何包时为空
    pub packages: Vec<PkgView>,
}

// 客户端不再由本模块构造：Qoder 的基址与身份头统一归 [`crate::qoder_api`] 持有。
// 这里原本自己拼了一份 Bx 头（且 `Cosy-ClientType` 误写成带引号的 `"10"`，实测服务端
// 两种都收，但源码是 `10`）—— 那正是「同一个后端在不同路径上收到两套身份」的写法。

// ---------------------------------------------------------------------------
// 解析：两种形态 → 统一的逐包明细
// ---------------------------------------------------------------------------

/// 取数值：额度字段可能是数字或字符串。
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 取毫秒时间戳：数字 / 数字字符串都认（真实响应里是数字，防它某天改成字符串）。
fn as_ms(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| as_f64(v).map(|f| f as i64))
}

/// 自顶向下找第一个名为 `want` 的**非空**成员，返回其引用。
///
/// 只沿 [`WRAPPER_KEYS`] 里的容器下钻（不深入数组），深度上限 [`MAX_DEPTH`]。
/// 借用的值绑在 `root` 的生命周期上，因此返回值可以安全地指回 `root`。
fn find_key<'a>(root: &'a Value, want: &str) -> Option<&'a Value> {
    fn walk<'a>(v: &'a Value, want: &str, depth: usize) -> Option<&'a Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(found) = v.get(want) {
            if !found.is_null() {
                return Some(found);
            }
        }
        for key in WRAPPER_KEYS {
            // 只深入对象节点：数组不可能是这些容器的本身
            if let Some(child) = v.get(key) {
                if child.is_object() {
                    if let Some(found) = walk(child, want, depth + 1) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }
    walk(root, want, 0)
}

/// 一个额度槽位（`userQuota` / `addOnQuota` 同构）。
struct Slot {
    total: f64,
    used: f64,
    remaining: f64,
}

/// 读一个额度槽位；缺失、或「总量与剩余都为 0」时返回 `None`（没额度就不该占一条包）。
fn read_slot(usage: &Value, key: &str) -> Option<Slot> {
    let obj = usage.get(key)?.as_object()?;
    let total = obj.get("total").and_then(as_f64).unwrap_or(0.0);
    let used = obj.get("used").and_then(as_f64).unwrap_or(0.0);
    let remaining = obj.get("remaining").and_then(as_f64).unwrap_or(0.0);
    if total <= 0.0 && remaining <= 0.0 {
        return None;
    }
    Some(Slot {
        total,
        used,
        remaining,
    })
}

/// 个人版形态：把 `userQuota` / `addOnQuota` 两个槽位当成两个包。
///
/// `cycle_start` 的取值理由见模块头「与台账的契约」：计划额度用周期终点当周期标识，
/// 附加额度留空（没有依据就不断言周期变化）。
fn packages_from_slots(usage: &Value) -> Vec<PkgView> {
    // `expiresAt` 与 `userQuota` / `addOnQuota` **同级**，即整个额度概览的到期时刻，
    // 所以两个槽位都用它 —— 这一条本来就对。
    //
    // 错的是把它当日期：没付费的账号（`plan_tier_name: "Free"`）这里放的是
    // 「永不过期」哨兵 253402214400000（实测，对应 `/api/v2/user/plan` 的
    // `end_date: 0`）。上一版就这样把 9999-12-31 一路送进了界面。
    // 归一交给 `ledger::normalize_expiry`，本模块不再自己判哨兵。
    let (period_end, never_expires) =
        crate::ledger::normalize_expiry(usage.get("expiresAt").and_then(as_ms));
    let plan_cycle = period_end.map(|ms| ms.to_string()).unwrap_or_default();
    let mut out = Vec::new();
    for (key, name, slot, cycle) in [
        (
            KEY_PLAN,
            "计划额度",
            read_slot(usage, "userQuota"),
            plan_cycle,
        ),
        (KEY_ADDON, "附加额度", read_slot(usage, "addOnQuota"), String::new()),
    ] {
        let Some(s) = slot else { continue };
        out.push(PkgView {
            key: key.to_string(),
            name: name.to_string(),
            size: s.total,
            used: s.used,
            cycle_start: cycle,
            // 到期时间先按概览给一个近似值（同一账号的额度周期），拿到发放凭据后
            // 会被凭据里那份更具体的盖掉（见 `ledger::PkgEntry::expiry`）。
            expiry_ms: period_end,
            // ⚠️ 「不过期」这句话**只对计划额度成立**：它来自概览的 `expiresAt`，
            // 而那个值说的就是计划的周期终点。附加额度在响应里**没有自己的到期字段**，
            // 照抄这句是**推断过头** —— 免费号上它是哨兵（「计划没有期限」），
            // 而签到赠送的那 100 Credits 官方明确「领取后 30 天有效」
            //（`benefit.validity = RELATIVE_DAYS/30`）。旧代码正是这么抄的，
            // 于是界面对免费号显示「不过期」，比「未知」更错。
            //
            // 所以这里恒 `false`：真实到期等发放凭据补（`ledger::note_grant_expiry`），
            // 补上之前如实显示「未知」。
            never_expires: key == KEY_PLAN && never_expires,
            remaining: s.remaining,
        });
    }
    out
}

/// 团队形态：`dedicatedResourcePackages[]` → 逐包明细。
///
/// 映射规则：`key` ← `id`、`name` ← `name`、`size` ← `total`、`used` ← `used`、
/// `remaining` ← `remaining`、`expiry_ms` ← `expiresAt`（毫秒时间戳，经
/// [`crate::ledger::normalize_expiry`] 归一 —— 个别包也会是「永不过期」哨兵）。
///
/// `cycle_start` 置空：该形态的 `used` 是包的累计用量，没有「本周期」概念，
/// 因此不给周期标识（累计量只增，台账不会误判翻周期）。
fn packages_from_dedicated(root: &Value) -> Vec<PkgView> {
    let Some(list) = find_key(root, "dedicatedResourcePackages").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|p| {
            let obj = p.as_object()?;
            let key = obj.get("id").and_then(Value::as_str)?.to_string();
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let total = obj.get("total").and_then(as_f64).unwrap_or(0.0);
            let used = obj.get("used").and_then(as_f64).unwrap_or(0.0);
            let remaining = obj.get("remaining").and_then(as_f64).unwrap_or(0.0);
            let (expiry_ms, never_expires) =
                crate::ledger::normalize_expiry(obj.get("expiresAt").and_then(as_ms));
            Some(PkgView {
                key,
                name,
                size: total,
                used,
                cycle_start: String::new(),
                expiry_ms,
                never_expires,
                remaining,
            })
        })
        .collect()
}

/// 把 usage 响应归一化成一份 [`ResourceView`]（纯函数，便于用真实响应做回归测试）。
///
/// 顺序：**先找逐包数组（团队形态），找不到再把两个槽位当包（个人形态）**。
/// 解析不出任何包时返回 `None` —— 调用方据此回退到「这一轮没读到」，而不是
/// 造一个「额度为 0」的假读数。
pub fn parse_view(root: &Value) -> Option<ResourceView> {
    let packages = {
        let dedicated = packages_from_dedicated(root);
        if dedicated.is_empty() {
            // 个人版的槽位挂在 `qoderUsage` 下；响应被包裹时也要能找得到
            let usage = find_key(root, "qoderUsage").unwrap_or(root);
            packages_from_slots(usage)
        } else {
            dedicated
        }
    };
    if packages.is_empty() {
        return None;
    }

    // 剩余积分合计 = 所有包 `remaining` 之和（保留两位小数，与界面展示口径一致）
    let credits = Some(crate::ledger::round2(
        packages.iter().map(|p| p.remaining).sum::<f64>(),
    ));
    // 还有余量的包里最早到期时间（毫秒）；已用完的包对路由排序没有意义，不参与
    let earliest_expiry_ms = packages
        .iter()
        .filter(|p| p.remaining > 0.0)
        .filter_map(|p| p.expiry_ms)
        .min();
    Some(ResourceView {
        credits,
        earliest_expiry_ms,
        packages,
    })
}

/// 拉一次 Qoder 额度总览。
///
/// best-effort：网络错误 / 非 JSON / 字段缺失 / 无包都回 [`ResourceView::default()`]，
/// 绝不影响调用方的主流程。`region` 取**账号自己的**区域 —— 两套部署的额度互不相通，
/// 打错域只会稳定 401；`token` 是登录态里的 access token
/// （来自 auth.v1.dat 的当前账号）。
pub async fn fetch_usage(region: Region, token: &str) -> ResourceView {
    let Some(body) =
        crate::qoder_api::get_json(region, token, USAGE_PATH, &[("product", "app")]).await
    else {
        return ResourceView::default();
    };
    parse_view(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析一段 JSON 文本；失败即测试失败（fixture 写错要立刻暴露，不能静默变成 None）
    fn parse(s: &str) -> Option<ResourceView> {
        let v: Value = serde_json::from_str(s).expect("fixture 必须是合法 JSON");
        parse_view(&v)
    }

    /// **实测响应**（2026-09-18，个人版 Pro Trial 账号）——逐字照抄，只省略了 userId。
    ///
    /// 它就是上一版代码会解析出空值的那份响应：没有 `dedicatedResourcePackages`。
    const PERSONAL_REAL: &str = r#"{
      "displayMode": "qoder",
      "qoderUsage": {
        "userId": "01a0b380-0000-0000-0000-000000000000",
        "userType": "personal_professional_trial",
        "usageType": "credits",
        "totalUsagePercentage": 0,
        "isQuotaExceeded": false,
        "expiresAt": 1790927528327,
        "upgradeUrl": "https://qoder.com/pricing?client=qoder",
        "userQuota": {
          "total": 300,
          "used": 0,
          "remaining": 300,
          "percentage": 0,
          "unit": "credits"
        },
        "addOnQuota": {
          "total": 100,
          "used": 0,
          "remaining": 100,
          "percentage": 0,
          "unit": "credits",
          "detailUrl": "https://qoder.com/account/usage"
        },
        "isPlanQuotaProrated": false
      }
    }"#;

    /// **实测响应**（2026-09-19，国内版免费号 `personal_standard`）——逐字照抄，
    /// 只把 `userId` 换掉了。
    ///
    /// 与 [`PERSONAL_REAL`] 只差一处，但那一处是致命的：`expiresAt` 是
    /// 「永不过期」哨兵（`/api/v2/user/plan` 那边对应 `end_date: 0`），
    /// 而 `userQuota` 全零 —— 于是整个账号只有那个 100 分的加油包。
    /// 上一版据此在「资源包列表」里显示「到期 9999-12-31」。
    const PERSONAL_FREE_NEVER_EXPIRES: &str = r#"{
      "displayMode": "qoder",
      "qoderUsage": {
        "userId": "019eb647-0000-0000-0000-000000000000",
        "userType": "personal_standard",
        "usageType": "credits",
        "totalUsagePercentage": 0,
        "isQuotaExceeded": false,
        "expiresAt": 253402214400000,
        "upgradeUrl": "https://qoder.com/pricing?client=qoder",
        "userQuota": {
          "total": 0,
          "used": 0,
          "remaining": 0,
          "percentage": 0,
          "unit": "credits"
        },
        "addOnQuota": {
          "total": 100,
          "used": 0,
          "remaining": 100,
          "percentage": 0,
          "unit": "credits",
          "detailUrl": "https://qoder.com/account/usage"
        },
        "isPlanQuotaProrated": false
      }
    }"#;

    /// 免费号：只有加油包，`expiresAt` 是「永不过期」哨兵。四件事必须同时成立，
    /// 缺一条界面就会说错话：① 空槽位不产出包；② 哨兵不进 `expiry_ms`；
    /// ③ 那句「不过期」**只对计划额度成立**，附加额度不能跟着说 —— 它说的是
    /// 「计划没有期限」，而签到赠送的 100 Credits 官方明确「领取后 30 天有效」；
    /// ④ `earliest_expiry_ms` 不拿它当日期（否则接管会把它排成一千年后的「大限」）。
    ///
    /// 2026-09-19 更正第 ③ 条：旧版把附加额度也标成 `never_expires = true`，
    /// 于是界面对免费号显示「不过期」。那比「未知」更错 —— 它会让人以为这批积分
    /// 永远不会作废，而真实到期（实测 2026-10-19）由发放凭据补上，见
    /// `ledger::note_grant_expiry`。
    #[test]
    fn a_free_account_reports_never_expires_instead_of_a_year_9999_date() {
        let v = parse(PERSONAL_FREE_NEVER_EXPIRES).expect("免费号也必须能解析出额度");
        assert_eq!(v.credits, Some(100.0));
        assert_eq!(v.packages.len(), 1, "userQuota 全零 ⇒ 不该多出一条 0/0 的假包");

        let addon = &v.packages[0];
        assert_eq!(addon.key, KEY_ADDON);
        assert_eq!(addon.expiry_ms, None, "哨兵绝不能落进 expiry_ms");
        assert!(
            !addon.never_expires,
            "「计划没有期限」≠「这笔赠送积分也不过期」：凭据到位前如实显示「未知」"
        );
        assert_eq!(addon.cycle_start, "", "没有期限也就没有周期标识");
        assert_eq!(v.earliest_expiry_ms, None);
    }

    /// 「不过期」这条判定**没有**被上面那条测试顺手删掉：把哨兵放到一个真有额度的
    /// 计划槽位上，它必须仍被显式记成 `never_expires`（否则界面会显示成 9999-12-31）。
    ///
    /// 少了这条，「把所有到期时间都吞成未知」那种改法也能让上一条测试变绿。
    #[test]
    fn the_never_expires_flag_survives_on_the_plan_slot_only() {
        let free_plan = PERSONAL_REAL.replace(
            "\"expiresAt\": 1790927528327",
            "\"expiresAt\": 253402214400000",
        );
        assert_ne!(free_plan, PERSONAL_REAL, "替换必须真的生效，否则这条测试是空转的");
        let v = parse(&free_plan).unwrap();
        let plan = v.packages.iter().find(|p| p.key == KEY_PLAN).unwrap();
        assert_eq!(plan.expiry_ms, None, "哨兵不进 expiry_ms");
        assert!(plan.never_expires, "计划额度自己说不过期，这条要留着");
        let addon = v.packages.iter().find(|p| p.key == KEY_ADDON).unwrap();
        assert!(!addon.never_expires, "附加额度不跟着说不过期");
        assert_eq!(v.earliest_expiry_ms, None, "两个包都没有真实到期日");
    }

    /// 反向断言：真日期不能被误判成「永不过期」。
    /// 少了这条，「把所有到期日都吞掉」那种改法也能让上面那条测试变绿。
    #[test]
    fn a_real_deadline_is_not_mistaken_for_never_expiring() {
        let v = parse(PERSONAL_REAL).unwrap();
        let plan = v.packages.iter().find(|p| p.key == KEY_PLAN).unwrap();
        assert_eq!(plan.expiry_ms, Some(1_790_927_528_327));
        assert!(!v.packages.iter().any(|p| p.never_expires));
        assert_eq!(v.earliest_expiry_ms, Some(1_790_927_528_327));
    }

    /// 团队形态的单个包同样可能是永不过期的（`expiresAt` 也是哨兵），
    /// 而且「最早到期」只该看那个有真实期限的包。
    #[test]
    fn dedicated_packages_can_also_be_never_expiring() {
        let v = parse(
            r#"{"qoderUsage":{"dedicatedResourcePackages":[
                 {"id":"rp-1","name":"永久包","total":10,"used":0,"remaining":10,
                  "expiresAt":253402214400000},
                 {"id":"rp-2","name":"期限包","total":10,"used":0,"remaining":10,
                  "expiresAt":1800000000000}
               ]}}"#,
        )
        .unwrap();
        let never = v.packages.iter().find(|p| p.key == "rp-1").unwrap();
        assert_eq!((never.expiry_ms, never.never_expires), (None, true));
        let dated = v.packages.iter().find(|p| p.key == "rp-2").unwrap();
        assert_eq!(
            (dated.expiry_ms, dated.never_expires),
            (Some(1_800_000_000_000), false)
        );
        assert_eq!(v.earliest_expiry_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn parses_personal_shape_into_two_slots() {
        let v = parse(PERSONAL_REAL).expect("个人版形态必须能解析出额度");
        // 交叉验算：合计 = 300 + 100，与两个槽位的 remaining 对得上
        assert_eq!(v.credits, Some(400.0));
        assert_eq!(v.packages.len(), 2);
        assert_eq!(v.earliest_expiry_ms, Some(1_790_927_528_327));

        let plan = v.packages.iter().find(|p| p.key == KEY_PLAN).unwrap();
        assert_eq!(plan.name, "计划额度");
        assert_eq!((plan.size, plan.used, plan.remaining), (300.0, 0.0, 300.0));
        assert_eq!(plan.expiry_ms, Some(1_790_927_528_327));
        // 计划额度用周期终点当周期标识，续期后终点后移 → 台账据此归档旧周期
        assert_eq!(plan.cycle_start, "1790927528327");

        let addon = v.packages.iter().find(|p| p.key == KEY_ADDON).unwrap();
        assert_eq!((addon.size, addon.used, addon.remaining), (100.0, 0.0, 100.0));
        // 附加额度没有任何周期信息 → 不给周期标识（宁可漏记翻周期，不虚报归零）
        assert_eq!(addon.cycle_start, "");
    }

    #[test]
    fn personal_shape_accepts_strings_and_missing_expiry() {
        // 数值写成字符串也要认；`qoderUsage` 缺失时直接从根上找槽位
        let v = parse(r#"{"userQuota":{"total":"300","used":"12.5","remaining":"287.5"}}"#).unwrap();
        assert_eq!(v.credits, Some(287.5));
        assert_eq!(v.packages[0].size, 300.0);
        assert_eq!(v.packages[0].used, 12.5);
        // 没有 expiresAt → 到期时间未知，但不是 0
        assert_eq!(v.packages[0].expiry_ms, None);
        assert_eq!(v.earliest_expiry_ms, None);
        assert_eq!(v.packages[0].cycle_start, "");
    }

    #[test]
    fn personal_shape_is_found_even_when_wrapped() {
        // 被网关包一层：槽位仍要能找到（否则额度又会静默变成空）
        let v = parse(r#"{"code":0,"data":{"qoderUsage":{"userQuota":{"total":50,"used":10,"remaining":40}}}}"#)
            .unwrap();
        assert_eq!(v.credits, Some(40.0));
        assert_eq!(v.packages[0].key, KEY_PLAN);
    }

    #[test]
    fn personal_shape_skips_empty_slot() {
        // 只用计划额度、没用加油包的账号：不该多出一条 0/0 的假包
        let v = parse(
            r#"{"qoderUsage":{
                 "userQuota":{"total":300,"used":0,"remaining":300},
                 "addOnQuota":{"total":0,"used":0,"remaining":0}}}"#,
        )
        .unwrap();
        assert_eq!(v.packages.len(), 1);
        assert_eq!(v.packages[0].key, KEY_PLAN);
    }

    #[test]
    fn parses_dedicated_packages_shape_and_wrapped_payload() {
        // 团队形态：逐包数组，且被 data.data 包了一层。
        // 注意 rp-2 与 rp-1 的 name 不同但 code 相同 —— 键必须用 id，不能拿 name/类型码当键
        // （用类型码当归并键的话，同类型的多个包会被并成一条，聚合结果永远偏小）。
        let v = parse(
            r#"{"code":0,"data":{"data":{"qoderUsage":{"dedicatedResourcePackages":[
                 {"id":"rp-1","code":"TYPE_A","name":"团队包A","total":1000,"used":250,
                  "remaining":750,"expiresAt":1800000000000},
                 {"id":"rp-2","code":"TYPE_A","name":"团队包B","total":500,"used":500,
                  "remaining":0,"expiresAt":1750000000000}
               ]}}}}"#,
        )
        .unwrap();
        assert_eq!(v.packages.len(), 2, "同 code 的两个包必须各自成条");
        assert_eq!(v.credits, Some(750.0));
        assert_eq!(v.packages[0].key, "rp-1");
        assert_eq!(v.packages[0].cycle_start, "");
        // 已用完的包不参与「最早到期」——否则路由会天天盯一个没存量的大限
        assert_eq!(v.earliest_expiry_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn dedicated_shape_wins_when_both_present() {
        // 两形态同时出现（企业账号也可能带槽位）：以逐包为准，不做相加
        let v = parse(
            r#"{"qoderUsage":{
                 "userQuota":{"total":300,"used":0,"remaining":300},
                 "dedicatedResourcePackages":[
                   {"id":"rp-1","name":"A","total":10,"used":1,"remaining":9}]}}"#,
        )
        .unwrap();
        assert_eq!(v.packages.len(), 1);
        assert_eq!(v.credits, Some(9.0));
    }

    #[test]
    fn no_packages_yields_none_not_a_fake_zero() {
        // 读不到 ≠ 额度是 0：返回 None，让调用方保留上一次读数
        assert!(parse(r#"{}"#).is_none());
        assert!(parse(r#"{"displayMode":"qoder","qoderUsage":{}}"#).is_none());
        // 两个槽位都是空的 → 同样算「没读到」
        assert!(parse(
            r#"{"qoderUsage":{
                 "userQuota":{"total":0,"used":0,"remaining":0},
                 "addOnQuota":{}}}"#
        )
        .is_none());
        // 401 / 错误体
        assert!(parse(r#"{"errorCode":"Unauthorized","errorMessage":"..."}"#).is_none());
        // dedicated 数组存在但为空 → 也不该造包
        assert!(parse(r#"{"qoderUsage":{"dedicatedResourcePackages":[]}}"#).is_none());
    }

    /// 端到端回归：**真实响应形状**（个人版两个槽位）走完「解析 → 台账 → 固化」三步，
    /// 必须产出一条**有数**的时条目。
    ///
    /// 这条钉的是「积分简报一条都出不来」那类故障的完整链路：任何一环把 `used` 读成
    /// 不动的字段、或把增量按 `max(0)` 抹掉，这里都会立刻红。
    ///
    /// 它原本住在 `checkin.rs` —— 那时解析函数还在那边（`parse_packages` / `sum_credits`）。
    /// 额度解析既然已经归本模块，测试也跟着搬过来：**换个地方钉同一件事，而不是删掉**。
    #[test]
    fn a_realistic_payload_ends_up_as_a_sealed_hour_entry() {
        // 同一个槽位，两次采样之间又消耗了 3.44（累计量只增，所以增量能算出来）
        let payload = |used: f64, remaining: f64| {
            format!(
                r#"{{"displayMode":"qoder","qoderUsage":{{
                     "expiresAt": 1790927528327,
                     "userQuota": {{"total":500,"used":{used},"remaining":{remaining}}}}}}}"#
            )
        };
        let accounts = [crate::accounts::Account {
            region: Region::Global,
            id: "a1".into(),
            name: "甲".into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            created_at: String::new(),
            last: None,
            checked_today: None,
            cosy_uid: None,
        }];

        let mut led = crate::ledger::Ledger::default();
        for (body, at) in [
            (payload(257.36999973, 242.63000027), "2026-09-16 15:00:00"),
            (payload(260.80999973, 239.19000027), "2026-09-16 15:55:00"),
        ] {
            let v = parse(&body).expect("真实响应形状必须能解析出额度");
            crate::ledger::observe(
                led.accts.entry("a1".into()).or_default(),
                &v.packages,
                v.credits,
                v.earliest_expiry_ms,
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
        let e = crate::briefing::build_hour(
            &led.accts,
            &accounts,
            "2026-09-16",
            15,
            "2026-09-16 16:00:00",
        )
        .expect("这一小时有动静，必须固化成条目");
        assert_eq!(e.consumed, 3.44);
        assert_eq!(e.accounts.len(), 1);
        assert_eq!(e.accounts[0].consumed, 3.44);
        assert_eq!(e.balance, Some(239.19), "读数落在同一小时里，应当带上");
    }

    /// 真实接口冒烟：用本机登录态打一次 usage，打印解析结果（token 只打长度）。
    /// 运行：`cargo test -- --ignored --nocapture smoke_real_usage_endpoint`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_usage_endpoint() {
        let list = crate::auth_file::discover_local_accounts().accounts;
        let a = list.first().expect("本机应存在 Qoder 登录信息");
        println!("token_len={}", a.token.len());
        let v = fetch_usage(a.region, &a.token).await;
        println!(
            "credits={:?} earliest_expiry_ms={:?} packages={}",
            v.credits,
            v.earliest_expiry_ms,
            v.packages.len()
        );
        for p in &v.packages {
            println!(
                "  [{}] {} size={} used={} remaining={} expiry={:?} cycle={:?}",
                p.key, p.name, p.size, p.used, p.remaining, p.expiry_ms, p.cycle_start
            );
        }
        assert!(v.credits.is_some(), "真实账号应当能读出额度");
    }
}
