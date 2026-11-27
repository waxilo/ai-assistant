//! `GET /v1/models`：把上游的私有模型清单翻成 OpenAI 结构。
//!
//! 上游没有 OpenAI 的 `/v1/models`（实测 404），只有
//! `GET /v2/enterprises/personal/models`，返回 `{data:{models:[{id, name, credits}]}}`。
//! 这里做三件事：
//!
//! 1. 结构转换（`object:"list"` + `object:"model"`）；
//! 2. **保序**：免费模型（`credits` 倍率为 `x0.00`）排前面，其余按 id 排序——
//!    客户端下拉框里一眼能看见"不花积分的"；
//! 3. 1 小时缓存：这是纯展示数据，没必要每个客户端启动都打一趟上游。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::catalog::rate_of;
use crate::routing;

const CACHE_TTL: Duration = Duration::from_secs(3600);

fn cache() -> &'static Mutex<Option<(Instant, String)>> {
    static CACHE: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    &CACHE
}

/// 缓存里还有效的那份清单
pub fn cached() -> Option<String> {
    let guard = cache().lock().ok()?;
    let (at, body) = guard.as_ref()?;
    (at.elapsed() < CACHE_TTL).then(|| body.clone())
}

fn put_cache(body: String) {
    if let Ok(mut g) = cache().lock() {
        *g = Some((Instant::now(), body));
    }
}

/// 拉上游模型清单（私有结构）
pub async fn fetch(host: &str, token: &str) -> Option<serde_json::Value> {
    let resp = crate::http::api_client_direct()
        .get(crate::upstream::models_url(host))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    resp.json().await.ok()
}

/// 私有结构 → OpenAI `/v1/models` 响应体（纯函数，便于单测）
pub fn to_openai(upstream: &serde_json::Value) -> String {
    let empty = Vec::new();
    let arr = upstream
        .get("data")
        .and_then(|d| d.get("models"))
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    let mut items: Vec<(bool, String, String, String)> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(id.as_str())
                .to_string();
            let credits = m
                .get("credits")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let free = rate_of(&credits).is_some_and(|x| x == 0.0);
            Some((free, id, name, credits))
        })
        .collect();

    // 免费在前；同组内按 id 排，保证多次请求顺序稳定（客户端不会看到列表乱跳）
    items.sort_by(|a, b| match (a.0, b.0) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.1.cmp(&b.1),
    });

    let data: Vec<serde_json::Value> = items
        .into_iter()
        .map(|(free, id, name, credits)| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "workbuddy",
                // 非标准字段：不认识的客户端会忽略，认识的（我们自己的页面）能直接用
                "workbuddy": { "name": name, "free": free, "credits": credits },
            })
        })
        .collect();

    serde_json::json!({ "object": "list", "data": data }).to_string()
}

/// 拉一次并写缓存；失败时回落到旧缓存（哪怕过期，也比 5xx 好）
pub async fn refresh(dir: &std::path::Path, host: &str) -> Option<String> {
    let account = routing::choose_account(dir, &[], None).await?;
    let raw = fetch(host, &account.token).await?;
    let body = to_openai(&raw);
    put_cache(body.clone());
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游真实响应（截取，2026-11-24）
    fn upstream_fixture() -> serde_json::Value {
        serde_json::json!({
            "code": 0,
            "data": {
                "models": [
                    { "id": "deepseek-v4.1-flash", "name": "DeepSeek-V4.1-Flash", "credits": "x0.11 credits" },
                    { "id": "hy4flash-free", "name": "Hy4 flash", "credits": "x0.00" },
                    { "id": "auto", "name": "Auto", "credits": null },
                    { "id": "glm-5.3", "name": "GLM-5.3", "credits": "x0.69 credits" }
                ]
            }
        })
    }

    #[test]
    fn maps_to_openai_list_shape() {
        let v: serde_json::Value = serde_json::from_str(&to_openai(&upstream_fixture())).unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"].as_array().unwrap().len(), 4);
        assert_eq!(v["data"][0]["object"], "model");
        assert_eq!(v["data"][0]["owned_by"], "workbuddy");
    }

    #[test]
    fn free_models_come_first_and_keep_stable_order() {
        let v: serde_json::Value = serde_json::from_str(&to_openai(&upstream_fixture())).unwrap();
        assert_eq!(v["data"][0]["id"], "hy4flash-free", "免费必须排第一");
        assert_eq!(v["data"][0]["workbuddy"]["free"], true);
        // 其余按 id 字典序：auto < deepseek... < glm...
        assert_eq!(v["data"][1]["id"], "auto");
        assert_eq!(v["data"][2]["id"], "deepseek-v4.1-flash");
        assert_eq!(v["data"][3]["id"], "glm-5.3");
    }

    #[test]
    fn unknown_credits_is_not_treated_as_free() {
        let v: serde_json::Value = serde_json::from_str(&to_openai(&upstream_fixture())).unwrap();
        let auto = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "auto")
            .unwrap();
        assert_eq!(auto["workbuddy"]["free"], false, "倍率缺失不能猜成免费");
    }

    #[test]
    fn empty_upstream_yields_empty_list_not_panic() {
        let v: serde_json::Value =
            serde_json::from_str(&to_openai(&serde_json::json!({}))).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 0);
    }
}
