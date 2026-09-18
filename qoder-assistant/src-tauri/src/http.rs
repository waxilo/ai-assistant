//! 出站请求的公共身份与客户端工厂。
//!
//! # 为什么要有这个模块
//!
//! 过去每个调用点各自拼 header：`reqwest::Client::new()` 打底，再手写
//! `Content-Type` / `Accept`，UA 又是另一处 `concat!`；`x-client-platform` 更是只出现在
//! 「积分资源查询」一处，签到、签到状态、令牌续签全都不带。
//!
//! 结果是**同一个程序发出了不一致的身份**。这种不一致比任何一种固定身份都显眼：
//! 一个客户端的头部集合本该稳定，忽有忽无、这里声明 `web` 那里什么都不说，
//! 恰恰是「拼装出来的客户端」的特征。
//!
//! 所以把 UA 与公共头收敛到一处：**应用是谁就说自己是谁，并且到处都一样**。
//! 需要说清楚的是，这不解决账号归属层的关联问题（那部分不是客户端能改的），
//! 它只是消灭「半吊子伪装」——要么一致地自报身份，要么被自己的不一致暴露。
//!
//! # 除了「长什么样」，「什么时候发」也归这里
//!
//! 上一轮补了签到路径的节奏，却漏了两条**用户看不见的批量循环**：拉取全部账号的
//! 积分快照、以及后台自动续签。它们都是「for 循环 + 零间隔连发」，N 个账号 × 每账号
//! 三四个请求在一条 IP 上一次性打出去——这正是脚本形态。而且这两条路径**没有对应的
//! 设置项**，指望用户去开什么开关是不现实的，所以节流做成了常量下限：不管设置怎样，
//! 批量路径都不会零间隔。

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE};
use std::time::Duration;

/// 应用自报身份：账号接口、令牌接口与上游转发共用同一个 UA。
pub const UA: &str = concat!("QoderAssistant/", env!("CARGO_PKG_VERSION"));

/// 账号类接口的默认超时：都是轻量 JSON 交互，超过这个时间按失败处理。
///
/// 它同时是 [`build_api_client`] 的**客户端级总超时**——即「从这里出来的客户端，
/// 每个请求都有上界」。调用点各自设的请求级超时更短（reqwest 里请求级覆盖客户端级），
/// 这一层只为兜住那些**没设请求级超时**的调用点。
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// 客户端语言偏好。
///
/// 与 [`UA`] 同一层级：它描述「这个客户端是谁」，与接口语义无关，所以**自建的出站请求
/// 一律带**（计费接口、插件授权接口都带）。唯独 `proxy` 的透传不带——那条路径的头由
/// CLI 自己提供，我们注入任何东西都会改变它的真实请求。
///
/// 用标准的 q 权重写法，这不是「装成浏览器」：`zh-CN` 就是使用者的真实语言环境，
/// 一个中文桌面应用声明它比不声明更诚实，q 值也是通用的 HTTP 语法而非浏览器专属。
pub const ACCEPT_LANG: &str = "zh-CN,zh;q=0.9";

/// 描述「客户端本身」的头：与接口语义无关，**任何自建出站请求都该带**。
///
/// `Accept` 之所以也归这一层：它表达的是「这个客户端想收 JSON」，是客户端自身的偏好，
/// 而不是某个接口族的规矩——`/v2/plugin/auth/*` 的响应同样是 JSON，同样该带上。
/// 单独成函数是为了让「哪些头属于客户端、哪些属于某个接口族」在结构上就分开：
/// 插件授权接口与计费接口共用这一份，两者的差异只剩 [`api_headers`] 里那一项族声明。
pub fn client_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static(ACCEPT_LANG));
    headers
}

/// 计费 / 账号接口的头集合 = [`client_headers`] + 该接口族的声明。
///
/// 单独抽成函数（而不是写死在 [`api_client`] 里）是为了能直测它的内容——
/// 这个模块存在的全部意义就是「这一组头永远一致」，它必须可断言。
///
/// - `x-client-platform: web` 是**本接口族唯一的专属声明**，沿用官方 Web 端对这些接口的
///   说法：它本来就只写在「积分资源查询」里，其余计费请求漏了；这里补齐成一致的那一份，
///   而不是删掉它去赌网关不校验。**别把它带到 `/v2/plugin/auth/*` 上**——那条族不是计费
///   接口，声明 `web` 没有依据（见 `oauth::http` 与 `refresh::refresh`）。
/// - **不放 `Content-Type`**：它由 `reqwest` 的 `.json()` 自动补，GET 请求不该带它。
/// - **故意不放 `Accept-Encoding`**：`Cargo.toml` 里 reqwest 是 `default-features = false`
///   + `["json", "rustls-tls"]`，没开 gzip/brotli，所以不发这个头。补它必须先开解压特性，
///   而 Cargo feature 是 **crate 级**的，会连带改变 `proxy` 透传客户端的行为（自动解压 +
///   抹掉 `Content-Encoding` 头），对 SSE 与二进制透传的保真度有风险。一个「有没有都不
///   引人注意」的头，不值得换这个风险。
pub fn api_headers() -> HeaderMap {
    let mut headers = client_headers();
    headers.insert("x-client-platform", HeaderValue::from_static("web"));
    headers
}

/// 账号 / 网关接口客户端的唯一构造内核。
///
/// 抽出来是为了让「身份」只有一处定义：所有出站客户端都必须从这里出，
/// 且**强制直连**（不继承 `HTTP_PROXY` / `HTTPS_PROXY`）。
fn build_api_client() -> reqwest::Client {
    // 为什么恒 `no_proxy()`：唯一的使用方是接管代理内部的主动查询（拉模型清单）。
    // 代理自己就站在网络中间层，若继承 shell 的代理变量，请求会被绕去无关代理。
    let builder = reqwest::Client::builder()
        // 客户端级总超时：**每个请求都必须有上界**。多数调用点会再设更短的请求级超时
        // （reqwest 里请求级覆盖客户端级，所以这里不会把它们的 10s/15s 放宽），但像
        // `proxy::fetch_models_value` 那种不带请求级超时的调用点，就只剩这一层兜底。
        // 它原来用的是上游透传客户端，那里有 `connect_timeout` + `read_timeout`；
        // 换成这里的客户端时若不带任何超时，一条「只建连、不返回」的连接就能把命令
        // 永久挂住（表现为接管页模型列表一直转圈）。别删这一行。
        .timeout(TIMEOUT)
        .user_agent(UA)
        .default_headers(api_headers())
        .no_proxy();
    // 构造失败只会发生在 TLS/解析器初始化不起来的时候——那时整个应用都发不出请求。
    // 兜底成裸客户端更糟：它会静默丢掉 UA 与全部身份头，正是本模块要消灭的那种
    // 「同一程序两套身份」，还会把失败原因藏起来。宁可响亮地失败（与 `proxy::CLIENT` 一致）。
    builder.build().expect("构建 HTTP 客户端失败")
}

/// 账号 / 网关接口客户端：UA + [`api_headers`]，**强制直连**。
///
/// 只给接管代理内部的两处主动查询用（拉模型清单、路由决策时重拉积分快照）——
/// 这就是它必须直连的原因，细节见 [`build_api_client`]。
///
/// **Qoder 自己的账号接口不走这里**（额度、活动权益、套餐、热力图都不走）：
/// 那一条统一归 [`crate::qoder_api`]，身份是官方的 Bx 头三件套。
/// 这个客户端留下的 `api_headers()` 里还有 CodeBuddy 时代的族声明，
/// 迁移时应整块收敛到 `qoder_api::client()`，而不是在这里再叠一层。
///
/// **别图省事写 `reqwest::Client::new()`**：裸客户端不带 UA、不带 [`api_headers`]，
/// 于是同一个接口会在不同路径上收到两套不同的头部集合——同一个程序对同一个接口忽而
/// 声明身份忽而什么都不说，比任何一种固定身份都显眼。
pub fn api_client_direct() -> reqwest::Client {
    build_api_client()
}

/// 批量循环里「换下一个账号」前的抖动区间（毫秒）。
///
/// 量级说明：这里不是模仿人的操作节奏，只是消灭「一条 IP 上瞬时连发 N 组请求」这条
/// 机器特征。脚本与真人的差别在这一层是**零间隔与非零间隔**，不需要人类级别的等待，
/// 所以是百毫秒级，而不是签到间隔的秒级——那两个常量与这个不为同一目的服务。
pub const ACCOUNT_GAP_MS: (u64, u64) = (200, 900);

/// 批量循环的账号间隔：按 [`ACCOUNT_GAP_MS`] 随机睡一觉。
///
/// 调用约定：**只在 i > 0 的迭代里调**。首个账号之前不该有等待，一是它前面本来就没有
/// 请求、不产生机器特征，二是让首条结果尽快回到界面上。
pub async fn account_gap() {
    let ms = crate::rng::range(ACCOUNT_GAP_MS.0, ACCOUNT_GAP_MS.1);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

// 这里曾经还有一对 `PROBE_GAP_MS` / `probe_gap()`：给「候选 host × 候选 path」枚举
// 之间留抖动，好让端点探测不像扫描器。签到线重写成 Qoder 的活动权益接口之后，
// **枚举本身没有了**（Qoder 只有一个域、一个接口），那对常量也就没有调用点 ——
// 一并删掉，而不是留着「以后可能用得上」。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_api_request_carries_the_same_identity() {
        let h = api_headers();
        assert_eq!(h.get(ACCEPT).unwrap(), "application/json");
        assert_eq!(h.get(ACCEPT_LANGUAGE).unwrap(), ACCEPT_LANG);
        assert_eq!(h.get("x-client-platform").unwrap(), "web");
        // Content-Type 必须留给 .json() 自己补：GET 带上它反而是异类
        assert!(h.get(reqwest::header::CONTENT_TYPE).is_none());
        // 故意不带：补它要开 crate 级解压特性，会波及 proxy 透传的保真度
        assert!(h.get(reqwest::header::ACCEPT_ENCODING).is_none());
        assert_eq!(h.len(), 3, "公共头只有这三项，别悄悄加东西：{h:?}");
    }

    #[test]
    fn client_headers_are_a_subset_of_api_headers() {
        // 两个接口族的差异必须只剩「族专属声明」那一项（x-client-platform）；
        // 客户端自身的头两个接口族都得有，否则又成了「同一程序两套身份」
        let c = client_headers();
        assert_eq!(c.get(ACCEPT).unwrap(), "application/json");
        assert_eq!(c.get(ACCEPT_LANGUAGE).unwrap(), ACCEPT_LANG);
        assert_eq!(c.len(), 2, "客户端自身头是 Accept + Accept-Language：{c:?}");
        assert!(
            c.get("x-client-platform").is_none(),
            "族声明不该出现在客户端层"
        );
        for (name, value) in c.iter() {
            assert_eq!(
                api_headers().get(name).unwrap(),
                value,
                "计费接口缺了客户端自身头 {name:?}"
            );
        }
    }

    #[test]
    fn user_agent_is_namespaced_and_versioned() {
        assert!(UA.starts_with("QoderAssistant/"), "UA 应自报应用名：{UA}");
        let version = UA.trim_start_matches("QoderAssistant/");
        assert!(!version.is_empty(), "UA 应带上版本号：{UA}");
        assert_eq!(TIMEOUT, Duration::from_secs(20));
    }

    #[test]
    fn the_direct_client_is_built_from_the_shared_identity() {
        // reqwest 不暴露「已设的默认头 / 超时」，这两样只能靠结构保证：它们都出自
        // build_api_client，`api_headers()` 与 `.timeout()` 只在那里设一次。
        // 这里确认这条路径真的构建得出来——构造失败现在会直接 panic，
        // 不再有「悄悄退化成裸客户端」的兜底把问题藏起来。
        let _ = api_client_direct();
        assert_eq!(api_headers().len(), 3, "身份头少了一项，客户端会跟着退化");
    }

    #[test]
    fn pacing_windows_are_subsecond_and_never_allow_zero_gap() {
        let (lo, hi) = ACCOUNT_GAP_MS;
        assert!(lo <= hi, "账号间隔区间写反了：{lo} > {hi}");
        // 下界为 0 就等于允许零间隔连发——那正是这个常量要消灭的形态
        assert!(lo > 0, "账号间隔下界为 0，节流形同虚设");
        // 上界必须远小于秒级：这层只负责「别零间隔」，不负责模仿人的等待时长，
        // 批量刷新被拖成几十秒就是帮倒忙
        assert!(hi < 1_000, "账号间隔上界过大，会明显拖慢批量路径：{hi}ms");
    }
}
