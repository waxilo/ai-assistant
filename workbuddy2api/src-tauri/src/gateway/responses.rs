//! `POST /v1/responses`：Responses API ↔ Chat Completions 双向转换。
//!
//! Codex 从 2026-02 起**只认 Responses API**（`wire_api = "chat"` 已被删除），
//! 而上游只有 `POST /v2/chat/completions`（实测 `/v2/responses` 是 404）。
//! 所以这一层不是"顺手转发"，是必需的双向翻译。
//!
//! # 请求方向（[`to_chat`]）
//!
//! | Responses | Chat Completions |
//! | --- | --- |
//! | `instructions` | 最前面一条 `system` 消息 |
//! | `input[].type=message` | `messages[]`（content 里的 `input_text` → `text`） |
//! | `input[].type=function_call` | 一条 `assistant` 消息带 `tool_calls` |
//! | `input[].type=function_call_output` | 一条 `role:"tool"` 消息 |
//! | `tools[].type=function`（扁平） | `tools[].function`（嵌套 name/parameters） |
//! | `max_output_tokens` | `max_tokens` |
//!
//! **不认识的字段一律丢弃**。上游是 Go 严格反序列化，多传一个它不认识的字段就
//! 可能整条 400（`tool_choice` 传对象被拒就是这么来的）。Responses 的
//! `store`/`include`/`prompt_cache_key`/`reasoning`/`text.format`/`metadata`
//! 这些上游都没有对应物，硬塞只是给自己找 400。
//!
//! # 响应方向（[`Translate`]）
//!
//! 把上游 Chat SSE 翻译成 Responses 事件序列。事件形状以本机实测的 kiro2api
//! （Codex 明确认可的实现）为准，帧原文见 `docs/responses-wire.md`：
//!
//! ```text
//! response.created → response.in_progress
//!   → [文本] output_item.added(message) → content_part.added
//!            → output_text.delta* → output_text.done → content_part.done
//!            → output_item.done
//!   → [工具] output_item.added(function_call)
//!            → function_call_arguments.delta* → function_call_arguments.done
//!            → output_item.done
//! → response.completed
//! ```
//!
//! # 上游 `reasoning_content` 为什么丢掉
//!
//! 实测 kiro2api 收到 `include:["reasoning.encrypted_content"]` 也**不发**任何
//! reasoning 事件，Codex 照常工作。发 reasoning item 需要 `encrypted_content`
//! 或完整 summary 结构，凭空合成一个不合规的 reasoning item 反而会让 Codex
//! 在校验时整轮失败。宁可不显示思考过程，也不要让 Codex 直接报错。

use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// 请求方向：Responses → Chat Completions
// ---------------------------------------------------------------------------

/// Responses 请求体 → 上游 Chat Completions 请求体。
///
/// 返回 `None` 表示请求体不是 JSON 对象（调用方回 400）。
pub fn to_chat(body: &[u8]) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let src = v.as_object()?;

    let mut out = Map::new();
    if let Some(m) = src.get("model").and_then(|x| x.as_str()) {
        out.insert("model".into(), json!(m));
    }
    // 上游 `stream:false` 直接 400/11101，内部恒为流式；下游要不要流式由网关决定
    out.insert("stream".into(), json!(true));

    let mut msgs: Vec<Value> = Vec::new();
    // instructions 是 Codex 的系统提示，等价于一条 system 消息
    if let Some(ins) = src.get("instructions").and_then(|x| x.as_str()) {
        if !ins.trim().is_empty() {
            msgs.push(json!({ "role": "system", "content": ins }));
        }
    }
    match src.get("input") {
        // 规范允许 input 直接是一个字符串（等价于单条 user 消息）
        Some(Value::String(s)) => msgs.push(json!({ "role": "user", "content": s })),
        Some(Value::Array(items)) => {
            for it in items {
                if let Some(m) = translate_input_item(it) {
                    msgs.push(m);
                }
            }
        }
        _ => {}
    }
    out.insert("messages".into(), Value::Array(msgs));

    if let Some(tools) = src.get("tools").and_then(|x| x.as_array()) {
        let list: Vec<Value> = tools.iter().filter_map(translate_tool).collect();
        if !list.is_empty() {
            out.insert("tools".into(), Value::Array(list));
        }
    }

    if let Some(tc) = src.get("tool_choice") {
        match tc {
            Value::String(s) => {
                out.insert("tool_choice".into(), json!(s));
            }
            Value::Null => {}
            // 对象形式（指定必须调某个函数）: 上游该字段是 string，传对象会
            // `cannot unmarshal object into Go struct field`，统一降级
            _ => {
                out.insert("tool_choice".into(), json!("auto"));
            }
        }
    }

    for k in ["temperature", "top_p"] {
        if let Some(x) = src.get(k).filter(|x| !x.is_null()) {
            out.insert(k.into(), x.clone());
        }
    }
    if let Some(n) = src.get("max_output_tokens").filter(|x| !x.is_null()) {
        out.insert("max_tokens".into(), n.clone());
    }

    serde_json::to_vec(&Value::Object(out)).ok()
}

/// 一条 Responses `input[]` 项 → 一条（或零条）Chat 消息。
fn translate_input_item(it: &Value) -> Option<Value> {
    let ty = it.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match ty {
        "message" => {
            let role = it.get("role").and_then(|x| x.as_str()).unwrap_or("user");
            // developer 是 system 的别名，但上游 role 白名单不认它（实测 400/11128）
            let role = if role.eq_ignore_ascii_case("developer") {
                "system"
            } else {
                role
            };
            Some(json!({ "role": role, "content": translate_content(it.get("content")) }))
        }
        // 上一轮的工具调用：assistant 消息里的 tool_calls
        "function_call" => {
            let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let call_id = it
                .get("call_id")
                .and_then(|x| x.as_str())
                .or_else(|| it.get("id").and_then(|x| x.as_str()))
                .unwrap_or("");
            // arguments 规范上是 JSON 字符串；万一被上游塞成对象就序列化回去
            let args = match it.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) if !other.is_null() => other.to_string(),
                _ => String::new(),
            };
            Some(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": args }
                }]
            }))
        }
        // 工具执行结果：Chat 里是独立的 role:"tool" 消息
        "function_call_output" => {
            let call_id = it.get("call_id").and_then(|x| x.as_str()).unwrap_or("");
            Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": flatten_output(it.get("output"))
            }))
        }
        // reasoning / item_reference / 未知类型：上游没有对应物，丢弃
        _ => None,
    }
}

/// Responses 的 content（字符串或 part 数组）→ Chat 的 content。
///
/// 纯文本尽量降级成**字符串**：部分上游模型只认字符串 content，收到数组会
/// 400。只有真的含图片时才保留数组形态。
fn translate_content(c: Option<&Value>) -> Value {
    match c {
        Some(Value::String(s)) => json!(s),
        Some(Value::Array(parts)) => {
            let mut out: Vec<Value> = Vec::new();
            for p in parts {
                match p.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                    "input_text" | "output_text" | "text" => {
                        if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                            out.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    "input_image" => {
                        if let Some(u) = p.get("image_url").and_then(|x| x.as_str()) {
                            out.push(json!({ "type": "image_url", "image_url": { "url": u } }));
                        }
                    }
                    _ => {}
                }
            }
            match out.len() {
                1 if out[0].get("type").and_then(|x| x.as_str()) == Some("text") => {
                    out[0].get("text").cloned().unwrap_or_else(|| json!(""))
                }
                _ => Value::Array(out),
            }
        }
        _ => json!(""),
    }
}

/// `function_call_output.output`：可能是字符串，也可能是 part 数组。
fn flatten_output(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// Responses 的扁平工具定义 → Chat 的嵌套 `function` 形态。
///
/// 非 `function` 类型（`web_search`、`local_shell`、`custom`…）上游都不支持，
/// 直接丢弃——带上就是 400。
fn translate_tool(t: &Value) -> Option<Value> {
    if t.get("type").and_then(|x| x.as_str()) != Some("function") {
        return None;
    }
    let name = t.get("name").and_then(|x| x.as_str())?;
    let mut f = Map::new();
    f.insert("name".into(), json!(name));
    if let Some(d) = t.get("description").filter(|d| !d.is_null()) {
        f.insert("description".into(), d.clone());
    }
    f.insert(
        "parameters".into(),
        t.get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
    );
    Some(json!({ "type": "function", "function": Value::Object(f) }))
}

// ---------------------------------------------------------------------------
// 响应方向：Chat SSE → Responses 事件
// ---------------------------------------------------------------------------

/// 正在拼装的文本 item
struct TextItem {
    id: String,
    index: usize,
    text: String,
}

/// 正在拼装的工具调用 item
struct ToolItem {
    /// Responses 侧的 item id（`fc_…`），与 `call_id` 不是一回事
    id: String,
    /// 上游给的调用 id（`call_…`），回灌时必须原样还给模型
    call_id: String,
    index: usize,
    /// 上游 `tool_calls[].index`，用来判「是不是换了一个工具」
    up_index: usize,
    name: String,
    args: String,
}

/// Chat SSE → Responses SSE 的流式翻译器。
///
/// 用法：反复 [`feed_bytes`](Self::feed_bytes) 喂上游 chunk，最后
/// [`finish`](Self::finish)。输出直接写进 `out`。
pub struct Translate {
    response_id: String,
    model: String,
    created_at: i64,
    seq: u64,
    next_index: usize,
    /// `response.created` / `in_progress` 是否已发
    opened: bool,
    text: Option<TextItem>,
    tool: Option<ToolItem>,
    output: Vec<Value>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    /// 行缓冲：上游 chunk 边界与 SSE 行边界**毫无关系**，必须自己攒
    buf: Vec<u8>,
    /// `finish()` 之后这里放完整的 response 对象（非流式直接用）
    final_response: Option<Value>,
}

impl Translate {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            response_id: format!("resp_{}", short_id()),
            model: model.unwrap_or("").to_string(),
            created_at: now_secs(),
            seq: 0,
            next_index: 0,
            opened: false,
            text: None,
            tool: None,
            output: Vec::new(),
            usage: None,
            finish_reason: None,
            buf: Vec::new(),
            final_response: None,
        }
    }

    /// 是否已经发出过 `response.created`（调用方据此决定能不能安全写 HTTP 头）
    pub fn opened(&self) -> bool {
        self.opened
    }

    /// 非流式路径用：`finish()` 之后取完整 response 对象
    pub fn final_response(&self) -> Option<&Value> {
        self.final_response.as_ref()
    }

    /// 喂一段上游字节，翻译结果追加到 `out`。
    pub fn feed_bytes(&mut self, bytes: &[u8], out: &mut String) {
        self.buf.extend_from_slice(bytes);
        // 只处理完整的行；半行留在 buf 里等下一块
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            self.feed_line(line.trim_end_matches(['\r', '\n']), out);
        }
    }

    fn feed_line(&mut self, line: &str, out: &mut String) {
        let Some(rest) = line.strip_prefix("data:") else {
            return; // `event:` 行、注释行、空行都不用管
        };
        let data = rest.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.finish_reason.get_or_insert_with(|| "stop".into());
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };

        // 第一帧到达才算「流真的开始了」——上游 200 但 body 是错误 JSON 时，
        // 这里不会有任何输出，调用方据此回 502 而不是一个空的 completed
        self.open(out);

        if self.model.is_empty() {
            if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                self.model = m.to_string();
            }
        }
        if let Some(c) = v.get("created").and_then(|x| x.as_i64()) {
            if c > 0 {
                self.created_at = c;
            }
        }
        // usage 一般只在最后一帧出现（实测），出现就覆盖
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(map_usage(u));
        }

        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return; // 纯 usage 帧
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            if !fr.is_empty() {
                self.finish_reason = Some(fr.to_string());
            }
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(s) = delta.get("content").and_then(|x| x.as_str()) {
            self.push_text(out, s);
        }
        if let Some(arr) = delta.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in arr {
                let up_index = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                let id = tc.get("id").and_then(|x| x.as_str());
                let f = tc.get("function");
                let name = f.and_then(|f| f.get("name")).and_then(|x| x.as_str());
                let args = f.and_then(|f| f.get("arguments")).and_then(|x| x.as_str());
                self.push_tool(out, up_index, id, name, args);
            }
        }
    }

    /// 流结束：关闭所有开着的 item，发 `response.completed`。
    ///
    /// 无论上游有没有给 finish_reason 都要调一次——调用方已经写了 200 响应头，
    /// 不补一个 completed，客户端会一直等下去。
    pub fn finish(&mut self, out: &mut String) {
        self.open(out);
        self.close_text(out);
        self.close_tool(out);

        let fr = self.finish_reason.clone().unwrap_or_else(|| "stop".into());
        let (status, incomplete) = match fr.as_str() {
            "length" | "max_tokens" => ("incomplete", json!({ "reason": "max_output_tokens" })),
            "content_filter" => ("incomplete", json!({ "reason": "content_filter" })),
            _ => ("completed", Value::Null),
        };
        let response = self.build_response(status, incomplete);
        self.emit(out, "response.completed", json!({ "response": response.clone() }));
        self.final_response = Some(response);
    }

    // -- 事件拼装 ----------------------------------------------------------

    fn emit(&mut self, out: &mut String, ty: &str, mut data: Value) {
        if let Some(o) = data.as_object_mut() {
            o.insert("type".into(), json!(ty));
            o.insert("sequence_number".into(), json!(self.seq));
        }
        self.seq += 1;
        // JSON 里不会有裸换行（serde 会转义），所以这一条就是完整的一帧
        out.push_str(&format!(
            "event: {ty}\ndata: {}\n\n",
            serde_json::to_string(&data).unwrap_or_default()
        ));
    }

    fn open(&mut self, out: &mut String) {
        if self.opened {
            return;
        }
        self.opened = true;
        self.emit(
            out,
            "response.created",
            json!({ "response": self.build_response("in_progress", Value::Null) }),
        );
        self.emit(
            out,
            "response.in_progress",
            json!({ "response": self.build_response("in_progress", Value::Null) }),
        );
    }

    fn take_index(&mut self) -> usize {
        let i = self.next_index;
        self.next_index += 1;
        i
    }

    fn push_text(&mut self, out: &mut String, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.text.is_none() {
            // 理论上文本之前不会有开着的工具，但真出现也要收干净，否则 output 乱序
            self.close_tool(out);
            let index = self.take_index();
            let id = format!("msg_{}", short_id());
            self.emit(
                out,
                "response.output_item.added",
                json!({
                    "output_index": index,
                    "item": {
                        "id": id, "type": "message", "status": "in_progress",
                        "role": "assistant", "content": []
                    }
                }),
            );
            self.emit(
                out,
                "response.content_part.added",
                json!({
                    "item_id": id, "output_index": index, "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] }
                }),
            );
            self.text = Some(TextItem { id, index, text: String::new() });
        }
        let t = self.text.as_mut().expect("刚插入");
        t.text.push_str(s);
        let (id, index) = (t.id.clone(), t.index);
        self.emit(
            out,
            "response.output_text.delta",
            json!({
                "item_id": id, "output_index": index, "content_index": 0,
                "delta": s, "logprobs": []
            }),
        );
    }

    fn push_tool(
        &mut self,
        out: &mut String,
        up_index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) {
        // 上游 index 变了 = 换了一个工具：先把上一个收尾
        if self.tool.as_ref().is_some_and(|t| t.up_index != up_index) {
            self.close_tool(out);
        }
        if self.tool.is_none() {
            self.close_text(out);
            let index = self.take_index();
            let call_id = id
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("call_{}", short_id()));
            let item_id = format!("fc_{}", short_id());
            self.emit(
                out,
                "response.output_item.added",
                json!({
                    "output_index": index,
                    "item": {
                        "id": item_id, "type": "function_call", "status": "in_progress",
                        "arguments": "", "call_id": call_id, "name": name.unwrap_or("")
                    }
                }),
            );
            self.tool = Some(ToolItem {
                id: item_id,
                call_id,
                index,
                up_index,
                name: name.unwrap_or("").to_string(),
                args: String::new(),
            });
        }
        let t = self.tool.as_mut().expect("刚插入");
        // 上游首帧带 id/name，后续帧 name 是空串（实测），所以要累积而不是覆盖
        if let Some(n) = name {
            if !n.is_empty() && t.name.len() < n.len() {
                t.name.push_str(n);
            }
        }
        if let Some(a) = args {
            if !a.is_empty() {
                t.args.push_str(a);
                let (id, index) = (t.id.clone(), t.index);
                self.emit(
                    out,
                    "response.function_call_arguments.delta",
                    json!({ "item_id": id, "output_index": index, "delta": a }),
                );
            }
        }
    }

    fn close_text(&mut self, out: &mut String) {
        let Some(t) = self.text.take() else { return };
        self.emit(
            out,
            "response.output_text.done",
            json!({
                "item_id": t.id, "output_index": t.index, "content_index": 0, "text": t.text
            }),
        );
        self.emit(
            out,
            "response.content_part.done",
            json!({
                "item_id": t.id, "output_index": t.index, "content_index": 0,
                "part": { "type": "output_text", "text": t.text, "annotations": [] }
            }),
        );
        let item = json!({
            "id": t.id, "type": "message", "status": "completed", "role": "assistant",
            "content": [{ "type": "output_text", "text": t.text, "annotations": [] }]
        });
        self.emit(
            out,
            "response.output_item.done",
            json!({ "output_index": t.index, "item": item }),
        );
        self.output.push(item);
    }

    fn close_tool(&mut self, out: &mut String) {
        let Some(t) = self.tool.take() else { return };
        self.emit(
            out,
            "response.function_call_arguments.done",
            json!({ "item_id": t.id, "output_index": t.index, "arguments": t.args }),
        );
        let item = json!({
            "id": t.id, "type": "function_call", "status": "completed",
            "arguments": t.args, "call_id": t.call_id, "name": t.name
        });
        self.emit(
            out,
            "response.output_item.done",
            json!({ "output_index": t.index, "item": item }),
        );
        self.output.push(item);
    }

    /// Responses 的 `response` 对象骨架。
    ///
    /// 字段照抄实测的 kiro2api 形状：Codex 的反序列化虽然多半带 `#[serde(default)]`，
    /// 但少给 `text.format` / `reasoning` 这类嵌套对象时，个别版本会在 UI 层 panic。
    fn build_response(&self, status: &str, incomplete: Value) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": self.output,
            "usage": self.usage.clone().unwrap_or(Value::Null),
            "error": Value::Null,
            "incomplete_details": incomplete,
            "instructions": Value::Null,
            "metadata": {},
            "parallel_tool_calls": true,
            "previous_response_id": Value::Null,
            "reasoning": { "effort": Value::Null, "summary": Value::Null },
            "store": false,
            "temperature": Value::Null,
            "text": { "format": { "type": "text" } },
            "tool_choice": "auto",
            "tools": [],
            "top_p": Value::Null,
        })
    }
}

/// Chat 的 usage → Responses 的 usage。
///
/// 上游只在最后一帧给 `{prompt_tokens, completion_tokens, total_tokens}`；
/// Responses 要求嵌套的 `*_details`，缺了会让 Codex 的 token 计数显示成 0。
fn map_usage(u: &Value) -> Value {
    let num = |k: &str| u.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let reasoning = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    json!({
        "input_tokens": num("prompt_tokens"),
        "input_tokens_details": { "cached_tokens": cached },
        "output_tokens": num("completion_tokens"),
        "output_tokens_details": { "reasoning_tokens": reasoning },
        "total_tokens": num("total_tokens"),
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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

    /// 上游真实工具调用帧（`glm-5.3-flash`）
    const TOOL_STREAM: &str = concat!(
        "data: {\"id\":\"t1\",\"model\":\"glm-5.3-flash\",\"object\":\"chat.completion.chunk\",\"created\":1795505479,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_89ff\",\"type\":\"function\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{\\\"\"},\"index\":0}]},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"glm-5.3-flash\",\"object\":\"chat.completion.chunk\",\"created\":1795505479,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"type\":\"function\",\"function\":{\"name\":\"\",\"arguments\":\"city\"},\"index\":0}]},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"glm-5.3-flash\",\"object\":\"chat.completion.chunk\",\"created\":1795505479,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"type\":\"function\",\"function\":{\"name\":\"\",\"arguments\":\"\\\":\\\"北京\\\"}\"},\"index\":0}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":164,\"completion_tokens\":12,\"total_tokens\":176}}\n\n",
        "data: [DONE]\n\n",
    );

    /// 把翻译结果拆成 `(event_type, data)` 列表，便于断言序列
    fn events(sse: &str) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for block in sse.split("\n\n") {
            let mut ty = None;
            let mut data = None;
            for line in block.lines() {
                if let Some(t) = line.strip_prefix("event: ") {
                    ty = Some(t.to_string());
                }
                if let Some(d) = line.strip_prefix("data: ") {
                    data = serde_json::from_str::<Value>(d).ok();
                }
            }
            if let (Some(t), Some(d)) = (ty, data) {
                out.push((t, d));
            }
        }
        out
    }

    fn translate(sse: &str, model: &str) -> Vec<(String, Value)> {
        let mut t = Translate::new(Some(model));
        let mut out = String::new();
        t.feed_bytes(sse.as_bytes(), &mut out);
        t.finish(&mut out);
        events(&out)
    }

    #[test]
    fn text_stream_becomes_the_documented_event_sequence() {
        let ev = translate(TEXT_STREAM, "hy4-flash-free");
        let types: Vec<&str> = ev.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }

    #[test]
    fn sequence_number_is_dense_and_monotonic() {
        let ev = translate(TEXT_STREAM, "m");
        for (i, (_, d)) in ev.iter().enumerate() {
            assert_eq!(d["sequence_number"], i, "序号必须连续，否则 Codex 会判流损坏");
            assert_eq!(d["type"], ev[i].0.as_str(), "data.type 必须与 event: 行一致");
        }
    }

    #[test]
    fn completed_carries_full_output_and_usage() {
        let ev = translate(TEXT_STREAM, "hy4-flash-free");
        let (_, done) = ev.last().unwrap();
        assert_eq!(done["response"]["status"], "completed");
        assert_eq!(done["response"]["output"][0]["content"][0]["text"], "Hi there");
        assert_eq!(done["response"]["output"][0]["type"], "message");
        // Chat 的 prompt/completion → Responses 的 input/output
        assert_eq!(done["response"]["usage"]["input_tokens"], 7);
        assert_eq!(done["response"]["usage"]["output_tokens"], 2);
        assert_eq!(done["response"]["usage"]["total_tokens"], 9);
    }

    #[test]
    fn tool_stream_never_emits_text_events() {
        let ev = translate(TOOL_STREAM, "glm-5.3-flash");
        let types: Vec<&str> = ev.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            !types.iter().any(|t| t.starts_with("response.output_text")),
            "纯工具调用不该出现文本事件：{types:?}"
        );
        assert_eq!(
            types,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }

    #[test]
    fn tool_arguments_are_reassembled_across_frames() {
        let ev = translate(TOOL_STREAM, "glm-5.3-flash");
        let (_, done) = ev.last().unwrap();
        let item = &done["response"]["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "get_time");
        assert_eq!(item["call_id"], "call_89ff", "回灌时必须原样还给模型");
        let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "北京");
    }

    #[test]
    fn text_then_tool_orders_output_items_by_arrival() {
        let mixed = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"let me check\"},\"finish_reason\":\"\"}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{}\"},\"index\":0}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let ev = translate(mixed, "m");
        let (_, done) = ev.last().unwrap();
        let out = done["response"]["output"].as_array().unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["type"], "message", "先说话的 item 必须排前面");
        assert_eq!(out[1]["type"], "function_call");
        // output_index 要与数组下标对齐，否则 Codex 按 index 回查会错位
        assert_eq!(out[0]["id"], ev.iter().find(|(t, _)| t == "response.output_item.added").unwrap().1["item"]["id"]);
    }

    #[test]
    fn two_tool_calls_get_two_output_items() {
        let two = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"a\",\"arguments\":\"{}\"},\"index\":0}]},\"finish_reason\":\"\"}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"},\"index\":1}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let ev = translate(two, "m");
        let (_, done) = ev.last().unwrap();
        let out = done["response"]["output"].as_array().unwrap();
        assert_eq!(out.len(), 2, "两个工具必须产出两个 item");
        assert_eq!(out[0]["call_id"], "call_a");
        assert_eq!(out[1]["call_id"], "call_b");
    }

    #[test]
    fn length_finish_reason_marks_response_incomplete() {
        let cut = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half\"},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let ev = translate(cut, "m");
        let (_, done) = ev.last().unwrap();
        assert_eq!(done["response"]["status"], "incomplete");
        assert_eq!(done["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    /// chunk 边界与 SSE 行边界毫无关系，跨块的行必须能拼回来
    #[test]
    fn survives_chunk_boundaries_inside_a_line() {
        let mut t = Translate::new(Some("m"));
        let mut out = String::new();
        let bytes = TEXT_STREAM.as_bytes();
        // 逐字节喂：最恶劣的切法（每一行都会被切开好几次）
        for b in bytes {
            t.feed_bytes(&[*b], &mut out);
        }
        t.finish(&mut out);
        let ev = events(&out);
        let (_, done) = ev.last().unwrap();
        assert_eq!(done["response"]["output"][0]["content"][0]["text"], "Hi there");
    }

    #[test]
    fn non_sse_body_produces_no_events_at_all() {
        let mut t = Translate::new(Some("m"));
        let mut out = String::new();
        t.feed_bytes(br#"{"code":11101,"msg":"nope"}"#, &mut out);
        assert!(out.is_empty(), "解析不出帧就不能发 created");
        assert!(!t.opened(), "调用方据此回 502 而不是一个空的 completed");
    }

    #[test]
    fn empty_stream_still_completes_after_opened() {
        // 上游给了一帧但没有内容（例如只有 role 的帧），也必须收尾成 completed
        let mut t = Translate::new(Some("m"));
        let mut out = String::new();
        t.feed_bytes(
            br#"data: {"id":"x","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":""}]}"#,
            &mut out,
        );
        t.finish(&mut out);
        let ev = events(&out);
        assert_eq!(ev.last().unwrap().0, "response.completed");
    }

    // -- 请求方向 ----------------------------------------------------------

    fn chat_of(req: &str) -> Value {
        serde_json::from_slice(&to_chat(req.as_bytes()).expect("应能转换")).unwrap()
    }

    #[test]
    fn instructions_become_a_leading_system_message() {
        let out = chat_of(
            r#"{"model":"m","instructions":"You are Codex.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "You are Codex.");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hi", "纯文本必须降级成字符串");
    }

    #[test]
    fn empty_instructions_are_not_emitted() {
        let out = chat_of(r#"{"model":"m","instructions":"   ","input":"hi"}"#);
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn string_input_is_a_single_user_message() {
        let out = chat_of(r#"{"model":"m","input":"hello"}"#);
        assert_eq!(out["messages"][0]["content"], "hello");
    }

    #[test]
    fn developer_role_in_input_becomes_system() {
        let out = chat_of(
            r#"{"model":"m","input":[{"type":"message","role":"developer","content":"be terse"}]}"#,
        );
        assert_eq!(out["messages"][0]["role"], "system");
    }

    #[test]
    fn function_call_becomes_assistant_tool_calls() {
        let out = chat_of(
            r#"{"model":"m","input":[{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"call_1"}]}"#,
        );
        let m = &out["messages"][0];
        assert_eq!(m["role"], "assistant");
        assert!(m["content"].is_null());
        assert_eq!(m["tool_calls"][0]["id"], "call_1");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], "{\"command\":[\"ls\"]}");
    }

    #[test]
    fn function_call_output_becomes_a_tool_message() {
        let out = chat_of(
            r#"{"model":"m","input":[{"type":"function_call_output","call_id":"call_1","output":"total 0"}]}"#,
        );
        let m = &out["messages"][0];
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "call_1");
        assert_eq!(m["content"], "total 0");
    }

    #[test]
    fn function_call_output_accepts_part_arrays() {
        let out = chat_of(
            r#"{"model":"m","input":[{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"line1"},{"type":"input_text","text":"line2"}]}]}"#,
        );
        assert_eq!(out["messages"][0]["content"], "line1line2");
    }

    #[test]
    fn reasoning_items_are_dropped() {
        let out = chat_of(
            r#"{"model":"m","input":[
                {"type":"reasoning","summary":[],"encrypted_content":"xxx"},
                {"type":"message","role":"user","content":"hi"}
            ]}"#,
        );
        assert_eq!(
            out["messages"].as_array().unwrap().len(),
            1,
            "reasoning 上游不认识，留着就是 400"
        );
    }

    #[test]
    fn flat_tools_become_nested_function_tools() {
        let out = chat_of(
            r#"{"model":"m","input":"hi","tools":[
                {"type":"function","name":"shell","description":"run","parameters":{"type":"object"},"strict":false},
                {"type":"web_search"}
            ]}"#,
        );
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "非 function 类型上游不支持，必须丢掉");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(tools[0]["function"]["description"], "run");
        assert!(tools[0]["function"].get("strict").is_none(), "strict 不是上游字段");
    }

    #[test]
    fn tool_choice_object_is_downgraded() {
        let out = chat_of(
            r#"{"model":"m","input":"hi","tool_choice":{"type":"function","name":"shell"}}"#,
        );
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn tool_choice_string_is_kept() {
        let out = chat_of(r#"{"model":"m","input":"hi","tool_choice":"required"}"#);
        assert_eq!(out["tool_choice"], "required");
    }

    #[test]
    fn max_output_tokens_becomes_max_tokens() {
        let out = chat_of(r#"{"model":"m","input":"hi","max_output_tokens":512}"#);
        assert_eq!(out["max_tokens"], 512);
        assert!(out.get("max_output_tokens").is_none());
    }

    #[test]
    fn responses_only_fields_are_not_forwarded() {
        let out = chat_of(
            r#"{"model":"m","input":"hi","store":false,"include":["reasoning.encrypted_content"],
                "prompt_cache_key":"abc","reasoning":{"effort":"high"},"metadata":{"a":1},
                "text":{"format":{"type":"json_schema"}},"parallel_tool_calls":false}"#,
        );
        for k in [
            "store",
            "include",
            "prompt_cache_key",
            "reasoning",
            "metadata",
            "text",
            "parallel_tool_calls",
        ] {
            assert!(out.get(k).is_none(), "{k} 上游没有对应字段，不能带过去");
        }
    }

    #[test]
    fn stream_is_always_forced_on() {
        let out = chat_of(r#"{"model":"m","input":"hi","stream":false}"#);
        assert_eq!(out["stream"], true, "上游只支持流式");
    }

    #[test]
    fn null_sampling_params_are_not_forwarded() {
        let out = chat_of(r#"{"model":"m","input":"hi","temperature":null,"top_p":null}"#);
        assert!(out.get("temperature").is_none());
        assert!(out.get("top_p").is_none());
    }

    #[test]
    fn image_parts_stay_as_multimodal_array() {
        let out = chat_of(
            r#"{"model":"m","input":[{"type":"message","role":"user","content":[
                {"type":"input_text","text":"what is this"},
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]}]}"#,
        );
        let c = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["type"], "text");
        assert_eq!(c[1]["type"], "image_url");
        assert_eq!(c[1]["image_url"]["url"], "data:image/png;base64,AAA");
    }

    #[test]
    fn malformed_body_is_rejected() {
        assert!(to_chat(b"not json").is_none());
        assert!(to_chat(br#"[1,2,3]"#).is_none(), "非对象也不是合法请求");
    }
}
