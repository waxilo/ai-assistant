//! 加速更新下载：多源**探测竞速** + 停顿/速度守门 + 签名自验，最后复用插件的安装链路。
//!
//! 为什么不用插件自带下载：tauri-plugin-updater 的 `download` 命令写死用 latest.json
//! 里的 GitHub 直链，且不暴露改 URL 的入口 —— 而直连 `release-assets.githubusercontent.com`
//! 往往被掐/超时。
//!
//! 为什么不是「按名单顺序挨个试」：2026-09-30 实测，镜像速度**按分钟级波动** ——
//! 同一条 URL 在 gh-proxy.com 上先后测到 3.0MB/s 与 1KB/s；ghproxy.net 常年 10–164KB/s；
//! ghfast.top 两次采样约 750KB/s。固定顺序一旦撞上慢档，就是 KB/s 爬完整个 10MB。
//! 所以选源必须**当刻实测**（`probe_race`），下载途中对「变慢/假死」要及时换源
//! （`attempt` 的守门），最后留一个不设速度下限的兜底，把更新下完。
//!
//! 事件协议与插件 `DownloadEvent` 完全同构（Started/Progress/Finished），前端渲染逻辑
//! 原样复用。唯一语义扩展：换源重下时会**再发一次 Started**，前端据此把累计量归零。
//!
//! 换源不续传（从头重下）：更新包只有 10MB 量级，换来的是**同一份字节只来自一个源**，
//! 不会把两个源的缓存拼在一起 —— 签名校验因此永远只面对「一个源是否陈旧」这一个问题。

use base64::Engine as _;
use serde::Serialize;
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::{Manager, ResourceId, Runtime, Webview};
use tauri_plugin_updater::Update;
use tokio::task::JoinSet;

/// 与插件 `commands::DownloadEvent` 完全同构的进度事件（`tag/content` + camelCase），
/// 前端现有对 `DownloadEvent` 的渲染逻辑可原样复用。插件该枚举未对外导出，故自行声明。
#[derive(Clone, Serialize)]
#[serde(tag = "event", content = "data")]
pub enum AccelEvent {
    #[serde(rename_all = "camelCase")]
    Started {
        content_length: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Progress {
        chunk_length: usize,
    },
    Finished,
}

/// 下载源：`(前缀, 展示名)`。前缀拼在原始 GitHub 直链前面；空串 = 原始直链本身。
///
/// 顺序只在两种场合说话：探测竞速的并列（毫秒级）、以及探测全军覆没后的逐个尝试。
/// 真正的选源依据是**当刻实测速度**，见 [`probe_race`]。
const SOURCES: &[(&str, &str)] = &[
    ("https://ghfast.top/", "ghfast.top"),
    ("https://gh-proxy.com/", "gh-proxy.com"),
    ("https://ghproxy.net/", "ghproxy.net"),
    ("", "GitHub 直链"),
];

/// 探测读取量：从每个源读满这么多字节，谁先读满谁赢。
/// 越小选源越快、越容易被瞬时抖动骗；128KB 在 10MB 量级的更新包面前是零头。
const PROBE_BYTES: usize = 128 * 1024;
/// 单源探测时限（各源并行，整体探测耗时 ≈ 最快源的用时）。
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
/// 建连超时：被墙/黑洞的源最多烧这么久，然后让位。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// 两块数据之间的停顿上限：超了视为「假死」，立刻换源。
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// 平均速度下限与检查间隔：均速跑不过它说明这个源已经慢到不值得等（40KB/s × 10MB ≈ 4 分钟）。
/// 只有探测出过赢家才启用 —— 探测全灭时没有测速依据，慢源也得先用着。
const MIN_AVG_SPEED: f64 = 40.0 * 1024.0;
const SPEED_CHECK_EVERY: Duration = Duration::from_secs(10);

/// 发布公钥：与 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey` 一致。
/// 两端产品共用同一发布校验 key。
const RELEASE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDc5RjM1MTUxMDgxRjJBREMKUldUY0toOElVVkh6ZVpaOWo2YnU2aWhNQUphbVgyYmdjaDM4RnE2b0crK0VtR3BDMnNCb25CRGYK";

/// 前端 `check()` 拿到新版本后调用：选源下载 → 自验签名 → 安装。
///
/// `rid` 是前端 `Update` 对象持有的资源 id；事件通过 `on_event` 逐块上报。
#[tauri::command]
pub async fn update_accelerated<R: Runtime>(
    webview: Webview<R>,
    rid: ResourceId,
    on_event: Channel<AccelEvent>,
) -> Result<(), String> {
    let update = webview
        .resources_table()
        .get::<Update>(rid)
        .map_err(|e| format!("找不到更新实例：{e}"))?;
    let update = (*update).clone();

    // 官方 latest.json 解析出的原始 GitHub 直链（最终打好的 release asset 地址）
    let original = update.download_url.to_string();

    let mut emit = |e: AccelEvent| {
        let _ = on_event.send(e);
    };
    let bytes = download_from_sources(&original, &mut emit).await?;

    verify_signature(&bytes, &update.signature, RELEASE_PUBKEY)
        .map_err(|e| format!("签名校验失败，已中止安装：{e}"))?;

    update
        .install(&bytes)
        .map_err(|e| format!("启动安装失败：{e}"))?;

    Ok(())
}

/// 选源 → 下载 → 换源 → 兜底：把「多源加速」实现成一个不惧慢源的过程。
///
/// `emit` 用回调而不是直接持有 Channel：单测可以直接收集事件（见本文件 tests）。
async fn download_from_sources(
    original: &str,
    emit: &mut impl FnMut(AccelEvent),
) -> Result<Vec<u8>, String> {
    // 镜像公网直连即可，显式关闭代理：避免环境变量里的代理把加速源也带歪。
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("初始化下载客户端失败：{e}"))?;

    let urls: Vec<String> = SOURCES
        .iter()
        .map(|(prefix, _)| clean_url(prefix, original))
        .collect();

    // 探测竞速：第一个读满 `PROBE_BYTES` 的源即本轮的「最快源」，其余立刻放弃。
    let winner = probe_race(&client, &urls).await;
    // 有赢家 ⇒ 从它开始，其余按名单序兜底，且启用速度守门；
    // 探测全灭 ⇒ 没有源跑得动（或都太慢），按名单逐个试，此时不设速度下限。
    let (order, guard): (Vec<usize>, bool) = match winner {
        Some((w, _)) => (
            std::iter::once(w)
                .chain((0..urls.len()).filter(|i| *i != w))
                .collect(),
            true,
        ),
        None => ((0..urls.len()).collect(), false),
    };

    let mut last_err: Option<String> = None;
    // (源下标, 已下字节) —— 全体失败后，用它挑「进度最多」的源做最后一次尝试
    let mut best: Option<(usize, usize)> = None;
    for idx in order {
        match attempt(&client, &urls[idx], SOURCES[idx].1, guard, emit).await {
            AttemptOutcome::Done(bytes) => return Ok(bytes),
            AttemptOutcome::Failed { got, err } => {
                if got > best.map(|(_, b)| b).unwrap_or(0) {
                    best = Some((idx, got));
                }
                last_err = Some(err);
            }
        }
    }

    // 兜底：常规尝试全军覆没。拿「进度最多」的源做最后一次**不设速度下限**的尝试 ——
    // 宁可慢，也把更新下完（停顿超时仍生效，纯假死的源照样会被放弃）。
    if let Some((idx, _)) = best {
        match attempt(&client, &urls[idx], SOURCES[idx].1, false, emit).await {
            AttemptOutcome::Done(bytes) => return Ok(bytes),
            AttemptOutcome::Failed { err, .. } => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| "所有下载源都失败了".to_string()))
}

/// 并行探测各源，返回第一个读满 `PROBE_BYTES` 的源（下标）及其 Content-Length。
///
/// 「第一个读满」≈ 当刻实际吞吐最高（含建连耗时），比任何静态排序可靠 ——
/// 实测 gh-proxy.com 同一条 URL 能在 3MB/s 与 1KB/s 之间横跳，按名单死等会吃大亏。
/// 其余任务立即 abort：已读的零头直接丢弃，重连一次的开销远小于等慢源。
async fn probe_race(client: &reqwest::Client, urls: &[String]) -> Option<(usize, Option<u64>)> {
    let mut set: JoinSet<(usize, Result<ProbeOk, String>)> = JoinSet::new();
    for (idx, url) in urls.iter().enumerate() {
        let client = client.clone();
        let url = url.clone();
        set.spawn(async move {
            match tokio::time::timeout(PROBE_DEADLINE, probe_one(&client, &url)).await {
                Ok(r) => (idx, r),
                Err(_) => (idx, Err(format!("探测超时（{}s）", PROBE_DEADLINE.as_secs()))),
            }
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((idx, Ok(probe))) = joined {
            set.abort_all();
            return Some((idx, probe.content_length));
        }
    }
    None
}

struct ProbeOk {
    /// 源声明的总长度（`Content-Length`），给 Started 事件与进度条当分母用。
    content_length: Option<u64>,
}

/// 从单个源读满 `PROBE_BYTES`（流提前结束也算）。只测速、不保留数据 ——
/// 获胜源随后会另起一条干净的请求完整下载。
async fn probe_one(client: &reqwest::Client, url: &str) -> Result<ProbeOk, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("请求失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!("返回 {}", response.status()));
    }
    let content_length = content_length_of(&response);
    let mut got = 0usize;
    while got < PROBE_BYTES {
        match response
            .chunk()
            .await
            .map_err(|e| format!("读取中断：{e}"))?
        {
            Some(chunk) => got += chunk.len(),
            None => break,
        }
    }
    Ok(ProbeOk { content_length })
}

enum AttemptOutcome {
    Done(Vec<u8>),
    /// `got` = 本轮已下字节数（供兜底选源），`err` = 展示用错误
    Failed { got: usize, err: String },
}

/// 从单个源完整拉取。
///
/// `allow_slow = false` 时启用速度守门（均速低于 `MIN_AVG_SPEED` 就放弃、换下一个源）；
/// 无论哪种模式，「两块之间停顿超时」都会放弃 —— 那是假死，不是慢。
///
/// 每轮尝试都重发 `Started`：前端收到就把累计归零，换源重下时进度条从 0 重走，
/// 而不是叠出「已下载 18 MB / 9.6 MB」。
async fn attempt(
    client: &reqwest::Client,
    url: &str,
    label: &str,
    allow_slow: bool,
    emit: &mut impl FnMut(AccelEvent),
) -> AttemptOutcome {
    let mut response = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            return AttemptOutcome::Failed {
                got: 0,
                err: format!("{label} 返回 {}（{url}）", r.status()),
            }
        }
        Err(e) => {
            return AttemptOutcome::Failed {
                got: 0,
                err: format!("{label} 请求失败：{e}（{url}）"),
            }
        }
    };

    emit(AccelEvent::Started {
        content_length: content_length_of(&response),
    });

    let started = Instant::now();
    let mut next_speed_check = started + SPEED_CHECK_EVERY;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(STALL_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                return AttemptOutcome::Failed {
                    got: buffer.len(),
                    err: format!("{label} 下载中断：{e}"),
                }
            }
            Err(_) => {
                return AttemptOutcome::Failed {
                    got: buffer.len(),
                    err: format!("{label} 停顿超过 {}s，放弃", STALL_TIMEOUT.as_secs()),
                }
            }
        };
        buffer.extend_from_slice(&chunk);
        emit(AccelEvent::Progress {
            chunk_length: chunk.len(),
        });
        if !allow_slow && Instant::now() >= next_speed_check {
            let elapsed = started.elapsed().as_secs_f64();
            if (buffer.len() as f64) / elapsed < MIN_AVG_SPEED {
                return AttemptOutcome::Failed {
                    got: buffer.len(),
                    err: format!("{label} 均速不足 {:.0}KB/s，换源", MIN_AVG_SPEED / 1024.0),
                };
            }
            next_speed_check = Instant::now() + SPEED_CHECK_EVERY;
        }
    }
    emit(AccelEvent::Finished);
    AttemptOutcome::Done(buffer)
}

fn content_length_of(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// 把镜像前缀拼到原始直链前。`prefix` 为空，或它本身指向 github 的主机，
/// 则原样返回（代表直连兜底），避免拼出 `https://github.com/https://...` 脏串。
fn clean_url(prefix: &str, original: &str) -> String {
    if prefix.is_empty() || prefix.trim_end_matches('/').ends_with("github.com") {
        return original.to_string();
    }
    // 仅当 original 是可解析的 http(s) 链接时才拼接
    if reqwest::Url::parse(original)
        .map(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or(false)
    {
        format!("{prefix}{original}")
    } else {
        original.to_string()
    }
}

/// 与 tauri-plugin-updater 完全一致的 minisign 签名校验。
fn verify_signature(data: &[u8], release_signature: &str, pub_key_str: &str) -> Result<(), String> {
    let pub_key_decoded = base64_to_string(pub_key_str)?;
    let public_key = minisign_verify::PublicKey::decode(&pub_key_decoded)
        .map_err(|e| format!("公钥非法：{e}"))?;
    let sig_decoded = base64_to_string(release_signature)?;
    let signature = minisign_verify::Signature::decode(&sig_decoded)
        .map_err(|e| format!("签名非法：{e}"))?;
    public_key
        .verify(data, &signature, true)
        .map_err(|e| e.to_string())
}

fn base64_to_string(s: &str) -> Result<String, String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败：{e}"))?;
    String::from_utf8(decoded).map_err(|e| format!("base64 不是有效 UTF-8：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_url_joins_prefix_and_original() {
        let original = "https://github.com/o/r/releases/download/t/a.tar.gz";
        assert_eq!(
            clean_url("https://ghfast.top/", original),
            format!("https://ghfast.top/{original}")
        );
        assert_eq!(clean_url("", original), original, "空前缀 = 直连");
        assert_eq!(
            clean_url("https://github.com/", original),
            original,
            "github.com 前缀 = 直连（不拼脏串）"
        );
        assert_eq!(clean_url("https://ghfast.top/", "not-a-url"), "not-a-url");
    }

    /// 真网冒烟（默认忽略，手动跑）：`cargo test --lib -- --ignored accel`
    /// 走一遍「探测竞速 → 换源下载 → 事件流」，验证完整链路真实可用。
    #[tokio::test]
    #[ignore = "需要外网：手动 rustup run stable cargo test --lib -- --ignored accel"]
    async fn smoke_downloads_latest_json_end_to_end() {
        let url =
            "https://github.com/waxilo/ai-assistant/releases/download/traework-latest/latest.json";
        let mut events: Vec<&'static str> = Vec::new();
        {
            let mut emit = |e: AccelEvent| match e {
                AccelEvent::Started { .. } => events.push("started"),
                AccelEvent::Progress { .. } => events.push("progress"),
                AccelEvent::Finished => events.push("finished"),
            };
            let bytes = download_from_sources(url, &mut emit)
                .await
                .expect("下载应成功");
            let doc: serde_json::Value =
                serde_json::from_slice(&bytes).expect("latest.json 应是合法 JSON");
            assert!(doc.get("version").is_some(), "latest.json 必须有 version");
        }
        assert_eq!(events.first().copied(), Some("started"));
        assert_eq!(events.last().copied(), Some("finished"));
    }
}
