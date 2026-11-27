//! 凭证池客户端：把**本机这一批账号**整池托管到 cred-broker，续签仍在本机执行。
//!
//! ## 为什么需要它
//!
//! 官方续签是**单链轮换**：续签返回一个新的 refresh token，服务端按账号只认最新那一条。
//! 几台机器各持同一份 RT 的副本时，谁先续签谁赢，其余副本连旧 AT 一起被踢；而「已用过的
//! refresh token 再次出现」还会被判成凭证泄露，把整条链作废。
//!
//! 所以这里做的事不是「同步凭证」，而是**保证同一时刻只有一台机器在续签**。
//!
//! ## 粒度是「一整池」
//!
//! 一池一个 uuid，闸也按池给。上传 = 新建一池并把本机账号放进去；绑定 = 把别处的 uuid
//! 抄过来。**解绑只摘本机绑定，云端那一池不动**，其他机器不受影响；本机与云端重复的
//! 凭证会从本机移除，只留本机独有的。
//!
//! 上传**只在未绑定时可用**：服务端建池永远是新建，不会覆盖，所以「已绑定再上传」会在
//! 云端留下第二池 —— 本机切到新池、旧池原地不删，两台机器于是各持一把闸，正是
//! 「一池一把闸」要消灭的局面。要重新上传，先解绑。
//! ⚠️ 不要改成「一个账号一份 uuid」：那样两台机器可以各自拿着不同账号的闸、同时提交同一批
//! 账号，于是出现「一半新一半旧」这种**谁也没签错**的错状态。
//!
//! ## 一轮同步（三步握手）
//!
//! ```text
//! POST /lease  抢闸（抢到的人**同时拿到那一刻的整池**，别自己再 GET 一次）
//!        ↓     把池并进本地 → 本机执行需要做的续签
//! PUT  /       提交整池（CAS，版本 +1，顺便归还闸）
//!        ↓ 失败
//! POST /abort  释放闸 + 记一段冷却，让所有机器都停手
//! ```
//!
//! 抢闸带回的池内容**必须**用它去签：本地那份 refresh token 可能早就被别的机器换掉了。
//!
//! ## 落盘只有两样：绑定 + 来源标记
//!
//! 版本号、上次同步时刻、上次错误都是**运行时**才知道的东西，写进文件只会多一份可能与云端
//! 对不上的副本。`broker.json` 里只有 `pool_uuid`（有它就等于绑定了）和 `cloud_ids`
//! （本机哪几条账号是跟着这一池走的）。
//!
//! 来源标记不是云端状态的副本：池里一条对应哪个**本地 id**，只有本机自己知道。
//! 它决定「云端删掉一条时，本机要不要跟着删」—— 没有它，客户端只能做并集，
//! 池里删掉的账号会被任何一台还持有它的机器原样加回来。

use crate::accounts::{self, Account};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// 管家地址：cred-broker 的公网入口（共享网关 cloudflared → nginx → 容器:80）。
/// 容器**不在本机** —— 旧常量 `http://127.0.0.1:7003` 预设「容器与桌面端同机」，
/// 这个前提 2026-09-30 被实测推翻：没装 Docker 的机器永远打不通，症状是凭证池
/// 恒显「还没同步过」、过期票没人救。入口公开后 uuid 即一池凭证的完整权限，
/// 服务端只另有 `x-cred-actor` 审计，没有第二道鉴权 —— 别把它当秘密外发。
/// **编译期常量**，不是配置项：改地址 = 改代码 + 重新构建。
pub const BASE: &str = "https://cred-broker.sloan.dpdns.org";

/// 客户端类型：与机器唯一标识拼成「组合 id」，写进池的 `renewer`。
///
/// 为什么要带类型：同一台机器上可能同时跑助手与 2api，两者对同一个池有各自的续签
/// 判断；只有类型 + 机器标识都相同，才算「同一台续签设备」。
pub const CLIENT_TYPE: &str = "workbuddy-assistant";

/// 旧设备「取消续签」后的宽限期：这段时间内新设备没认领，旧设备自动把自己写回去。
///
/// 没有这一步，换设备时只要新设备没来得及操作，整池就会停在「没有续签设备」——
/// 云端账号不续签，直到有人想起来再点一次。
pub const RELEASE_GRACE_MS: i64 = 10_000;

/// 距上次同步超过这么久才会再问一次管家。
///
/// 太短只是白打接口（接管转发是热路径）；太长会让本地继续用别处已经轮换掉的凭证。
/// 注意它只挡「重复的整池同步」，不挡「续签」本身 —— 续签阈值是 48 小时，
/// 两分钟的窗口相对它完全可以忽略。
pub const SYNC_TTL_MS: i64 = 120_000;

/// 「云端账号」弹窗读数的缓存寿命。
///
/// 弹窗的数据源是 `GET /v1/pool/{uuid}` —— **云端那一份**，不是本机账号列表。
/// 缓存只用来挡「同一次浏览里反复打接口」：打开弹窗、手动同步后都会强制刷新。
pub const CLOUD_TTL_MS: i64 = 60_000;

/// 本机的「整池同步单飞闸」。
///
/// 为什么光有 [`SYNC_TTL_MS`] 不够：那个节流的形状是「读一次 `last_sync_ms` → 比一下 →
/// 放行」，而判定与真正发出 `POST /lease` 之间隔着好几个 await。同一窗口内并发进来的
/// 调用会**全部通过判定**（读到的都是同一个旧时间戳），于是同一台机器同时打好几个
/// lease，再各自 `PUT` 整池 —— 服务端只认一个 owner，其余白打一趟，最坏还会互相踩 CAS。
/// 而它的调用点全在热路径上：反代每个请求、拉模型清单、签到、刷新。
///
/// 语义按调用方分开：
/// - 自动路径（`force = false`）：**抢不到就跳过**。它本来就是「顺手问一下」，
///   排队只会把反代请求一起拖住；
/// - 手动路径（`force = true`）：**排队等**。用户点了按钮就是要一轮确定的结果，
///   回一句「正在同步，本次跳过」等于骗人。
static SYNC_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const BROKER_FILE: &str = "broker.json";

// ── 状态 ──────────────────────────────────────────────────────────────────

/// 落盘配置：绑定 + 来源标记 —— 见模块头「落盘只有两样」。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct BrokerConfig {
    pub pool_uuid: Option<String>,
    /// 本机唯一标识（首次启动生成一次，随配置落盘）。
    ///
    /// 与 [`CLIENT_TYPE`] 拼成组合 id 写进池的 `renewer`。用它而不是 hostname：
    /// hostname 会重名、会随网络改名，而「谁有权续签」不能靠一个会变的字符串。
    /// 清空数据目录等于换了一台设备 —— 此时旧 id 会留在池上，需要在旧设备上取消续签。
    pub machine_id: String,
    /// 本机账号里哪些是**跟着池走**的（按本机 id 记，见模块头）。
    ///
    /// - 只登记**从池里收养的**账号；本机登录后再上传的不算（它们始终由本机续签）；
    /// - 第一次同步（`origin_seeded == false`）把池里已有的全部登记一遍，兼容升级；
    /// - 之后池里那条消失 → 本机一并删掉；
    /// - 解绑 / 换池时整组作废：旧池的来源标记套到新池上没有意义。
    pub cloud_ids: Vec<String>,
    /// 来源标记是否已经做过第一次登记。
    ///
    /// 没有它的话，「本机没有任何云端账号」与「还没登记过」是同一个空集，
    /// 每次同步都会把刚上传的本机登录账号误登记成云端。
    pub origin_seeded: bool,
    /// 本机是否已经见过「续签设备」这套机制（升级迁移用，只做一次）。
    ///
    /// 老池的 `renewer` 一开始是 NULL。第一次同步时若仍是 NULL，本机主动认领一次，
    /// 免得升级后云端账号无人续签（直到有人想起来点按钮时票可能已经过期）。
    /// 认领过（或池上已有别人）就把这一位落下；之后的换设备必须照旧
    /// 「旧设备取消 → 新设备手动认领」。
    pub renewer_seen: bool,
    /// 本机删掉、还**没让云端知道**的账号 key（同步成功后清空）。
    ///
    /// 删除必须由客户端告诉云端，否则下一次同步的「并集」会把它从池里原样加回来 ——
    /// 用户看到的就是「删了又回来」。所以 `remove_account` 在这里记一笔，同步时：
    /// 合并跳过它、提交时不带它（服务端把没出现的条目记成逻辑删除），提交成功后清空。
    /// 绑定别的池时整组作废（key 是跟池走的东西）。
    pub deleted_keys: Vec<String>,
    /// 云端已逻辑删除的账号（墓碑快照，最近一次成功同步时带回）。
    ///
    /// 只留「没有活条目与之对应」的最近 [`MAX_TOMBSTONES`] 条。它是「已删除」列表的
    /// 数据源，也是恢复的唯一凭据（含 token）：恢复时把整条重新加进本机账号，再随
    /// 下一轮提交复活服务端那条墓碑。
    pub deleted_items: Vec<PoolItem>,
    /// 刚从墓碑恢复、还没提交的账号本机 id。
    ///
    /// 抢闸带回来的仍是「已删除」快照（提交前服务端还没复活它），若不跳过剪枝，
    /// 恢复的账号会在同一轮同步里又被删掉。提交成功后清空。
    pub reviving_ids: Vec<String>,
}

/// 「已删除」列表最多保留多少条墓碑：够看最近删的，又不会让 broker.json 无限长。
pub const MAX_TOMBSTONES: usize = 50;

/// 只存在于内存的运行时状态（进程重启就重来，不影响正确性）
#[derive(Default)]
struct Runtime {
    /// 上次「真的问过管家」的时刻，用于节流
    last_sync_ms: i64,
    /// 上次成功同步的时刻（界面展示「上次同步」）
    last_ok_ms: i64,
    /// 云端那一池的版本号（抢闸 / 提交时得知）
    version: Option<i64>,
    /// 上次失败的原因，成功后清空
    error: Option<String>,
    /// 正在同步（避免界面在长请求里显示「从未同步」）
    syncing: bool,
    /// 池上当前的续签设备（最近一次抢闸 / 认领 / 释放得知）
    renewer: Option<String>,
    /// 是否已经问过续签设备。`false` = 还没同步过，界面显示「未知」而不是「没有」。
    renewer_known: bool,
    /// 取消续签后的宽限截止（毫秒）；宽限内没有新设备，本机自动写回。
    grace_until: Option<i64>,
    /// 最近一次**直接读云端池**的结果（云端账号弹窗 / 池栏计数，见 [`cloud_snapshot`]）。
    ///
    /// 用 `Arc` 是因为 [`status`] 每次都克隆一份运行时状态：整池条目含票，
    /// 深拷贝一遍只为了让界面显示一个数字并不划算。
    cloud: Option<Arc<CloudCache>>,
}

struct State {
    dir: PathBuf,
    cfg: BrokerConfig,
    rt: Runtime,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// 进程启动时加载一次。**只在 `lib.rs` 的 setup 里调**。
pub fn init(dir: &Path) {
    let mut cfg = load_config(dir);
    let generated = cfg.machine_id.trim().is_empty();
    if generated {
        cfg.machine_id = uuid::Uuid::new_v4().to_string();
    }
    let saved = if generated {
        save_config(dir, &cfg)
    } else {
        Ok(())
    };
    let _ = STATE.get_or_init(|| {
        Mutex::new(State {
            dir: dir.to_path_buf(),
            cfg,
            rt: Runtime::default(),
        })
    });
    if let Err(e) = saved {
        crate::logs::push(
            dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("设备标识落盘失败：{e}（重启后会自动再生成一个，池上的续签设备可能对不上）"),
        );
    }
}

fn load_config(dir: &Path) -> BrokerConfig {
    let path = dir.join(BROKER_FILE);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<BrokerConfig>(&t).ok())
        .unwrap_or_default()
}

/// 把绑定关系（uuid）落盘。
///
/// ⚠️ 两个决定都反直觉，改这块前先读完：
/// 1. **序列化失败要报错，不能写 `{}`**。旧的 `unwrap_or_else(|_| "{}")` 看着像兜底，
///    实际是把**已有的绑定**覆盖成空：下一轮 `load_config` 读回默认值，本机安静地退回
///    单机运行，而云端那一池还认为这台机器绑着 —— 两台机器各持一把闸。
/// 2. **原子写**（[`crate::accounts::write_atomic`]）：uuid 等于一池凭证的完整权限，
///    半截落盘等于这台机器「没绑过池」；权限在建临时文件时一并收到 0600。
fn save_config(dir: &Path, cfg: &BrokerConfig) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::other(format!("凭证池配置序列化失败：{e}")))?;
    crate::accounts::write_atomic(&dir.join(BROKER_FILE), &text)
}

/// 读一份快照（uuid + 运行时状态），**不持锁跨 await**。
fn snapshot() -> (Option<String>, Runtime) {
    let Some(lock) = STATE.get() else {
        return (None, Runtime::default());
    };
    match lock.lock() {
        Ok(s) => (s.cfg.pool_uuid.clone(), clone_runtime(&s.rt)),
        Err(_) => (None, Runtime::default()),
    }
}

/// 数据目录（`init` 时写入）。解绑要动本机账号，需要它。
fn data_dir() -> Option<PathBuf> {
    let lock = STATE.get()?;
    match lock.lock() {
        Ok(s) => Some(s.dir.clone()),
        Err(_) => None,
    }
}

fn clone_runtime(rt: &Runtime) -> Runtime {
    Runtime {
        last_sync_ms: rt.last_sync_ms,
        last_ok_ms: rt.last_ok_ms,
        version: rt.version,
        error: rt.error.clone(),
        syncing: rt.syncing,
        renewer: rt.renewer.clone(),
        renewer_known: rt.renewer_known,
        grace_until: rt.grace_until,
        cloud: rt.cloud.clone(),
    }
}

fn mutate<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let lock = STATE.get()?;
    match lock.lock() {
        Ok(mut s) => Some(f(&mut s)),
        Err(_) => None,
    }
}

/// 这台机器绑定到某一池了吗？**续签路径的分岔判据**（见 `commands::ensure_fresh_token`）。
pub fn bound() -> bool {
    snapshot().0.is_some()
}

/// 当前绑定的池 uuid
pub fn uuid() -> Option<String> {
    snapshot().0
}

/// 本机删掉、还没同步给云端的账号 key（见 [`BrokerConfig::deleted_keys`]）。
fn pending_deletions() -> HashSet<String> {
    mutate(|s| s.cfg.deleted_keys.iter().cloned().collect()).unwrap_or_default()
}

/// 删除账号**之前**调一次：绑定期间把它的 key 记进「待删除」。
///
/// 不记这一笔的话，下一次同步的合并会把这条账号从池里原样加回来（用户看到「删了又回来」）。
/// 未绑定时不记：key 是跟池走的东西，单机运行没有云端可通知。
pub fn note_deleted_account(account: &Account) {
    note_deleted_key(&item_key_of(account));
}

/// 只知道 key 的删除（「云端账号」弹窗里那条可能没有本机副本）。
pub fn note_deleted_key(key: &str) {
    let key = key.trim();
    if key.is_empty() {
        return;
    }
    mutate(|s| {
        if s.cfg.pool_uuid.is_none() || s.cfg.deleted_keys.iter().any(|k| k == key) {
            return;
        }
        s.cfg.deleted_keys.push(key.to_string());
        if let Err(e) = save_config(&s.dir, &s.cfg) {
            crate::logs::push(
                &s.dir,
                crate::logs::CAT_SYSTEM,
                "凭证池",
                false,
                format!("待删除账号的标记没能落盘：{e}（下次同步可能把这条账号又带回来）"),
            );
        }
    });
}

/// 撤回一条「刚删、还没提交」的账号：恢复时要先撤掉待删除，
/// 否则下一轮提交照样漏掉它，服务端仍会记成墓碑。
pub fn cancel_pending_deletion(key: &str) {
    let written = mutate(|s| {
        let before = s.cfg.deleted_keys.len();
        s.cfg.deleted_keys.retain(|k| k.trim() != key.trim());
        if s.cfg.deleted_keys.len() == before {
            return Ok(());
        }
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("撤回待删除标记失败：{e}（恢复的账号可能仍被云端判成已删）"),
        );
    }
}

/// 整池提交成功后清空本机待办：云端已经按「本轮没出现」记好墓碑、也收到恢复。
fn clear_pending_changes() {
    mutate(|s| {
        if s.cfg.deleted_keys.is_empty() && s.cfg.reviving_ids.is_empty() {
            return;
        }
        s.cfg.deleted_keys.clear();
        s.cfg.reviving_ids.clear();
        let _ = save_config(&s.dir, &s.cfg);
    });
}

fn pending_revives() -> HashSet<String> {
    mutate(|s| s.cfg.reviving_ids.iter().cloned().collect()).unwrap_or_default()
}

/// 恢复时记一笔：提交前跳过这条账号的剪枝（服务端此刻还认为它是墓碑）。
pub fn note_reviving(id: &str) {
    if !bound() {
        return;
    }
    let written = mutate(|s| {
        if s.cfg.reviving_ids.iter().any(|x| x == id) {
            return Ok(());
        }
        s.cfg.reviving_ids.push(id.to_string());
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("恢复账号的标记没能落盘：{e}（本轮同步可能又把它删掉）"),
        );
    }
}

/// 云端池里的一条（发给界面）：**不带令牌** —— 界面只需要认出账号、看到有效期。
#[derive(Serialize, Clone, Debug)]
pub struct CloudAccount {
    /// 跨机身份锚点（手机号 → 昵称 → 本地 id），删除 / 恢复都用它
    pub key: String,
    pub name: String,
    pub phone: String,
    /// 访问票到期：字段缺失时从票里折算（见 [`item_expiry`]）
    pub expires_at: Option<i64>,
    pub rt_expires_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// 只在「已删除」里出现
    pub deleted_at: Option<i64>,
}

/// `broker_cloud_snapshot` 的返回：一次读取云端池的结果（可能来自缓存）。
///
/// 这是**云端那一份**：主表里的「云端副本」是它的本地投影，两者不共享来源。
#[derive(Serialize, Clone, Debug)]
pub struct CloudSnapshot {
    pub uuid: String,
    pub accounts: Vec<CloudAccount>,
    pub deleted: Vec<CloudAccount>,
    pub version: Option<i64>,
    /// 这份读数入缓存的时刻（毫秒）
    pub fetched_at: i64,
    /// `true` = 这次没打通云端，返回的是旧缓存（原因在 `error`）
    pub stale: bool,
    pub error: Option<String>,
}

/// 最近一次直接读取的云端池：完整条目（含恢复所需的票），只在内存里。
#[derive(Clone, Debug)]
struct CloudCache {
    uuid: String,
    items: Vec<PoolItem>,
    deleted: Vec<PoolItem>,
    version: Option<i64>,
    at_ms: i64,
}

fn cloud_account(item: &PoolItem) -> CloudAccount {
    CloudAccount {
        key: item.key.clone(),
        name: if item.name.trim().is_empty() {
            item.key.clone()
        } else {
            item.name.clone()
        },
        phone: item.phone.clone(),
        expires_at: item_expiry(item),
        rt_expires_at: item.rt_expires_at,
        updated_at: item.updated_at,
        deleted_at: item.deleted_at,
    }
}

/// 缓存 → 界面视图。
///
/// 「本机已请求删除、还没提交」的条目先从在线列表里挡掉：不挡的话，刚点完删除
/// 再打开弹窗，它又出现在云端列表里（服务端要等下一轮整池提交才记墓碑）。
fn cloud_view(
    cache: &CloudCache,
    pending: &HashSet<String>,
    stale: bool,
    error: Option<String>,
) -> CloudSnapshot {
    let pending: HashSet<String> = pending.iter().map(|k| k.trim().to_string()).collect();
    let accounts: Vec<CloudAccount> = cache
        .items
        .iter()
        .filter(|i| i.deleted_at.is_none() && !pending.contains(i.key.trim()))
        .map(cloud_account)
        .collect();
    let mut deleted: Vec<CloudAccount> = cache.deleted.iter().map(cloud_account).collect();
    deleted.sort_by(|a, b| b.deleted_at.unwrap_or(0).cmp(&a.deleted_at.unwrap_or(0)));
    CloudSnapshot {
        uuid: cache.uuid.clone(),
        accounts,
        deleted,
        version: cache.version,
        fetched_at: cache.at_ms,
        stale,
        error,
    }
}

fn cloud_cache() -> Option<Arc<CloudCache>> {
    snapshot().1.cloud
}

/// 当前缓存对应的视图（不打网络）。没读过、或池已换绑时返回 `None`。
pub fn cached_snapshot() -> Option<CloudSnapshot> {
    let uuid = snapshot().0;
    let cache = cloud_cache()?;
    if uuid.as_deref() != Some(cache.uuid.as_str()) {
        return None;
    }
    Some(cloud_view(&cache, &pending_deletions(), false, None))
}

/// 云端池的读取结果（`GET /v1/pool/{uuid}`）。
#[derive(Deserialize, Debug)]
struct PoolResp {
    #[serde(default)]
    items: Vec<PoolItem>,
    #[serde(default)]
    deleted: Vec<PoolItem>,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    renewer: Option<String>,
}

/// 直接读云端那一池，**不经过本地账号列表**。
///
/// 缓存 [`CLOUD_TTL_MS`]：`force` 时照样先取网络（打开弹窗、手动同步）；
/// 打不通就回落到旧缓存（`stale = true`），连缓存都没有才报错。
pub async fn cloud_snapshot(force: bool) -> Result<CloudSnapshot, String> {
    let Some(uuid) = uuid() else {
        return Err("本机未绑定凭证池".to_string());
    };
    let cached = cloud_cache().filter(|c| c.uuid == uuid);
    let now = chrono::Utc::now().timestamp_millis();
    if !force {
        if let Some(c) = cached.as_ref() {
            if now - c.at_ms < CLOUD_TTL_MS {
                return Ok(cloud_view(c, &pending_deletions(), false, None));
            }
        }
    }
    match request(reqwest::Method::GET, &format!("/v1/pool/{uuid}"), None).await {
        Ok(raw) => {
            let renewer_present = raw.get("renewer").is_some();
            let resp: PoolResp = serde_json::from_value(raw)
                .map_err(|e| format!("凭证管家的池内容看不懂：{e}"))?;
            let cache = Arc::new(CloudCache {
                uuid: uuid.clone(),
                items: resp.items.clone(),
                deleted: resp.deleted.clone(),
                version: Some(resp.version),
                at_ms: now,
            });
            mutate(|s| {
                s.rt.version = Some(resp.version);
                if renewer_present {
                    s.rt.renewer = resp.renewer.clone();
                    s.rt.renewer_known = true;
                }
                s.rt.cloud = Some(cache.clone());
            });
            // 墓碑快照照旧落盘：离线时换绑前的「已删除」还能恢复
            store_tombstones(resp.deleted, &resp.items);
            Ok(cloud_view(&cache, &pending_deletions(), false, None))
        }
        Err((kind, m)) => match cached {
            Some(c) => Ok(cloud_view(&c, &pending_deletions(), true, Some(m))),
            None => Err(on_failure(kind, m)),
        },
    }
}

/// 删除成功后把缓存里那条挪进「已删除」：界面不用干等下一轮同步。
pub fn cache_mark_deleted(key: &str) {
    let key = key.trim();
    let now = chrono::Utc::now().timestamp_millis();
    mutate(|s| {
        let Some(arc) = s.rt.cloud.as_mut() else {
            return;
        };
        let cache = Arc::make_mut(arc);
        let Some(pos) = cache.items.iter().position(|i| i.key.trim() == key) else {
            return;
        };
        let mut item = cache.items.remove(pos);
        item.deleted_at = Some(now);
        cache.deleted.retain(|i| i.key.trim() != key);
        cache.deleted.insert(0, item);
    });
}

/// 恢复成功后把缓存里那条挪回「在线」：服务端要到下一轮提交才真正复活。
pub fn cache_mark_revived(key: &str) {
    let key = key.trim();
    mutate(|s| {
        let Some(arc) = s.rt.cloud.as_mut() else {
            return;
        };
        let cache = Arc::make_mut(arc);
        let Some(pos) = cache.deleted.iter().position(|i| i.key.trim() == key) else {
            return;
        };
        let mut item = cache.deleted.remove(pos);
        item.deleted_at = None;
        cache.items.retain(|i| i.key.trim() != key);
        cache.items.push(item);
    });
}

/// 取一条墓碑并把账本交给调用方（**不落盘**：账号先写成功，调用方再 [`take_tombstone`]）。
pub fn find_tombstone(key: &str) -> Option<PoolItem> {
    let lock = STATE.get()?;
    let s = lock.lock().ok()?;
    s.cfg
        .deleted_items
        .iter()
        .find(|i| i.key == key)
        .cloned()
}

/// 恢复用的墓碑：优先最近一次**直接从云端读到**的「已删除」，没有再看同步落盘的快照。
pub fn find_cloud_tombstone(key: &str) -> Option<PoolItem> {
    let key = key.trim();
    if let Some(cache) = cloud_cache() {
        if let Some(i) = cache.deleted.iter().find(|i| i.key.trim() == key) {
            return Some(i.clone());
        }
    }
    find_tombstone(key)
}

/// 从「已删除」里移除一条（恢复成功后调）。
pub fn take_tombstone(key: &str) {
    let written = mutate(|s| {
        let before = s.cfg.deleted_items.len();
        s.cfg.deleted_items.retain(|i| i.key != key);
        if s.cfg.deleted_items.len() == before {
            return Ok(());
        }
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("已删除列表落盘失败：{e}（这条可能还会出现在列表里）"),
        );
    }
}

/// 恢复后把账号登记成云端来源（否则下一轮同步会把它当成本机登录账号）。
pub fn mark_cloud_member(id: &str) {
    let written = mutate(|s| {
        if !s.cfg.cloud_ids.iter().any(|x| x == id) {
            s.cfg.cloud_ids.push(id.to_string());
        }
        s.cfg.origin_seeded = true;
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("云端来源标记落盘失败：{e}（下次同步会重新判断）"),
        );
    }
}

/// 两条池条目是不是同一个账号（用于「已删除」快照去重）。
///
/// 除了 key 与 JWT `sub`，还要认**同一张票 / 同一条续签链**：手机号 / 昵称改了 key 就变了，
/// 而不透明的票读不出 `sub` —— 只比 key 会把「刚改名的那条」当成删除，列出一个重复账号。
fn same_entry(a: &PoolItem, b: &PoolItem) -> bool {
    a.key.trim() == b.key.trim()
        || (!a.access_token.trim().is_empty() && a.access_token == b.access_token)
        || (!a.refresh_token.trim().is_empty() && a.refresh_token == b.refresh_token)
        || same_subject(&a.access_token, &b.access_token)
}

/// 用本轮提交后的整池内容刷新墓碑快照。
///
/// 只留「当前没有活条目对应」的墓碑：手机号改了之后旧 key 也会成为墓碑，
/// 但它和活条目指向同一个账号，列出来只会诱导用户恢复出一个重复账号。
fn merge_tombstones(mut tombs: Vec<PoolItem>, live: &[PoolItem]) -> Vec<PoolItem> {
    tombs.retain(|t| !live.iter().any(|p| same_entry(p, t)));
    tombs.sort_by(|a, b| b.deleted_at.unwrap_or(0).cmp(&a.deleted_at.unwrap_or(0)));
    let mut seen = HashSet::new();
    tombs.retain(|t| seen.insert(t.key.clone()));
    tombs.truncate(MAX_TOMBSTONES);
    tombs
}

/// 提交成功后应落库的墓碑：服务端已有的 + 本轮从池里消失的条目。
///
/// 「本轮消失」要补进来，是因为服务端要等下一次 GET/lease 才会把它们列进 `deleted`；
/// 不补的话用户刚删就打开「已删除」会看到空列表。与 `live`（提交后的整池）有对应关系的
/// 一律不收 —— 那是手机号 / 昵称漂移留下的旧 key，不是删除。
fn tombstones_after_commit(
    server: &[PoolItem],
    sent: &[PoolItem],
    live: &[PoolItem],
    now: i64,
) -> Vec<PoolItem> {
    let mut tombs = server.to_vec();
    for item in sent {
        let still_live = live.iter().any(|p| same_entry(p, item));
        if !still_live {
            let mut t = item.clone();
            if t.deleted_at.is_none() {
                t.deleted_at = Some(now);
            }
            tombs.push(t);
        }
    }
    tombs
}

fn same_tombstones(a: &[PoolItem], b: &[PoolItem]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.key == y.key
                && x.name == y.name
                && x.phone == y.phone
                && x.access_token == y.access_token
                && x.deleted_at == y.deleted_at
        })
}

fn store_tombstones(tombs: Vec<PoolItem>, live: &[PoolItem]) {
    let tombs = merge_tombstones(tombs, live);
    let written = mutate(|s| {
        // 每两分钟同步一次，快照没变就别白写一次盘
        if same_tombstones(&s.cfg.deleted_items, &tombs) {
            return Ok(());
        }
        s.cfg.deleted_items = tombs;
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("已删除列表落盘失败：{e}（本轮删除的账号可能暂时看不到）"),
        );
    }
}

/// 本机唯一标识（见 [`BrokerConfig::machine_id`]）。
fn machine_id() -> String {
    let Some(lock) = STATE.get() else {
        return String::new();
    };
    match lock.lock() {
        Ok(s) => s.cfg.machine_id.clone(),
        Err(_) => String::new(),
    }
}

/// 组合 id：客户端类型 + 机器唯一标识。池的 `renewer` 存的就是它。
pub fn self_id() -> String {
    format!("{CLIENT_TYPE}:{}", machine_id())
}

/// 组合 id 的短显示（界面/提示用）：保留类型，机器标识只露前 8 位。
pub fn short_self_id(id: &str) -> String {
    match id.split_once(':') {
        Some((kind, machine)) => {
            let short: String = machine.chars().take(8).collect();
            format!("{kind}:{short}")
        }
        None => id.chars().take(16).collect(),
    }
}

/// 本机「跟着池走」的账号 id 集合（来源标记，见 [`BrokerConfig::cloud_ids`]）。
fn cloud_ids() -> Vec<String> {
    let Some(lock) = STATE.get() else {
        return Vec::new();
    };
    match lock.lock() {
        Ok(s) => s.cfg.cloud_ids.clone(),
        Err(_) => Vec::new(),
    }
}

fn origin_seeded() -> bool {
    let Some(lock) = STATE.get() else {
        return false;
    };
    match lock.lock() {
        Ok(s) => s.cfg.origin_seeded,
        Err(_) => false,
    }
}

/// 本机是否已经见过「续签设备」这套机制（见 [`BrokerConfig::renewer_seen`]）。
fn renewer_seen() -> bool {
    let Some(lock) = STATE.get() else {
        return true;
    };
    match lock.lock() {
        Ok(s) => s.cfg.renewer_seen,
        Err(_) => true,
    }
}

/// 落下「已见过续签设备」这一位。落盘失败只影响下一次要不要再迁一次，不影响正确性。
fn set_renewer_seen(seen: bool) {
    let written = mutate(|s| {
        if s.cfg.renewer_seen == seen {
            return Ok(());
        }
        s.cfg.renewer_seen = seen;
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("续签设备迁移标记落盘失败：{e}（下次同步会重新判断一次）"),
        );
    }
}

/// 账号来源（发给界面 / 决定续签规则）：`cloud` / `local`。
///
/// 与 [`BrokerConfig::cloud_ids`] 同源：池里收养来的才算云端。本机登录后再上传的
/// 仍是 `local` —— 否则「本地登录账号无条件续签」在绑池后就会悄悄失效。
pub fn origin_of(account: &Account) -> &'static str {
    if cloud_ids().iter().any(|id| id == &account.id) {
        "cloud"
    } else {
        "local"
    }
}

/// 续签三分类里本机视角的类别（手动导入类只在 2api 存在，见那边的实现）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenewClass {
    /// 本机登录：无条件续签
    Local,
    /// 云端同步：只有池上的续签设备（组合 id 相同）才续
    Cloud,
}

fn renew_class(account: &Account, cloud: &HashSet<String>) -> RenewClass {
    if cloud.contains(&account.id) {
        RenewClass::Cloud
    } else {
        RenewClass::Local
    }
}

/// 写回来源标记 + 第一次登记的完成标志。
///
/// 落盘失败退化成「这一轮不删、下一轮再试」 —— 不拿它否掉整轮同步。
fn set_origin_state(ids: Vec<String>, seeded: bool) {
    let written = mutate(|s| {
        if s.cfg.cloud_ids == ids && s.cfg.origin_seeded == seeded {
            return Ok(());
        }
        s.cfg.cloud_ids = ids;
        s.cfg.origin_seeded = seeded;
        save_config(&s.dir, &s.cfg)
    });
    if let Some(Err(e)) = written {
        let dir = data_dir().unwrap_or_default();
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            false,
            format!("云端来源标记落盘失败：{e}（本机下次同步会重新登记）"),
        );
    }
}

/// 记录 / 清除绑定，并**当场落盘**，返回落盘结果。
///
/// 为什么返回值不能丢（修 4）：绑定只写进内存的话，本次运行看起来一切正常 ——
/// 续签走池、界面显示「已绑定」，可应用一重启 `load_config` 读回默认值，本机安静地
/// 退回单机运行。此时云端那把闸仍认为这台机器绑着，于是另一台机器永远签不动，
/// 而本机又开始单机轮换那些一次性 refresh token。旧写法 `let _ = save_config(..)`
/// 把这条链路唯一的通知机会吞掉了。调用方负责把它说给用户听。
fn set_uuid(uuid: Option<String>) -> Result<(), String> {
    let written = mutate(|s| {
        s.cfg.pool_uuid = uuid;
        // 来源标记只对「当前这一池」有意义：解绑 / 换池后旧的 id 集合必须作废，
        // 否则再绑回来时会把一批本来只是本机独有的账号误判成「云端已删」。
        // 下一轮同步会按新池重新登记（`origin_seeded` 一并归零）。
        s.cfg.cloud_ids.clear();
        s.cfg.origin_seeded = false;
        s.cfg.renewer_seen = false;
        // 「待删除 / 待恢复」和墓碑都是相对某一池的：
        // 换池后旧 key 与旧账号 id 没有意义，也不该在新池上生效
        s.cfg.deleted_keys.clear();
        s.cfg.reviving_ids.clear();
        s.cfg.deleted_items.clear();
        // 云端读数缓存跟着这一池走：换池 / 解绑后旧的「云端账号」不能再显示
        s.rt.cloud = None;
        save_config(&s.dir, &s.cfg)
    });
    match written {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(format!(
            "绑定状态已在本机生效，但没能写入 {}：{e}（重启后会退回改动前的状态）",
            BROKER_FILE
        )),
        None => Err(format!("凭证池状态不可用，{} 没有改动", BROKER_FILE)),
    }
}

/// 把一条「本次操作没能持久化」这类补充说明拼到主提示后面；空串就原样返回。
///
/// 之所以要有这一小段：三处绑定动作（[`upload`] / [`link`] / [`unbind`]）都得在
/// **成功**的提示里带上落盘失败的风险，而「成功了，但……」这句话不能各写一遍口径。
fn append_note(main: String, note: String) -> String {
    if note.trim().is_empty() {
        return main;
    }
    format!("{main}｜{note}")
}

fn note_start() {
    mutate(|s| s.rt.syncing = true);
}

fn note_ok(version: Option<i64>) {
    mutate(|s| {
        s.rt.syncing = false;
        s.rt.error = None;
        if let Some(v) = version {
            s.rt.version = Some(v);
        }
        let now = chrono::Utc::now().timestamp_millis();
        s.rt.last_sync_ms = now;
        s.rt.last_ok_ms = now;
    });
}

fn note_touched() {
    // 被闸拒 / 被节流：也算「问过了」，否则热路径会每一跳都再打一次接口
    mutate(|s| {
        s.rt.syncing = false;
        s.rt.last_sync_ms = chrono::Utc::now().timestamp_millis();
    });
}

fn note_error(message: impl Into<String>) {
    let msg = message.into();
    mutate(|s| {
        s.rt.syncing = false;
        s.rt.error = Some(msg);
        s.rt.last_sync_ms = chrono::Utc::now().timestamp_millis();
    });
}

// ── 发给前端的形状 ────────────────────────────────────────────────────────

/// `inline`：绑定状态。前端只读它，改配置一律走 upload / link / unbind 三个动作。
#[derive(Serialize, Clone, Debug)]
pub struct BrokerStatus {
    pub bound: bool,
    pub uuid: Option<String>,
    pub version: Option<i64>,
    pub last_sync_ms: Option<i64>,
    pub last_ok_ms: Option<i64>,
    pub error: Option<String>,
    pub syncing: bool,
    /// 本机的组合 id（客户端类型 + 机器标识）
    pub self_id: String,
    /// 池上当前的续签设备；`null` 且 `renewer_known` = 确实没有设备认领
    pub renewer: Option<String>,
    /// 是否已经问过池：`false` 时界面显示「同步后可知」，不要把 null 当成「没有」
    pub renewer_known: bool,
    /// 本机是不是续签设备（只有它续签云端账号）
    pub is_renewer: bool,
    /// 取消续签的宽限截止；宽限内没有新设备，本机会自动写回去
    pub grace_until: Option<i64>,
}

pub fn status() -> BrokerStatus {
    let (uuid, rt) = snapshot();
    let self_id = self_id();
    BrokerStatus {
        bound: uuid.is_some(),
        uuid,
        version: rt.version,
        last_sync_ms: (rt.last_sync_ms > 0).then_some(rt.last_sync_ms),
        last_ok_ms: (rt.last_ok_ms > 0).then_some(rt.last_ok_ms),
        error: rt.error,
        syncing: rt.syncing,
        is_renewer: rt.renewer_known && rt.renewer.as_deref() == Some(self_id.as_str()),
        self_id,
        renewer: rt.renewer,
        renewer_known: rt.renewer_known,
        grace_until: rt.grace_until,
    }
}

/// 上传 / 绑定的结果：**uuid 是唯一要展示给用户复制的东西**
#[derive(Serialize, Clone, Debug)]
pub struct PoolOp {
    pub uuid: String,
    pub account_count: usize,
    pub merged: usize,
    pub message: String,
}

/// 一轮同步的结论（给界面提示用）
#[derive(Serialize, Clone, Debug)]
pub struct SyncReport {
    pub changed: bool,
    pub deferred: bool,
    pub merged: usize,
    pub refreshed: usize,
    pub failed: usize,
    /// 本机**签不动**的条数（票已失效，重试也是同一结论）。它不算 `failed`，
    /// 因为它不该让整池进冷却 —— 见 `sync` 里那条分支。
    #[serde(default)]
    pub blocked: usize,
    /// 因「云端已删」而从本机移除的条数（见 [`prune_cloud_gone`]）。
    #[serde(default)]
    pub pruned: usize,
    /// 本机**故意没续**的云端账号数：本机不是续签设备（或池上还没指定）。
    /// 它不算失败，也不触发冷却 —— 本来就不该由这台机器去轮换那条链。
    #[serde(default)]
    pub deferred_cloud: usize,
    pub version: Option<i64>,
    pub message: String,
}

// ── 条目 ──────────────────────────────────────────────────────────────────

/// 池里的一条账号凭证。字段名与 `Account` **故意不同**（`access_token` vs `token`）：
/// 这一层是跨机器的数据契约，不该随本地落盘模型改名而变。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PoolItem {
    /// 跨机身份锚点（手机号 → 昵称 → 本地 id）
    pub key: String,
    pub name: String,
    pub phone: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<i64>,
    pub rt_expires_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// 逻辑删除时间（服务端墓碑）。`items` 里不会出现；只在响应顶层的 `deleted` 里出现。
    pub deleted_at: Option<i64>,
}

/// 跨机认人的锚点：**手机号 → 昵称 → 本地 id**。
///
/// 手机号最稳（token 会轮换、昵称会改、本地 id 每台机器都不一样），所以排第一。
pub fn item_key_of(account: &Account) -> String {
    if let Some(p) = account.phone.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return p.to_string();
    }
    if !account.name.trim().is_empty() {
        return account.name.trim().to_string();
    }
    account.id.clone()
}

pub fn to_item(account: &Account) -> PoolItem {
    PoolItem {
        key: item_key_of(account),
        name: account.name.clone(),
        phone: account.phone.clone().unwrap_or_default(),
        access_token: account.token.clone(),
        refresh_token: account.refresh_token.clone().unwrap_or_default(),
        // 上传**推得出的**到期，而不是本地那个可能为空白的字段：`expires_at: null`
        // 到了别的机器上就是「永不续签」，而真实寿命一直写在这张票里。
        expires_at: acct_expiry(account),
        rt_expires_at: account.rt_expires_at,
        updated_at: Some(chrono::Utc::now().timestamp_millis()),
        deleted_at: None,
    }
}

/// 本账号的到期时间：字段优先，空了就从票里的 JWT `exp` 折算（见 `refresh::expiry_ms`）。
fn acct_expiry(account: &Account) -> Option<i64> {
    crate::refresh::expiry_ms(account.expires_at, &account.token)
}

/// 池里那一条的到期时间，判据同 [`acct_expiry`]。
///
/// 别的机器（或更早版本的我们）上传的条目可能是 `"expires_at": null`，
/// 而票是完整的 —— 那种 null 不是「永不过期」，也不是「已过期」，只是**没带上**。
fn item_expiry(item: &PoolItem) -> Option<i64> {
    crate::refresh::expiry_ms(item.expires_at, &item.access_token)
}

/// 池里的一条 → 本机新账号（本地还没有这个 key 时用）
pub fn account_from_item(item: &PoolItem) -> Account {
    let name = if item.name.trim().is_empty() {
        format!(
            "账号-{}",
            &item.key.chars().take(6).collect::<String>()
        )
    } else {
        item.name.clone()
    };
    Account {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        phone: Some(item.phone.clone()).filter(|s| !s.trim().is_empty()),
        token: item.access_token.clone(),
        refresh_token: Some(item.refresh_token.clone()).filter(|s| !s.trim().is_empty()),
        expires_at: item.expires_at,
        rt_expires_at: item.rt_expires_at,
        base_url: None,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        last: None,
        checked_today: None,
        invalidated_at: None,
        renew_blocked_until: None,
    }
}

/// 池里那一条是否**严格新于**本机这一条 —— [`adopt`] 用的判据（云端 → 本地方向）。
///
/// 两个方向必须共用同一套新序判据（这里是对称的两条：[`cloud_is_newer`] / [`local_is_newer`]）：
/// 各写一套就会出现「A 看 B 更新、B 看 A 也更新」，两台机器每轮同步都改一次整池版本、永不收敛。
///
/// 「读不出到期」不等于「更旧」：一侧读得出、另一侧读不出时，读得出的算新；
/// 两侧都读不出（不透明票又没带到期字段）就**谁也不许盖谁**，来历不明的不算更新。
/// 两张票是不是同一个账号签出来的（JWT `sub` 相同）。
///
/// 手机号是池条目的 key，但客户端会把手机号打码（`138****0000`），而池里那条仍是
/// 完整号码 —— 只比 key 会把同一个账号拆成「本机一条 + 池里一条」，
/// 表现为**点一次导入出现两条，一真一假**。`sub` 是唯一能穿透打码的身份锚点。
fn same_subject(a: &str, b: &str) -> bool {
    match (
        crate::checkin::token_subject(a),
        crate::checkin::token_subject(b),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn cloud_is_newer(item: &PoolItem, account: &Account) -> bool {
    // 先用 `iat`（签发时间）判：同一账号换票时它单调递增，而 `exp` 会骗人 ——
    // 已被顶掉的死票常把 `exp` 写得更晚（09-28 那张 exp 11-22 比 10-24 新票的 11-21 还晚），
    // 按 exp 判就会拿死票盖掉活票。
    match (
        crate::checkin::token_issued_at(&item.access_token),
        crate::checkin::token_issued_at(&account.token),
    ) {
        (Some(p), Some(l)) if p != l => return p > l,
        _ => {}
    }
    match (item_expiry(item), acct_expiry(account)) {
        (Some(p), Some(l)) => p > l,
        (Some(_), None) => true,
        _ => false,
    }
}

/// 本机这一条是否**严格新于**池里那一份 —— [`cloud_is_newer`] 的反方向，供本地 → 云端回写用。
fn local_is_newer(account: &Account, item: &PoolItem) -> bool {
    match (
        crate::checkin::token_issued_at(&account.token),
        crate::checkin::token_issued_at(&item.access_token),
    ) {
        (Some(l), Some(p)) if l != p => return l > p,
        _ => {}
    }
    match (acct_expiry(account), item_expiry(item)) {
        (Some(l), Some(p)) => l > p,
        (Some(_), None) => true,
        _ => false,
    }
}

/// 本机这一条现在还能用吗？
///
/// 不可用 = 已经救不回来：被官方判死（`invalidated_at`），或者有续签链但链本身过期，
/// 或者没有续签链、access token 又已经过期。**「不知道」不算不可用** ——
/// 到期时间读不出来时宁可当它还有效，也别拿一句猜测把它换掉。
fn account_usable(account: &Account, now: i64) -> bool {
    if account.token.trim().is_empty() || account.invalidated_at.is_some() {
        return false;
    }
    let has_rt = account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    if has_rt {
        return !crate::refresh::rt_is_expired(account.rt_expires_at, now);
    }
    acct_expiry(account).map(|exp| exp > now).unwrap_or(true)
}

/// 池里那一条现在还能用吗？判据与 [`account_usable`] 对称（池里没有 `invalidated` 标记）。
fn item_usable(item: &PoolItem, now: i64) -> bool {
    if item.access_token.trim().is_empty() {
        return false;
    }
    if !item.refresh_token.trim().is_empty() {
        return !crate::refresh::rt_is_expired(item.rt_expires_at, now);
    }
    item_expiry(item).map(|exp| exp > now).unwrap_or(true)
}

/// 把池里那一条采纳到本地账号上。返回是否真的改动了。
///
/// 判据分三层（按优先级）：
///
/// 1. **来源优先级**：本地登录 > 云端同步 > 手动导入。高优先级那一份**可用**时，
///    低优先级的一律不许覆盖它 —— 不再拿「谁的 expires_at 更晚」当唯一判据。
/// 2. **不可用直接覆盖**：本机这条已判死 / 续签链过期 / 没链又已过期时，
///    只要池里那份可用就无条件换血。2026-10-08「本地死票锁死」的抢救也走这一条。
///    反过来，**池里那份自己不可用就整个跳过**：不接管、不补字段（见函数开头的判断）。
/// 3. **同源之间**（云端账号 ↔ 池里那份）仍按新序：`iat` 更新或 `exp` 更晚才算更新，
///    防止先到的旧副本把新凭证盖回去。
///
/// 不满足覆盖条件时仍会补本地缺的字段（老账号没有 refresh token、没有手机号），
/// 因为那是从「不知道」变成「知道」，不存在覆盖新值的风险。
fn adopt(account: &mut Account, item: &PoolItem, class: RenewClass, now: i64) -> bool {
    let has_token = !item.access_token.trim().is_empty() || !item.refresh_token.trim().is_empty();
    if !has_token {
        // 空 / 纯空白 token 一律忽略：采纳它等于把账号弄成登录不上
        return false;
    }
    let differs = item.access_token.trim() != account.token
        || (!item.refresh_token.trim().is_empty()
            && item.refresh_token.trim() != account.refresh_token.as_deref().unwrap_or("").trim());
    // 池里那份已经失效（判死 / 续签链过期 / 无链又已过期）：**整个跳过** ——
    // 不换血、也不补字段。拿一具尸体补进来的只有两样：签不动的票，或一个马上又要
    // 被判死的链。真正要救这条账号，靠的是本机可用时在回写方向把它压回池里。
    if !item_usable(item, now) {
        return false;
    }
    let account_ok = account_usable(account, now);
    let overwrite = if !account_ok {
        true
    } else {
        match class {
            RenewClass::Local => false,
            RenewClass::Cloud => cloud_is_newer(item, account),
        }
    };
    if !(overwrite && differs) {
        let mut touched = false;
        if account.refresh_token.is_none() && !item.refresh_token.trim().is_empty() {
            account.refresh_token = Some(item.refresh_token.clone());
            touched = true;
        }
        // 「补空字段」也算改动：从「不知道」变成「知道」是信息增加，不是覆盖。
        // 但两侧都是 `None` 时**不算** —— 那只是把 `None` 赋给 `None`，
        // 报成改动会让调用方平白多落一次盘，同步提示里也多报一个「并入 N 个」。
        if account.expires_at.is_none() && item.expires_at.is_some() {
            account.expires_at = item.expires_at;
            touched = true;
        }
        if account.rt_expires_at.is_none() && item.rt_expires_at.is_some() {
            account.rt_expires_at = item.rt_expires_at;
            touched = true;
        }
        if account.phone.is_none() && !item.phone.trim().is_empty() {
            account.phone = Some(item.phone.clone());
            touched = true;
        }
        return touched;
    }
    if !item.access_token.trim().is_empty() {
        account.token = item.access_token.clone();
    }
    if !item.refresh_token.trim().is_empty() {
        account.refresh_token = Some(item.refresh_token.clone());
    }
    account.expires_at = item
        .expires_at
        .or_else(|| item_expiry(item))
        .or(account.expires_at);
    account.rt_expires_at = item.rt_expires_at.or(account.rt_expires_at);
    // 新票在手：本机的活性/冷却标记一并清掉 —— 换血本身就是对死锁的解除
    account.invalidated_at = None;
    account.renew_blocked_until = None;
    if account.phone.is_none() && !item.phone.trim().is_empty() {
        account.phone = Some(item.phone.clone());
    }
    if account.name.trim().is_empty() && !item.name.trim().is_empty() {
        account.name = item.name.clone();
    }
    true
}

/// 本机这一条对得上池里的某一条吗？认人规则与 [`merge_into`] 一致（key 相等，或 JWT `sub`
/// 相同），另加一条 refresh token 相同 —— 它只在「票刚在本机轮换过、手机号又漂移了」这种
/// 两边都认不出的窄缝里兜底。剪枝方向宁可多留：多留一条下一轮还能再判，误删一条就没了。
fn corresponds(account: &Account, item: &PoolItem) -> bool {
    let key = item_key_of(account);
    let rt = account.refresh_token.as_deref().unwrap_or("").trim();
    (!item.key.trim().is_empty() && key == item.key.trim())
        || same_subject(&account.token, &item.access_token)
        || (!rt.is_empty() && rt == item.refresh_token.trim())
}

/// 云端已删 → 本机跟着删。返回被删账号的名字（给日志用）。
///
/// **只对来源标记是「云端」的账号生效**，本地独有的一条都不碰；也**只在抢到闸之后调用** ——
/// 那时手里的 `items` 才是整池的权威快照。抢闸被拒 / 打不通时一律不删：没有快照就删人
/// 等于瞎删。
///
/// 「池里还在」的判据与 [`merge_into`] 完全一致：宁可多留一条（下一轮再判），
/// 也不能把一条还在池里的账号误删。
fn prune_cloud_gone(
    accounts: &mut Vec<Account>,
    cloud_ids: &HashSet<String>,
    items: &[PoolItem],
    protected: &HashSet<String>,
) -> Vec<String> {
    // **空快照一律不剪**：池子合法为空只会发生在「本机已经把账号删光」之后，
    // 而服务端读出口对坏 JSON / 坏条目是静默当空池（见 cred-broker README）——
    // 那种「空」不能拿来当删除全部本机账号的依据。
    if items.is_empty() {
        return Vec::new();
    }
    let mut removed = Vec::new();
    accounts.retain(|a| {
        // 刚从墓碑恢复、还没提交的账号：服务端此刻的快照里当然没有它，不能据此删
        if protected.contains(&a.id) || !cloud_ids.contains(&a.id) {
            return true;
        }
        let present = items.iter().any(|i| corresponds(a, i));
        if !present {
            removed.push(a.name.clone());
        }
        present
    });
    removed
}

/// 云端墓碑 → 本机跟着删。返回被删账号的名字（给日志用）。
///
/// 服务端的逻辑删除会在响应顶层的 `deleted` 里带上墓碑；**不能靠「live 快照里没有」来剪**
/// 唯一一个池被删到一条不剩的情形 —— 那时 live 是空集，而 `prune_cloud_gone` 出于
/// 「空快照不剪」的安全阀（防坏 JSON 被读成空池）什么都不会删，于是本机那些账号
/// 会被下一轮并集又传回云端，删除等于没发生。墓碑是**明确的删除证据**，所以照剪。
///
/// 判据与合并一致（key / JWT sub / RT），且**只动来源标记是「云端」的账号**。
fn prune_cloud_deleted(
    accounts: &mut Vec<Account>,
    cloud_ids: &HashSet<String>,
    deleted: &[PoolItem],
    live: &[PoolItem],
    protected: &HashSet<String>,
) -> Vec<String> {
    if deleted.is_empty() {
        return Vec::new();
    }
    let mut removed = Vec::new();
    accounts.retain(|a| {
        // 恢复中、或本机来源的账号，一律不跟着墓碑删
        if protected.contains(&a.id) || !cloud_ids.contains(&a.id) {
            return true;
        }
        // 同一账号还有活条目（手机号 / 昵称漂移后旧 key 留下的墓碑、或别的机器刚恢复），
        // 不是「被删了」，不能连坐
        let gone = deleted.iter().any(|i| corresponds(a, i))
            && !live.iter().any(|i| corresponds(a, i));
        if gone {
            removed.push(a.name.clone());
        }
        !gone
    });
    removed
}

/// 把「这一轮池快照里确实有的本机账号」登记为云端来源（按本机 id）。
///
/// 这是老版本账号的**迁移入口**：升级前没有来源标记，第一次同步时池里有什么就登记什么，
/// 只加不减 —— 池里没有的本机账号保持「本地」，永远不会被这条规则删掉。
fn note_cloud_members(
    accounts: &[Account],
    items: &[PoolItem],
    cloud_ids: &mut HashSet<String>,
) {
    for a in accounts {
        if items.iter().any(|i| corresponds(a, i)) {
            cloud_ids.insert(a.id.clone());
        }
    }
}

/// 合并之后：**本轮才出现的账号**才算「从池里收养的」，登记成云端来源。
///
/// 不能拿「池里有这个 key」当判据：本机登录后上传的账号也在池里，那样它会在下一次
/// 同步丢掉本地身份，绑池后就不再无条件续签了。`before` 是合并前的本机 id 集合。
fn note_adopted(accounts: &[Account], before: &HashSet<String>, cloud_ids: &mut HashSet<String>) {
    for a in accounts {
        if !before.contains(&a.id) {
            cloud_ids.insert(a.id.clone());
        }
    }
}

/// 把池里那份并进本机账号：**并集** —— 本地独有的账号一个都不删。
/// 池里已失效的条目自动跳过（既不为它建新账号，也不拿它补字段）。返回改动的条目数。
fn merge_into(
    accounts: &mut Vec<Account>,
    items: &[PoolItem],
    cloud: &HashSet<String>,
    now: i64,
) -> usize {
    let mut changed = 0;
    for item in items {
        if item.key.trim().is_empty() {
            continue;
        }
        // 墓碑不是账号。正常路径上服务端把它们放在 `deleted` 里、不会进 `items`，
        // 这里再挡一次：哪怕调用方传错了列表，也不能让一条已删账号被「收养」回来。
        if item.deleted_at.is_some() {
            continue;
        }
        // 失效条目连「建新账号」都不做：否则本机凭空多出一条签不动的账号，
        // 界面上看着像有、续签时才发现是死的。
        if !item_usable(item, now) {
            continue;
        }
        match accounts
            .iter_mut()
            .find(|a| item_key_of(a) == item.key.trim() || same_subject(&a.token, &item.access_token))
        {
            Some(a) => {
                let class = renew_class(a, cloud);
                if adopt(a, item, class, now) {
                    changed += 1;
                }
            }
            None => {
                accounts.push(account_from_item(item));
                changed += 1;
            }
        }
    }
    changed
}

/// 这张本地票**允许回写**云端吗（sync ④ 的守卫）。
///
/// 已被官方接口判死（`invalidated_at` 有值）的票绝不进池：`expires_at` 比较分不清
/// 「更新的轮换」和「到期时间声明得更晚、但已被吊销的死票」，放行会把死票盖在活票上、
/// 污染所有绑池机器（2026-10-08 事故里最危险的一半）。
fn write_back_allowed(account: &Account) -> bool {
    account.invalidated_at.is_none()
}

/// 本地 → 云端：本机这份该不该盖掉池里那条？
///
/// 与 [`adopt`] 同一套优先级，方向相反：本机**可用**的本地账号永远压过云端那份
/// （本地登录就是权威副本）；云端账号照旧比新旧。本机不可用时绝不回写 ——
/// 死票一旦进池，会把所有绑池机器一起污染。
fn should_upload(account: &Account, item: &PoolItem, class: RenewClass, now: i64) -> bool {
    if !write_back_allowed(account) || !account_usable(account, now) {
        return false;
    }
    if !item_usable(item, now) {
        return true;
    }
    match class {
        RenewClass::Local => true,
        RenewClass::Cloud => local_is_newer(account, item),
    }
}

/// 提交给管家的整池内容 = **云端那份 ∪ 本机账号**。
///
/// 不能只拿云端那份起步：`PUT` 是**整池替换**，漏掉「本机有、池里没有」的账号，就等于
/// 把并集悄悄缩回云端那一份 —— 两台机器各绑同一池时，池里会永远只有第一个上传者的账号，
/// 另一台的账号只进本地、不出本地。
fn union_pool(
    cloud: &[PoolItem],
    local: &[Account],
    cloud_ids: &HashSet<String>,
    now: i64,
) -> Vec<PoolItem> {
    // 墓碑一律不带回云端：`PUT` 是整池替换，把墓碑原样传回去等于复活它。
    let mut pool: Vec<PoolItem> = cloud
        .iter()
        .filter(|i| i.deleted_at.is_none())
        .cloned()
        .collect();
    for acct in local {
        let key = item_key_of(acct);
        if key.trim().is_empty() {
            continue;
        }
        let hit = pool
            .iter()
            .position(|i| i.key.trim() == key || same_subject(&i.access_token, &acct.token));
        match hit {
            // 同一个账号池里已经有一条：按来源优先级决定谁留下（见 [`should_upload`]）。
            // 本地登录是权威副本，可用时无条件替换；云端账号只在「更新」时才覆盖。
            Some(idx) => {
                let class = renew_class(acct, cloud_ids);
                if should_upload(acct, &pool[idx], class, now) {
                    pool[idx] = to_item(acct);
                }
            }
            None => pool.push(to_item(acct)),
        }
    }
    pool
}

// ── HTTP ──────────────────────────────────────────────────────────────────

/// 失败的三态。**必须分开**，否则「池被删」会被当成「网络抖动」，
/// 而前者要求用户重新绑定、后者只要求保持本地凭证继续用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 打不通 / 超时 / 5xx：保持本地凭证继续用，不打扰用户
    Unreachable,
    /// 池不存在（404 gone）：池在别处被解绑了，
    /// **必须清掉本地 uuid** 并让用户重新上传或绑定
    Gone,
    /// 请求被明确拒绝（其它 4xx）：多半是版本对不上，属于要修的错
    Denied,
}

fn classify(status: u16, body: &str) -> Option<FailKind> {
    if (200..300).contains(&status) {
        return None;
    }
    if status == 404 && body.contains("gone") {
        return Some(FailKind::Gone);
    }
    if status >= 500 {
        return Some(FailKind::Unreachable);
    }
    Some(FailKind::Denied)
}

/// 管家的出站客户端：**全进程复用一份**，并且强制直连。
///
/// - 复用：原来是每次 [`request`] 都新建一个，而调用点在热路径上（反代每个请求都可能
///   顺带同步一轮）。每次新建 = 新连接池 + 重新 TLS 握手 + 重跑一遍代理/DNS 判定，
///   而 `reqwest::Client` 内部就是 Arc，clone 出去比新建便宜得多。
/// - 直连：`reqwest` 默认跟随 `HTTP_PROXY` / `HTTPS_PROXY`，于是「管家到底走哪条路」
///   取决于进程启动时继承了什么环境变量 —— 同一个版本在两台机器上行为不同，而这种差别
///   在日志里看不出来。管家是自托管 + cloudflared 的公网入口，本来就不需要中间还有一棒，
///   所以与 [`crate::http::api_client_direct`] 一样明确直连。
///
///   ⚠️ **它治的不是 `530 / error code 1033`**：2026-10-21 实测同一个域名直连与走代理
///   返回完全相同的 530（`error code: 1033`），那是 cloudflared 隧道/源站那侧不可达。
///   这里只是把「出口」这个变量从排查里去掉，别指望改它能修好 530。
///   代价也要说清：如果哪天必须经代理访问管家，得改这一行 —— 环境变量在这里不生效。
fn client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(crate::http::TIMEOUT)
                .user_agent(format!("{}-broker", crate::http::UA))
                .no_proxy()
                .build()
                .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))
        })
        .clone()
}

/// 这台机器的标识。**只进服务端的审计日志，不参与任何判断** ——
/// 所以它是什么名字都不影响正确性，拿不到就用 hostname 兜底。
fn actor() -> String {
    static ACTOR: OnceLock<String> = OnceLock::new();
    ACTOR
        .get_or_init(|| {
            for key in ["HOSTNAME", "COMPUTERNAME"] {
                if let Ok(v) = std::env::var(key) {
                    let v = v.trim();
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
            if let Ok(o) = std::process::Command::new("hostname").output() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            "unknown".to_string()
        })
        .clone()
}

async fn request(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, (FailKind, String)> {
    let c = client().map_err(|e| (FailKind::Unreachable, e))?;
    let url = format!("{BASE}{path}");
    let mut req = c.request(method, &url).header("x-cred-actor", actor());
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (FailKind::Unreachable, format!("连接凭证管家失败：{e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if let Some(kind) = classify(status, &text) {
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("error"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(120).collect());
        return Err((kind, format!("凭证管家返回 HTTP {status}：{detail}")));
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|_| (FailKind::Denied, "凭证管家返回的不是 JSON".to_string()))
}

#[derive(Deserialize, Debug)]
struct LeaseResp {
    #[serde(default)]
    granted: bool,
    #[serde(default)]
    items: Vec<PoolItem>,
    /// 云端已逻辑删除的账号（墓碑）。老服务端没有这个字段 → 空。
    #[serde(default)]
    deleted: Vec<PoolItem>,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    retry_after: i64,
    #[serde(default)]
    lease_owner: Option<String>,
    /// 池上唯一的续签设备（老服务端没有这个字段 → null）
    #[serde(default)]
    renewer: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CreateResp {
    uuid: String,
    #[serde(default)]
    version: i64,
}

#[derive(Deserialize, Debug)]
struct VersionResp {
    #[serde(default)]
    version: i64,
}

/// 把 `(FailKind, msg)` 变成给用户看的一句话。`Gone` 额外清掉本地 uuid。
fn on_failure(kind: FailKind, message: String) -> String {
    match kind {
        FailKind::Gone => {
            // 这里连「摘干净」都不保证：落盘失败时磁盘上仍留着那串已经不存在的 uuid，
            // 重启后会再绑一次、再吃一次 Gone。不致命（自愈），但得让人看见。
            let note = set_uuid(None).err().unwrap_or_default();
            format!(
                "云端那一池已经不存在了（可能已被清理），本机已退回单机运行，请重新上传或绑定。{note}"
            )
        }
        FailKind::Unreachable => format!("{message}（本次继续用本机凭证）"),
        FailKind::Denied => message,
    }
}

// ── 动作 ──────────────────────────────────────────────────────────────────

/// 已绑定就不许再建新池。
///
/// 规则单独拎成纯函数，是为了能直接钉住它 —— 真实 `upload` 会打接口，
/// 而这条规则必须在**发出任何请求之前**生效。
fn upload_guard(already_bound: bool) -> Result<(), String> {
    if already_bound {
        return Err(
            "本机已经绑定了云端凭证池。要重新上传，请先解绑（解绑只摘本机绑定，云端那一池保留）。"
                .to_string(),
        );
    }
    Ok(())
}

/// 把本机这一批账号整体上传：管家颁发一串 uuid 并当场绑定。
///
/// 这是**整台机器**的动作，不是「某个账号」的 —— 所以不收账号参数。
///
/// ⚠️ **已绑定时直接拒绝**（见 [`upload_guard`]）：服务端建池永远是新建、不会覆盖，
/// 放行一次就会在云端留下第二池，让两台机器各持一把闸。
pub async fn upload(dir: &Path) -> Result<PoolOp, String> {
    upload_guard(bound())?;
    let accounts = accounts::load_accounts(dir);
    if accounts.is_empty() {
        return Err("本机还没有账号，先「导入本机账号」或「登录新账号」".to_string());
    }
    let items: Vec<PoolItem> = accounts.iter().map(to_item).collect();
    let me = self_id();
    let v = request(
        reqwest::Method::POST,
        "/v1/pool",
        Some(serde_json::json!({ "items": items, "renewer": me.clone() })),
    )
    .await
    .map_err(|(k, m)| on_failure(k, m))?;
    let created: CreateResp = serde_json::from_value(v)
        .map_err(|e| format!("凭证管家返回的建池结果看不懂：{e}"))?;

    // 建池已经成功、绑定也写进内存了，所以这里**不失败**（失败就把云端那池退回未绑定？
    // 更糟：云端有池、本机不知道）。但必须把「这份绑定不一定留得住」写进提示 ——
    // 见 [`set_uuid`]：没落盘成功的话，重启后本机安静地退回单机，而云端那把闸还占着。
    let bind_note = set_uuid(Some(created.uuid.clone())).err().unwrap_or_default();
    mutate(|s| {
        s.rt.version = Some(created.version);
        s.rt.error = None;
        // 建池时上传者就是第一个续签设备，服务端已经写进去了
        s.rt.renewer = Some(me);
        s.rt.renewer_known = true;
        s.rt.grace_until = None;
    });
    note_ok(Some(created.version));

    Ok(PoolOp {
        uuid: created.uuid,
        account_count: items.len(),
        merged: 0,
        message: append_note(
            format!("已把本机的 {} 个账号放上云端", items.len()),
            bind_note,
        ),
    })
}

/// 绑定别处复制过来的 uuid：先验证这一池真的存在，再并进本地。
///
/// 先验证是必要的防呆：把一串打不通的 uuid 写进本地配置，之后每次同步都失败，
/// 而用户以为已经绑好了。
pub async fn link(dir: &Path, raw_uuid: &str) -> Result<PoolOp, String> {
    let uuid = raw_uuid.trim();
    if uuid.is_empty() {
        return Err("请先粘贴云端凭证池的 uuid".to_string());
    }
    // ⚠️ **不要复用 `on_failure`**：`Gone` 分支里有 `set_uuid(None)`，而这里本机可能
    // 正绑在**另一池**上（用户就是想换过去）。抄错几位 uuid 就把正在用的绑定清掉，
    // 等于把一次打错字升级成一次服务中断。绑定失败就原样失败，本地绑定一动不动。
    let v = match request(reqwest::Method::GET, &format!("/v1/pool/{uuid}"), None).await {
        Ok(v) => v,
        Err((FailKind::Gone, _)) => {
            return Err(
                "云端没有这一池：uuid 可能抄错了，或者它已不存在。\
                 本机的绑定没有变动。"
                    .to_string(),
            )
        }
        Err((_, m)) => return Err(format!("绑定失败，本机的绑定没有变动：{m}")),
    };
    let items: Vec<PoolItem> = v
        .get("items")
        .cloned()
        .and_then(|x| serde_json::from_value(x).ok())
        .unwrap_or_default();
    let version = v.get("version").and_then(|x| x.as_i64());
    let renewer_field = v.get("renewer").cloned();
    let renewer = renewer_field
        .as_ref()
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let renewer_known = renewer_field.is_some();

    // 同 `upload`：本次运行确实绑上了，不该谎报失败；但「能不能留住」必须说清楚。
    let bind_note = set_uuid(Some(uuid.to_string())).err().unwrap_or_default();
    mutate(|s| {
        s.rt.version = version;
        s.rt.error = None;
        s.rt.last_sync_ms = 0; // 下一次调用立刻做一轮完整同步
        s.rt.renewer = renewer;
        s.rt.renewer_known = renewer_known;
        s.rt.grace_until = None;
    });

    let mut accounts = accounts::load_accounts(dir);
    // 刚清掉来源标记：本机已有的账号都按「本地」对待 —— 池里那份不许盖掉它们，
    // 只补缺失字段。真正的对齐交给下面那一轮同步（本地优先回写）。
    let merged = merge_into(
        &mut accounts,
        &items,
        &HashSet::new(),
        chrono::Utc::now().timestamp_millis(),
    );
    accounts::save_accounts(dir, &accounts).map_err(|e| e.to_string())?;

    // 绑定后立刻整池同步一轮：把「本地独有的账号」也推上云（并集才是这一池的真相）。
    // 失败不影响绑定本身 —— 已经绑上了，下一跳还会再试。
    let sync_note = match sync(dir, true).await {
        Ok(r) => r.message,
        Err(e) => format!("已绑定，但首次同步未完成：{e}"),
    };

    Ok(PoolOp {
        uuid: uuid.to_string(),
        account_count: items.len(),
        merged,
        message: append_note(sync_note, bind_note),
    })
}

/// 老池升级迁移：池上还没有续签设备时，本机主动认领一次。
///
/// 老版本没有「一池一个续签设备」这个概念，池的 `renewer` 一开始就是 NULL。放着不管的话，
/// 升级后云端账号会**谁都不续**——直到有人想起来去点按钮，而那时可能已经有一批票过期了。
/// 只在本机第一次同步（`renewer_seen == false`）时做；之后换设备照旧「旧设备取消 →
/// 新设备手动认领」，不会因为这里而出现两台同时续签。
///
/// 另一台机器同时迁移也没关系：认领是 CAS，输的那一方把赢家的 id 取回来，
/// 界面照样显示「续签设备：其他机器」而不是「未指定」。
async fn claim_orphan_renewer(dir: &Path, current: Option<String>) -> Option<String> {
    if current.is_some() {
        return current;
    }
    let Some(pool) = uuid() else {
        return current;
    };
    let path = format!("/v1/pool/{pool}/renewer");
    let me = self_id();
    match request(
        reqwest::Method::PUT,
        &path,
        Some(serde_json::json!({ "renewer": me })),
    )
    .await
    {
        Ok(v) => {
            let now = v.get("renewer").and_then(|x| x.as_str()).map(str::to_string);
            if now.is_some() {
                crate::logs::push(
                    dir,
                    crate::logs::CAT_SYSTEM,
                    "凭证池",
                    true,
                    "这一池还没有续签设备，本机已自动认领（老池升级迁移，只做一次）".to_string(),
                );
            }
            now
        }
        Err((_, m)) if m.contains("renewer_taken") => {
            match request(reqwest::Method::GET, &path, None).await {
                Ok(v) => v.get("renewer").and_then(|x| x.as_str()).map(str::to_string),
                Err(_) => None,
            }
        }
        // 网络问题：不落标记，下一轮同步再试
        Err(_) => None,
    }
}

/// 认领 / 取消续签设备时的错误人话。
///
/// 服务端的 409 只会回 `renewer_taken` / `not_renewer` 两个错误码，客户端把它翻成
/// 「该去哪台机器上操作」—— 否则用户只会看到一句 HTTP 409。
fn renewer_error(kind: FailKind, message: String, action: &str) -> String {
    if kind == FailKind::Gone {
        return on_failure(kind, message);
    }
    if message.contains("renewer_taken") {
        return "这一池已经有续签设备了。要换设备，先在旧设备上点「取消本机续签」，再在这里设置。"
            .to_string();
    }
    if message.contains("not_renewer") {
        return "当前续签设备不是这台机器，取消请求被拒绝。请在显示为「续签设备」的那台机器上操作。"
            .to_string();
    }
    format!("{action}续签设备失败：{message}")
}

/// 把本机设为这一池的续签设备。
///
/// 服务端只在「空着或本来就是自己」时放行；别的设备占着就 409 —— 不提供强制覆盖，
/// 否则换设备可以绕过「旧设备先退位」这一步，又回到两台同时续签。
pub async fn claim_renewer() -> Result<BrokerStatus, String> {
    let uuid = uuid().ok_or_else(|| "本机未绑定凭证池".to_string())?;
    let me = self_id();
    let v = request(
        reqwest::Method::PUT,
        &format!("/v1/pool/{uuid}/renewer"),
        Some(serde_json::json!({ "renewer": me.clone() })),
    )
    .await
    .map_err(|(kind, m)| renewer_error(kind, m, "设置"))?;
    let current = v.get("renewer").and_then(|x| x.as_str()).map(str::to_string);
    mutate(|s| {
        s.rt.renewer = current;
        s.rt.renewer_known = true;
        s.rt.grace_until = None;
        s.rt.error = None;
    });
    Ok(status())
}

/// 取消本机续签设备身份，并启动 **10 秒回落**。
///
/// 为什么需要回落：换设备的标准动作是「旧设备清空 → 新设备认领」，中间那段池上没有
/// 任何续签设备。新设备没来得及操作（人走开了 / 网络断了）时，整池就停在「云端账号
/// 谁都不续签」—— 看起来一切正常，直到某张票过期。10 秒后若仍空着，本机把自己写回去；
/// 如果新设备已经认领，这次 PUT 会撞 409，什么都不做。
pub async fn release_renewer() -> Result<BrokerStatus, String> {
    let pool = uuid().ok_or_else(|| "本机未绑定凭证池".to_string())?;
    let me = self_id();
    request(
        reqwest::Method::POST,
        &format!("/v1/pool/{pool}/renewer/release"),
        Some(serde_json::json!({ "renewer": me.clone() })),
    )
    .await
    .map_err(|(kind, m)| renewer_error(kind, m, "取消"))?;

    let grace_until = chrono::Utc::now().timestamp_millis() + RELEASE_GRACE_MS;
    mutate(|s| {
        s.rt.renewer = None;
        s.rt.renewer_known = true;
        s.rt.grace_until = Some(grace_until);
        s.rt.error = None;
    });

    let dir = data_dir();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(RELEASE_GRACE_MS as u64)).await;
        // 宽限期间解绑 / 换池了：旧池的身份不关新池的事
        if uuid() != Some(pool.clone()) {
            return;
        }
        let path = format!("/v1/pool/{pool}/renewer");
        match request(
            reqwest::Method::PUT,
            &path,
            Some(serde_json::json!({ "renewer": me.clone() })),
        )
        .await
        {
            Ok(v) => {
                let current = v.get("renewer").and_then(|x| x.as_str()).map(str::to_string);
                mutate(|s| {
                    s.rt.renewer = current;
                    s.rt.renewer_known = true;
                    s.rt.grace_until = None;
                });
                if let Some(dir) = dir.as_deref() {
                    crate::logs::push(
                        dir,
                        crate::logs::CAT_SYSTEM,
                        "凭证池",
                        true,
                        "取消续签后 10 秒内没有新设备接手，本机已自动恢复为续签设备".to_string(),
                    );
                }
            }
            Err((FailKind::Denied, m)) if m.contains("renewer_taken") => {
                // 新设备在宽限内接走了：把缓存刷新成它，界面不再显示「没有续签设备」
                if let Ok(v) = request(reqwest::Method::GET, &path, None).await {
                    let current = v.get("renewer").and_then(|x| x.as_str()).map(str::to_string);
                    mutate(|s| {
                        s.rt.renewer = current;
                        s.rt.renewer_known = true;
                        s.rt.grace_until = None;
                    });
                }
            }
            _ => {
                // 网络问题：保持「不知道」，下一轮同步会再问；不要在这里谎报已恢复
                mutate(|s| s.rt.grace_until = None);
            }
        }
    });

    Ok(status())
}

/// 解绑：**只摘掉本地 uuid，云端那一池不动**。
///
/// 同时把本机与云端重复的账号（token）从本地移除 —— 凭证已托管在云端，本机不再持有副本，
/// 免得解绑后本机单机续签把云端那条链轮换掉；本机独有的账号（云端没有的）原样保留。
///
/// 云端那一池还在时，必须先拿到池内容才知道哪些是重复的：拿不到（网络不可达 / 被拒绝）
/// 就返回 Err 且**保留本地绑定** —— 静默只摘 uuid，会让本机继续持有一批与云端相同的凭证，
/// 而用户以为自己已经解绑了。`gone`（池已不存在）例外：云端都没了，本机没有「与云端
/// 相同」的东西，全部保留、照常解绑。
pub async fn unbind() -> Result<BrokerStatus, String> {
    let Some(uuid) = uuid() else {
        return Ok(status());
    };
    let cloud: Vec<PoolItem> = match request(reqwest::Method::GET, &format!("/v1/pool/{uuid}"), None).await {
        Ok(v) => v
            .get("items")
            .cloned()
            .and_then(|x| serde_json::from_value(x).ok())
            .unwrap_or_default(),
        // 池早就不在了：本机没有「与云端相同」的凭证，全部保留、照常解绑
        Err((FailKind::Gone, _)) => Vec::new(),
        Err((_, m)) => {
            return Err(format!("没拿到云端那一池，本机绑定保持不变：{m}"));
        }
    };
    let dir = data_dir().ok_or_else(|| "凭证池状态未初始化".to_string())?;

    let cloud_keys: HashSet<String> = cloud
        .iter()
        .map(|i| i.key.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    let mut accounts = accounts::load_accounts(&dir);
    let before = accounts.len();
    accounts.retain(|a| !cloud_keys.contains(item_key_of(a).trim()));
    if accounts.len() != before {
        // 落盘失败就整体拒绝：摘了 uuid 但账号没删掉，等于静默失败，让用户以为已经解绑了
        accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    }

    // 解绑这个动作**只有在落盘之后才算完成**：内存里摘掉 uuid 只影响本次运行，
    // 磁盘上仍留着它的话，重启会重新绑上同一池，并把刚删掉的云端副本再收养回来 ——
    // 用户看到的症状是「解绑没生效」，而界面上那一刻确实显示了解绑。
    set_uuid(None).map_err(|e| {
        crate::logs::push(
            &dir,
            crate::logs::CAT_SYSTEM,
            "系统",
            false,
            format!("凭证池解绑落盘失败：{e}"),
        );
        e
    })?;
    mutate(|s| {
        s.rt = Runtime::default();
    });
    Ok(status())
}

/// 一轮完整同步（三步握手）。**未绑定、被节流、抢不到闸都是正常返回**，不是错误。
pub async fn sync(dir: &Path, force: bool) -> Result<SyncReport, String> {
    let Some(uuid) = uuid() else {
        return Ok(SyncReport {
            changed: false,
            deferred: true,
            merged: 0,
            refreshed: 0,
            failed: 0,
            blocked: 0,
            pruned: 0,
            deferred_cloud: 0,
            version: None,
            message: "本机未绑定凭证池".to_string(),
        });
    };

    let now = chrono::Utc::now().timestamp_millis();
    let (_, rt) = snapshot();
    if !force && now - rt.last_sync_ms < SYNC_TTL_MS {
        return Ok(SyncReport {
            changed: false,
            deferred: true,
            merged: 0,
            refreshed: 0,
            failed: 0,
            blocked: 0,
            pruned: 0,
            deferred_cloud: 0,
            version: rt.version,
            message: "距上次同步不到两分钟，这次跳过".to_string(),
        });
    }

    // 单飞闸放在节流**之后**：被节流挡掉的那次根本不该去抢锁。
    let _gate = if force {
        SYNC_GATE.lock().await
    } else {
        match SYNC_GATE.try_lock() {
            Ok(g) => g,
            // 另一轮正在跑：自动路径直接跳过（返回值仍是「正常跳过」，不是错误 ——
            // 它挂在签到 / 刷新的主流程上，不该因为「别人在同步」把主流程染红）。
            Err(_) => {
                return Ok(SyncReport {
                    changed: false,
                    deferred: true,
                    merged: 0,
                    refreshed: 0,
                    failed: 0,
                    blocked: 0,
                    pruned: 0,
                    deferred_cloud: 0,
                    version: rt.version,
                    message: "另一轮整池同步正在进行，本次跳过".to_string(),
                });
            }
        }
    };

    note_start();
    let lease_value = match request(
        reqwest::Method::POST,
        &format!("/v1/pool/{uuid}/lease"),
        Some(serde_json::json!({})),
    )
    .await
    {
        Ok(v) => v,
        Err((kind, m)) => {
            let msg = on_failure(kind, m);
            note_error(msg.clone());
            return Err(msg);
        }
    };
    // 老服务端没有 `renewer` 字段：此时不要把「没问过」误当成「没有续签设备」
    let renewer_present = lease_value.get("renewer").is_some();
    let lease: LeaseResp = serde_json::from_value(lease_value)
        .map_err(|e| format!("凭证管家的抢闸结果看不懂：{e}"))?;
    // 老池升级迁移：本机第一次见到这套机制、而池上还没有续签设备时，主动认领一次。
    let mut renewer_now = lease.renewer.clone();
    if renewer_present && !renewer_seen() {
        renewer_now = claim_orphan_renewer(dir, renewer_now).await;
        if renewer_now.is_some() {
            set_renewer_seen(true);
        }
    }
    if renewer_present {
        let current = renewer_now.clone();
        mutate(|s| {
            s.rt.renewer = current;
            s.rt.renewer_known = true;
        });
    }

    if !lease.granted {
        // 别的机器正在签。**不更新本地凭证**，但记下「问过了」，
        // 否则热路径每一跳都会再打一次接口。
        note_touched();
        // 「冷却中」比「闸在别人手里」更需要说清楚：前者意味着整池都在等，
        // 后者下一秒可能就好了
        let cooldown_s = (lease.retry_after - chrono::Utc::now().timestamp_millis()) / 1000;
        let message = if cooldown_s > 0 {
            format!("整池在冷静期（还有 {cooldown_s}s），本次跳过")
        } else {
            let owner = lease.lease_owner.unwrap_or_else(|| "另一台机器".to_string());
            format!("{owner} 正在续签，本次跳过")
        };
        return Ok(SyncReport {
            changed: false,
            deferred: true,
            merged: 0,
            refreshed: 0,
            failed: 0,
            blocked: 0,
            pruned: 0,
            deferred_cloud: 0,
            version: rt.version,
            message,
        });
    }

    let version = lease.version;

    // ① 池并进本地（并集，本地独有的保留）。
    //
    //    顺序是有意的：**先按旧标记删「云端已删」，再合并**。合并只会新增/更新池里还在的
    //    账号，被删的那条不会因为合并又回来。
    //
    //    「本机删掉、还没让云端知道」的账号（`pending`）不参与合并，也不进提交正文：
    //    提交是整池替换，本轮没出现的 key 就是服务端要记的墓碑。少了这一步，
    //    下一轮合并会把它从池里加回来 —— 用户看到「删了又回来」。
    let mut accounts = accounts::load_accounts(dir);
    let current_cloud = cloud_ids();
    let already_seeded = origin_seeded();
    let mut cloud: HashSet<String> = current_cloud.iter().cloned().collect();
    let pending = pending_deletions();
    let reviving = pending_revives();
    let live_items: Vec<PoolItem> = lease
        .items
        .iter()
        .filter(|i| !pending.contains(i.key.trim()))
        .cloned()
        .collect();
    let mut pruned_names =
        prune_cloud_deleted(&mut accounts, &cloud, &lease.deleted, &live_items, &reviving);
    pruned_names.extend(prune_cloud_gone(
        &mut accounts,
        &cloud,
        &live_items,
        &reviving,
    ));
    // 合并前先记下已有的 id：合并后多出来的才是「从池里收养的」，要登记成云端。
    // **不能把「池里出现的所有账号」都登记** —— 本机登录后上传的账号也在池里，
    // 那样它们会丢掉「本地登录」身份，绑池后就不再无条件续签了。
    let before_merge: HashSet<String> = accounts.iter().map(|a| a.id.clone()).collect();
    let merged = merge_into(&mut accounts, &live_items, &cloud, now);
    if already_seeded {
        note_adopted(&accounts, &before_merge, &mut cloud);
    } else {
        // 升级 / 首次绑定：池里已有的全部登记一遍（兼容老池），之后只认新收养的。
        note_cloud_members(&accounts, &live_items, &mut cloud);
    }
    // 本机已经删掉的账号不留在标记里（手动删除 / 剪枝都会留下悬空 id）。
    cloud.retain(|id| accounts.iter().any(|a| &a.id == id));
    let mut cloud_list: Vec<String> = cloud.iter().cloned().collect();
    cloud_list.sort();
    {
        let mut current = current_cloud.clone();
        current.sort();
        if cloud_list != current || !already_seeded {
            set_origin_state(cloud_list, true);
        }
    }
    if !pruned_names.is_empty() {
        crate::logs::push(
            dir,
            crate::logs::CAT_SYSTEM,
            "凭证池",
            true,
            format!(
                "云端凭证池里已经没有这些账号，本机按来源标记一并删除：{}",
                pruned_names.join("、")
            ),
        );
    }

    // ② 本机执行需要做的续签。**只用抢闸带回来的那一份**做判断：
    //    本地那份可能早就被别的机器换掉了。
    //
    //    提交的初值取「云端 ∪ 本机」：`PUT` 是**整池替换**，若从 `lease.items` 起步，
    //    本机独有的账号就永远进不了正文，等于每同步一次都把并集缩回云端那份。
    let mut pool = union_pool(&live_items, &accounts, &cloud, now);
    let mut refreshed = 0usize;
    let mut failed = 0usize;
    let mut blocked = 0usize;
    let mut deferred_cloud = 0usize;
    // 只有本机是池上那台续签设备时，才碰**云端同步账号**。
    // 本机登录账号不受这条约束（它们是这台机器自己登进来的）；池上没有续签设备
    // （renewer = null）时云端账号一律不动 —— 这正是换设备时旧设备清空后的状态。
    let me = self_id();
    let may_renew_cloud = renewer_now.as_deref() == Some(me.as_str());
    // 告警用的明细：绑池时整池由这一轮签，本地逐账号那条路是被取消的 ——
    // 「谁签不动」只有这里看得见，别把它留给只有翻文件才会看到的事件。
    let mut dead_lines: Vec<String> = Vec::new();
    let mut fail_lines: Vec<String> = Vec::new();
    for acct in accounts.iter_mut() {
        // 「该续」= 进了续签窗口，**或已被上游判死**（吊销票的到期时间还在未来，
        // 按窗口判断它永远轮不到续，而续签是它唯一的自救通道）。
        // 冷却中的账号直接算 blocked：退避已随账号落盘，重启也不会失忆。
        if acct.refresh_token.is_none()
            || (!crate::refresh::refresh_due(acct.expires_at, &acct.token, now)
                && acct.invalidated_at.is_none())
        {
            continue;
        }
        if renew_class(acct, &cloud) == RenewClass::Cloud && !may_renew_cloud {
            deferred_cloud += 1;
            continue;
        }
        if crate::refresh::renew_blocked(acct, now) {
            blocked += 1;
            continue;
        }
        match crate::commands::refresh_account_in_place(acct).await {
            Ok(()) => {
                refreshed += 1;
                // 把新的凭证写回池里那一份（保持并集：池里别的条目原样带回去）
                if let Some(slot) = pool.iter_mut().find(|i| i.key.trim() == item_key_of(acct)) {
                    *slot = to_item(acct);
                } else {
                    pool.push(to_item(acct));
                }
            }
            // 这条链本机续不动了（票已失效 / 需要重登）。它**不该拖停整池**：
            // 重试一千次也是同一句答复，而冷却是按整池算的 —— 一个等着重登的账号
            // 会把别的还能救的账号一起挡在续签窗口外，最后表现成「一池集体过期」。
            Err(e) if e.dead => {
                blocked += 1;
                dead_lines.push(format!("{}（{e}）", acct.name));
                crate::logs::push(
                    dir,
                    crate::logs::CAT_RENEW,
                    &acct.name,
                    false,
                    format!("这条链已死，跳过它（不拖停整池）：{e}"),
                );
            }
            Err(e) => {
                failed += 1;
                fail_lines.push(format!("{}（{e}）", acct.name));
                crate::logs::push(
                    dir,
                    crate::logs::CAT_RENEW,
                    &acct.name,
                    false,
                    format!("整池续签失败：{e}"),
                );
            }
        }
    }
    // 冷却 key 与非绑池路径（`scheduler::maybe_auto_refresh`）共用，两条路同时走到也不推两遍。
    if !dead_lines.is_empty() {
        crate::alerts::notify(
            dir,
            "renew-dead",
            crate::alerts::DEAD_COOLDOWN_MS,
            &crate::alerts::batch_message(
                &format!(
                    "自动续签：{} 个账号的链已死，只有重新登录能救",
                    dead_lines.len()
                ),
                &dead_lines,
                "这些账号的票一到期，接管就会一路 401。重新登录对应账号即可恢复自动续签。",
            ),
        );
    }
    if !fail_lines.is_empty() {
        crate::alerts::notify(
            dir,
            "renew-fail",
            crate::alerts::COOLDOWN_MS,
            &crate::alerts::batch_message(
                &format!("整池续签失败：{} 个账号本轮没续上", fail_lines.len()),
                &fail_lines,
                "临时失败下一轮会再试；反复签不动会被判成死链并单独告警。",
            ),
        );
    }
    accounts::save_accounts(dir, &accounts).map_err(|e| e.to_string())?;

    let pruned = pruned_names.len();
    let pruned_note = if pruned > 0 {
        format!("，移除 {pruned} 个云端已删账号")
    } else {
        String::new()
    };
    // 本机没续的那些云端账号要说清楚「为什么」：不是失败，是这台机器没有续签权。
    // 池上没设备时更要点名 —— 否则界面上只看得到「续签 0 个」，看起来像坏了。
    let cloud_note = if deferred_cloud == 0 {
        String::new()
    } else if let Some(current) = renewer_now.as_deref() {
        format!(
            "；{deferred_cloud} 个云端账号由 {} 续签",
            short_self_id(current)
        )
    } else {
        format!(
            "；{deferred_cloud} 个云端账号本轮没续（本池还没指定续签设备）"
        )
    };

    // ③ 有失败就 abort（记冷却，让所有机器都停手），没有才提交
    if failed > 0 {
        let note = format!("{failed} 个账号续签失败");
        let _ = request(
            reqwest::Method::POST,
            &format!("/v1/pool/{uuid}/abort"),
            Some(serde_json::json!({ "note": note })),
        )
        .await;
        note_error(format!("{note}，已让管家进入冷静期"));
        return Ok(SyncReport {
            changed: merged > 0 || refreshed > 0 || pruned > 0,
            deferred: false,
            merged,
            refreshed,
            failed,
            blocked,
            pruned,
            deferred_cloud,
            version: Some(version),
            message: format!(
                "{refreshed} 个账号续签成功、{failed} 个失败{pruned_note}{cloud_note}；已暂停整池续签一会儿{}",
                if blocked > 0 {
                    format!("（另有 {blocked} 个是票已失效、重试没用的）")
                } else {
                    String::new()
                }
            ),
        });
    }

    // ④ 本地 → 云端方向：把**本机比池里更新**的票回写进池。上面只回写「本轮续出来的」，
    //    这一步兜住「早就换过新票、但从没被传上去」的 —— 最典型：绑池期间在本机重新登录了一次
    //    （accounts.json 里已是新票，云端还停在旧票，别的机器每轮拿到的都是过期票）。
    //    判据与云端 → 本地方向对称（[`local_is_newer`]），只有严格更新才回写。
    let mut uploaded = 0usize;
    for acct in accounts.iter() {
        let key = item_key_of(acct);
        if key.trim().is_empty() {
            continue;
        }
        // 判据与云端 → 本地方向同源（[`should_upload`]）：本地账号可用就压过池里那份，
        // 云端账号只在更新时回写；死票在任何情况下都不许进池。
        let hit = pool.iter().find(|i| i.key.trim() == key);
        let write = hit.is_some_and(|item| {
            should_upload(acct, item, renew_class(acct, &cloud), now)
        });
        if write {
            if let Some(slot) = pool.iter_mut().find(|i| i.key.trim() == key) {
                *slot = to_item(acct);
            }
            uploaded += 1;
        }
    }

    // 提交整池：CAS 由服务端做（`version = ? AND lease_owner = ?`），新版本号就在响应里。
    // **不要再补一次 GET 去问版本** —— 那是热路径上白打的一趟接口。
    let next_version = match request(
        reqwest::Method::PUT,
        &format!("/v1/pool/{uuid}"),
        Some(serde_json::json!({ "version": version, "items": pool })),
    )
    .await
    {
        Ok(v) => serde_json::from_value::<VersionResp>(v)
            .ok()
            .map(|r| r.version)
            .or(Some(version + 1)),
        Err((FailKind::Gone, _)) => return Err(on_failure(FailKind::Gone, String::new())),
        Err((kind, m)) => {
            // 提交失败不改本地：本机的续签结果已经落盘了，下一次同步再推上去。
            // 服务端那份还是旧版本 → 下次抢闸拿到它，接着重走一遍。
            note_error(format!("整池提交失败：{m}"));
            return Err(match kind {
                FailKind::Unreachable => format!("整池提交失败（本机凭证已更新）：{m}"),
                _ => m,
            });
        }
    };
    note_ok(next_version);
    // 刷新「已删除」快照：服务端墓碑 + 本轮从池里消失的条目（待删除 / 别处删的）。
    // 之后按「是否还有活条目对应」清一遍 —— 手机号漂移留下的旧 key 不算删除。
    store_tombstones(
        tombstones_after_commit(&lease.deleted, &lease.items, &pool, now),
        &pool,
    );
    // 提交成功 = 云端已经按「本轮没出现」记好墓碑，也收到了恢复。
    // 提交失败 / abort 的路径都在这之前 return，三份待办会留到下一轮再试。
    clear_pending_changes();

    let upload_note = if uploaded > 0 {
        format!("，回写 {uploaded} 个更新的本地票")
    } else {
        String::new()
    };
    Ok(SyncReport {
        changed: merged > 0 || refreshed > 0 || uploaded > 0 || pruned > 0,
        deferred: false,
        merged,
        refreshed,
        failed: 0,
        blocked,
        pruned,
        deferred_cloud,
        version: next_version,
        message: if blocked > 0 {
            format!("整池已同步（并入 {merged} 个，续签 {refreshed} 个{upload_note}{pruned_note}{cloud_note}；{blocked} 个已死掉、需要重新登录）")
        } else {
            format!("整池已同步（并入 {merged} 个，续签 {refreshed} 个{upload_note}{pruned_note}{cloud_note}）")
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, phone: Option<&str>, token: &str) -> Account {
        Account {
            id: format!("id-{name}"),
            name: name.into(),
            phone: phone.map(str::to_string),
            token: token.into(),
            refresh_token: None,
            expires_at: None,
            rt_expires_at: None,
            base_url: None,
            created_at: String::new(),
            last: None,
            checked_today: None,
            invalidated_at: None,
            renew_blocked_until: None,
        }
    }

    fn item(key: &str, token: &str, expires: Option<i64>) -> PoolItem {
        PoolItem {
            key: key.into(),
            name: key.into(),
            phone: key.into(),
            access_token: token.into(),
            refresh_token: format!("rt-{token}"),
            expires_at: expires,
            rt_expires_at: expires.map(|e| e + 1),
            updated_at: Some(1),
            deleted_at: None,
        }
    }

    // ── 身份锚点 ────────────────────────────────────────────────────────

    #[test]
    fn key_prefers_phone_then_name_then_local_id() {
        assert_eq!(item_key_of(&acct("n", Some("138"), "t")), "138");
        assert_eq!(item_key_of(&acct("n", None, "t")), "n");
        assert_eq!(item_key_of(&acct("", None, "t")), "id-");
    }

    #[test]
    fn blank_phone_falls_through_to_name() {
        assert_eq!(item_key_of(&acct("n", Some("   "), "t")), "n");
    }

    // ── 采纳规则 ────────────────────────────────────────────────────────

    #[test]
    fn adopts_the_newer_rotation() {
        let mut a = acct("n", Some("138"), "old");
        a.expires_at = Some(1_000);
        assert!(adopt(&mut a, &item("138", "new", Some(2_000)), RenewClass::Cloud, 0));
        assert_eq!(a.token, "new");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(a.expires_at, Some(2_000));
    }

    // ── 失效抢救（2026-10-08「本地死票锁死」事故的主修）─────────────────

    /// 本机已被官方判死（invalidated_at）+ 池里那份与本地不同 → **无条件换血**，
    /// 不再比 expires_at：死票的到期时间声明得更晚时，常规判据会让它永远挡住池里的活票。
    #[test]
    fn a_marked_dead_local_token_is_rescued_from_the_pool() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.invalidated_at = Some(5_000);
        // 池里那张 expires_at 更早（别处更早一轮的活票）—— exp 判据会拒绝，活性规则必须放行
        let it = item("138", "pool-live", Some(1_000));
        assert!(
            adopt(&mut a, &it, RenewClass::Local, 0),
            "失效抢救不受 expires_at 判据约束"
        );
        assert_eq!(a.token, "pool-live");
        // 换血成功后活性标记清除
        assert_eq!(a.invalidated_at, None);
        assert_eq!(a.renew_blocked_until, None);
    }

    /// 池里若是**同一张**死票（token 与 refresh token 都相同），换血无意义：
    /// 常规判据（expires_at 不更晚）也过不了 → 不采纳，照旧走续签自救。
    #[test]
    fn the_same_dead_token_in_the_pool_is_not_adopted() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.invalidated_at = Some(5_000);
        a.refresh_token = Some("rt-dead-local".into());
        a.expires_at = Some(2_000);
        a.rt_expires_at = Some(2_001);
        assert!(!adopt(
            &mut a,
            &item("138", "dead-local", Some(1_000)),
            RenewClass::Local,
            0
        ));
        assert_eq!(a.invalidated_at, Some(5_000), "没换血就不清标记");
    }

    /// 已被判死的本地票**永不回写云端**（sync ④ 的守卫 [`write_back_allowed`]）：
    /// expires_at 更晚的死票一旦回写，会把活票盖掉、污染所有绑池机器。
    #[test]
    fn local_is_newer_refuses_to_overwrite_the_pool_with_a_dead_token() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.invalidated_at = Some(5_000);
        a.expires_at = Some(2_000);
        let it = item("138", "pool-live", Some(1_000));
        // 没有守卫时，expires_at 更晚的死票会被判成「本地更新」——守卫是必需的
        assert!(local_is_newer(&a, &it));
        assert!(!write_back_allowed(&a), "判死的票不许参与本地→云端回写");
        a.invalidated_at = None;
        assert!(write_back_allowed(&a));
    }

    #[test]
    fn never_lets_an_older_copy_win() {
        // 这正是「点刷新积分反而退回去了」那一类症状在凭证上的翻版：
        // 库里那条是更旧的一次轮换，采纳它等于把刚换到的凭证作废。
        // 本地字段都齐（没有「空」可补），所以这一次必须原样返回 false。
        let mut a = acct("n", Some("138"), "fresh");
        a.expires_at = Some(9_000);
        a.rt_expires_at = Some(9_001);
        a.refresh_token = Some("rt-fresh".into());
        let stale = item("138", "stale", Some(1_000));
        assert!(!adopt(&mut a, &stale, RenewClass::Local, 0));
        assert_eq!(a.token, "fresh", "更旧的那次轮换绝不能覆盖");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-fresh"));
        assert_eq!(a.expires_at, Some(9_000));
        assert_eq!(a.rt_expires_at, Some(9_001));
    }

    /// 同源之间（云端账号 ↔ 池里那份）：「本地读不出到期」仍按「更旧」处理，
    /// 于是池里那份读得出到期的票算新 —— 不这么判，另一台机器换到的新链永远收不进来。
    #[test]
    fn unknown_local_expiry_is_treated_as_older_within_the_cloud_class() {
        let mut a = acct("n", Some("138"), "local");
        assert!(adopt(&mut a, &item("138", "cloud", Some(1)), RenewClass::Cloud, 0));
        assert_eq!(a.token, "cloud");
    }

    #[test]
    fn copies_only_the_gaps_when_not_newer() {
        let mut a = acct("n", None, "local");
        a.expires_at = Some(9_000);
        assert!(
            adopt(&mut a, &item("138", "older", Some(1_000)), RenewClass::Local, 0),
            "补空字段也算改动"
        );
        assert_eq!(a.token, "local", "凭证不动");
        assert_eq!(a.phone.as_deref(), Some("138"), "本地缺的手机号补上");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-older"), "本地没有 RT 时补上");
        assert_eq!(a.rt_expires_at, Some(1_001));
    }

    // ── 同账号合并优先级：本地 > 云端 > 导入（assistant 没有导入类）────────

    /// 本地登录的那份**可用**时优先级最高：池里那一条就算签发得更晚也不许覆盖。
    #[test]
    fn a_newer_cloud_rotation_still_does_not_overwrite_a_usable_local_account() {
        let local = jwt_claims("s", 100);
        let cloud = jwt_claims("s", 900); // 池里那张更「新」
        let mut a = acct("n", Some("138"), &local);
        a.refresh_token = Some("rt-local".into());
        a.expires_at = Some(2_000_000);
        a.rt_expires_at = Some(3_000_000);
        let it = item("138", &cloud, Some(4_000_000));
        assert!(
            !adopt(&mut a, &it, RenewClass::Local, 0),
            "本地可用时，云端那份不许盖掉它（只允许补空字段，这里没有空可补）"
        );
        assert_eq!(a.token, local);
        assert_eq!(a.refresh_token.as_deref(), Some("rt-local"));
    }

    /// 本地这份**不可用**（续签链已过期）⇒ 来源优先级让位，可用就无条件换血。
    #[test]
    fn an_unusable_local_ticket_is_overwritten_by_any_usable_copy() {
        let mut a = acct("n", Some("138"), "dead-local");
        a.refresh_token = Some("rt-dead".into());
        a.expires_at = Some(1_000);
        a.rt_expires_at = Some(1_000); // 1e6 ms，早已过期
        // 池里那份同样很旧，但按现在的时钟仍然可用 —— 不可用的本机票没资格挑三拣四
        let it = item("138", "pool-live", Some(9_000));
        assert!(adopt(&mut a, &it, RenewClass::Cloud, 2_000_000));
        assert_eq!(a.token, "pool-live");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-pool-live"));
    }

    /// 反过来也成立：池里那份自己不可用时，绝不能拿它盖掉一条可用的本地票。
    #[test]
    fn an_unusable_pool_copy_never_replaces_a_usable_local_account() {
        let mut a = acct("n", Some("138"), "live-local");
        a.refresh_token = Some("rt-live".into());
        a.expires_at = Some(9_000);
        a.rt_expires_at = Some(9_000);
        let dead = item("138", "pool-dead", Some(1_000)); // rt 1e6 ms，已过期
        assert!(!adopt(&mut a, &dead, RenewClass::Cloud, 2_000_000));
        assert_eq!(a.token, "live-local");
    }


    /// 失效条目**不接管**：池里那份已过期 / 续签链已过期时，连建新账号都跳过。
    #[test]
    fn a_dead_pool_item_is_never_taken_over_as_a_new_account() {
        let mut accs: Vec<Account> = Vec::new();
        let dead = item("138", "pool-dead", Some(1)); // rt 2e3 ms，早已过期
        assert_eq!(merge_into(&mut accs, &[dead], &HashSet::new(), 2_000_000), 0);
        assert!(accs.is_empty(), "接进来只会多一条签不动的账号");
    }

    /// 本机没有匹配账号时，失效条目也不许「补」任何字段（手机号、到期时间、RT 都不行）。
    #[test]
    fn nothing_is_filled_in_from_a_dead_pool_copy() {
        let mut a = acct("n", None, "live");
        a.refresh_token = Some("rt-live".into());
        a.expires_at = Some(9_000);
        a.rt_expires_at = Some(9_000);
        let dead = item("138", "pool-dead", Some(1));
        assert!(!adopt(&mut a, &dead, RenewClass::Cloud, 2_000_000));
        assert_eq!(a.phone, None, "失效条目不许补手机号");
        assert_eq!(a.token, "live");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-live"));
    }

    #[test]
    fn blank_tokens_are_ignored_entirely() {
        let mut a = acct("n", Some("138"), "keep");
        let mut empty = item("138", "   ", Some(99_999));
        empty.refresh_token = String::new();
        assert!(!adopt(&mut a, &empty, RenewClass::Local, 0));
        assert_eq!(a.token, "keep");
    }

    #[test]
    fn two_unknown_expiries_do_not_count_as_a_change() {
        // 本地字段都齐、两侧过期时间又都「不知道」时，一个字段都不该动：
        // 把 `None` 赋成 `None` 报成改动，会让调用方平白多落一次盘、
        // 同步提示里也多报一个「并入 N 个」。
        let mut a = acct("n", Some("138"), "keep");
        a.refresh_token = Some("rt-keep".into());
        // item 的 `expires_at` 为 None 时，`rt_expires_at` 也是 None（见本模块的 `item`）
        let blank = item("138", "older", None);
        assert!(!adopt(&mut a, &blank, RenewClass::Local, 0));
        assert_eq!(a.token, "keep");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-keep"));
    }

    /// 两侧字段都是 null、票却读得出到期时，**必须**比出个新旧。
    ///
    /// 以前云端那条 `expires_at: null` 一律判成「不更新」，本机就永远收养不到别的机器
    /// 换到的新链；而上传的那一份又带着 null 下去，下一台机器同样读不出到期。
    #[test]
    fn null_expiry_rows_are_still_compared_by_the_ticket() {
        let jwt = signed_jwt_with_exp(1_800_000_000);
        let mut a = acct("n", Some("138"), &jwt);
        a.refresh_token = Some("rt-local".into());
        let mut cloud = item("138", &signed_jwt_with_exp(1_800_003_600), None);
        cloud.expires_at = None;
        assert!(
            adopt(&mut a, &cloud, RenewClass::Cloud, 0),
            "两边字段都空，也要按票里的 exp 判新旧"
        );
        assert!(a.token.starts_with("eyJ"), "采纳的是云端那张票");
        // 采纳之后本地必须有一个**能拿来判下次续签**的到期时间，不能还是 null
        assert_eq!(a.expires_at, Some(1_800_003_600_000));
        // 上传时同理：票读得出到期就不该交 null 上去
        let mut blank = acct("m", Some("139"), &signed_jwt_with_exp(1_800_007_200));
        blank.expires_at = None;
        assert_eq!(to_item(&blank).expires_at, Some(1_800_007_200_000));
    }

    /// 拼一张本项目真实形态的票（三段式 JWT，载荷里 `exp` 是**秒**）。
    fn signed_jwt_with_exp(exp: i64) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let payload = format!(r#"{{"iss":"https://www.workbuddy.cn","exp":{exp}}}"#);
        let b64 = STANDARD
            .encode(payload)
            .replace('+', "-")
            .replace('/', "_")
            .trim_end_matches('=')
            .to_string();
        format!("eyJhbGciOiJSUzI1NiJ9.{b64}.sig")
    }

    // ── 并集 ────────────────────────────────────────────────────────────

    #[test]
    fn merge_is_a_union_and_a_cloud_copy_never_overwrites_a_usable_local_account() {
        let mut accs = vec![acct("本地独有", Some("111"), "t1")];
        let changed = merge_into(
            &mut accs,
            &[item("111", "t1-new", Some(2_000)), item("222", "t2", Some(1))],
            &HashSet::new(),
            0,
        );
        assert_eq!(accs.len(), 2, "本地独有的一个都不许删");
        assert_eq!(changed, 2, "补空字段 + 新增一个");
        // 本地登录的那份可用 ⇒ 优先级最高，池里那个「更新」的云端副本也只能补空字段
        assert_eq!(accs[0].token, "t1");
        assert_eq!(accs[0].refresh_token.as_deref(), Some("rt-t1-new"), "缺的 RT 照补");
        assert_eq!(accs[0].expires_at, Some(2_000));
        assert_eq!(accs[1].phone.as_deref(), Some("222"));
    }

    #[test]
    fn merge_skips_items_without_a_key() {
        let mut accs = vec![acct("a", Some("111"), "t1")];
        let mut keyless = item("", "t", Some(5));
        keyless.key = String::new();
        assert_eq!(merge_into(&mut accs, &[keyless], &HashSet::new(), 0), 0);
        assert_eq!(accs.len(), 1);
    }

    #[test]
    fn merge_round_trips_through_item_and_back() {
        let original = {
            let mut a = acct("demo-user", Some("13800000000"), "at");
            a.expires_at = Some(1_760_000_000_000);
            a.rt_expires_at = Some(1_770_000_000_000);
            a.refresh_token = Some("rt".into());
            a
        };
        let as_item = to_item(&original);
        assert_eq!(as_item.key, "13800000000");
        assert_eq!(as_item.access_token, "at");

        // 另一台机器收到这一条：本地没有 → 建一个新账号，字段逐项相等
        let born = account_from_item(&as_item);
        assert_eq!(born.token, "at");
        assert_eq!(born.refresh_token.as_deref(), Some("rt"));
        assert_eq!(born.expires_at, Some(1_760_000_000_000));
        assert_eq!(born.rt_expires_at, Some(1_770_000_000_000));
        assert_eq!(item_key_of(&born), "13800000000", "两机认的是同一个 key");
        assert_eq!(born.name, "demo-user");
    }

    #[test]
    fn account_from_item_falls_back_to_a_generated_name() {
        let mut nameless = item("13800000000", "t", Some(1));
        nameless.name = String::new();
        assert_eq!(account_from_item(&nameless).name, "账号-138000");
    }

    /// 造一张只带 `sub` / `iat` 的 JWT（本地不验签，只用来判身份与新旧）。
    fn jwt_claims(sub: &str, iat: i64) -> String {
        use base64::Engine as _;
        let payload = format!(r#"{{"sub":"{sub}","iat":{iat}}}"#);
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("h.{enc}.s")
    }

    /// 回归（2026-10-24 实测）：本机那条手机号被打码 `138****0000`，池里那条是完整号码
    /// `13800000000` —— 只比 key 就并不到一起，一次导入出现**两条同名账号，一真一假**。
    /// 而且池里那张是 09-28 的死票，`exp` 反而更晚，不能被它盖掉。
    #[test]
    fn masked_phone_merges_by_subject_and_never_downgrades_the_token() {
        let fresh = jwt_claims("aaaaaaaa-0000", 1_700);
        let stale = jwt_claims("aaaaaaaa-0000", 900);
        let mut accs = vec![{
            let mut a = acct("demo-user", Some("138****0000"), &fresh);
            a.expires_at = Some(500_000);
            a
        }];
        let mut pool_item = item("13800000000", &stale, Some(900_000));
        pool_item.name = "demo-user".into();

        merge_into(&mut accs, &[pool_item], &HashSet::new(), 0);

        assert_eq!(accs.len(), 1, "同一个 sub 不该变成两条");
        assert_eq!(accs[0].token, fresh, "死票的 exp 更晚也不许盖掉活票");
    }

    #[test]
    fn newer_iat_wins_even_when_its_expiry_is_earlier() {        let mut a = acct("n", Some("138"), &jwt_claims("s", 100));
        a.expires_at = Some(900_000);
        let mut it = item("138", &jwt_claims("s", 300), Some(500_000));
        it.refresh_token = "rt-new".into();

        assert!(adopt(&mut a, &it, RenewClass::Cloud, 0));
        assert_eq!(a.token, jwt_claims("s", 300), "签发时间更晚 ⇒ 采用");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-new"));
    }

    #[test]
    fn union_pool_replaces_a_stale_cloud_entry_for_the_same_account() {
        let fresh = jwt_claims("s1", 1_700);
        let stale = jwt_claims("s1", 900);
        let cloud = vec![item("13800000000", &stale, Some(900_000))];
        let local = vec![{
            let mut a = acct("demo-user", Some("138****0000"), &fresh);
            a.expires_at = Some(500_000);
            a
        }];

        let pool = union_pool(&cloud, &local, &HashSet::new(), 0);

        assert_eq!(pool.len(), 1, "同一账号不许在池里留两份");
        assert_eq!(pool[0].access_token, fresh, "用本机的新票替换云端的死票");
    }

    #[test]
    fn account_from_item_without_phone_leaves_it_empty_instead_of_blank_string() {
        let mut no_phone = item("x", "t", Some(1));
        no_phone.phone = String::new();
        assert_eq!(account_from_item(&no_phone).phone, None);
    }

    // ── 提交正文必须带全本机账号 ────────────────────────────────────────
    //
    // 「绑定后新增的账号会自动上云」这条承诺就落在这里：`PUT` 是整池替换，
    // 正文里少一个 key，云端那一池就少一个账号。

    #[test]
    fn union_pool_carries_local_only_accounts_into_the_body() {
        let cloud = vec![item("111", "cloud", Some(10))];
        let local = vec![
            acct("交集", Some("111"), "local-a"),
            acct("本机新增", Some("222"), "local-b"),
        ];
        let pool = union_pool(&cloud, &local, &HashSet::new(), 0);
        assert_eq!(pool.len(), 2, "本机新增的账号必须进正文");
        assert_eq!(pool[1].key, "222");
        assert_eq!(pool[1].access_token, "local-b");
    }

    #[test]
    fn union_pool_keeps_the_leased_copy_for_a_cloud_account_that_is_not_newer() {
        // 云端来源的账号、本机这份又不更新 ⇒ 保留**闸带回来的**那一条：
        // 闸里那份是云端的最新轮换，本机那份可能在别处已经被换掉了。
        // 本地登录来源则相反（可用就压过去，见上一个用例）。
        let cloud = vec![item("111", "leased", Some(10))];
        let local = vec![acct("交集", Some("111"), "stale-local")];
        let cloud_ids: HashSet<String> = local.iter().map(|a| a.id.clone()).collect();
        let pool = union_pool(&cloud, &local, &cloud_ids, 0);
        assert_eq!(pool.len(), 1, "同一个 key 不许出现两条");
        assert_eq!(pool[0].access_token, "leased");
    }

    #[test]
    fn union_pool_of_an_empty_cloud_is_just_the_local_accounts() {
        let local = vec![acct("a", Some("1"), "t1"), acct("b", Some("2"), "t2")];
        assert_eq!(union_pool(&[], &local, &HashSet::new(), 0).len(), 2);
    }

    // ── 建池的唯一入口约束 ──────────────────────────────────────────────

    #[test]
    fn upload_is_refused_once_bound() {
        // 服务端建池只会新建、不会覆盖 ⇒ 放行一次就多一池、多一把闸，
        // 于是两台机器能同时续签。错误里必须说清出路（先解绑）。
        assert!(upload_guard(false).is_ok());
        let err = upload_guard(true).unwrap_err();
        assert!(err.contains("先解绑"), "错误文案要给出路：{err}");
    }

    // ── 失败分类 ────────────────────────────────────────────────────────

    #[test]
    fn classifies_the_three_failure_kinds() {
        assert_eq!(classify(200, "{}"), None);
        assert_eq!(classify(201, "{}"), None);
        assert_eq!(classify(404, r#"{"error":"gone"}"#), Some(FailKind::Gone));
        assert_eq!(classify(500, "boom"), Some(FailKind::Unreachable));
        assert_eq!(classify(502, "bad gateway"), Some(FailKind::Unreachable));
        assert_eq!(classify(400, r#"{"error":"bad_uuid"}"#), Some(FailKind::Denied));
        assert_eq!(classify(409, r#"{"error":"stale"}"#), Some(FailKind::Denied));
        // 404 但不是 gone（比如路径写错）不该被当成「池被删」
        assert_eq!(classify(404, r#"{"error":"not_found"}"#), Some(FailKind::Denied));
    }

    #[test]
    fn config_file_carries_the_binding_and_the_cloud_set() {
        let cfg = BrokerConfig {
            pool_uuid: Some("abc".into()),
            machine_id: "m-1".into(),
            cloud_ids: vec!["id-1".into()],
            origin_seeded: true,
            renewer_seen: true,
            deleted_keys: vec![],
            deleted_items: vec![],
            reviving_ids: vec![],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("pool_uuid"));
        assert!(json.contains("machine_id"));
        assert!(json.contains("cloud_ids"));
        assert!(json.contains("origin_seeded"));
        assert!(json.contains("renewer_seen"));
        assert!(json.contains("deleted_keys"));
        assert!(!json.contains("version"), "版本号是运行时状态，不该落盘");

        // 旧版本残留的未知字段要能读进来而不是整份报废；
        // 老文件缺各项必须能读成默认值（升级那一刻不能删任何账号，也不能认错续签设备）
        let restored: BrokerConfig =
            serde_json::from_str(r#"{"pool_uuid":"abc","enabled":true,"url":"x"}"#).unwrap();
        assert_eq!(restored.pool_uuid.as_deref(), Some("abc"));
        assert!(restored.cloud_ids.is_empty());
        assert!(restored.machine_id.is_empty());
        assert!(!restored.origin_seeded);
        assert!(!restored.renewer_seen);
    }

    #[test]
    fn self_id_combines_client_type_and_machine() {
        let id = format!("{}:machine-1234", CLIENT_TYPE);
        assert_eq!(short_self_id(&id), format!("{}:machine-", CLIENT_TYPE));
        assert_eq!(short_self_id("no-colon-here"), "no-colon-here");
    }

    #[test]
    fn only_cloud_accounts_are_gated_by_the_renewer() {
        let local = acct("local", Some("111"), "t");
        let cloud = acct("cloud", Some("222"), "t");
        let set: HashSet<String> = [cloud.id.clone()].into_iter().collect();
        assert_eq!(renew_class(&local, &set), RenewClass::Local, "本机登录无条件");
        assert_eq!(renew_class(&cloud, &set), RenewClass::Cloud);
    }

    /// 合并后只登记**新出现**的账号；本机登录后上传的老账号不能被改判成云端。
    #[test]
    fn adopted_accounts_are_registered_but_local_uploads_stay_local() {
        let local = acct("local", Some("111"), "t-local");
        let adopted = acct("adopted", Some("222"), "t-adopted");
        let before: HashSet<String> = [local.id.clone()].into_iter().collect();
        let mut cloud = HashSet::new();
        note_adopted(&[local, adopted], &before, &mut cloud);
        assert!(!cloud.contains("id-local"), "本机登录后上传的账号仍是本地");
        assert!(cloud.contains("id-adopted"), "合并时才出现的账号才是云端");
    }

    /// 真实接口冒烟：上传一池 → 整池同步 → 解绑（云端保留），跑完手动把测试池删干净。
    ///
    /// **这是唯一能验证「客户端与服务端的字段名、响应形状真的对得上」的手段** ——
    /// 两边各写一套类型，字段名写错时单测全绿、跑起来永远 deferred。
    /// token 用的是假的，所以不会碰到任何真实凭证，也不会去打官方续签接口。
    ///
    /// 运行：
    /// ```text
    /// cargo test --lib -- --ignored broker::tests::smoke --nocapture
    /// ```
    /// **不需要再设 `HTTPS_PROXY`**：`client()` 已经是 `no_proxy()`（理由见那里）。
    /// 若某天直连真的不通，先看返回体 —— `530 / error code 1033` 是 cloudflared 隧道
    /// 或源站那一侧的问题，换代理也救不回来。
    #[tokio::test]
    #[ignore]
    async fn smoke_real_broker_roundtrip() {
        let dir = std::env::temp_dir().join("cred-broker-smoke");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::create_dir_all(dir.join("logs")).ok();

        // 有效期放到 60 天后：`should_refresh` 会判成「不用续」，于是这一趟只验证协议，
        // 不会真的拿假 token 去打官方接口（那必然失败并让整池进冷静期）
        let far_future = chrono::Utc::now().timestamp_millis() + 60 * 24 * 3600 * 1000;
        let mut fake = acct("冒烟测试", Some("13900000000"), "smoke-at");
        fake.refresh_token = Some("smoke-rt".into());
        fake.expires_at = Some(far_future);
        fake.rt_expires_at = Some(far_future);
        accounts::save_accounts(&dir, &[fake]).expect("写测试账号");

        init(&dir);
        assert!(!bound(), "测试开始前不该是绑定的");

        let op = upload(&dir).await.expect("上传失败");
        println!("[冒烟] 上传成功：uuid={} 条数={} 说明={}", op.uuid, op.account_count, op.message);
        assert!(bound());
        assert_eq!(op.account_count, 1);

        let rep = sync(&dir, true).await.expect("整池同步失败");
        println!("[冒烟] 同步：{}（deferred={}）", rep.message, rep.deferred);
        assert!(!rep.deferred, "强制同步不该被跳过");
        assert!(rep.version.is_some(), "同步后应拿到云端版本号");

        let st = status();
        println!(
            "[冒烟] 状态：version={:?} last_ok_ms={:?} error={:?}",
            st.version, st.last_ok_ms, st.error
        );
        assert!(st.last_ok_ms.is_some(), "成功同步后必须有 last_ok_ms");
        assert!(st.error.is_none(), "成功路径不该留下错误：{:?}", st.error);

        // 再同步一轮：这次会被节流挡下（两分钟窗口），是正常的
        let throttled = sync(&dir, false).await.expect("节流路径不该报错");
        assert!(throttled.deferred, "两分钟内第二次同步应被节流");

        unbind().await.expect("解绑失败");
        assert!(!bound());
        // 解绑只摘本机绑定、云端保留：本机与云端重复的账号（这里就是那 1 个）应从本机移除
        assert_eq!(
            accounts::load_accounts(&dir).len(),
            0,
            "解绑后本机应移除与云端相同的凭证"
        );
        println!("[冒烟] 已解绑，云端那一池保留");

        // 解绑不再删云端池，这里手动清理，不留孤儿测试池
        request(reqwest::Method::DELETE, &format!("/v1/pool/{}", op.uuid), None)
            .await
            .expect("清理测试池失败");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pool_item_parses_with_every_field_missing() {
        // 服务端将来加字段 / 客户端旧版本读新池，都不该整条失败
        let parsed: PoolItem = serde_json::from_str(r#"{"key":"k"}"#).unwrap();
        assert_eq!(parsed.key, "k");
        assert_eq!(parsed.access_token, "");
        assert_eq!(parsed.expires_at, None);
    }

    #[test]
    fn cloud_view_hides_pending_deletions_and_sorts_tombstones() {
        let cache = CloudCache {
            uuid: "u".into(),
            items: vec![
                item("138", "alive", Some(1)),
                item("139", "pending", Some(1)),
            ],
            deleted: vec![
                PoolItem {
                    key: "old".into(),
                    name: "旧".into(),
                    deleted_at: Some(10),
                    ..PoolItem::default()
                },
                PoolItem {
                    key: "older".into(),
                    name: "更旧".into(),
                    deleted_at: Some(20),
                    ..PoolItem::default()
                },
            ],
            version: Some(7),
            at_ms: 123,
        };
        let pending: HashSet<String> = ["139".to_string()].into_iter().collect();
        let view = cloud_view(&cache, &pending, false, None);
        let keys: Vec<&str> = view.accounts.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["138"], "已请求删除、还没提交的不该出现在在线列表");
        let tomb: Vec<&str> = view.deleted.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(tomb, vec!["older", "old"], "墓碑按删除时间倒序");
        assert_eq!(view.version, Some(7));
    }

    #[test]
    fn cloud_snapshot_never_carries_tokens() {
        // 弹窗只需要认人 + 看有效期：整条序列化出去时不能带票
        let view = cloud_view(
            &CloudCache {
                uuid: "u".into(),
                items: vec![item("138", "a", Some(1))],
                deleted: Vec::new(),
                version: Some(1),
                at_ms: 1,
            },
            &HashSet::new(),
            false,
            None,
        );
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("access_token"));
        assert!(!json.contains("refresh_token"));
        assert!(json.contains("\"accounts\""));
    }

    // ── 本地 → 云端方向（[`local_is_newer`]，`cloud_is_newer` 的反方向）──
    //
    // 与 [`adopt`] 同一套优先级：可用的本地登录账号永远压过池里那份；云端账号只在
    // 严格更新时才回写；死票（判死 / 链过期）任何情况下都不进池。

    #[test]
    fn a_usable_local_account_always_supersedes_the_pool_copy() {
        let mut a = acct("n", Some("138"), "local");
        a.expires_at = Some(1_000);
        // 池里那份「更晚到期」也不行：本地登录是权威副本
        assert!(should_upload(
            &a,
            &item("138", "pool", Some(9_000)),
            RenewClass::Local,
            0
        ));
    }

    #[test]
    fn a_cloud_account_only_uploads_when_it_is_strictly_newer() {
        let mut a = acct("n", Some("138"), "local");
        a.expires_at = Some(1_000);
        let pool = item("138", "pool", Some(2_000));
        assert!(!should_upload(&a, &pool, RenewClass::Cloud, 0));
        a.expires_at = Some(3_000);
        assert!(should_upload(&a, &pool, RenewClass::Cloud, 0));
    }

    #[test]
    fn dead_or_unusable_local_tickets_are_never_uploaded() {
        // 被判死：无论池里那条如何都不回写
        let mut dead = acct("n", Some("138"), "dead");
        dead.invalidated_at = Some(1);
        assert!(!should_upload(&dead, &item("138", "pool", Some(1)), RenewClass::Local, 0));

        // 续签链过期 = 不可用：同样不许回写（那会把死票污染进池）
        let mut expired = acct("m", Some("139"), "expired");
        expired.refresh_token = Some("rt-expired".into());
        expired.rt_expires_at = Some(1_000);
        assert!(!should_upload(
            &expired,
            &item("139", "pool", Some(1)),
            RenewClass::Local,
            2_000_000
        ));
    }



    #[test]
    fn uploads_only_a_strictly_newer_local_ticket() {
        let mut a = acct("n", Some("138"), "new");
        a.expires_at = Some(2_000);
        assert!(local_is_newer(&a, &item("138", "old", Some(1_000))));

        // 反向：本机更旧绝不能盖云端 —— 那等于把别的机器刚续好的凭证作废，
        // 两台机器来回盖就是同步永不收敛。
        a.expires_at = Some(1_000);
        assert!(!local_is_newer(&a, &item("138", "cloud", Some(2_000))));
        // 同一到期时间也不算更新（严格大于）
        a.expires_at = Some(2_000);
        assert!(!local_is_newer(&a, &item("138", "cloud", Some(2_000))));
    }

    /// 单侧读不出到期：读得出的算新（与云端 → 本地方向同一条规则的方向版本）。
    #[test]
    fn a_readable_local_expiry_beats_a_pool_entry_with_no_expiry() {
        let mut a = acct("n", Some("138"), "local");
        a.expires_at = Some(1_000);
        assert!(local_is_newer(&a, &item("138", "cloud", None)));
        // 反过来「不知道」不等于「更旧」：本机读不出到期就不许盖云端
        a.expires_at = None;
        assert!(!local_is_newer(&a, &item("138", "cloud", None)));
    }

    /// 两个方向必须**互斥**：同一对凭证不能既「该采纳云端」又「该回写云端」，
    /// 否则两台机器每轮同步都会改一次整池版本，永远收敛不了。
    /// （用两条方向判据本身，不用 `adopt` 的返回值 —— 它还会因为「补上空字段」返回 true，
    /// 那不是覆盖，不构成收敛问题。）
    #[test]
    fn the_two_directions_never_both_claim_to_be_newer() {
        let cases: [(Account, PoolItem); 4] = [
            (acct("a", Some("1"), "opaque"), item("1", "other", None)),
            ({
                let mut a = acct("b", Some("2"), "x");
                a.expires_at = Some(1_000);
                a
            }, item("2", "y", Some(2_000))),
            ({
                let mut a = acct("c", Some("3"), "x");
                a.expires_at = Some(2_000);
                a
            }, item("3", "y", Some(1_000))),
            ({
                let mut a = acct("d", Some("4"), "x");
                a.expires_at = Some(1_000);
                a
            }, item("4", "y", Some(1_000))),
        ];
        for (a, cloud) in &cases {
            assert!(
                !(cloud_is_newer(cloud, a) && local_is_newer(a, cloud)),
                "「{}」两侧同时认定对方该被覆盖，同步会永不收敛",
                a.name
            );
        }
    }

    // ── 来源标记与「云端已删」────────────────────────────────────────

    /// 云端删掉的那条 → 本机跟着删；本地来源 / 池里还在的都不许动。
    #[test]
    fn pruning_only_removes_cloud_accounts_missing_from_the_snapshot() {
        let local = acct("local", Some("111"), "t-local");
        let still = acct("still", Some("222"), "t-still");
        let gone = acct("gone", Some("333"), "t-gone");
        let mut accounts = vec![local, still, gone.clone()];
        let cloud: HashSet<String> = ["id-still".to_string(), "id-gone".to_string()]
            .into_iter()
            .collect();

        let removed = prune_cloud_gone(
            &mut accounts,
            &cloud,
            &[item("111", "t-local", None), item("222", "t-still", None)],
            &HashSet::new(),
        );

        assert_eq!(removed, vec!["gone"]);
        assert_eq!(
            accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["id-local", "id-still"],
            "本地来源的一条都不许动"
        );
    }

    /// 本机手机号被打码、池里是完整号码（key 对不上）时，靠 JWT `sub` 仍要认出「池里还在」，
    /// 绝不能因为 key 不等就把一条活账号剪掉。
    #[test]
    fn pruning_matches_by_subject_when_the_phone_is_masked() {
        let tok = jwt_claims("bbbbbbbb-0000", 1_700);
        let masked = acct("demo", Some("138****0000"), &tok);
        let cloud: HashSet<String> = [masked.id.clone()].into_iter().collect();
        let mut accounts = vec![masked];
        let removed = prune_cloud_gone(
            &mut accounts,
            &cloud,
            &[item("13800000000", &tok, None)],
            &HashSet::new(),
        );
        assert!(removed.is_empty());
        assert_eq!(accounts.len(), 1);
    }

    /// 空快照不剪：服务端坏数据 / 坏 JSON 会被静默读成空池，不能据此删光本机账号。
    #[test]
    fn an_empty_snapshot_never_prunes() {
        let mut accounts = vec![acct("a", Some("111"), "t")];
        let cloud: HashSet<String> = ["id-a".to_string()].into_iter().collect();
        assert!(prune_cloud_gone(&mut accounts, &cloud, &[], &HashSet::new()).is_empty());
        assert_eq!(accounts.len(), 1);
    }

    /// 墓碑是**明确的删除证据**：整池 live 已经一条不剩时也要照剪
    /// （`prune_cloud_gone` 的空快照安全阀在这里不适用）。
    #[test]
    fn tombstones_prune_even_when_no_live_items_remain() {
        let gone = acct("gone", Some("333"), "t-gone");
        let local = acct("local", Some("111"), "t-local");
        let mut accounts = vec![gone, local];
        let cloud: HashSet<String> = ["id-gone".to_string()].into_iter().collect();
        let mut tomb = item("333", "t-gone", None);
        tomb.deleted_at = Some(1);

        let removed = prune_cloud_deleted(&mut accounts, &cloud, &[tomb], &[], &HashSet::new());

        assert_eq!(removed, vec!["gone"]);
        assert_eq!(
            accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["id-local"],
            "本地来源的一条都不许动"
        );
    }

    /// 墓碑就算被错传进合并 / 并集，也不能变成账号、更不能被回传云端。
    #[test]
    fn tombstones_never_become_accounts_or_get_reuploaded() {
        let mut tomb = item("333", "t-gone", None);
        tomb.deleted_at = Some(66);
        let mut accounts = vec![];
        assert_eq!(
            merge_into(&mut accounts, &[tomb.clone()], &HashSet::new(), 0),
            0
        );
        assert!(accounts.is_empty(), "墓碑不是账号");
        assert!(
            union_pool(&[tomb], &[], &HashSet::new(), 0).is_empty(),
            "墓碑不能出现在提交正文里（整池替换等于复活它）"
        );
    }

    // ── 逻辑删除 / 恢复 ────────────────────────────────────────────────

    /// 手机号漂移后旧 key 也会留下墓碑，但它和活条目指向同一个账号 ——
    /// 列出来只会诱导用户再恢复出第二条；必须过滤掉。
    #[test]
    fn tombstones_of_renamed_keys_are_dropped_when_a_live_item_matches() {
        let tok = jwt_claims("cccccccc-0000", 1_700);
        let mut old = item("111", &tok, None);
        old.deleted_at = Some(100);
        let live = item("222", &tok, None);

        let kept = merge_tombstones(vec![old.clone()], &[live.clone()]);
        assert!(kept.is_empty(), "同一个账号还有活条目，不算删除");

        // 没有活条目的墓碑照留
        assert_eq!(merge_tombstones(vec![old], &[]).len(), 1);
        assert_eq!(merge_tombstones(vec![], &[live]).len(), 0);

        // 不透明的票读不出 sub：靠「同一张票」也要认出来
        let opaque_old = item("111", "opaque-token", None);
        let opaque_live = item("222", "opaque-token", None);
        assert!(
            merge_tombstones(vec![opaque_old], &[opaque_live]).is_empty(),
            "改 key 但票没换，仍是同一个账号"
        );
    }

    /// 恢复中的账号要同时躲过两种剪枝：服务端的墓碑（`prune_cloud_deleted`）
    /// 和「本轮 lease 里没有它」（`prune_cloud_gone`）。
    #[test]
    fn reviving_accounts_survive_both_prunes() {
        let tok = jwt_claims("dddddddd-0000", 1_700);
        let restored = acct("restored", Some("333"), &tok);
        let cloud: HashSet<String> = [restored.id.clone()].into_iter().collect();
        let protected: HashSet<String> = [restored.id.clone()].into_iter().collect();
        let mut tomb = item("333", &tok, None);
        tomb.deleted_at = Some(9);
        let mut accounts = vec![restored];

        assert!(prune_cloud_deleted(&mut accounts, &cloud, &[tomb], &[], &protected).is_empty());
        assert!(prune_cloud_gone(&mut accounts, &cloud, &[], &protected).is_empty());
        assert_eq!(accounts.len(), 1);

        // 去掉保护后，同一份数据确实会被剪掉 —— 证明上面不是「规则失效」
        // （快照不能是空的：空快照有防误删安全阀，见 `an_empty_snapshot_never_prunes`）
        let other = item("999", "t-other", None);
        let mut accounts = vec![acct("restored", Some("333"), &tok)];
        assert_eq!(
            prune_cloud_gone(&mut accounts, &cloud, &[other], &HashSet::new()),
            vec!["restored"]
        );
    }

    /// 墓碑快照要按删除时间倒序、按 key 去重、并截断上限。
    #[test]
    fn tombstone_snapshot_is_sorted_deduped_and_capped() {
        let mk = |key: &str, at: i64| {
            let mut t = item(key, "t", None);
            t.deleted_at = Some(at);
            t
        };
        let mut tombs = vec![mk("a", 1), mk("b", 3), mk("a", 2)];
        let over = MAX_TOMBSTONES + 5;
        for i in 0..over {
            tombs.push(mk(&format!("x{i}"), 10 + i as i64));
        }

        let kept = merge_tombstones(tombs, &[]);

        assert_eq!(kept.len(), MAX_TOMBSTONES);
        assert_eq!(kept[0].deleted_at, Some(10 + over as i64 - 1), "最新的排最前");
        let mut keys: Vec<&str> = kept.iter().map(|t| t.key.as_str()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), kept.len(), "同一个 key 只留一条");
        assert!(!kept.iter().any(|t| t.key == "b"), "最旧的被截掉");
    }

    /// 提交后的墓碑快照：服务端已有的留着；本轮消失的补进来；
    /// 和活条目指向同一账号的旧 key（手机号 / 昵称漂移）不算删除。
    #[test]
    fn commit_snapshot_keeps_server_tombstones_and_adds_disappeared_items() {
        let tok = jwt_claims("ffffffff-0000", 1_700);
        let mut srv = item("gone-old", "t-old", None);
        srv.deleted_at = Some(5);
        let gone = item("gone", "t-gone", None);
        let renamed = item("old-key", &tok, None);
        let live = item("new-key", &tok, None);

        let kept = tombstones_after_commit(
            &[srv.clone()],
            &[gone.clone(), renamed, live.clone()],
            &[live],
            99,
        );

        assert_eq!(kept.len(), 2, "只收服务端墓碑与本轮消失的那条");
        assert_eq!(kept[0].key, "gone-old");
        assert_eq!(kept[0].deleted_at, Some(5), "服务端的删除时间原样保留");
        assert_eq!(kept[1].key, "gone");
        assert_eq!(kept[1].deleted_at, Some(99), "新墓碑记本轮时间");
    }

    /// 迁移：第一次同步只把「池里确实有」的登记为云端来源（只加不减）。
    #[test]
    fn the_first_sync_registers_pool_members_without_pruning() {
        let in_pool = acct("in", Some("111"), "t-in");
        let local_only = acct("out", Some("222"), "t-out");
        let mut cloud = HashSet::new();
        note_cloud_members(
            &[in_pool, local_only],
            &[item("111", "t-in", None)],
            &mut cloud,
        );
        assert!(cloud.contains("id-in"));
        assert!(!cloud.contains("id-out"), "池里没有的账号不能登记成云端");
    }

    /// 单飞闸的两种取法必须保持这两个语义，否则 [`sync`] 的两个分支都会退化：
    /// 自动路径拿走 `lock().await` 会把反代请求排队等在前一轮同步后面；
    /// 手动路径退回 `try_lock` 则会在「刚好有人在同步」时回一句「本次跳过」。
    #[tokio::test]
    async fn sync_gate_is_single_flight_but_manual_can_queue() {
        let held = SYNC_GATE.lock().await;
        assert!(
            SYNC_GATE.try_lock().is_err(),
            "闸被占住时 try_lock 必须失败（自动路径靠这个跳过）"
        );
        drop(held);
        assert!(
            SYNC_GATE.try_lock().is_ok(),
            "闸放掉后必须能立刻拿到（否则并发窗口就永久卡住了）"
        );
    }
}
