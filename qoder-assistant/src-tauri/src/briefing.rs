//! 积分简报：以**小时**为最小结算单位的消耗 / 新增记录。
//!
//! # 三个概念，两层数据
//!
//! - **时条目**（[`HourEntry`]）：一个小时固化成一条，里面是**逐账号**的扣费明细。
//!   它是唯一落盘的东西，也是「钱花在什么时候」的唯一答案。
//! - **日条目**（[`DayEntry`]）：**当天所有时条目之和**，读的时候现算（[`day_entries`]），
//!   **不落盘**。这样「日 = 时之和」是结构上的事实，而不是一条要靠人守住的约定：
//!   不可能出现「日条目和它下面几个时条目对不上」这种最没得解释的 bug。
//! - **台账**（[`crate::ledger`]）：采样与累计，只回答「谁、在哪个小时、涨了多少」。
//!
//! # 时条目是怎么产生的
//!
//! 由后台**每小时**结算一次（见 [`crate::scheduler`]）：整点之前采一次样，让这一小时的
//! 增量落在即将结束的那个小时里；整点一过，就把所有已经走完、台账里有数据的小时
//! 固化成时条目。固化的输入只有台账里的小时桶，所以它**不依赖网络、也不依赖当时的进程状态**
//! —— 应用关了两天再打开，那两天的时条目一样补得出来（桶留 60 天）。
//!
//! 固过的小时不会重复固（[`unsealed_hours`] 用已有的时条目去重），因此这个动作幂等，
//! 每次调度都跑一遍也没有副作用。
//!
//! **界面上没有任何「手动生成一条」的入口**：条目只由这个定时动作产生。
//!
//! # 台账没采到就没有明细
//!
//! 应用没运行的时段不会采样，那几格的桶是空的 ⇒ 既不会产生时条目，日条目里也没有那一块。
//! 恢复运行后的第一次采样会把这段空白期攒下的增量整块记进「恢复后的那个小时」，
//! 这是台账既有的口径（增量归入采样时刻所属的小时），界面上的说明也照实写。

use crate::accounts::Account;
use crate::ledger::{self, AcctLedger};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 保留的时条目上限。与台账的小时桶同口径（60 天 × 24），
/// 超出后丢最旧的 —— 台账那边桶被剪掉之后也补不出来了，留着上限只会让文件无限长大。
pub const MAX_HOURS: usize = (ledger::KEEP_DAYS as usize) * 24;

/// 简报文件的 schema 版本。与台账/旧日报同一套策略：**版本对不上直接丢弃**，
/// 宁可少几条展示，也不把语义已经变了的旧数据混进来读。
///
/// 从 1 升到 2 的原因是底层口径换了组：旧时条目是用 `Capacity*`（接口不维护的那组）
/// 算出来的，几乎全是 0；留着它们会和新条目混在一张表里，谁也说不清哪个数可信。
const SCHEMA: u32 = 2;

/// 一条时条目里的一行：某个账号在这一个小时里的动静。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BriefAccount {
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub phone: Option<String>,
    /// 这一小时（日条目里是这一天）的消耗
    pub consumed: f64,
    /// 这一小时（日条目里是这一天）的新增
    pub gained: f64,
    /// 读数时刻的剩余积分（取不到为 None —— 不谎报 0）
    #[serde(default)]
    pub balance: Option<f64>,
}

/// 一条时条目：某一天某一个小时的消耗与新增，**带逐账号明细**。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HourEntry {
    /// 归属日期 `YYYY-MM-DD`
    pub date: String,
    /// 归属小时（0–23）
    pub hour: u8,
    /// 固化时刻（本地时间串）
    pub generated_at: String,
    pub consumed: f64,
    pub gained: f64,
    #[serde(default)]
    pub balance: Option<f64>,
    /// 这一小时各账号的情况（**按消耗降序**）：
    ///
    /// - 有动静的排前面；
    /// - 没动静、但这一小时采到了余额读数的账号**也在里面**（消耗/新增记 0，只有余额）——
    ///   「剩余」这一列与合计因此覆盖全部账号，与账号管理页的总积分同口径。
    ///   只统计有动静的那几个会得到一个谁也解释不清的偏小数（曾经就是这样）。
    ///
    /// 两者都没有的账号（没动静、也没读到余额）不进这里：那是纯粹的噪音行。
    pub accounts: Vec<BriefAccount>,
    /// 是否是「开启简报」时落下的**起点条目**：消耗与新增都是 0，只记下那一刻读到的余额。
    ///
    /// 拨完开关马上就得有东西可看，而且下一小时那条要有得对比 —— 所以开启时立刻落一条。
    /// 它带来两条特殊规则（都不是随手加的）：
    /// 1. 它是**唯一**允许「没有动静」的条目 —— 别的小时没动静就根本不生成；
    /// 2. 它**不参与去重**（见 [`unsealed_hours`]）：这一小时走完后的真实固化会把它**覆盖**
    ///    掉，否则开启那一刻之后的消耗会被这条占位挡住、永久丢失。
    #[serde(default)]
    pub baseline: bool,
}

/// 一条日条目：**当天所有时条目之和**。读的时候现算，不落盘。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DayEntry {
    /// 归属日期 `YYYY-MM-DD`（列表按它倒序）
    pub date: String,
    /// 这个自然日是否已经走完（今天为 false ⇒ 界面显示「进行中」）
    pub sealed: bool,
    pub consumed: f64,
    pub gained: f64,
    /// 当天**最后一个**有时点读数的小时的余额合计 —— 「这天结束时还剩多少」
    #[serde(default)]
    pub balance: Option<f64>,
    /// 当天全部时条目（按小时升序）
    pub hours: Vec<HourEntry>,
    /// 当天各账号的合计（由时条目相加而来，按消耗降序）
    pub accounts: Vec<BriefAccount>,
}

/// 从某个账号的小时桶里取某个小时的值（没有该天/该小时时为 0）
fn bucket(b: &ledger::HourBuckets, date: &str, hour: u8) -> f64 {
    b.get(date).map_or(0.0, |h| h[hour as usize])
}

/// 消耗多的排前面：看简报的第一诉求是「谁在花」，而不是「谁在列表最前面」
fn sort_by_spend(rows: &mut [BriefAccount]) {
    rows.sort_by(|a, b| {
        b.consumed
            .partial_cmp(&a.consumed)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.gained.partial_cmp(&a.gained).unwrap_or(std::cmp::Ordering::Equal))
    });
}

/// 把台账里某个小时的桶固化成一条时条目。
///
/// 桶在采样那一刻就归好位了（见 [`crate::ledger`]），所以这里只做聚合：
/// **不需要网络、也不需要额外的「封口」语义** —— 任何时刻都能把已经走完的小时补出来。
///
/// 全部账号都没有动静时返回 `None`：一条全是 0 的条目没有信息量，
/// 只会让「这一天到底哪几个小时在花」这件事变得更难看清。
///
/// 余额读数取自台账里**最近一次拉取**（账户管理页显示的就是同一个数），但只认
/// 「读数时刻与被固化的那个小时属同一小时」的读数 —— 补算历史时读数是此刻的，
/// 写进去就是拿今天的余额假装当时（见 [`AcctLedger::credits_in_hour`]）。
/// 这类条目余额显示「—」，好过编一个数出来。
pub fn build_hour(
    accts: &BTreeMap<String, AcctLedger>,
    accounts: &[Account],
    date: &str,
    hour: u8,
    generated_at: &str,
) -> Option<HourEntry> {
    build_entry(accts, accounts, date, hour, generated_at, false)
}

/// 「开启简报」那一刻落下的**起点条目**（见 [`HourEntry::baseline`]）。
///
/// 与 [`build_hour`] 的区别只有一条：**不要求这一小时有动静** —— 开启时桶刚被清空，
/// 按常规口径它必然是空的，而那正是用户最需要看到反馈的一刻（否则页面空着，
/// 要到下一个整点过后的固化才知道有没有生效，中间还少一个可以对比的起点）。
///
/// 收哪些账号：**这一刻确实读到余额的那些**。没有读数的账号放进来只会是一行横杠，
/// 而这条条目的全部意义就是「当对照」。
pub fn build_baseline(
    accts: &BTreeMap<String, AcctLedger>,
    accounts: &[Account],
    date: &str,
    hour: u8,
    generated_at: &str,
) -> Option<HourEntry> {
    build_entry(accts, accounts, date, hour, generated_at, true)
}

/// 两个入口共用的聚合主体（`baseline` 只影响「收谁」与标记位）。
///
/// 列表里放**所有在这一小时有话说**的账号：有动静的，以及虽然没动静、但这一小时采到了
/// 余额读数的。这样「剩余」这一列（以及合计）与账号管理页的总积分是同一个口径 ——
/// 只统计「有动静的那几个」会得到一个谁也解释不清的偏小的数。
/// 「谁在花」仍然一眼可见：按扣费降序排，有动静的天然在前面（前端把 0 值渲染成「—」）。
///
/// 条目**是否生成**仍由「这一小时有没有动静」决定（`has_movement`）：没有动静的小时不留
/// 全 0 条目 —— 否则每个整点都会多出一条什么都没有的记录，把「哪几个小时在花钱」搅浑。
/// 唯一的例外是开启简报时那条起点条目（见 [`build_baseline`]）。
fn build_entry(
    accts: &BTreeMap<String, AcctLedger>,
    accounts: &[Account],
    date: &str,
    hour: u8,
    generated_at: &str,
    baseline: bool,
) -> Option<HourEntry> {
    let mut rows = Vec::new();
    let (mut consumed, mut gained) = (0.0f64, 0.0f64);
    let mut total_balance = 0.0f64;
    let mut any_balance = false;
    let mut has_movement = false;

    for a in accounts {
        let Some(entry) = accts.get(&a.id) else { continue };
        let c = bucket(&entry.hours_used, date, hour);
        let g = bucket(&entry.hours_granted, date, hour);
        let balance = entry.credits_in_hour(date, hour);
        if c > 0.0 || g > 0.0 {
            has_movement = true;
        } else if balance.is_none() {
            // 既没动静、这一小时又没读到余额：它在这条条目里没有任何可说的东西
            continue;
        }
        consumed += c;
        gained += g;
        if let Some(b) = balance {
            total_balance += b;
            any_balance = true;
        }
        rows.push(BriefAccount {
            account_id: a.id.clone(),
            name: a.name.clone(),
            phone: a.phone.clone(),
            consumed: ledger::round2(c),
            gained: ledger::round2(g),
            balance: balance.map(ledger::round2),
        });
    }

    if rows.is_empty() || (!has_movement && !baseline) {
        return None;
    }
    sort_by_spend(&mut rows);
    Some(HourEntry {
        date: date.to_string(),
        hour,
        generated_at: generated_at.to_string(),
        consumed: ledger::round2(consumed),
        gained: ledger::round2(gained),
        balance: any_balance.then(|| ledger::round2(total_balance)),
        accounts: rows,
        baseline,
    })
}

/// 列出「台账里已经有数据、但还没固化成时条目」的小时，按时间升序。
///
/// `(today, hour)` 是**此刻**：当前这一小时还在走，它的桶随时会长，必须排除掉 ——
/// 否则每次调度都会把半小时前固化的那条时条目再改一遍，界面上「这一小时花了多少」
/// 会一直跳。
///
/// 候选只从台账的桶里来，所以集合天然不含「应用根本没运行的时段」；
/// 已有的时条目用来去重，所以重复调用不会重复固化（幂等）。
///
/// ⚠️ **起点条目（`baseline`）不计入去重**：它只是「开启那一刻的占位 + 基准」，
/// 这一小时走完后的真实固化必须能覆盖它 —— 否则开启那一刻之后采到的消耗会被它挡掉，
/// 那一小时就永远只剩一个 0。覆盖走的是 [`upsert`] 的同键替换，覆盖后标记自然消失。
pub fn unsealed_hours(
    accts: &BTreeMap<String, AcctLedger>,
    sealed: &[HourEntry],
    today: &str,
    hour: u8,
) -> Vec<(String, u8)> {
    let done: BTreeSet<(&str, u8)> = sealed
        .iter()
        .filter(|e| !e.baseline)
        .map(|e| (e.date.as_str(), e.hour))
        .collect();
    let mut out: BTreeSet<(String, u8)> = BTreeSet::new();
    for a in accts.values() {
        for (date, buckets) in a.hours_used.iter().chain(a.hours_granted.iter()) {
            for (h, v) in buckets.iter().enumerate() {
                let h = h as u8;
                if *v <= 0.0 {
                    continue; // 这一小时没有记录（应用没运行过 / 真的没动静）
                }
                if (date.as_str(), h) >= (today, hour) {
                    continue; // 还没走完的小时
                }
                if done.contains(&(date.as_str(), h)) {
                    continue; // 已经固化过
                }
                out.insert((date.clone(), h));
            }
        }
    }
    out.into_iter().collect()
}

/// 把时条目按天聚合 —— **日条目就是当天时条目之和**。
///
/// 不落盘的代价是每次读都要算一遍，收益是「日 = 时之和」不可能被破坏；
/// 这个函数因此必须保持纯粹：不许查网络、不许读别的文件、不许有副作用。
///
/// `today` 用来判断某一天是否已经走完（`sealed`）。
pub fn day_entries(hours: &[HourEntry], today: &str) -> Vec<DayEntry> {
    let mut by_date: BTreeMap<&str, Vec<&HourEntry>> = BTreeMap::new();
    for h in hours {
        by_date.entry(h.date.as_str()).or_default().push(h);
    }

    let mut days: Vec<DayEntry> = by_date
        .into_iter()
        .map(|(date, mut list)| {
            list.sort_by_key(|h| h.hour);
            let (mut consumed, mut gained) = (0.0f64, 0.0f64);
            let mut last_balance: Option<f64> = None;
            // 用 `BTreeMap` 先归并再排序：账号顺序在结果里必须是稳定的（按消耗降序）
            let mut acc: BTreeMap<&str, BriefAccount> = BTreeMap::new();

            for h in &list {
                consumed += h.consumed;
                gained += h.gained;
                if h.balance.is_some() {
                    last_balance = h.balance;
                }
                for a in &h.accounts {
                    let row = acc.entry(a.account_id.as_str()).or_insert_with(|| BriefAccount {
                        account_id: a.account_id.clone(),
                        name: a.name.clone(),
                        phone: a.phone.clone(),
                        consumed: 0.0,
                        gained: 0.0,
                        balance: None,
                    });
                    row.consumed += a.consumed;
                    row.gained += a.gained;
                    if a.balance.is_some() {
                        row.balance = a.balance;
                    }
                    // 昵称中途改过时以最新一条为准；手机号同理（可能后来才补上）
                    if !a.name.is_empty() {
                        row.name = a.name.clone();
                    }
                    if a.phone.is_some() {
                        row.phone = a.phone.clone();
                    }
                }
            }

            let mut accounts: Vec<BriefAccount> = acc
                .into_values()
                .map(|mut r| {
                    r.consumed = ledger::round2(r.consumed);
                    r.gained = ledger::round2(r.gained);
                    r
                })
                .collect();
            sort_by_spend(&mut accounts);

            DayEntry {
                date: date.to_string(),
                sealed: date < today,
                consumed: ledger::round2(consumed),
                gained: ledger::round2(gained),
                balance: last_balance,
                hours: list.into_iter().cloned().collect(),
                accounts,
            }
        })
        .collect();

    // 新的在前（界面直接顺序渲染）
    days.sort_by(|a, b| b.date.cmp(&a.date));
    days
}

/// 简报的推送文案（与 `notify::send` 的纯文本约定一致）。
///
/// 只推**已经走完的那一天**（时条目每小时结算，但推送按天走 —— 每小时推一条
/// 会把通知刷成流水账，而「今天花了多少」要等当天结束才有定论）。
pub fn message(day: &DayEntry) -> String {
    let mut s = format!(
        "积分简报 {}｜消耗 {:.2}｜新增 {:.2}",
        day.date, day.consumed, day.gained
    );
    if let Some(b) = day.balance {
        s.push_str(&format!("｜剩余 {b:.2}"));
    }
    s.push_str(&format!(
        "\n统计范围：全天 00:00–24:00，共 {} 个小时条目",
        day.hours.len()
    ));
    // 明细只列有变化的账号，避免推送被一串 0 刷屏
    let mut lines: Vec<String> = day
        .accounts
        .iter()
        .filter(|a| a.consumed > 0.0 || a.gained > 0.0)
        .map(|a| format!("· {} 耗 {:.2} / 增 {:.2}", a.name, a.consumed, a.gained))
        .collect();
    if lines.is_empty() {
        lines.push("· 这一天还没有账号产生消耗或新增".to_string());
    }
    s.push('\n');
    s.push_str(&lines.join("\n"));
    s
}

// ── 落盘 ───────────────────────────────────────────────────────

/// 简报文件的包装：带 schema 版本（与台账文件同一套原子写 + 版本丢弃策略）
#[derive(Serialize, Deserialize)]
struct BriefingsFile {
    v: u32,
    hours: Vec<HourEntry>,
}

pub fn briefings_file(dir: &Path) -> PathBuf {
    dir.join("credit_briefings.json")
}

/// 读时条目（**新的在前**：日期降序、同日小时内降序）
pub fn load(dir: &Path) -> Vec<HourEntry> {
    decode(&fs::read_to_string(briefings_file(dir)).unwrap_or_default())
}

/// 解析简报文件。抽出来是为了能单测「旧格式被安全丢弃」这件事 ——
/// 这类「读旧文件读到错数据」的 bug 只在真实升级路径上出现，手点很难覆盖。
fn decode(raw: &str) -> Vec<HourEntry> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(f) = serde_json::from_str::<BriefingsFile>(raw) else {
        return Vec::new();
    };
    if f.v != SCHEMA {
        return Vec::new();
    }
    f.hours
}

pub fn save(dir: &Path, hours: &[HourEntry]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let body = serde_json::to_string_pretty(&BriefingsFile {
        v: SCHEMA,
        hours: hours.to_vec(),
    })?;
    let target = briefings_file(dir);
    // 原子写：先写 `.tmp` 再 rename，避免中途崩溃留下半截 JSON
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &target)?;
    crate::accounts::set_private_permissions(&target);
    Ok(())
}

/// 清空全部时条目（日条目是算出来的，随之一起消失）
pub fn clear(dir: &Path) -> std::io::Result<()> {
    save(dir, &[])
}

/// 落盘前的统一整理：按 (日期, 小时) 降序 + 截断到上限。
///
/// 固化路径是「读全量 → 就地改 → 整体写回」，不经过任何单条写入函数，
/// 所以必须在这里再收一次口，否则文件会无限增长。
pub fn normalize(hours: &mut Vec<HourEntry>) {
    hours.sort_by(|a, b| b.date.cmp(&a.date).then(b.hour.cmp(&a.hour)));
    hours.truncate(MAX_HOURS);
}

/// 写入一条时条目：同一个 (日期, 小时) **覆盖**，否则新增。
///
/// 覆盖是必须的：补算会把某个已经存在的（比如上次因为崩溃只固化了半截的）小时重算一遍，
/// 若追加而不是覆盖，列表里就会出现同一个小时的两条记录。
pub fn upsert(hours: &mut Vec<HourEntry>, entry: HourEntry) {
    match hours
        .iter()
        .position(|h| h.date == entry.date && h.hour == entry.hour)
    {
        Some(i) => hours[i] = entry,
        None => hours.push(entry),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, name: &str) -> Account {
        Account {
            id: id.into(),
            name: name.into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            created_at: String::new(),
            last: None,
            checked_today: None,
        }
    }

    /// 造一条时条目：`spec` = [(账号 id, 消耗, 新增)]，小时固定 9
    fn hour(date: &str, hour: u8, spec: &[(&str, f64, f64)]) -> HourEntry {
        let accounts: Vec<BriefAccount> = spec
            .iter()
            .map(|(id, c, g)| BriefAccount {
                account_id: (*id).into(),
                name: (*id).to_string(),
                phone: None,
                consumed: *c,
                gained: *g,
                balance: Some(1000.0),
            })
            .collect();
        HourEntry {
            date: date.into(),
            hour,
            generated_at: format!("{date} {:02}:00:00", hour + 1),
            consumed: ledger::round2(accounts.iter().map(|a| a.consumed).sum()),
            gained: ledger::round2(accounts.iter().map(|a| a.gained).sum()),
            balance: Some(1000.0),
            accounts,
            baseline: false,
        }
    }

    fn accts_with(id: &str, date: &str, spec: &[(u8, f64, f64)]) -> BTreeMap<String, AcctLedger> {
        let mut accts = BTreeMap::new();
        let mut led = AcctLedger::default();
        for (h, used, granted) in spec {
            if *used > 0.0 {
                led.hours_used.entry(date.to_string()).or_insert([0.0; 24])[*h as usize] += used;
            }
            if *granted > 0.0 {
                led.hours_granted.entry(date.to_string()).or_insert([0.0; 24])[*h as usize] += granted;
            }
        }
        accts.insert(id.to_string(), led);
        accts
    }

    // ── 固化 ─────────────────────────────────────────────────

    #[test]
    fn build_hour_lists_everyone_with_something_to_say_about_that_hour() {
        let mut accts = accts_with("a1", "2026-09-15", &[(9, 12.0, 0.0)]);
        accts.extend(accts_with("a2", "2026-09-15", &[(9, 8.0, 100.0)]));
        // 丙：13 点才有动静，9 点既没动静也没读数 → 纯噪音，不进列表
        accts.extend(accts_with("a3", "2026-09-15", &[(13, 5.0, 0.0)]));
        // 丁：没动静，但 9 点采到了余额读数 → **要进列表**（只有余额那一列）
        accts.extend(accts_with("a4", "2026-09-15", &[]));

        for (id, v) in [("a1", 100.0), ("a2", 200.0), ("a4", 400.0)] {
            let l = accts.get_mut(id).unwrap();
            l.credits = Some(v);
            l.credits_at = "2026-09-15 09:55:00".into();
        }

        let accounts = [
            account("a1", "甲"),
            account("a2", "乙"),
            account("a3", "丙"),
            account("a4", "丁"),
        ];
        let e = build_hour(&accts, &accounts, "2026-09-15", 9, "t").unwrap();

        assert_eq!((e.consumed, e.gained), (20.0, 100.0));
        // 余额合计要覆盖**所有**读到读数的账号（含没动静的丁）——
        // 只加「有动静的那几个」会得到一个比账号页总积分小的、解释不清的数
        assert_eq!(e.balance, Some(700.0));
        let rows: Vec<(&str, f64, Option<f64>)> = e
            .accounts
            .iter()
            .map(|a| (a.account_id.as_str(), a.consumed, a.balance))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("a1", 12.0, Some(100.0)),
                ("a2", 8.0, Some(200.0)),
                ("a4", 0.0, Some(400.0)),
            ],
            "有动静的按消耗降序在前，没动静但有读数的跟在后面"
        );
        assert!(
            !e.accounts.iter().any(|a| a.account_id == "a3"),
            "既没动静又没读数的不该进列表"
        );
    }

    /// 「采到了余额」不等于「有动静」：安静的一小时不该因为有个读数就冒出一条全 0 条目。
    #[test]
    fn a_quiet_hour_with_readings_still_produces_no_entry() {
        let accounts = [account("a1", "甲")];
        let mut accts = accts_with("a1", "2026-09-15", &[]);
        {
            let l = accts.get_mut("a1").unwrap();
            l.credits = Some(100.0);
            l.credits_at = "2026-09-15 09:55:00".into();
        }
        assert!(
            build_hour(&accts, &accounts, "2026-09-15", 9, "t").is_none(),
            "余额读数只决定「条目里能写谁」，不该让安静的小时冒出条目"
        );
        // 唯一的例外就是开启那一刻的起点条目
        assert!(build_baseline(&accts, &accounts, "2026-09-15", 9, "t").is_some());
    }

    /// 余额读数只在**与条目同小时**时才写进去。
    ///
    /// 补算历史（应用关了两天再打开）时，台账里的读数是「此刻」的 ——
    /// 拿它去标注两天前那一个小时就是编数字，所以宁可显示「—」。
    #[test]
    fn a_balance_reading_from_another_hour_is_never_written_into_the_entry() {
        let accounts = [account("a1", "甲")];
        let mut accts = accts_with("a1", "2026-09-15", &[(9, 12.0, 0.0)]);
        {
            let l = accts.get_mut("a1").unwrap();
            l.credits = Some(100.0);
            l.credits_at = "2026-09-17 10:00:00".into(); // 此刻读到，条目却是 09-15 的 9 点
        }
        let e = build_hour(&accts, &accounts, "2026-09-15", 9, "t").unwrap();
        assert_eq!(e.balance, None, "不同小时的读数不能写进这条");
        assert_eq!(e.accounts[0].balance, None);

        // 同小时的读数才认
        accts.get_mut("a1").unwrap().credits_at = "2026-09-15 09:55:00".into();
        let e = build_hour(&accts, &accounts, "2026-09-15", 9, "t").unwrap();
        assert_eq!(e.balance, Some(100.0));
    }

    #[test]
    fn build_hour_returns_none_when_nothing_moved() {
        let accts = accts_with("a1", "2026-09-15", &[(9, 0.0, 0.0)]);
        let accounts = [account("a1", "甲")];
        assert!(
            build_hour(&accts, &accounts, "2026-09-15", 9, "t").is_none(),
            "没有动静的小时不该留下一条全是 0 的条目"
        );
    }

    #[test]
    fn unsealed_hours_excludes_the_current_hour_and_what_is_already_sealed() {
        let accts = accts_with("a1", "2026-09-15", &[(8, 1.0, 0.0), (9, 2.0, 0.0), (14, 3.0, 0.0)]);
        // 9 点已经固化过了
        let sealed = vec![hour("2026-09-15", 9, &[("a1", 2.0, 0.0)])];

        let pending = unsealed_hours(&accts, &sealed, "2026-09-15", 14);
        assert_eq!(
            pending,
            vec![("2026-09-15".to_string(), 8)],
            "8 点该补，9 点已固化，14 点还在走"
        );

        // 幂等：把这些都固化之后再问一次，什么也不剩
        let mut all = sealed;
        all.push(hour("2026-09-15", 8, &[("a1", 1.0, 0.0)]));
        assert!(unsealed_hours(&accts, &all, "2026-09-15", 14).is_empty());

        // 跨天：前一天的 23 点已经走完，即使「今天」才刚过 0 点也该补出来
        let cross = accts_with("a1", "2026-09-14", &[(23, 7.0, 0.0)]);
        assert_eq!(
            unsealed_hours(&cross, &[], "2026-09-15", 0),
            vec![("2026-09-14".to_string(), 23)]
        );
    }

    /// 开启简报时落的那条起点条目：**没有动静也要生成**（桶刚被清空），
    /// 只收「这一刻读到余额」的账号 —— 它的全部意义是当对照。
    #[test]
    fn a_baseline_entry_is_born_without_any_movement() {
        let accounts = [account("a1", "甲"), account("a2", "乙")];
        let mut accts = accts_with("a1", "2026-09-16", &[]);
        accts.extend(accts_with("a2", "2026-09-16", &[]));
        accts.get_mut("a1").unwrap().credits = Some(3027.31);
        accts.get_mut("a1").unwrap().credits_at = "2026-09-16 16:20:00".into();
        // a2 这一刻没读到余额（接口失败）→ 不进这条，免得只有一行横杠

        let e = build_baseline(&accts, &accounts, "2026-09-16", 16, "2026-09-16 16:20:00")
            .expect("起点条目不该因为「没动静」而不生成");
        assert!(e.baseline);
        assert_eq!((e.consumed, e.gained), (0.0, 0.0));
        assert_eq!(e.balance, Some(3027.31));
        let ids: Vec<&str> = e.accounts.iter().map(|a| a.account_id.as_str()).collect();
        assert_eq!(ids, vec!["a1"], "没读到余额的账号不进起点条目");

        // 对照：常规固化在同一条件下（桶空）不生成任何东西
        assert!(build_hour(&accts, &accounts, "2026-09-16", 16, "t").is_none());
    }

    /// 起点条目只是**占位**：这一小时走完后的真实固化必须能覆盖它，
    /// 否则开启那一刻之后采到的消耗会被它挡住、那一小时永远只剩一个 0。
    #[test]
    fn a_baseline_entry_does_not_block_the_real_seal_of_that_hour() {
        let mut accts = accts_with("a1", "2026-09-16", &[(16, 12.0, 0.0)]);
        accts.get_mut("a1").unwrap().credits = Some(3027.31);
        accts.get_mut("a1").unwrap().credits_at = "2026-09-16 16:55:00".into();

        // 开启那一刻落的起点条目
        let base = build_baseline(&accts, &[account("a1", "甲")], "2026-09-16", 16, "t").unwrap();
        assert!(base.baseline);
        let mut all = vec![base];

        // 17:00 固化：16 点这一格必须重新成为候选（起点条目不计入去重）
        assert_eq!(
            unsealed_hours(&accts, &all, "2026-09-16", 17),
            vec![("2026-09-16".to_string(), 16)],
            "起点条目不该把它占着的那一小时挡在候选之外"
        );
        let real = build_hour(&accts, &[account("a1", "甲")], "2026-09-16", 16, "t").unwrap();
        assert_eq!(real.consumed, 12.0);
        assert!(!real.baseline, "真实固化出来的条目不是起点条目");
        upsert(&mut all, real);
        assert_eq!(all.len(), 1, "同键覆盖，不该出现两条 16 点");
        assert_eq!(all[0].consumed, 12.0);
        assert!(!all[0].baseline);
        // 覆盖之后它就正常参与去重了，不会再被反复固化
        assert!(unsealed_hours(&accts, &all, "2026-09-16", 17).is_empty());
    }

    // ── 聚合 ─────────────────────────────────────────────────

    #[test]
    fn day_is_the_sum_of_its_hours() {
        let hours = vec![
            hour("2026-09-15", 9, &[("a1", 30.0, 0.0), ("a2", 10.0, 0.0)]),
            hour("2026-09-15", 21, &[("a1", 5.0, 0.0), ("a2", 0.0, 100.0)]),
            hour("2026-09-14", 8, &[("a1", 1.0, 0.0)]),
        ];
        let days = day_entries(&hours, "2026-09-16");
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2026-09-15", "新的在前");

        let d = &days[0];
        assert_eq!(d.consumed, 45.0);
        assert_eq!(d.gained, 100.0);
        assert!(d.sealed, "2026-09-15 相对 09-16 已经走完");
        // 日 = 时之和（逐小时相加，含新增）
        let sum: f64 = d.hours.iter().map(|h| h.consumed).sum();
        assert_eq!(ledger::round2(sum), d.consumed);
        // 时条目按小时升序
        assert_eq!(d.hours.iter().map(|h| h.hour).collect::<Vec<_>>(), vec![9, 21]);
        // 账号合计按消耗降序：甲 35、乙 10
        let rows: Vec<(&str, f64, f64)> = d
            .accounts
            .iter()
            .map(|a| (a.account_id.as_str(), a.consumed, a.gained))
            .collect();
        assert_eq!(rows, vec![("a1", 35.0, 0.0), ("a2", 10.0, 100.0)]);
        assert_eq!(d.balance, Some(1000.0), "取当天最后一个有时点读数的小时");
    }

    #[test]
    fn today_is_not_sealed_yet() {
        let hours = vec![hour("2026-09-16", 9, &[("a1", 1.0, 0.0)])];
        let days = day_entries(&hours, "2026-09-16");
        assert!(!days[0].sealed, "当天还在走，界面据此显示「进行中」");
    }

    #[test]
    fn a_balance_reading_that_is_missing_stays_none_instead_of_a_misleading_zero() {
        let mut h = hour("2026-09-15", 9, &[("a1", 1.0, 0.0)]);
        h.balance = None;
        h.accounts[0].balance = None;
        let days = day_entries(&[h], "2026-09-16");
        assert_eq!(days[0].balance, None);
        assert_eq!(days[0].accounts[0].balance, None);
    }

    // ── 落盘 ─────────────────────────────────────────────────

    #[test]
    fn file_round_trips_and_discards_a_foreign_schema() {
        let dir = std::env::temp_dir().join(format!("wba-brief-{}", uuid::Uuid::new_v4()));
        let mut hours = Vec::new();
        upsert(&mut hours, hour("2026-09-15", 9, &[("a1", 1.0, 0.0)]));
        upsert(&mut hours, hour("2026-09-14", 23, &[("a1", 2.0, 0.0)]));
        save(&dir, &hours).unwrap();

        let back = load(&dir);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].date, "2026-09-15", "读出来就该是新的在前");
        assert_eq!(back[1].hour, 23);

        // 坏文件 / 空文件都不该 panic
        assert!(decode("{ 不是 json").is_empty());
        assert!(decode("").is_empty());
        // 版本对不上直接丢弃（旧语义的数据混进来比少几条更糟）
        assert!(decode(r#"{"v":99,"hours":[]}"#).is_empty());
        assert!(decode(r#"{"hours":[]}"#).is_empty(), "缺版本号的老格式也要丢弃");
        // v1 的条目是拿「接口不维护的那组字段」算出来的（几乎全是 0），同样丢弃
        assert!(
            decode(r#"{"v":1,"hours":[{"date":"2026-09-15","hour":9,"consumed":0.0,
                "gained":0.0,"generated_at":"t","accounts":[]}]}"#)
                .is_empty(),
            "换口径前的时条目必须整体丢弃，否则新老两种数会混在一张表里"
        );

        clear(&dir).unwrap();
        assert!(load(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_replaces_the_same_hour_and_normalize_keeps_newest_first() {
        let mut hours = Vec::new();
        upsert(&mut hours, hour("2026-09-15", 9, &[("a1", 1.0, 0.0)]));
        upsert(&mut hours, hour("2026-09-15", 9, &[("a1", 42.0, 0.0)]));
        assert_eq!(hours.len(), 1, "同一个小时不该出现两条");
        assert_eq!(hours[0].consumed, 42.0);

        // 上限：超出后丢最旧的
        let mut many: Vec<HourEntry> = Vec::new();
        for h in 0..24u8 {
            for d in 0..(MAX_HOURS / 24 + 3) {
                upsert(&mut many, hour(&format!("2026-{:02}-{:02}", 1 + d / 28, 1 + d % 28), h, &[("a1", 1.0, 0.0)]));
            }
        }
        normalize(&mut many);
        assert_eq!(many.len(), MAX_HOURS);
        assert!(many[0].date >= many[1].date);
    }

    #[test]
    fn message_lists_only_accounts_with_movement() {
        let d = DayEntry {
            date: "2026-09-15".into(),
            sealed: true,
            consumed: 12.5,
            gained: 100.0,
            balance: Some(2.0),
            hours: vec![hour("2026-09-15", 9, &[("a", 12.5, 100.0)])],
            accounts: vec![
                BriefAccount {
                    account_id: "a".into(),
                    name: "甲".into(),
                    phone: None,
                    consumed: 12.5,
                    gained: 100.0,
                    balance: Some(1.0),
                },
                BriefAccount {
                    account_id: "b".into(),
                    name: "乙".into(),
                    phone: None,
                    consumed: 0.0,
                    gained: 0.0,
                    balance: Some(1.0),
                },
            ],
        };
        let m = message(&d);
        assert!(m.contains("积分简报 2026-09-15"));
        assert!(m.contains("消耗 12.50"));
        assert!(m.contains("新增 100.00"));
        assert!(m.contains("甲 耗 12.50 / 增 100.00"));
        assert!(m.contains("共 1 个小时条目"));
        assert!(!m.contains("乙"), "没有变化的账号不该出现在明细里：{m}");
    }
}
