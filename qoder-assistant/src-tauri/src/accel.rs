//! 加速更新下载：多镜像源尝试 + 签名自验，最后复用插件的安装链路。
//!
//! tauri-plugin-updater 的 `download` 命令被写死用 latest.json 里的 GitHub 直链
//! 下载，且不暴露改 URL 的入口 —— 而直连 `release-assets.githubusercontent.com`
//! 往往被掐/超时。所以这里自研下载：前端 `check()` 拿到 `Update` 后把它的资源 id
//! 交给本命令，本命令把原始 GitHub 直链依次拼上加速镜像前缀逐个尝试，下载成功后用
//! 发布公钥自验签名（与插件相同的 `minisign` 校验），再调用 `Update::install` 走原有安装。
//!
//! 源列表（按速度/可用性排序，见测速记录）：gh-proxy.com > ghproxy.net；GitHub 直链
//! 作为最后的兜底。每个加速源 = `前缀 + 原始GitHub直链`。

use base64::Engine as _;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{Manager, ResourceId, Runtime, Webview};
use tauri_plugin_updater::Update;

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

/// 加速镜像前缀，按实测速度/可用性排序；最后一个空串代表原始 GitHub 直链（兜底）。
const MIRROR_PREFIXES: &[&str] = &[
    "https://gh-proxy.com/",
    "https://ghproxy.net/",
    "https://github.com/",
    "",
];

/// 发布公钥：与 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey` 一致。
/// 两端产品共用同一发布校验 key。
const RELEASE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDc5RjM1MTUxMDgxRjJBREMKUldUY0toOElVVkh6ZVpaOWo2YnU2aWhNQUphbVgyYmdjaDM4RnE2b0crK0VtR3BDMnNCb25CRGYK";

/// 前端 `check()` 拿到新版本后调用：多源下载 → 自验签名 → 安装。
///
/// `rid` 是前端 `Update` 对象持有的资源 id；`on_event` 复用了与插件 `DownloadEvent`
/// 相同的结构，前端现有的进度渲染（Started / Progress / Finished）可原样复用。
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

    let bytes = download_from_mirrors(&original, &on_event).await?;

    verify_signature(&bytes, &update.signature, RELEASE_PUBKEY)
        .map_err(|e| format!("签名校验失败，已中止安装：{e}"))?;

    update
        .install(&bytes)
        .map_err(|e| format!("启动安装失败：{e}"))?;

    Ok(())
}

/// 依次尝试各镜像源，下载成功（任一源拿到完整字节）即返回。
/// 进度事件通过 `on_event` 逐块上报，前端据此渲染进度条。
async fn download_from_mirrors(
    original: &str,
    on_event: &Channel<AccelEvent>,
) -> Result<Vec<u8>, String> {
    // 镜像公网直连即可，显式关闭代理：避免环境变量里的代理把加速源也带歪。
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("初始化下载客户端失败：{e}"))?;

    let mut last_err: Option<String> = None;
    for prefix in MIRROR_PREFIXES {
        let url = clean_url(prefix, original);
        let mut response = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                last_err = Some(format!("{prefix} 返回 {}（{url}）", r.status()));
                continue;
            }
            Err(e) => {
                last_err = Some(format!("{prefix} 请求失败：{e}（{url}）"));
                continue;
            }
        };

        let content_length: Option<u64> = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());

        let mut buffer = Vec::new();
        let mut first_chunk = true;
        // 分块读取：chunk() 不需要 extra feature，进度也能如实上报
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("{prefix} 下载中断：{e}"))?
        {
            if first_chunk {
                first_chunk = false;
                let _ = on_event.send(AccelEvent::Started { content_length });
            }
            let _ = on_event.send(AccelEvent::Progress {
                chunk_length: chunk.len(),
            });
            buffer.extend_from_slice(&chunk);
        }
        let _ = on_event.send(AccelEvent::Finished);
        return Ok(buffer);
    }

    Err(last_err.unwrap_or_else(|| "所有下载源都失败了".to_string()))
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