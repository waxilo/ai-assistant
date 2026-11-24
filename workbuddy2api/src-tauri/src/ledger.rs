//! 积分台账：采样、累计值，以及「按小时的增量桶」。
//!
//! 这一层只回答一个问题：**谁、在哪个小时、涨了多少**。把它固化成可长期回看的
//! 「时条目」、再聚合成「日条目」是 [`crate::briefing`] 的事 —— 两层分开是因为
//! 两者的要求相反：台账要求**每次采样都算准**（只在内存里做，越快越密越好），
//! 简报要求**长期可读**（落盘、有上限、能剪枝）。台账里的小时桶只留 60 天，
//! 剪掉的只是「明细」，已经固化下来的时条目不受影响。
//!
//! # 为什么不用「定时抓一次余额、算涨跌」
//!
//! 最直觉的做法是每 12 点记一次剩余积分，用差值当「今天消耗」。它有两个致命问题：
//!
//! 1. **分不清消耗与新增**。同一天里先消耗 100 再签到得 100，余额回到原点是差值为 0，
//!    而那 100 的消耗凭空消失。采样越稀，这种抵消越严重。
//! 2. **多客户端一起消耗时无法归因**。这个应用只是其中一个消耗方（还可能被直连绕过），
//!    任何「按请求记账」的写法都会在并发下算错。
//!
//! # 所以改成读接口的**本周期**累计量
//!
//! `get-user-resource` 每个资源包都带两组「容量」字段，**只有 `Cycle*` 那组是真的**：
//!
//! | 字段族 | 实测行为 |
//! |---|---|
//! | `CapacityUsed` / `CapacitySize` / `CapacityRemain` | **根本不维护** —— 恒等于 0 / size / size |
//! | `CycleCapacityUsed` / `CycleCapacitySize` / `CycleCapacityRemain` | 真值，随消耗实时变动 |
//!
//! 证据是**间隔采样做 diff**（静态看一次响应，两组字段长得一模一样）：在账号确实在服务流量的
//! 120 秒里，`CapacityUsed` 增量 `+0.0000`，`CycleCapacityUsed` 增量 `+1.4400`，
//! 且 `CycleCapacityRemain` 同步 `−1.4400`（两者互补）。界面的「剩余积分」一直读的就是
//! `CycleCapacityRemain`（见 [`crate::checkin::sum_credits`]），所以余额从来是对的 ——
//! 只有台账读错了字段组，于是增量恒为 0、简报一条都固化不出来。
//!
//! ⚠️ **曾经被「已用满的包」骗过**：包用满时 `CycleCapacityUsed == CapacitySize`，
//! 两组字段数值天然相等，于是「实测两者取值相同，取累计的那个」这个结论看起来被数据支持了。
//! 别再退回 `Capacity*` 组 —— 判活的唯一手段是间隔 diff，不是看字段名像不像累计量。
//!
//! 本周期量的差值**天然覆盖多客户端**：谁消耗的都会让计数器上涨，不需要知道是谁、
//! 也不需要按请求归属，因此并发不会算错。这与「抓余额」的区别是本质的——
//! 计数器只增不减（周期内），消耗与新增各自留痕，不会互相抵消。
//!
//! # 这里同时也是「账号还剩多少积分」的唯一来源
//!
//! 每一次拉取（手动刷新 / 简报采样 / 签到 / 接管路由决策）都会把这一轮读到的
//! **剩余积分、读数时刻、最早过期时间**写进同一个账号条目（见 [`observe`] 与
//! [`record_credits`]）。账户管理与简报都从这里读，**谁拉的就写这里，没有第二份**
//! —— 这是为了根治「同一个数字有四个写入点、各写各的」这类漂移。

//!
//! # 台账为什么按「包」存
//!
//! 直接对「全部包的合计」取差值会踩两个坑：
//!
//! - **包会过期消失**。查询按 `PackageEndTime >= now` 过滤，包一过期就不再出现在响应里，
//!   合计值会**掉下来**，简报立刻变成负数消耗。
//! - **周期会重置**。同一 `PackageCode` 跨周期后 `CapacityUsed` 可能归零。
//!
//! 所以台账以 `ResourceId` 为键逐包记账（**注意不是 `PackageCode`**，见 [`PkgView::key`]），
//! 且每个包的数值**只取观测到的最大值**——包过期后条目留在台账里（不再更新），
//! 合计因此永不回退；只有观测到「同一包、周期起点变了、已用量真的变小了」时
//! 才把旧周期的量归档到 `rolled_used`，既不让合计倒退，也不把重置后的用量重复计入。
//!
//! # 增量在采样那一刻就归位到「采样时刻所属的小时」
//!
//! 每个账号除了累计值，还维护一张**按小时的增量桶** `hours`，由每次采样时
//! 「与上次采样相比涨了多少」累加而成。关键设计：**增量落进「采样时刻所属的那个小时」，
//! 之后不再移动**。这样「第 24 格把余额清零推进新一天」这件事根本不会发生 ——
//! 不需要「封口」这一步，也就没有「封口时机错了就丢数据」的风险。
//!
//! 由此带来一条对上层有约束的推论：**想让某段增量算进第 H 小时，采样就必须在第 H 小时
//! 之内发生**。简报的每小时采样正是按这条规则安排在整点之前的（见
//! [`crate::scheduler`]）。
//!
//! 代价是精度受采样密度限制：**应用没运行的时段不会采样**，那几格就是空的；
//! 恢复运行后的第一次采样会把这段空白期攒下的增量整块记进「恢复后的那个小时」。

use crate::accounts::set_private_permissions;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 小时桶保留天数。60 天 = 1440 个 (日期, 小时) 组合 / 账号，
/// JSON 体积可忽略，但足够回看两个月里的任意一天。
pub const KEEP_DAYS: i64 = 60;

/// 日期 → 24 个增量桶（下标 = 小时）。只保留 `KEEP_DAYS` 天。
pub type HourBuckets = BTreeMap<String, [f64; 24]>;

/// 金额统一保留两位小数，与界面展示口径一致（接口给的是 `805.14000097` 这种精度）
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 一个资源包的观测值 —— 台账的输入。
///
/// 由 `checkin` 从 `get-user-resource` 的响应里解析出来；定义在这里是因为它就是
/// 台账的输入契约（`checkin` 负责解析，`ledger` 负责记账，两边不互相依赖内部结构）。
///
/// ⚠️ `size` / `used` 取的是**本周期**那一组字段（`CycleCapacitySize` / `CycleCapacityUsed`），
/// 不是 `CapacitySize` / `CapacityUsed` —— 后者接口根本不维护（证据见模块头）。
/// 两组字段在「包已用满」时数值相等，所以别凭一次静态观测就换回另一组。
#[derive(Clone, Debug, PartialEq)]
pub struct PkgView {
    /// 资源包唯一标识（`ResourceId`，缺失时退化到 `PackageCode@CycleStartTime`）
    pub key: String,
    pub name: String,
    /// 本周期授予（`CycleCapacitySize`）
    pub size: f64,
    /// 本周期已用（`CycleCapacityUsed`）
    pub used: f64,
    /// 当前周期起点（`CycleStartTime`）：它变化意味着这个包翻了新周期
    pub cycle_start: String,
    /// 本包到期时间（毫秒，`CycleEndTime`）；未知为 `None` —— 供前端「资源包列表」展示
    pub expiry_ms: Option<i64>,
    /// 本包剩余积分（`CycleCapacityRemainPrecise`）—— 展示用，与 `earliest_cycle_end`("余量>0") 口径一致
    pub remaining: f64,
}

/// 台账里一个资源包的条目。数值只增不减（见模块头注释）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PkgEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub used: f64,
    #[serde(default)]
    pub cycle_start: String,
    #[serde(default)]
    pub last_seen: String,
    /// 本包到期时间（毫秒）；`None` = 未知。展示口径，latest-wins 覆盖，不取 max
    #[serde(default)]
    pub expiry_ms: Option<i64>,
    /// 本包剩余积分；展示口径，latest-wins 覆盖，不取 max
    #[serde(default)]
    pub remaining: f64,
}

/// 单个账号的台账。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AcctLedger {
    #[serde(default)]
    pub pkgs: BTreeMap<String, PkgEntry>,
    /// 已翻过周期的包的「旧周期已用量」归档：保住合计不倒退
    #[serde(default)]
    pub rolled_used: f64,
    /// 已翻过周期的包的「旧周期授予量」归档：与 `pkgs` 里各包的 `size` 一起构成累计授予。
    /// 周期重置时授予量不该重复计入「新增」，但**累计值的单调性**仍要保住 ——
    /// 否则「授予量 × 已用比例」这类会回退的包，会在基线里留下一个填不平的坑（见 [`merge_account`]）。
    #[serde(default)]
    pub rolled_granted: f64,
    #[serde(default)]
    pub last_seen: String,
    /// 是否已经采过样。首次真正读到包时只对齐基线：否则「开始统计」那一刻
    /// 会把账号里已有的全部历史积分当成一个小时的新增/消耗报出来。
    #[serde(default)]
    pub seeded: bool,
    /// 按小时的增量桶（消耗），键 = 日期 `YYYY-MM-DD`
    #[serde(default)]
    pub hours_used: HourBuckets,
    /// 按小时的增量桶（授予）
    #[serde(default)]
    pub hours_granted: HourBuckets,
    /// 上次采样时刻（本地时间串）。小时桶靠它算「这次比上次涨了多少」，
    /// 因此**必须与 `last_seen`（只用于展示）分开**：任何写 `last_seen` 的地方
    /// 都不能顺手改它，否则会把一段真实增量抹成 0。
    #[serde(default)]
    pub last_sample_at: String,
    /// 最近一次拉取到的**剩余积分**（任何拉取路径都写这里，见 [`record_credits`]）。
    ///
    /// 这是账户管理与简报**共用**的那个数：不再散落在 `accounts.json` 的
    /// `credit_snapshot` / 签到记录 / 代理路由的内存快照里。
    /// `None` = 还没读到过（界面显示「—」，绝不谎报 0）。
    #[serde(default)]
    pub credits: Option<f64>,
    /// 上面这个读数的时刻（本地 `%Y-%m-%d %H:%M:%S`）。
    /// 两个用途：判断路由快照是否过期；简报只把**与被固化小时同一小时**的读数写进条目
    /// （否则就是拿此刻的余额去假装当时）。
    #[serde(default)]
    pub credits_at: String,
    /// 同一份响应里「还有余量的包里最早的重置/过期时刻」（毫秒）。接管按积分过期优先路由时用它。
    #[serde(default)]
    pub earliest_expiry_ms: Option<i64>,
}

impl AcctLedger {
    /// 累计授予（台账口径，只增）
    pub fn granted(&self) -> f64 {
        self.rolled_granted + self.pkgs.values().map(|p| p.size).sum::<f64>()
    }
    /// 累计已用（含已归档的旧周期用量，只增）
    pub fn used(&self) -> f64 {
        self.rolled_used + self.pkgs.values().map(|p| p.used).sum::<f64>()
    }

    /// 删掉 `KEEP_DAYS` 天以前的桶，避免文件无限增长。
    fn prune_hours(&mut self, today: chrono::NaiveDate) {
        let cutoff = (today - chrono::Duration::days(KEEP_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        // 日期是 `YYYY-MM-DD`，字典序即时间序 —— 直接比字符串即可
        self.hours_used.retain(|d, _| d.as_str() >= cutoff.as_str());
        self.hours_granted.retain(|d, _| d.as_str() >= cutoff.as_str());
    }

    /// 清空小时桶（保留逐包累计值）。只有「清空简报历史」这一条路径会用到它：
    /// 桶是简报唯一的明细来源，不清掉的话下一次封口会把刚清掉的历史原样重建出来。
    pub fn clear_hours(&mut self) {
        self.hours_used.clear();
        self.hours_granted.clear();
    }

    /// 最近一次余额读数，且**只有当它恰好落在 `(date, hour)` 这一小时里**才返回。
    ///
    /// 固化历史小时（应用关了两天再打开、或补算）时，台账里的读数是「此刻」的，
    /// 拿它去标注两天前那一个小时就是编数字。所以宁可显示「—」。
    pub fn credits_in_hour(&self, date: &str, hour: u8) -> Option<f64> {
        let (d, h) = parse_at(&self.credits_at)?;
        if d == date && h == hour as usize {
            self.credits
        } else {
            None
        }
    }
}

/// 全账号台账。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Ledger {
    #[serde(default)]
    pub accts: BTreeMap<String, AcctLedger>,
}

/// 把一次观测并进每个资源包，返回「这一轮是否真的读到了包」。
///
/// 只做合并与归档，**不产生任何增量**：增量记账在 [`merge_account`] 里，
/// 而「只对齐基线」的 [`rebaseline`] 走的正是本函数。
///
/// 规则（每条都对应模块头里说的一个坑）：
/// - 新包 → 直接入账（它的 `size` 就是这段时间的新增）
/// - 已存在 → `size`/`used` 取观测最大值，包过期消失时条目留在台账里，合计不会倒退
/// - 同一包换了周期且 `used` 真的回退了 → 把旧周期用量归档进 `rolled_used` 并重设该包基线，
///   使 `used()` 保持不降；授予量同样归档（不重复计入新增，但保住单调性）
fn merge_pkgs(led: &mut AcctLedger, views: &[PkgView], at: &str) -> bool {
    let mut touched = false;
    for v in views {
        touched = true;
        match led.pkgs.get_mut(&v.key) {
            None => {
                led.pkgs.insert(
                    v.key.clone(),
                    PkgEntry {
                        name: v.name.clone(),
                        size: v.size,
                        used: v.used,
                        cycle_start: v.cycle_start.clone(),
                        last_seen: at.to_string(),
                        expiry_ms: v.expiry_ms,
                        remaining: v.remaining,
                    },
                );
            }
            Some(e) => {
                if e.cycle_start != v.cycle_start && v.used < e.used {
                    // 翻了周期且已用量回退：把旧的观测值整体归档，然后**按最大值**并入新周期的观测。
                    //
                    // 这里不能「用新值覆盖旧值」：`size` 比 `used` 更常回退
                    // （「授予量 × 已用比例」型的包每月都会把授予量重新算一遍，可以缩水），
                    // 一旦 `granted()` 掉下去，而增量按 `max(0)` 记账，那个缺口就**永远补不回来** ——
                    // 之后每次给这个包授予积分都会被缺口先吃掉，表现为「新增积分迟迟不显示」。
                    // 取最大值则两个不变量同时成立：合计单调不降，且**任何真实增量都不被吞掉**
                    //（`size` 因取 max 而不上浮，新增就完整地表现成增量）。
                    led.rolled_used += e.used;
                    led.rolled_granted += e.size;
                    e.used = v.used;
                    e.size = v.size;
                } else {
                    if v.used > e.used {
                        e.used = v.used;
                    }
                    if v.size > e.size {
                        e.size = v.size;
                    }
                }
                if v.name != e.name {
                    e.name = v.name.clone();
                }
                e.cycle_start = v.cycle_start.clone();
                e.last_seen = at.to_string();
                // 到期时间 / 剩余是**展示口径**，latest-wins 直接覆盖，不取 max：
                // 包续期会推晚到期、消耗会使剩余下降，若被 max 卡住就永远停在旧值上了。
                e.expiry_ms = v.expiry_ms;
                e.remaining = v.remaining;
            }
        }
    }
    touched
}

/// 只记「剩余积分」读数 —— 给**没有逐包明细**的那条路径用（签到：只查一次汇总值）。
///
/// 它刻意**不碰** `earliest_expiry_ms`：签到这条路径根本不知道包的过期时间，
/// 顺手清掉它会让接管路由的排序依据丢一次。
///
/// `credits` 为 `None`（这一轮没读到）时**整段不动**：读不到 ≠ 余额变成 0，
/// 保留上一次读数比抹成一个假的 0 有用得多。
pub fn record_credits(led: &mut AcctLedger, credits: Option<f64>, at: &str) {
    if credits.is_none() {
        return;
    }
    led.credits = credits;
    led.credits_at = at.to_string();
}

/// 记录一次**完整**读数（余额 + 最早过期时间）。有逐包明细的路径走 [`observe`]，
/// 它内部调用的就是这个 —— 两者是同一组字段的唯二写入点。
fn record_view(led: &mut AcctLedger, credits: Option<f64>, expiry_ms: Option<i64>, at: &str) {
    if credits.is_none() {
        return;
    }
    led.credits = credits;
    // 余额为零/未知时这里就是 None：路由该把它当「没存量」，而不是留着上一次的过期时间。
    led.earliest_expiry_ms = expiry_ms;
    led.credits_at = at.to_string();
}

/// 一次观测要按哪种口径并进台账。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 正常记账：把「与上次采样相比的增量」累加进当前小时桶。
    Normal,
    /// 只对齐基线：包明细照常并入（累计值不倒退），但这段时间的增量**整段丢弃**。
    /// 唯一的用例是「开启简报」：那一刻要的是「从现在开始算」。
    Baseline,
}

/// 把一次观测并进某个账号的台账：**并入逐包明细 + 记下读数**，并按 `mode` 决定是否记增量。
///
/// `at` 是本次采样时刻（本地 `YYYY-MM-DD HH:MM:SS`）。
///
/// 三个分支的顺序是命门：**没读到包 → 首次读到 / 只对齐基线 → 正常记账**。
/// `last_sample_at` 只在这几个分支里推进，函数末尾**不能有无条件兜底赋值**
/// （会覆盖「空视图不推进采样时刻」这条守卫）。
pub fn observe(
    led: &mut AcctLedger,
    views: &[PkgView],
    credits: Option<f64>,
    expiry_ms: Option<i64>,
    at: &str,
    mode: Mode,
) {
    // 读数先落地：即便下面因为空视图提前返回，「这一轮确实读到了余额」也是事实
    record_view(led, credits, expiry_ms, at);

    // 采样前先记下累计值：本次增量 = 合并之后 − 合并之前
    let before = (led.granted(), led.used());
    let touched = merge_pkgs(led, views, at);

    if !touched {
        // `views` 为空（账号一个包都没有 / 接口失败）时**不能**记增量：
        // 那不是「没有变化」，只是「这一轮没读到」。累计值仍是上次观测的最大值，
        // 既没有增量可记，也**不能推进采样时刻** —— 否则下一次真正读到时会凭空多出
        // 一段横跨整个空档的增量，全部落进一个尴尬的小时里，看起来就像那个小时
        // 突然花掉一大笔。首次采样拿到空视图时同理**不能置 `seeded`**：
        // 那会让「全部历史积分」在下次采样时被当成一个小时的增量报出来。
        led.last_seen = at.to_string();
        return;
    }

    if !led.seeded || mode == Mode::Baseline {
        // 首次真的读到包 / 显式要求只对齐基线：只建立基线，不产生任何桶
        // （历史量不属于「这一小时」，断档期的量也归不到具体的小时）
        led.seeded = true;
        led.last_sample_at = at.to_string();
    } else if let Some((date, hour)) = parse_at(at) {
        let (granted, used) = (led.granted(), led.used());
        // `max(0)` 是防御：合并规则已保证累计不降，负增量只可能来自手改的 json
        let d_used = (used - before.1).max(0.0);
        let d_granted = (granted - before.0).max(0.0);
        // 归入「本次采样时刻」所属的小时。跨过整点的那段增量会整块落进后一个小时 ——
        // 采样越密越准，这正是「应用运行期间逐小时」的含义。
        if d_used > 0.0 {
            led.hours_used.entry(date.clone()).or_insert([0.0; 24])[hour] += d_used;
        }
        if d_granted > 0.0 {
            led.hours_granted.entry(date.clone()).or_insert([0.0; 24])[hour] += d_granted;
        }
        led.prune_hours(
            chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .unwrap_or_else(|_| chrono::Local::now().date_naive()),
        );
        led.last_sample_at = at.to_string();
    }

    // 注意：`last_sample_at` 只在上面各分支里推进，**不在这里兜底** ——
    // 「空视图不推进」正是靠它不被无条件覆盖。（`last_seen` 只用于展示，随便更新。）
    led.last_seen = at.to_string();
}

/// 只关心「逐包明细 + 小时增量」时的简写（内部就是 [`observe`]）。
///
/// 生产代码里没有调用点 —— 余额读数是「积分事实」的唯一来源，任何真实拉取都必须把读到的
/// 汇总值一起交上来；只有**测试**会只喂包明细，写成 `observe(..., None, None, at, Mode::Normal)`
/// 会把「这里没有读数」这个信息藏进一串 `None` 里。
#[cfg(test)]
fn merge_account(led: &mut AcctLedger, views: &[PkgView], at: &str) {
    observe(led, views, None, None, at, Mode::Normal);
}

/// 从 `YYYY-MM-DD HH:MM:SS` 里取出（日期, 小时）。格式不对则返回 None（不记账，
/// 但不影响累计值 —— 宁可少一格明细，也不能写进错误的日期）。
fn parse_at(at: &str) -> Option<(String, usize)> {
    let (d, rest) = at.split_once(' ')?;
    let hour: usize = rest.get(0..2)?.parse().ok()?;
    if hour > 23 {
        return None;
    }
    // 顺便校验日期段，避免把畸形串写进 hours 的键
    chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
    Some((d.to_string(), hour))
}

/// 把台账里比 `KEEP_DAYS` 旧的桶清掉。
///
/// `observe` 每次写入时已经会顺手剪一次，这里是给「长期没采样、刚打开应用」
/// 的情况补一刀：否则首次采样前文件里可能还躺着几个月前的桶。
pub fn prune_buckets(accts: &mut BTreeMap<String, AcctLedger>, today: chrono::NaiveDate) {
    for a in accts.values_mut() {
        a.prune_hours(today);
    }
}

// ── 落盘 ───────────────────────────────────────────────────────

pub fn ledger_file(dir: &Path) -> PathBuf {
    dir.join("credit_ledger.json")
}

/// 原子写：先写 `.tmp` 再 rename，避免中途崩溃留下半截 JSON
fn write_atomic(target: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, target)?;
    set_private_permissions(target);
    Ok(())
}

/// 台账文件的 schema 版本。
///
/// **对不上直接丢弃**（与简报文件同一套策略）。这一版从 1 升到 2，原因是记账换组了：
/// 旧文件里存的是 `Capacity*` 家族的最大值（那一组接口根本不维护、多为 0），
/// 若拿它当 `Cycle*` 的基线，换组后的第一次采样就会把「0 → 本周期已用」整笔算成
/// 某一个小时的消耗 —— 那是纯粹的假账。丢掉旧台账的代价只是重新对齐一次基线
/// （`seeded = false` ⇒ 首次只建基线、不产生增量），没有任何真实消耗会因此少算。
pub const SCHEMA: u32 = 2;

/// 台账文件的包装：带 schema 版本
#[derive(Serialize, Deserialize)]
struct LedgerFile {
    v: u32,
    #[serde(default)]
    accts: BTreeMap<String, AcctLedger>,
}

/// 解析台账文件。抽出来是为了能单测「旧格式被安全丢弃」这件事 ——
/// 这类「读旧文件读到错口径」的 bug 只在真实升级路径上出现，手点很难覆盖。
fn decode(raw: &str) -> Ledger {
    if raw.trim().is_empty() {
        return Ledger::default();
    }
    let Ok(f) = serde_json::from_str::<LedgerFile>(raw) else {
        return Ledger::default();
    };
    if f.v != SCHEMA {
        return Ledger::default();
    }
    Ledger { accts: f.accts }
}

pub fn load_ledger(dir: &Path) -> Ledger {
    let f = ledger_file(dir);
    if !f.exists() {
        return Ledger::default();
    }
    decode(&fs::read_to_string(&f).unwrap_or_default())
}

pub fn save_ledger(dir: &Path, led: &Ledger) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(&LedgerFile {
        v: SCHEMA,
        accts: led.accts.clone(),
    })?;
    write_atomic(&ledger_file(dir), &body)
}

/// 投影给界面的「积分事实」：账户管理与简报共用同一份来源（台账）。
///
/// 它**不落盘**（落盘的是台账本身），只用于把读数随账号列表一起发给前端 ——
/// 前端因此不需要知道台账的存在，也不会出现第二份会漂移的副本。
#[derive(Serialize, Clone, Debug, Default)]
pub struct CreditFact {
    /// 剩余积分（`None` = 还没读到过 ⇒ 界面显示「—」）
    pub credits: Option<f64>,
    /// 读数时刻（本地时间串），界面上「上次读到」用它
    pub at: String,
    /// 最早重置/过期时刻（毫秒）
    pub earliest_expiry_ms: Option<i64>,
    /// 逐资源包明细（`{name, remaining, expiry_ms}`），供前端「资源包列表」展示
    #[serde(default)]
    pub packages: Vec<CreditPackage>,
}

/// 一个资源包的展示快照：只含「还有余量的包」（余量为 0 的包对用户没有意义）。
#[derive(Serialize, Clone, Debug)]
pub struct CreditPackage {
    pub name: String,
    pub remaining: f64,
    pub expiry_ms: Option<i64>,
}

/// 从台账的逐包条目投影出展示用的资源包列表（过滤掉余量为 0 的包）。
fn project_packages(a: &AcctLedger) -> Vec<CreditPackage> {
    let mut out: Vec<CreditPackage> = a
        .pkgs
        .values()
        .filter(|p| p.remaining > 0.0)
        .map(|p| CreditPackage {
            name: p.name.clone(),
            remaining: p.remaining,
            expiry_ms: p.expiry_ms,
        })
        .collect();
    // 排序稳定：最早到期的排前面，无到期的排最后；方便前端直接吃第 0 项当「快过期」
    out.sort_by_key(|p| p.expiry_ms.unwrap_or(i64::MAX));
    out
}

/// 取某个账号的积分事实：台账里**存过读数**才有 (否则返回 `None`，
/// 由调用方回退到其它展示口径，而不是编一个 0 出来)。
pub fn fact(led: &Ledger, id: &str) -> Option<CreditFact> {
    let a = led.accts.get(id)?;
    if a.credits_at.is_empty() {
        return None;
    }
    Some(CreditFact {
        credits: a.credits,
        at: a.credits_at.clone(),
        earliest_expiry_ms: a.earliest_expiry_ms,
        packages: project_packages(a),
    })
}

/// 一次观测的读数（**采集阶段的产物**，还没并进台账）。
///
/// 存在的理由是把「打接口」与「记账」分开：采集要跨 `await`、要几秒到几十秒，
/// 而台账锁只能在内存操作期间持有。先攒齐读数、再一次性并账，
/// 锁的持有时间就从「整个采样过程」缩到微秒级（见 [`Store::update`]）。
pub struct Reading {
    pub id: String,
    pub packages: Vec<PkgView>,
    pub credits: Option<f64>,
    pub expiry_ms: Option<i64>,
}

/// 台账的**进程内唯一副本**：所有读写都走它，落盘由它统一负责。
///
/// # 为什么必须收口
///
/// 过去每条路径（签到 / 刷新 / 每小时采样 / 接管路由 / 开启简报 …）都是
/// 「读文件 → 改内存 → 写文件」。两个路径并发时会**互相覆盖**：调度线程刚采到的读数，
/// 可能被一条命令手里那份「读于采样之前」的旧副本整体盖回去 —— 典型的读-改-写竞态，
/// 而且因为每次都写全量文件，丢失是**整块**丢失，不是少一条。
///
/// 收成一个 `Mutex<Ledger>` 之后：并发被串行化、落盘只剩一个出口、
/// 界面上两个页面读到的也必然是同一份数（见 `commands::emit_credits`）。
///
/// # 锁的边界
///
/// 只在内存操作期间持锁，**绝不跨 `await`**：采集在锁外完成（[`Reading`]），
/// 攒齐后一次性 [`Store::apply`]。跨 `await` 持锁会把签到、刷新、路由全部堵死。
pub struct Store {
    dir: PathBuf,
    led: Mutex<Ledger>,
    /// 写入后的通知（见 [`ChangeHook`]）。
    ///
    /// 挂在实例上而不是做全局静态：生产只有一个 `Store`（见 [`store`]），语义完全一样；
    /// 但测试可以各建各的实例、各自注册，不会互相污染。
    hook: OnceLock<ChangeHook>,
}

impl Store {
    /// 把一批读数并进台账：**唯一的记账入口**，锁内只做纯内存操作。
    ///
    /// `at` 是这批读数的采样时刻（本地 `YYYY-MM-DD HH:MM:SS`），
    /// 增量按它归属到「采样时刻所属的那个小时」。
    pub fn apply(&self, readings: &[Reading], at: &str, mode: Mode) -> std::io::Result<()> {
        self.update(|led| {
            for r in readings {
                observe(
                    led.accts.entry(r.id.clone()).or_default(),
                    &r.packages,
                    r.credits,
                    r.expiry_ms,
                    at,
                    mode,
                );
            }
            // 清掉超期的桶：`observe` 只对「读到了包的账号」剪枝，
            // 接口连续失败的账号永远轮不到它，桶会一直涨。
            prune_buckets(&mut led.accts, chrono::Local::now().date_naive());
        })
    }

    /// 只记「剩余积分」读数（签到那条路径：没有逐包明细）。
    ///
    /// 与 [`Store::apply`] 的区别是**不碰最早过期时间**：签到接口不返回包的过期时间，
    /// 顺手清掉只会让接管路由的排序依据丢一次（见 [`record_credits`]）。
    /// 每项是 `(账号 id, 余额, 读数时刻)` —— 时刻逐条不同，签到是一个账号一个账号走的。
    pub fn apply_credits(&self, readings: &[(String, f64, String)]) -> std::io::Result<()> {
        self.update(|led| {
            for (id, balance, at) in readings {
                record_credits(led.accts.entry(id.clone()).or_default(), Some(*balance), at);
            }
        })
    }

    /// 某个账号的积分事实（只投影一条，不把整本台账搬出来）
    pub fn fact_of(&self, id: &str) -> Option<CreditFact> {
        self.read(|led| fact(led, id))
    }

    /// 注册「台账被写过」的回调（幂等：只认第一次注册）。
    pub fn on_change(&self, hook: ChangeHook) {
        let _ = self.hook.set(hook);
    }

    /// 通知一次变更。**必须在台账锁之外调用** —— 回调要重新读一遍台账。
    fn notify_change(&self) {
        if let Some(hook) = self.hook.get() {
            hook();
        }
    }

    /// 只在测试里用：探一下锁此刻是不是空的。
    ///
    /// 「通知发生在锁外」这条约束要能断言，而真的去抢锁会把实现写错的情况变成
    /// 测试挂死（而不是失败）。`try_lock` 让它变成一次可读的断言失败。
    #[cfg(test)]
    fn lock_is_free(&self) -> bool {
        self.led.try_lock().is_ok()
    }

    fn open(dir: PathBuf) -> Self {
        let led = load_ledger(&dir);
        Store {
            dir,
            led: Mutex::new(led),
            hook: OnceLock::new(),
        }
    }

    /// 读一份投影。闭包里只许做纯内存操作 —— 持锁打接口会把别的路径全卡住。
    pub fn read<R>(&self, f: impl FnOnce(&Ledger) -> R) -> R {
        // 锁中毒（持锁线程 panic）不该让整个应用再也读不到台账：
        // 台账是「只增的计数器」，中毒后数据仍然自洽，取回内层继续用即可。
        let g = self.led.lock().unwrap_or_else(|e| e.into_inner());
        f(&g)
    }

    /// 改 + 落盘（**唯一写入口**）。返回闭包的结果；落盘失败把错误交给调用方决定。
    ///
    /// 落盘失败不回滚内存：内存里的值是对的，下次任意一次写入都会再落一遍，
    /// 所以调用方通常只需记一笔日志。
    pub fn update<R>(&self, f: impl FnOnce(&mut Ledger) -> R) -> std::io::Result<R> {
        let mut g = self.led.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut g);
        let saved = save_ledger(&self.dir, &g);
        // 先放锁再通知：回调要重新读一遍台账（投影出给界面的那份），还握着锁就是自锁。
        drop(g);
        // 落盘失败也要通知 —— 内存里的值是对的，界面该显示的就是它（见上「不回滚」）。
        self.notify_change();
        saved?;
        Ok(r)
    }
}

/// 进程内唯一的台账副本。首次调用按 `dir` 打开，之后忽略该参数。
///
/// 不做成 Tauri 的 managed state 是为了让 `scheduler` / `proxy` 两个后台线程也能收口 ——
/// 它们手上只有 `dir`，走单例就不必把 `AppHandle` 沿调用链传下去。
/// `dir` 在整个进程里唯一（`app_data_dir`），所以「谁先调用」不影响结果。
pub fn store(dir: &Path) -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| Store::open(dir.to_path_buf()))
}

/// 「台账被写过」的回调（应用启动时注册一次，见 `lib.rs`）。
///
/// 把「写台账」与「通知界面」绑在同一处，正是这次抽象要做的事：台账既然收成了
/// **全局唯一的内存变量**，它的变更就该自己传播出去 —— 而不是由每个调用点各自
/// 记得广播一遍。后者一定会漏，而且已经漏过：接管路由补拉到的读数
/// （`proxy::choose_account` 手上没有 `AppHandle`）从来没有回流过界面，
/// 于是「账户展示的资源包过期时间」与「路由排序用的那一份」各自为政。
type ChangeHook = Box<dyn Fn() + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(key: &str, size: f64, used: f64, cycle: &str) -> PkgView {
        PkgView {
            key: key.into(),
            name: format!("包{key}"),
            size,
            used,
            cycle_start: cycle.into(),
            // 既有用例都不关心这两项：到期时间未知，余量按「授予 − 已用」推
            expiry_ms: None,
            remaining: size - used,
        }
    }

    /// 采样并顺手返回该账号的桶（绝大多数用例只关心桶，不关心累计值）
    fn sample(led: &mut Ledger, id: &str, views: &[PkgView], at: &str) {
        merge_account(led.accts.entry(id.into()).or_default(), views, at);
    }

    /// 某个账号某一天的 24 个桶
    fn day(led: &Ledger, id: &str, date: &str) -> [f64; 24] {
        led.accts[id].hours_used.get(date).copied().unwrap_or([0.0; 24])
    }

    // ── 累计值不倒退 ──────────────────────────────────────────

    #[test]
    fn merge_keeps_maxima_so_an_expiring_package_never_rewinds_the_total() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("a", 100.0, 40.0, "d1")], "2026-09-15 10:00:00");
        assert_eq!((led.granted(), led.used()), (100.0, 40.0));

        merge_account(&mut led, &[pkg("a", 100.0, 70.0, "d1")], "2026-09-15 11:00:00");
        assert_eq!(led.used(), 70.0);

        // 包过期、从响应里消失 —— 合计必须保持 70，不能掉回 0
        merge_account(&mut led, &[], "2026-09-15 12:00:00");
        assert_eq!((led.granted(), led.used()), (100.0, 70.0));
    }

    #[test]
    fn cycle_rollover_archives_the_old_cycle_instead_of_rewinding() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("a", 500.0, 320.0, "9月")], "2026-09-30 10:00:00");
        // 翻到新周期：同一个月度包，已用量归零
        merge_account(&mut led, &[pkg("a", 500.0, 0.0, "10月")], "2026-10-01 10:00:00");
        assert_eq!(led.used(), 320.0, "归零应被归档，合计不得倒退");
        // 新周期里再消耗 80 → 合计 400
        merge_account(&mut led, &[pkg("a", 500.0, 80.0, "10月")], "2026-10-01 11:00:00");
        assert_eq!(led.used(), 400.0);
        // 授予量：归档的 500 + 新周期的 500。**周期重置后授予量确实又发了一次**，
        // 所以这里不是 500 而是 1000 —— 它是「累计授予」这一计数器本身的语义。
        // 「不重复计入新增」靠的是增量记账（旧周期没产生 500 的授予增量），
        // 而不是靠把累计值压在 500。
        assert_eq!(led.granted(), 1000.0);
    }

    /// 回归：授予量会随周期回退的包（「授予量 × 已用比例」型）。
    ///
    /// 关键在于 `granted()` 必须**单调不降**：一旦它掉下去，而增量又按 `max(0)` 记账，
    /// 那个缺口永远补不回来 —— 之后每次给这个包授予积分都会被缺口先吃掉一部分，
    /// 表现为「新增积分要等好久才显示出来」。这条断言就是钉住这个坑。
    #[test]
    fn rollover_that_also_shrinks_the_grant_keeps_granted_monotonic() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("m", 100.0, 90.0, "9月")], "2026-09-30 10:00:00");
        assert_eq!(led.granted(), 100.0);

        // 新周期：授予量缩到 50、已用归零 → 累计 = 归档 100 + 新周期 50
        merge_account(&mut led, &[pkg("m", 50.0, 0.0, "10月")], "2026-10-01 10:00:00");
        assert_eq!(led.granted(), 150.0, "授予量不得因周期重置而回退");
        assert_eq!(led.used(), 90.0);

        // 同一个周期内，观测值从 50 涨回 100：这就是真实发生的「又授予 50」。
        // **必须报成 +50 的新增** —— 若此时还拿归档的 100 去取 max，
        // 新增会被永久吞掉，用户看到的就是「发了积分但简报不动」。
        merge_account(&mut led, &[pkg("m", 100.0, 0.0, "10月")], "2026-10-01 11:00:00");
        assert_eq!(led.granted(), 200.0, "新周期内涨的量必须算进累计，不能被归档值盖住");
    }

    /// 与上一条相反的情形：新周期的授予量**更高**时，高出来的部分必须是「新增」。
    #[test]
    fn rollover_that_raises_the_grant_reports_the_increase_as_gained() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("m", 100.0, 90.0, "9月")], "2026-09-30 10:00:00");

        // 新周期授予量升到 120：累计 = 归档 100 + 新周期 120 = 220
        merge_account(&mut led, &[pkg("m", 120.0, 0.0, "10月")], "2026-10-01 10:00:00");
        assert_eq!(led.granted(), 220.0);
        assert_eq!(led.used(), 90.0, "已用量归零要归档，不能倒退");
    }

    // ── 小时桶 ────────────────────────────────────────────────

    #[test]
    fn first_sample_only_seeds_the_baseline_and_writes_no_bucket() {
        // 账号里本来就躺着 100 授予 / 90 已用（历史用量）。
        // 第一次采样只建立基线：不产生任何小时桶，否则历史量会被算成「这一小时的消耗」。
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("a", 100.0, 90.0, "d1")], "2026-09-15 09:30:00");
        let a = &led.accts["a1"];
        assert!(a.seeded);
        assert!(a.hours_used.is_empty(), "首采样不该写桶");
        assert!(a.hours_granted.is_empty());

        // 此后真的又消耗 10 → 只报这 10
        sample(&mut led, "a1", &[pkg("a", 100.0, 100.0, "d1")], "2026-09-15 09:45:00");
        assert_eq!(day(&led, "a1", "2026-09-15")[9], 10.0);
    }

    /// 回归：**首采样拿到空视图**（接口失败 / 账号里一个包都没有）时不能置 `seeded`。
    ///
    /// 置了的话，下一次真读到包时就会被当成「上次采样到现在涨了这么多」——
    /// 而那个「上次」根本没有基准（累计值是 0），于是**装机以来的全部历史用量**
    /// 会被一次性报进某一个小时里。这条断言钉住这个坑。
    #[test]
    fn an_empty_first_view_does_not_seed_the_baseline() {
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[], "2026-09-15 09:00:00");
        assert!(!led.accts["a1"].seeded, "空视图不是「采到了」，不能建立基线");

        // 第一次真正读到：只建基线，不报增量
        sample(&mut led, "a1", &[pkg("p", 500.0, 300.0, "d")], "2026-09-15 10:00:00");
        assert!(led.accts["a1"].seeded);
        assert!(led.accts["a1"].hours_used.is_empty(), "历史量不该变成某一个小时的消耗");

        // 之后涨的才算数
        sample(&mut led, "a1", &[pkg("p", 500.0, 310.0, "d")], "2026-09-15 10:30:00");
        assert_eq!(day(&led, "a1", "2026-09-15")[10], 10.0);
    }

    #[test]
    fn increments_land_in_the_hour_of_the_sample_that_observed_them() {
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-09-15 08:00:00");

        // 08 点这一段 +30
        sample(&mut led, "a1", &[pkg("p", 1000.0, 30.0, "d")], "2026-09-15 08:59:00");
        // 跨过整点后的一次采样：整段 +20 归入 09 点
        sample(&mut led, "a1", &[pkg("p", 1000.0, 50.0, "d")], "2026-09-15 09:01:00");
        // 11 点（10 点整点之前那段没有采样，不产生桶）
        sample(&mut led, "a1", &[pkg("p", 1000.0, 80.0, "d")], "2026-09-15 11:30:00");

        let hu = day(&led, "a1", "2026-09-15");
        assert_eq!(hu[8], 30.0);
        assert_eq!(hu[9], 20.0);
        assert_eq!(hu[10], 0.0, "没有采样的时段不该凭空有值");
        assert_eq!(hu[11], 30.0);
        // 桶之和 = 当天累计增量
        assert_eq!(hu.iter().sum::<f64>(), 80.0);
    }

    #[test]
    fn a_sample_landing_in_the_next_day_opens_that_day_bucket() {
        // 跨天采样：增量整块记进「采样时刻」那一天，前一天的桶不受影响。
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 23:00:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 70.0, "d")], "2026-09-15 23:50:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 90.0, "d")], "2026-09-16 00:10:00");

        assert_eq!(day(&led, "a1", "2026-09-15")[23], 70.0);
        assert_eq!(day(&led, "a1", "2026-09-16")[0], 20.0);
        // 两天相加 = 累计增量，不重不漏
        let total: f64 = led.accts["a1"].hours_used.values().flatten().sum();
        assert_eq!(total, 90.0);
    }

    #[test]
    fn an_empty_view_does_not_create_a_delta_or_move_the_sample_clock() {
        // 接口失败 / 账号一个包都没有时 `views` 为空。这**不是**「没有变化」，
        // 只是这一轮没读到 —— 累计值仍是上次的最大值，所以不该产生增量，
        // 采样时刻也不能动，否则下次真读到时会凭空多出一段横跨空档的增量。
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 09:00:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 40.0, "d")], "2026-09-15 10:00:00");

        sample(&mut led, "a1", &[], "2026-09-15 11:00:00");
        let a = &led.accts["a1"];
        assert!(a.hours_used.is_empty() || day(&led, "a1", "2026-09-15")[11] == 0.0);
        assert_eq!(a.last_sample_at, "2026-09-15 10:00:00", "空视图不该推进采样时刻");
    }

    #[test]
    fn negative_movement_is_never_recorded_as_a_bucket() {
        // 包过期消失 / 周期重置都不是「消耗」，不能写成负数把某天的量冲掉
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 500.0, 300.0, "9月")], "2026-09-30 10:00:00");
        sample(&mut led, "a1", &[pkg("p", 500.0, 0.0, "10月")], "2026-10-01 09:00:00");
        assert!(
            day(&led, "a1", "2026-10-01").iter().all(|v| *v == 0.0),
            "周期重置不产生桶"
        );
        assert_eq!(led.accts["a1"].used(), 300.0, "但累计值保住了");
    }

    #[test]
    fn a_malformed_timestamp_is_skipped_without_corrupting_the_bucket_map() {
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 10:00:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 40.0, "d")], "坏时刻");
        let a = &led.accts["a1"];
        assert!(a.hours_used.is_empty(), "畸形时刻不记账，也不能写进错误的日期");
        assert_eq!(a.used(), 40.0, "累计值照常更新");
    }

    #[test]
    fn buckets_are_pruned_but_recent_days_stay() {
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-01-01 00:00:00");
        // 造两个桶：一个远超出保留期、一个就在「今天」。
        // 注意第一次 sample 时 2026-01-02 就已经超期了（远离 03-20），会被当场剪掉 ——
        // 剪枝是在**每次写入时**做的，不是只在读的时候。
        sample(&mut led, "a1", &[pkg("p", 1000.0, 10.0, "d")], "2026-01-02 05:00:00");
        sample(&mut led, "a1", &[pkg("p", 1000.0, 20.0, "d")], "2026-03-20 05:00:00");
        assert_eq!(led.accts["a1"].hours_used.len(), 1, "超期桶当场被剪掉");
        assert!(led.accts["a1"].hours_used.contains_key("2026-03-20"));

        // 保留期**之内**的旧天不该被剪掉
        let mut led2 = Ledger::default();
        sample(&mut led2, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-03-19 00:00:00");
        sample(&mut led2, "a1", &[pkg("p", 1000.0, 10.0, "d")], "2026-03-19 05:00:00");
        sample(&mut led2, "a1", &[pkg("p", 1000.0, 20.0, "d")], "2026-03-20 05:00:00");
        assert_eq!(led2.accts["a1"].hours_used.len(), 2, "昨天的桶必须留着");

        // 显式剪枝到某天：比保留期旧的清掉，近期的留下
        let today = chrono::NaiveDate::parse_from_str("2026-06-01", "%Y-%m-%d").unwrap();
        prune_buckets(&mut led2.accts, today);
        assert!(led2.accts["a1"].hours_used.is_empty(), "3 月的桶在 6 月该清掉");
    }

    // ── 只对齐基线 ────────────────────────────────────────────

    /// `Mode::Baseline` 要同时做两件相反的事：包明细照常并入（累计值不许倒退），
    /// 但增量一个都不许记。开启简报时全靠它避免「开启当天凭空多出一大笔消耗」。
    #[test]
    fn rebaseline_merges_packages_but_records_no_bucket() {
        let mut led = AcctLedger::default();
        // 上一次采样是三天前：这中间攒下的 400 点消耗
        merge_account(&mut led, &[pkg("p", 500.0, 0.0, "d")], "2026-09-12 10:00:00");
        assert_eq!(led.used(), 0.0);

        observe(
            &mut led,
            &[pkg("p", 500.0, 400.0, "d")],
            Some(2100.0),
            Some(1789954208000),
            "2026-09-15 10:00:00",
            Mode::Baseline,
        );
        assert_eq!(led.used(), 400.0, "累计值必须跟上，否则下次增量会把它重复记一遍");
        assert!(led.hours_used.is_empty(), "断档期的增量不该被塞进开启后的第一个小时");
        assert_eq!(led.last_sample_at, "2026-09-15 10:00:00");
        // 只对齐基线也要记下读数：那一刻读到的余额就是界面上要显示的数
        assert_eq!(led.credits, Some(2100.0));
        assert_eq!(led.credits_at, "2026-09-15 10:00:00");
        assert_eq!(led.earliest_expiry_ms, Some(1789954208000));

        // 基线已对齐 ⇒ 之后涨的那 30 才是真的增量
        //（这条同时排除了「rebaseline 被写成永久闭嘴」）
        merge_account(&mut led, &[pkg("p", 500.0, 430.0, "d")], "2026-09-15 10:30:00");
        assert_eq!(led.hours_used["2026-09-15"][10], 30.0);
    }

    /// 开启简报时要清的是「明细」，不是累计值：桶清了，逐包累计必须原样留着，
    /// 否则下一次采样会把装机以来的 `CapacityUsed` 整个算成一个小时的消耗。
    #[test]
    fn clear_hours_keeps_the_running_totals() {
        let mut led = Ledger::default();
        sample(&mut led, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-09-15 08:00:00");
        sample(&mut led, "a1", &[pkg("p", 1000.0, 90.0, "d")], "2026-09-15 09:00:00");
        assert_eq!(day(&led, "a1", "2026-09-15")[9], 90.0);

        led.accts.get_mut("a1").unwrap().clear_hours();
        assert!(led.accts["a1"].hours_used.is_empty());
        assert_eq!(led.accts["a1"].used(), 90.0, "累计值不属于「明细」，必须留着");
        assert_eq!(led.accts["a1"].last_sample_at, "2026-09-15 09:00:00");
    }

    // ── 积分读数（账户管理与简报共用这一份）──────────────────────

    /// 读数只由 [`record_credits`] / [`observe`] 写，且**没读到就不许覆盖** —— 读不到 ≠ 余额变 0。
    #[test]
    fn a_missing_credit_reading_never_overwrites_the_last_one() {
        let mut led = AcctLedger::default();
        record_credits(&mut led, None, "2026-09-16 10:00:00");
        assert_eq!(led.credits, None, "从没读到过就是未知，不能编一个 0");
        assert_eq!(led.credits_at, "");

        record_credits(&mut led, Some(3000.0), "2026-09-16 11:00:00");
        assert_eq!(led.credits, Some(3000.0));
        assert_eq!(led.credits_at, "2026-09-16 11:00:00");

        // 这一轮接口失败：读数与时刻都必须原样保留
        record_credits(&mut led, None, "2026-09-16 12:00:00");
        assert_eq!(led.credits, Some(3000.0));
        assert_eq!(led.credits_at, "2026-09-16 11:00:00");
    }

    /// 签到那条路径只有汇总读数、不知道包的过期时间，所以它**不许**顺手清掉过期时间 ——
    /// 清了接管路由的排序依据就丢一次。
    #[test]
    fn a_credits_only_reading_does_not_wipe_the_expiry_hint() {
        let mut led = AcctLedger::default();
        observe(
            &mut led,
            &[pkg("p", 500.0, 0.0, "d")],
            Some(500.0),
            Some(1789954208000),
            "2026-09-16 09:00:00",
            Mode::Normal,
        );
        assert_eq!(led.earliest_expiry_ms, Some(1789954208000));

        record_credits(&mut led, Some(500.0), "2026-09-16 09:30:00");
        assert_eq!(led.credits_at, "2026-09-16 09:30:00", "读数时刻要跟上");
        assert_eq!(
            led.earliest_expiry_ms,
            Some(1789954208000),
            "签到不知道过期时间，不能把它清掉"
        );
    }

    /// 一次带明细的观测要同时做三件事：并包、记桶、记读数。
    #[test]
    fn observe_records_packages_buckets_and_the_reading_in_one_go() {
        let mut led = AcctLedger::default();
        observe(
            &mut led,
            &[pkg("p", 500.0, 0.0, "d")],
            Some(500.0),
            None,
            "2026-09-16 09:00:00",
            Mode::Normal,
        );
        assert!(led.hours_used.is_empty(), "首采样只对齐基线");

        observe(
            &mut led,
            &[pkg("p", 500.0, 30.0, "d")],
            Some(470.0),
            Some(1789954208000),
            "2026-09-16 10:00:00",
            Mode::Normal,
        );
        assert_eq!(led.hours_used["2026-09-16"][10], 30.0);
        assert_eq!(led.credits, Some(470.0));
        assert_eq!(led.credits_at, "2026-09-16 10:00:00");
        assert_eq!(led.earliest_expiry_ms, Some(1789954208000));
    }

    /// 投影给界面的积分事实：没有读数就是 `None`（界面显示「—」），不编 0。
    #[test]
    fn fact_projects_only_what_was_actually_read() {
        let mut led = Ledger::default();
        observe(
            led.accts.entry("a1".into()).or_default(),
            &[pkg("p", 100.0, 0.0, "d")],
            None,
            None,
            "2026-09-16 10:00:00",
            Mode::Normal,
        );
        assert!(fact(&led, "a1").is_none(), "只有包明细、没有读数 ⇒ 不投影");
        assert!(fact(&led, "从未见过的账号").is_none());

        observe(
            led.accts.entry("a1".into()).or_default(),
            &[pkg("p", 100.0, 10.0, "d")],
            Some(90.0),
            Some(1789954208000),
            "2026-09-16 11:00:00",
            Mode::Normal,
        );
        let f = fact(&led, "a1").unwrap();
        assert_eq!(f.credits, Some(90.0));
        assert_eq!(f.at, "2026-09-16 11:00:00");
        assert_eq!(f.earliest_expiry_ms, Some(1789954208000));
    }

    // ── 唯一的写入口（Store）──────────────────────────────────

    /// 一批读数一次入账：内存与落盘都要有 —— 落盘的那份就是内存的快照，不是第二本账。
    #[test]
    fn the_store_takes_a_whole_batch_in_one_go() {
        let dir = std::env::temp_dir().join(format!("wba-store-{}", uuid::Uuid::new_v4()));
        let store = Store::open(dir.clone());
        store
            .apply(
                &[
                    Reading {
                        id: "a1".into(),
                        packages: vec![pkg("p", 100.0, 0.0, "d")],
                        credits: Some(100.0),
                        expiry_ms: None,
                    },
                    Reading {
                        id: "a2".into(),
                        packages: vec![pkg("p", 50.0, 10.0, "d")],
                        credits: Some(40.0),
                        expiry_ms: None,
                    },
                ],
                "2026-09-16 10:00:00",
                Mode::Normal,
            )
            .unwrap();

        assert_eq!(store.fact_of("a2").unwrap().credits, Some(40.0));
        let on_disk = load_ledger(&dir);
        assert_eq!(on_disk.accts["a2"].used(), 10.0);
        assert_eq!(on_disk.accts["a2"].credits, Some(40.0));
        let _ = fs::remove_dir_all(&dir);
    }

    /// 签到那条路径只带回余额，**不许碰最早过期时间** —— 它不知道包的过期时刻，
    /// 顺手清掉会让接管路由的排序依据丢一次。
    #[test]
    fn the_store_keeps_the_expiry_hint_on_a_credits_only_write() {
        let dir = std::env::temp_dir().join(format!("wba-store2-{}", uuid::Uuid::new_v4()));
        let store = Store::open(dir.clone());
        store
            .apply(
                &[Reading {
                    id: "a1".into(),
                    packages: vec![pkg("p", 100.0, 0.0, "d")],
                    credits: Some(100.0),
                    expiry_ms: Some(1789954208000),
                }],
                "2026-09-16 10:00:00",
                Mode::Normal,
            )
            .unwrap();
        assert_eq!(
            store.fact_of("a1").unwrap().earliest_expiry_ms,
            Some(1789954208000)
        );

        store
            .apply_credits(&[("a1".into(), 90.0, "2026-09-16 11:00:00".into())])
            .unwrap();
        let f = store.fact_of("a1").unwrap();
        assert_eq!(f.credits, Some(90.0));
        assert_eq!(f.at, "2026-09-16 11:00:00", "读数与时刻要跟上");
        assert_eq!(
            f.earliest_expiry_ms,
            Some(1789954208000),
            "签到不知道过期时间，不能把它清掉"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 台账的写入口到界面之间只有一条线：**写一次、通知一次**，且通知必须发生在
    /// 放锁之后。后者是这套设计的前提 —— 通知要做的事（`commands::emit_credits`）
    /// 恰恰是重新读一遍台账去投影；在锁里通知就是自锁，签到 / 路由 / 采样会一个接一个
    /// 卡住，而且只在特定时序下复现。验证走**独立实例**（`Store::open`），
    /// 不碰进程级单例，也就不会干扰别的用例。
    #[test]
    fn a_write_notifies_after_the_lock_is_released() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("wba-hook-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(Store::open(dir.clone()));
        let writes = Arc::new(AtomicUsize::new(0));
        let lock_free = Arc::new(AtomicBool::new(false));
        {
            // 闭包捕获的这份要另起个名：`store` 同时还是 `on_change` 的接收者，
            // 同名就成了「借用期间移动」。
            let (inner, writes, lock_free) = (
                Arc::clone(&store),
                Arc::clone(&writes),
                Arc::clone(&lock_free),
            );
            store.on_change(Box::new(move || {
                writes.fetch_add(1, Ordering::SeqCst);
                lock_free.store(inner.lock_is_free(), Ordering::SeqCst);
            }));
        }

        store
            .apply(
                &[Reading {
                    id: "a1".into(),
                    packages: vec![pkg("p", 100.0, 0.0, "d")],
                    credits: Some(100.0),
                    expiry_ms: Some(1789954208000),
                }],
                "2026-09-16 10:00:00",
                Mode::Normal,
            )
            .unwrap();

        assert_eq!(writes.load(Ordering::SeqCst), 1, "写一次只通知一次");
        assert!(
            lock_free.load(Ordering::SeqCst),
            "通知发生时台账锁必须已经放掉"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ── 落盘 ─────────────────────────────────────────────────

    #[test]
    fn ledger_round_trips_and_tolerates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("wba-ledger-{}", uuid::Uuid::new_v4()));
        assert!(load_ledger(&dir).accts.is_empty(), "文件不存在时应是空台账");
        let mut led = Ledger::default();
        observe(
            led.accts.entry("a1".into()).or_default(),
            &[pkg("p", 5.0, 1.0, "d")],
            Some(4.0),
            None,
            "2026-09-15 10:00:00",
            Mode::Normal,
        );
        save_ledger(&dir, &led).unwrap();
        let back = load_ledger(&dir);
        assert_eq!(back.accts["a1"].used(), 1.0);
        assert!(back.accts["a1"].seeded);
        assert_eq!(back.accts["a1"].credits, Some(4.0), "读数也要扛得住重启");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 换口径那一刻最容易出的错：**拿旧口径的最大值当新口径的基线**。
    /// 旧文件（含无版本号的 v1 形状）必须整体丢弃 ⇒ 首次只对齐基线、不产生假增量。
    #[test]
    fn a_file_written_under_the_old_counting_scheme_is_discarded() {
        // v1（换组前的真实形状：只带 Capacity* 最大值）
        let old = r#"{"accts":{"a1":{"pkgs":{"p":{"size":5000.0,"used":0.0,"cycle_start":"d"}},
            "seeded":true,"last_sample_at":"2026-09-16 14:55:13"}}}"#;
        assert!(decode(old).accts.is_empty(), "无版本号的老文件必须丢弃");

        // 版本号对不上的占位文件同理
        assert!(decode(r#"{"v":1,"accts":{"a1":{}}}"#).accts.is_empty());
        assert!(decode("{ 不是 json").accts.is_empty());
        assert!(decode("").accts.is_empty());

        // 新版本照常读
        let mut led = Ledger::default();
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p", 500.0, 0.0, "d")], "t");
        let body = serde_json::to_string(&LedgerFile { v: SCHEMA, accts: led.accts.clone() }).unwrap();
        assert_eq!(decode(&body).accts.len(), 1);
        assert!(decode(&body).accts["a1"].used() == 0.0);
    }
}
