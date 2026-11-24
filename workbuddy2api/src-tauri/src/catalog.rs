//! 上游模型清单的整理：从 `/v1/models` 里挑出**能聊天**的模型，按价格梯度排。
//!
//! 这段原先是给 Codex 写模型目录用的，Cursor 接入同样需要，所以抽成独立模块。
//! 它只做纯数据变换，不碰任何文件 —— 写不写、写到哪由调用方决定。

use serde::Serialize;

/// 一个可接入第三方客户端的模型。
///
/// 只给一个 slug 列表，用户没法判断点下去会扣多少 —— 所以倍率与免费标记要一起带出来。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChatModel {
    pub slug: String,
    pub name: String,
    /// 上游倍率原文（`x0.00` / `x0.29 credits`）；空 = 上游没给
    pub credits: String,
    /// `credits` 解析出来是 0 才叫免费；解析不出来一律算**收费**（不能猜成免费）
    pub free: bool,
}

/// 从 `/v1/models` 的 OpenAI 结构里抽出可接入第三方客户端的模型。
///
/// 顺手滤掉非对话模型：它们出现在客户端的模型选择器里，用户点了就是每次
/// 请求 400，而客户端不会告诉用户「这个模型不是用来聊天的」。
pub fn chat_models(body: &str) -> Vec<ChatModel> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(arr) = v.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ChatModel> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|x| x.as_str())?;
            if !is_chat_model(id) {
                return None;
            }
            let wb = m.get("workbuddy");
            let name = wb
                .and_then(|w| w.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or(id);
            let credits = wb
                .and_then(|w| w.get("credits"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let free = wb
                .and_then(|w| w.get("free"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            Some(ChatModel {
                slug: id.to_string(),
                name: name.to_string(),
                credits,
                free,
            })
        })
        .collect();

    // 免费 → 倍率升序 → 倍率未知。上游清单有 36 个模型、倍率从 x0.00 到 x1.00，
    // 按 id 字典序摆出来是乱的：「选模型」这个动作最需要的是**价格梯度**。
    out.sort_by(|a, b| {
        let ka = (u8::from(!a.free), rate_of(&a.credits).unwrap_or(f64::MAX));
        let kb = (u8::from(!b.free), rate_of(&b.credits).unwrap_or(f64::MAX));
        ka.0.cmp(&kb.0)
            .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    out
}

/// 倍率串 → 数值（`x0.29 credits` → 0.29）。解析不出来返回 `None`（排序时垫底）。
fn rate_of(credits: &str) -> Option<f64> {
    credits
        .split_whitespace()
        .next()?
        .trim_start_matches(['x', 'X'])
        .parse::<f64>()
        .ok()
}

/// 这个模型适不适合给对话型客户端用。
///
/// 嵌入/重排/语音/绘图模型接进去只会在每次请求时 400。上游清单没有能力字段，
/// 只能按 id 关键词排除。
pub fn is_chat_model(id: &str) -> bool {
    const NOT_CHAT: &[&str] = &[
        "embed",
        "rerank",
        "tts",
        "whisper",
        "asr",
        "dall",
        "flux",
        "stable-diffusion",
        "veo",
        "sora",
    ];
    let l = id.to_ascii_lowercase();
    !NOT_CHAT.iter().any(|k| l.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_chat_models_are_filtered_out() {
        assert!(is_chat_model("hy4flash-free"));
        assert!(is_chat_model("deepseek-v4.1-flash"));
        assert!(!is_chat_model("text-embedding-3"));
        assert!(!is_chat_model("bge-rerank-large"));
        assert!(!is_chat_model("whisper-1"));
        assert!(!is_chat_model("FLUX.1-schnell"));
    }

    #[test]
    fn reads_models_from_the_openai_list_shape() {
        let body = r#"{"object":"list","data":[
            {"id":"hy4flash-free","object":"model","created":0,"owned_by":"workbuddy",
             "workbuddy":{"name":"Hy4 flash","free":true,"credits":"x0.00"}},
            {"id":"text-embedding-3","object":"model","created":0,"owned_by":"workbuddy",
             "workbuddy":{"name":"Embedding","free":false,"credits":"x0.05"}}
        ]}"#;
        let models = chat_models(body);
        assert_eq!(models.len(), 1, "嵌入模型不能进选择器");
        assert_eq!(models[0].slug, "hy4flash-free");
        assert_eq!(models[0].name, "Hy4 flash");
    }

    #[test]
    fn broken_model_list_yields_empty_not_panic() {
        assert!(chat_models("not json").is_empty());
        assert!(chat_models(r#"{"object":"list"}"#).is_empty());
    }

    #[test]
    fn chat_models_carry_free_and_credits_for_the_picker() {
        let body = r#"{"object":"list","data":[
            {"id":"hy4flash-free","object":"model","created":0,"owned_by":"workbuddy",
             "workbuddy":{"name":"Hy4 flash","free":true,"credits":"x0.00"}},
            {"id":"glm-5.3","object":"model","created":0,"owned_by":"workbuddy",
             "workbuddy":{"name":"GLM-5.3","free":false,"credits":"x0.69 credits"}},
            {"id":"auto","object":"model","created":0,"owned_by":"workbuddy",
             "workbuddy":{"name":"Auto","free":false,"credits":""}},
            {"id":"text-embedding-3","object":"model","created":0,"owned_by":"workbuddy",
             "workbuddy":{"name":"Embedding","free":false,"credits":"x0.05"}}
        ]}"#;
        let ms = chat_models(body);
        assert_eq!(ms.len(), 3, "嵌入模型不进下拉框");
        assert_eq!(ms[0].slug, "hy4flash-free");
        assert!(ms[0].free);
        assert_eq!(ms[0].credits, "x0.00");
        assert!(!ms[1].free);
        assert_eq!(ms[1].credits, "x0.69 credits", "原文保留，界面直接显示");
        // 倍率缺失不能猜成免费
        assert!(!ms[2].free);
        assert_eq!(ms[2].credits, "");
    }

    #[test]
    fn picker_is_sorted_by_price_then_unknown_last() {
        let body = r#"{"object":"list","data":[
            {"id":"deep-model","workbuddy":{"name":"极致","free":false,"credits":"x1.00"}},
            {"id":"hy4flash-free","workbuddy":{"name":"Hy4 flash","free":true,"credits":"x0.00"}},
            {"id":"auto","workbuddy":{"name":"Auto","free":false,"credits":""}},
            {"id":"hy4flash","workbuddy":{"name":"Hy4 flash","free":false,"credits":"x0.03"}},
            {"id":"hy4std","workbuddy":{"name":"Hy4","free":false,"credits":"x0.29 credits"}}
        ]}"#;
        let parsed = chat_models(body);
        let slugs: Vec<&str> = parsed.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["hy4flash-free", "hy4flash", "hy4std", "deep-model", "auto"],
            "免费 → 倍率升序 → 未知垫底"
        );
    }
}
