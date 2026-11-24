//! `POST /v1/chat/completions`：转发 + 非流式聚合。
//!
//! 上游只给流式（实测 `stream:false` 直接 400/11101），所以：
//!
//! - 下游要流式 → 逐 chunk 原样转发，零改动；
//! - 下游要非流式 → 网关内部按流式收全，再把 SSE 帧**聚合**成一个标准
//!   `chat.completion` 返回。聚合逻辑是纯函数，见 [`aggregate_sse`]。
//!
//! 为什么要自己聚合而不是让上游支持非流式：上游没有这条路（不是配置问题）。

use std::collections::BTreeMap;

/// 一个正在拼装的工具调用
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
}

/// 把上游 SSE 帧聚合成一个 `chat.completion` JSON。
///
/// 返回 `None` 表示「这不是一段可聚合的 SSE」——通常是上游直接回了错误 JSON，
/// 或者流是空的。调用方应该把原始字节原样回给客户端，让错误信息不被吃掉。
pub fn aggregate_sse(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let mut id = String::new();
    let mut model = String::new();
    let mut created = 0i64;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<serde_json::Value> = None;
    let mut tools: BTreeMap<usize, ToolAcc> = BTreeMap::new();
    let mut saw_frame = false;
    let mut saw_done = false;

    for line in text.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let data = rest.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            saw_done = true;
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        saw_frame = true;

        if id.is_empty() {
            if let Some(s) = v.get("id").and_then(|x| x.as_str()) {
                id = s.to_string();
            }
        }
        if model.is_empty() {
            if let Some(s) = v.get("model").and_then(|x| x.as_str()) {
                model = s.to_string();
            }
        }
        if created == 0 {
            if let Some(n) = v.get("created").and_then(|x| x.as_i64()) {
                created = n;
            }
        }
        // usage 只在最后一帧出现（实测），出现就覆盖
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            usage = Some(u.clone());
        }

        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            continue;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            if !fr.is_empty() {
                finish_reason = Some(fr.to_string());
            }
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(s) = delta.get("content").and_then(|x| x.as_str()) {
            content.push_str(s);
        }
        if let Some(s) = delta.get("reasoning_content").and_then(|x| x.as_str()) {
            reasoning.push_str(s);
        }
        if let Some(arr) = delta.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in arr {
                // index 缺失时按 0 处理：单工具调用是最常见形态
                let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                let slot = tools.entry(idx).or_insert_with(|| ToolAcc {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                if let Some(s) = tc.get("id").and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        slot.id = s.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(s) = f.get("name").and_then(|x| x.as_str()) {
                        if !s.is_empty() {
                            slot.name.push_str(s);
                        }
                    }
                    if let Some(s) = f.get("arguments").and_then(|x| x.as_str()) {
                        slot.arguments.push_str(s);
                    }
                }
            }
        }
    }

    // 一个数据帧都没有 → 不是 SSE（多半是上游错误 JSON），交给调用方原样转发
    if !saw_frame {
        return None;
    }
    // 有帧但流没正常结束，且内容为空：宁可当成失败，也别回一个空 200
    if !saw_done && content.is_empty() && tools.is_empty() {
        return None;
    }

    let tool_calls: Vec<serde_json::Value> = tools
        .into_iter()
        .map(|(idx, t)| {
            serde_json::json!({
                "index": idx,
                "id": t.id,
                "type": "function",
                "function": { "name": t.name, "arguments": t.arguments }
            })
        })
        .collect();

    let mut message = serde_json::json!({
        "role": "assistant",
        // 纯工具调用时 content 必须是 null，空串会让部分客户端渲染出一个空气泡
        "content": if content.is_empty() && !tool_calls.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(content)
        },
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = serde_json::Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::Value::Array(tool_calls);
    }

    let body = serde_json::json!({
        "id": if id.is_empty() { "chatcmpl-gateway".to_string() } else { id },
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string()),
        }],
        "usage": usage.unwrap_or(serde_json::Value::Null),
    });
    Some(body.to_string())
}

/// 从请求体里取 `model`（选号要用：冷却按「账号 × 模型」记账）
pub fn body_model(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string())
}

/// 下游是否要流式。缺省按流式处理（OpenAI 客户端多数显式传 true）
pub fn body_wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(true)
}

/// 上游请求体改写（单 pass）。四条规则，每条都有实测依据：
///
/// 1. **强制 `stream:true`** —— 上游 `stream:false` 直接
///    `400 code:11101 "Non-stream chat request is currently not supported"`。
/// 2. **`tool_choice` 对象形式降级成 `"auto"`** —— 上游（Go 服务）该字段是 string，
///    传对象会 `cannot unmarshal object into Go struct field Request.tool_choice of type string`。
///    对象形式（指定必须调某个函数）在 Chat Completions 里很少见，降级不改变语义方向。
/// 3. **`developer` 角色归一成 `system`** —— 上游对 `role` 做白名单校验，
///    `developer` 不在白名单内，命中即 `400 code:11128 "Illegal API invocation from
///    an unapproved channel"`。而 `developer` 本来就是 OpenAI 新规范里 `system` 的别名
///    （Codex、新版 SDK 会发），归一不改变语义。**只改这一个值**，其余 role
///    （含未知值）原样保留——不认识的角色不该被我们悄悄改写。
/// 4. **`max_completion_tokens` 翻译成 `max_tokens`** —— OpenAI 已把 `max_tokens` 标为
///    deprecated、让新 SDK 发 `max_completion_tokens`，但上游只认 `max_tokens`；
///    别名原样透传会被忽略，然后回落到默认输出上限（表现为「设了 4096 却只出几百 token」）。
///    显式 `max_tokens` 优先，别名一律删（无论翻不翻译，留着只是噪音）。
pub fn upstream_body(body: &[u8]) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert("stream".into(), serde_json::Value::Bool(true));
    if let Some(tc) = obj.get("tool_choice") {
        if !tc.is_string() && !tc.is_null() {
            obj.insert("tool_choice".into(), serde_json::Value::String("auto".into()));
        }
    }
    normalize_roles(obj);
    translate_max_completion_tokens(obj);
    serde_json::to_vec(&v).ok()
}

/// 把 `messages[].role == "developer"` 改写成 `"system"`（其余角色不动）
fn normalize_roles(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(msgs) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for m in msgs.iter_mut() {
        let Some(role) = m.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        if role.eq_ignore_ascii_case("developer") {
            if let Some(o) = m.as_object_mut() {
                o.insert("role".into(), serde_json::Value::String("system".into()));
            }
        }
    }
}

/// `max_completion_tokens` → `max_tokens`（显式 `max_tokens` 优先）
fn translate_max_completion_tokens(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(alias) = obj.remove("max_completion_tokens") else {
        return;
    };
    if obj.contains_key("max_tokens") {
        return; // 显式 max_tokens 优先：别名只删不译
    }
    let n = alias
        .as_i64()
        .or_else(|| alias.as_u64().and_then(|u| i64::try_from(u).ok()))
        .or_else(|| alias.as_f64().filter(|f| *f >= 1.0).map(|f| f as i64));
    if let Some(n) = n.filter(|n| *n > 0) {
        obj.insert("max_tokens".into(), serde_json::Value::from(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游真实帧（2026-11-24 抓取，`hy4flash-free`）
    const TEXT_STREAM: &str = concat!(
        "data: {\"id\":\"abc\",\"model\":\"hy4-flash-free\",\"object\":\"chat.completion.chunk\",\"created\":1795505461,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"abc\",\"model\":\"hy4-flash-free\",\"object\":\"chat.completion.chunk\",\"created\":1795505461,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"abc\",\"model\":\"hy4-flash-free\",\"object\":\"chat.completion.chunk\",\"created\":1795505461,\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
        "data: [DONE]\n\n",
    );

    /// 上游真实工具调用帧（`glm-5.3-flash`，首帧带 id/name，后续只有 arguments 增量）
    const TOOL_STREAM: &str = concat!(
        "data: {\"id\":\"t1\",\"model\":\"glm-5.3-flash\",\"object\":\"chat.completion.chunk\",\"created\":1795505479,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_89ff\",\"type\":\"function\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{\\\"\"},\"index\":0}]},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"glm-5.3-flash\",\"object\":\"chat.completion.chunk\",\"created\":1795505479,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"type\":\"function\",\"function\":{\"name\":\"\",\"arguments\":\"city\"},\"index\":0}]},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"glm-5.3-flash\",\"object\":\"chat.completion.chunk\",\"created\":1795505479,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"type\":\"function\",\"function\":{\"name\":\"\",\"arguments\":\"\\\":\\\"北京\\\"}\"},\"index\":0}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":164,\"completion_tokens\":12,\"total_tokens\":176}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn aggregates_text_stream_into_a_completion() {
        let out = aggregate_sse(TEXT_STREAM.as_bytes()).expect("应能聚合");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["id"], "abc");
        assert_eq!(v["model"], "hy4-flash-free");
        assert_eq!(v["choices"][0]["message"]["content"], "Hi there");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 9);
    }

    #[test]
    fn aggregates_tool_call_arguments_across_frames() {
        let out = aggregate_sse(TOOL_STREAM.as_bytes()).expect("应能聚合");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_89ff");
        assert_eq!(tc["function"]["name"], "get_time");
        // 关键：arguments 是跨 3 帧拼起来的完整 JSON
        let args: serde_json::Value =
            serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "北京");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert!(
            v["choices"][0]["message"]["content"].is_null(),
            "纯工具调用时 content 必须是 null"
        );
    }

    #[test]
    fn refuses_to_fake_an_empty_success() {
        assert_eq!(aggregate_sse(b""), None);
        assert_eq!(aggregate_sse(b"data: [DONE]\n\n"), None, "只有 [DONE] 等于空流");
    }

    #[test]
    fn non_sse_body_is_not_aggregated() {
        let err = br#"{"code":11101,"msg":"Non-stream chat request is currently not supported"}"#;
        assert_eq!(aggregate_sse(err), None, "上游错误 JSON 必须原样透传");
    }

    #[test]
    fn forces_stream_and_downgrades_tool_choice_object() {
        let body = br#"{"model":"m","messages":[],"stream":false,"tool_choice":{"type":"function","function":{"name":"f"}}}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body).unwrap()).unwrap();
        assert_eq!(out["stream"], true);
        assert_eq!(out["tool_choice"], "auto", "上游只接受字符串");
    }

    #[test]
    fn string_tool_choice_is_left_alone() {
        let body = br#"{"model":"m","messages":[],"tool_choice":"required"}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body).unwrap()).unwrap();
        assert_eq!(out["tool_choice"], "required");
    }

    #[test]
    fn reads_model_and_stream_flag() {
        let body = br#"{"model":"deepseek-v4-pro","stream":false}"#;
        assert_eq!(body_model(body).as_deref(), Some("deepseek-v4-pro"));
        assert!(!body_wants_stream(body));
        assert!(body_wants_stream(br#"{"model":"m"}"#), "缺省按流式");
    }

    /// 实测（2026-11-24）：`role:"developer"` → `400 {"code":11128,"msg":"Illegal API
    /// invocation from an unapproved channel"}`。Codex 和新版 SDK 都会发这个角色。
    #[test]
    fn normalizes_developer_role_to_system() {
        let body = r#"{"model":"m","messages":[
            {"role":"developer","content":"你是助手"},
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"yo"},
            {"role":"tool","content":"t","tool_call_id":"c1"}
        ]}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body.as_bytes()).unwrap()).unwrap();
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "你是助手", "只改角色，内容一字不动");
        // 其余角色原样保留（含 tool，上游认它）
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][2]["role"], "assistant");
        assert_eq!(out["messages"][3]["role"], "tool");
    }

    #[test]
    fn unknown_roles_are_not_rewritten() {
        let body = br#"{"model":"m","messages":[{"role":"critic","content":"x"}]}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body).unwrap()).unwrap();
        assert_eq!(
            out["messages"][0]["role"], "critic",
            "不认识的角色不该被悄悄改写"
        );
    }

    #[test]
    fn translates_max_completion_tokens() {
        let body = br#"{"model":"m","messages":[],"max_completion_tokens":4096}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body).unwrap()).unwrap();
        assert_eq!(out["max_tokens"], 4096);
        assert!(out.get("max_completion_tokens").is_none(), "别名必须删掉");
    }

    #[test]
    fn explicit_max_tokens_wins_over_alias() {
        let body = br#"{"model":"m","messages":[],"max_tokens":100,"max_completion_tokens":4096}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body).unwrap()).unwrap();
        assert_eq!(out["max_tokens"], 100, "显式 max_tokens 优先");
        assert!(out.get("max_completion_tokens").is_none());
    }

    #[test]
    fn malformed_max_completion_tokens_is_dropped_not_translated() {
        let body = br#"{"model":"m","messages":[],"max_completion_tokens":"lots"}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&upstream_body(body).unwrap()).unwrap();
        assert!(out.get("max_tokens").is_none(), "畸形值不能翻译成垃圾");
        assert!(out.get("max_completion_tokens").is_none());
    }
}
