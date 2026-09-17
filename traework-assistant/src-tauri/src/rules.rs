//! 接管规则的**热加载**配置：`<数据目录>/proxy-rules.json`。
//!
//! ## 为什么要有这个文件
//!
//! 「哪些请求该换成账号池凭据」是**靠实测收敛**的，不是靠读文档能定下来的：
//! TraeWork 换个接口名、把推理从 HTTP 挪到 WebSocket、某个路径只是长得像扣费路径，
//! 判错的代价都是**静默**的（界面上一切正常，池子账号一分不扣）。
//!
//! 把这几个旋钮放进一个**运行时读取**的 JSON，意味着调这些参数**不需要重新编译、重新签名、
//! 重新启动** —— 改完立即生效。这在真机对账时是决定性的：一轮「发消息 → 看扣了谁」只要几十秒，
//! 而重编译一轮要好几分钟，还会把「哪次改动导致了变化」这件事搅浑。
//!
//! ## 默认值是**保守**的
//!
//! 文件不存在时用 [`Rules::default`]：只换内置表里那几条**已经被验证不会弄坏应用**的前缀，
//! **不**动 WebSocket。想扩大战果就显式写进文件 —— 默认值绝不允许「开启即坏」。
//!
//! ## 观察模式
//!
//! `observe_only: true` 时**一个凭据都不换**，但仍然：走完整条隧道、记录每条请求的
//! 域名/路径/判定结果/上游状态码。这是排查「接管开着为什么没省额度」的**首选形态** ——
//! 先看清应用在和谁说什么，再决定改哪条规则。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const RULES_FILE: &str = "proxy-rules.json";

/// `swap_token_prefixes` 命中的请求，默认用哪个头承载身份。
///
/// 真机实测（2026-09-16）：Trae 的 `/api/agent/v3/*` **不带 `Authorization`**，
/// 身份在 `x-ide-token` 里（长度 1004，与本机 `accounts.json` 的 `token` 同长）。
pub const DEFAULT_TOKEN_HEADER: &str = "x-ide-token";

/// 规则文件的缓存有效期。文件很小，但**每个请求**都要问一次「这条路径要不要换号」，
/// 所以既不能每次读盘，也不能永远缓存（那就失去了热加载的意义）。
const CACHE_TTL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// 只观察、不换凭据。诊断用，也是唯一「绝对弄不坏应用」的形态。
    pub observe_only: bool,
    /// 覆盖内置扣费前缀表（空 = 用内置表）。
    pub swap_http_prefixes: Vec<String>,
    /// 是否连 WebSocket 握手里的 `Authorization` 一起换。
    ///
    /// ⚠️ 这不是可有可无的：端点是**连 `ws` 服务一起改写**的（见 `endpoint::PATCHED_SERVICES`），
    /// 所以实时通道真的会经过本机反代 —— 端点模式下它照样在管辖范围内。
    pub swap_ws: bool,
    /// **仅诊断**：把这些前缀强制判为「透传」，即使内置表命中。
    pub never_swap_prefixes: Vec<String>,
    /// **身份挂在别的头里**的路径域（空 = 不启用这条路线）。
    ///
    /// 为什么非有不可：Trae 的 agent 域（`/api/agent/v3/*`）**不带 `Authorization`** ——
    /// 身份在 [`DEFAULT_TOKEN_HEADER`] 里。只换 `Authorization` 对它等于什么都没做，
    /// 于是整条对话链一分钱都不走账号池，而界面上一切正常
    /// （2026-09-16 用户的「接管没生效、勾了账号1却扣账号2」就是这个形态）。
    ///
    /// ⚠️ **必须整域一致**：同一段身份域里「有的请求换了、有的没换」是一种更坏的失败 ——
    /// 上游看到「用 B 的凭据动 A 名下的东西」。所以宁可写宽一点（例如 `"/api/"`，
    /// 让整个应用统一用同一个账号的身份），也别只放一两条子路径进去。
    ///
    /// ⚠️ **默认留空是有意的**：换 token 这条路线到 2026-09-16 为止**还没有真机验证过**
    /// （上游到底认不认「另一个账号的 token」，只有它的回答能定论）。
    /// 默认值铁律是「绝不允许开启即坏」，所以它只能由使用者显式写进文件 —— 而文件是热加载的，
    /// 改完立即生效，验证通过之后再考虑上移成默认（`proxy.rs` 里对 401/403 有回落保护）。
    pub swap_token_prefixes: Vec<String>,
    /// `swap_token_prefixes` 命中的请求改用哪个头承载身份。
    ///
    /// 用**字段级** `default` 而不是只靠容器级的：容器级 `#[serde(default)]` 对缺失字段
    /// 取的是各字段类型自己的 `Default`（`String` = 空串），而空串在这里等价于
    /// 「把身份头删掉」—— 必须显式钉住。
    #[serde(default = "default_token_header")]
    pub token_header: String,
}

fn default_token_header() -> String {
    DEFAULT_TOKEN_HEADER.to_string()
}

impl Default for Rules {
    /// 手写而不是 `derive(Default)`：`token_header` 的默认值必须是
    /// [`DEFAULT_TOKEN_HEADER`] 而不是空串 —— 空串会让「换 token」退化成
    /// 「删掉身份头」，那是比不换糟得多的结果。
    fn default() -> Self {
        Rules {
            observe_only: false,
            swap_http_prefixes: Vec::new(),
            swap_ws: false,
            never_swap_prefixes: Vec::new(),
            swap_token_prefixes: Vec::new(),
            token_header: DEFAULT_TOKEN_HEADER.to_string(),
        }
    }
}

impl Rules {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(RULES_FILE)
    }

    /// 读规则（带 1s 缓存）。文件不存在 / 读不懂 → 用默认值，**绝不因为配置坏了就停摆**。
    pub fn load(dir: &Path) -> Rules {
        let path = Self::path(dir);
        if let Ok(map) = cache().lock() {
            if let Some((at, rules)) = map.get(&path) {
                if at.elapsed() < CACHE_TTL {
                    return rules.clone();
                }
            }
        }
        let rules = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Rules>(&text).unwrap_or_default(),
            Err(_) => Rules::default(),
        };
        if let Ok(mut map) = cache().lock() {
            // 清掉同一路径的过期项，顺手把没人再问的路径删掉 —— 键是数据目录，数量天然有限
            map.retain(|p, (at, _)| *p == path || at.elapsed() < CACHE_TTL);
            map.insert(path, (Instant::now(), rules.clone()));
        }
        rules
    }

    /// 把内置默认写成一份**带说明**的文件（幂等：已存在就不动）。
    ///
    /// 写出来的目的不是「让用户去编辑」，而是**让这些旋钮可见** ——
    /// 排查接管不生效时，第一件要确认的事就是「现在到底用的是哪张表」。
    pub fn write_default_if_absent(dir: &Path) -> Result<PathBuf, String> {
        let path = Self::path(dir);
        if path.exists() {
            return Ok(path);
        }
        let text = serde_json::to_string_pretty(&Rules::default())
            .map_err(|e| format!("序列化默认规则失败：{e}"))?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
        Ok(path)
    }

    /// 这条路径要不要换成账号池凭据。
    ///
    /// `builtin` = 代码里那张内置表 —— 文件里的 `swap_http_prefixes` 非空时**完全取代**它
    /// （而不是叠加）：测试时需要一个「干净、只由文件决定」的状态，否则永远分不清
    /// 命中的是内置表还是自己写的规则。
    pub fn should_swap(&self, path: &str, builtin: &[&str]) -> bool {
        if self.observe_only {
            return false;
        }
        if self.never_swap_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
            return false;
        }
        if self.swap_http_prefixes.is_empty() {
            return builtin.iter().any(|p| path.starts_with(p));
        }
        self.swap_http_prefixes.iter().any(|p| path.starts_with(p.as_str()))
    }

    /// 命中的「身份在 token 头里」域前缀（没命中为 `None`）。
    ///
    /// 返回**前缀本身**而不是 `bool`，是因为调用方要拿它当「这条路线到底行不行」的
    /// 进程内结论缓存的键（见 `proxy::mark_token_swap_dead`）。按完整路径记就失去意义了 ——
    /// 那样一个域里每条子路径都会各试一次 401。
    pub fn token_prefix_of(&self, path: &str) -> Option<String> {
        if self.observe_only {
            return None;
        }
        if self.never_swap_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
            return None;
        }
        self.swap_token_prefixes
            .iter()
            .find(|p| !p.is_empty() && path.starts_with(p.as_str()))
            .cloned()
    }
}

/// 规则缓存：**按路径分别缓存**。
///
/// 曾经这里只存一条（`Option<(PathBuf, …)>`）。生产环境只有一个数据目录，所以看不出问题；
/// 但并行跑的测试会各自用不同的临时目录，一条缓存会被互相顶掉 —— 表现为「规则明明写对了
/// 却时灵时不灵」的随机失败。键是数据目录，数量天然有限，改成 map 没有代价。
type Cache = Mutex<HashMap<PathBuf, (Instant, Rules)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 丢掉缓存。写入方（`takeover_save_rules`）调用它，好让界面**立刻**看到新值 ——
/// 否则会有最多 1 秒的「我刚改的规则怎么没生效」。
pub fn invalidate() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTIN: &[&str] = &["/api/remote/v1/chat_sessions", "/api/remote/v1/models"];

    #[test]
    fn default_rules_use_the_builtin_table_and_never_touch_ws() {
        let r = Rules::default();
        assert!(!r.observe_only);
        assert!(r.swap_http_prefixes.is_empty(), "默认必须用内置表");
        assert!(!r.swap_ws, "默认绝不动 WebSocket —— 那是未验证的领域");
        assert!(r.should_swap("/api/remote/v1/chat_sessions?x=1", BUILTIN));
        assert!(!r.should_swap("/api/agent/v3/llm_utils_chat", BUILTIN));
    }

    #[test]
    fn observe_only_swallows_everything() {
        let r = Rules { observe_only: true, ..Rules::default() };
        assert!(!r.should_swap("/api/remote/v1/chat_sessions", BUILTIN));
    }

    #[test]
    fn file_table_replaces_the_builtin_one() {
        let r = Rules {
            swap_http_prefixes: vec!["/api/agent/v3/".into()],
            ..Rules::default()
        };
        assert!(r.should_swap("/api/agent/v3/llm_utils_chat", BUILTIN));
        assert!(
            !r.should_swap("/api/remote/v1/chat_sessions", BUILTIN),
            "文件里的表是**取代**内置表，不是叠加"
        );
    }

    #[test]
    fn never_swap_wins_over_everything() {
        let r = Rules {
            swap_http_prefixes: vec!["/api/".into()],
            never_swap_prefixes: vec!["/api/agent/v3/sync_history_state".into()],
            ..Rules::default()
        };
        assert!(r.should_swap("/api/agent/v3/llm_utils_chat", BUILTIN));
        assert!(!r.should_swap("/api/agent/v3/sync_history_state", BUILTIN));
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("twa-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = Rules::load(&dir);
        assert_eq!(r.observe_only, Rules::default().observe_only);
        // 坏 JSON 也不能让接管停摆：解析失败同样落到默认值
        std::fs::write(Rules::path(&dir), "{ 这不是合法 JSON").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        let r2 = Rules::load(&dir);
        assert!(!r2.observe_only);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 换 `x-ide-token` 这条路线**默认必须不启用**：它到 2026-09-16 还没被真机验证过，
    /// 而 `Rules` 默认值的铁律是「绝不允许开启即坏」。
    #[test]
    fn token_swap_is_off_by_default_and_opts_in_by_file() {
        let r = Rules::default();
        assert!(r.swap_token_prefixes.is_empty(), "未经实测的路线不做默认值");
        assert_eq!(r.token_header, DEFAULT_TOKEN_HEADER);
        assert!(r.token_prefix_of("/api/agent/v3/llm_utils_chat").is_none());

        let r = Rules {
            swap_token_prefixes: vec!["/api/agent/v3/".into()],
            ..Rules::default()
        };
        assert_eq!(
            r.token_prefix_of("/api/agent/v3/llm_utils_chat?x=1").as_deref(),
            Some("/api/agent/v3/"),
            "返回的必须是**前缀**：它要拿去做「这条路行不行」的缓存键"
        );
        assert!(r.token_prefix_of("/api/remote/v1/models").is_none(), "表外的域一个都不碰");
    }

    /// 文件里没写 `token_header` 时**必须回落成默认头名**。
    ///
    /// 空串在这里不是「无所谓」：它会退化成「把身份头删掉」，比不换糟得多。
    /// 容器级 `#[serde(default)]` 对缺失字段取的是字段类型自己的 `Default`（空串），
    /// 所以这一条必须有字段级 `default` 顶着。
    #[test]
    fn token_header_falls_back_when_the_file_omits_it() {
        let r: Rules = serde_json::from_str(r#"{"swap_token_prefixes":["/api/"]}"#).unwrap();
        assert_eq!(r.token_header, DEFAULT_TOKEN_HEADER);
        assert!(r.token_prefix_of("/api/agent/v3/llm_utils_chat").is_some());
    }

    /// 两个既有的总开关对 token 域同样有效 —— 「一键关回」不能只在一条路线上管用。
    #[test]
    fn observe_only_and_never_swap_still_win_over_token_swap() {
        let r = Rules {
            swap_token_prefixes: vec!["/api/".into()],
            observe_only: true,
            ..Rules::default()
        };
        assert!(r.token_prefix_of("/api/agent/v3/llm_utils_chat").is_none());

        let r = Rules {
            swap_token_prefixes: vec!["/api/".into()],
            never_swap_prefixes: vec!["/api/agent/v3/sync_history_state".into()],
            ..Rules::default()
        };
        assert!(r.token_prefix_of("/api/agent/v3/llm_utils_chat").is_some());
        assert!(r.token_prefix_of("/api/agent/v3/sync_history_state").is_none());
    }
}
