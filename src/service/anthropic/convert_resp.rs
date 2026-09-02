//! Chat Completions 响应 → Anthropic Messages 响应（非流式）。
//!
//! cc-switch `transform.rs` `openai_to_anthropic`（:552）移植：
//! - reasoning_content → thinking 块；content → text 块（refusal 兜底）
//! - tool_calls → tool_use 块（arguments 字符串反序列化为对象，失败兜底 `{}`）
//! - finish_reason → stop_reason（content_filter→end_turn 语义丢失仅告警；
//!   无 finish_reason 但有 tool_use → 强制 tool_use）
//! - usage 三桶恒等式：OpenAI prompt_tokens 含缓存命中，Anthropic input_tokens 不含
//!   → input = prompt − cached − cache_creation（saturating）

use serde_json::{Value, json};

use super::super::responses::dto::Usage as ChatUsage;

/// Chat 响应 → Anthropic 响应。`id`：本网关生成的消息 id（`msg_<uuid>`）。
pub fn chat_response_to_anthropic(resp: &Value, id: &str) -> Result<Value, String> {
    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    // L20：choices 非空但 message 缺失/全空的畸形体 → 转换层显式失败（proxy 层
    // has_usable_choices 只挡 choices:[]；cc-switch transform.rs:553-567 同款）
    let Some(message) = resp.pointer("/choices/0/message") else {
        return Err("no message in first choice".into());
    };

    // 内容块：thinking → text → tool_use（保持上游顺序类别内稳定）
    let mut content: Vec<Value> = Vec::new();
    let mut reasoning = extract_reasoning(message);
    // M20：text 提取覆盖 output_text part 与 part 级 refusal；refusal 与正文并存时
    // 追加为独立 text 块（Anthropic 无 refusal 块类型；cc-switch transform.rs:585-611 同款）
    let (mut text, refusal) = extract_text_and_refusal(message);
    // M1：非流式正文开头的 <think>…</think> 分离为 thinking 块（与流式路径
    // InlineThinkState 行为对齐；此前思考污染 text 块且计费口径失真）
    if let Some((think, rest)) = crate::service::inline_think::split_leading_think(&text) {
        reasoning = if reasoning.is_empty() {
            think
        } else {
            format!("{reasoning}\n{think}")
        };
        text = rest.trim().to_string();
    }
    if !reasoning.is_empty() {
        content.push(json!({"type": "thinking", "thinking": reasoning}));
    }
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(refusal) = refusal.filter(|r| !r.trim().is_empty()) {
        content.push(json!({"type": "text", "text": refusal}));
    }
    let mut has_tool_use = false;
    let mut dropped_tools = 0usize;
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for (i, tc) in tool_calls.iter().enumerate() {
            // P1-10：与流式 M17 口径统一——有载荷（id/name/arguments 任一）即保留：
            // 缺 id 兜底 tool_call_{i}、缺 name 兜底 unknown_tool（此前整只丢弃 →
            // 非流式工具循环断裂；cc-switch transform.rs:612-635 保留语义 + 流式
            // streaming.rs:542-602 unknown_tool 合成同款）
            if let Some(block) = chat_tool_call_to_tool_use(tc, i) {
                has_tool_use = true;
                content.push(block);
            } else {
                dropped_tools += 1;
            }
        }
    }
    // L4：legacy `function_call` 形态（顶层 name/arguments，非 tool_calls 数组）
    // 部分旧上游仍下发；网关流式路径已兼容（dto lenient_tool_call），非流式对齐。
    // M21：仅当无 tool_calls 时处理（两形态并存时只认 tool_calls，防重复输出）；
    // 保留 function_call.id；arguments 兼容字符串/对象/数组形态
    if !has_tool_use {
        if let Some(fc) = message.get("function_call") {
            if let Some(block) = legacy_function_call_to_tool_use(fc, id) {
                has_tool_use = true;
                content.push(block);
            }
        }
    }
    // M5：全部工具调用因缺名被丢弃且无任何其它内容 → 显式失败，不得伪装成
    // content:[] + end_turn 的空成功（Claude Code 会静默收尾卡死 agent loop；
    // 与本模块流式路径 / responses 非流式路径的 #4341 护栏对齐）
    if dropped_tools > 0 && content.is_empty() {
        return Err(
            "upstream returned tool_calls without function name and no other output".into(),
        );
    }
    // finish_reason → stop_reason
    let finish_reason = resp
        .pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let stop_reason = stop_reason_from_finish(&finish_reason, has_tool_use);

    // usage 三桶换算
    let usage: Option<ChatUsage> = resp
        .get("usage")
        .filter(|u| u.is_object())
        .map(ChatUsage::from_value_lenient);
    let usage_out = usage.as_ref().map(anthropic_usage);

    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_out.unwrap_or_else(|| json!({
            "input_tokens": 0, "output_tokens": 0,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
        }))
    }))
}

/// finish_reason → stop_reason（cc-switch map_stop_reason 语义）：
/// content_filter→end_turn（Anthropic 无对应语义，丢失仅告警）；未知值兜底 end_turn
pub fn stop_reason_from_finish(finish_reason: &str, has_tool_use: bool) -> String {
    match finish_reason.trim() {
        // P0-5 对齐：finish=stop 但消息含 tool_use 块 → tool_use（Anthropic
        // 语义：以 tool_use 块结尾的回合 stop_reason 恒为 tool_use，end_turn 会
        // 让 Claude Code 跳过工具执行；此前非流式 stop+工具报 end_turn）
        "stop" => {
            if has_tool_use {
                "tool_use".into()
            } else {
                "end_turn".into()
            }
        }
        "length" | "max_tokens" | "token_limit" => "max_tokens".into(),
        // finish_reason=tool_calls 但全部工具调用被丢弃（无名）→ end_turn：
        // stop_reason=tool_use 而无 tool_use 块会让客户端协议解析失败
        "tool_calls" | "function_call" => {
            if has_tool_use {
                "tool_use".into()
            } else {
                "end_turn".into()
            }
        }
        "content_filter" => {
            tracing::warn!(
                finish_reason,
                "content_filter has no Anthropic stop_reason; mapping to end_turn"
            );
            "end_turn".into()
        }
        other if !other.is_empty() => {
            tracing::warn!(
                finish_reason = other,
                "unknown finish_reason; mapping to end_turn"
            );
            "end_turn".into()
        }
        // 无 finish_reason 但带 tool_use → tool_use（上游常见缺省）
        _ if has_tool_use => "tool_use".into(),
        _ => "end_turn".into(),
    }
}

/// Chat usage → Anthropic usage（三桶恒等式：input + cache_read + cache_creation == prompt_tokens）
pub fn anthropic_usage(u: &ChatUsage) -> Value {
    let cached = u.cached_tokens();
    let cache_creation = u.cache_write_tokens();
    let input = u
        .input()
        .saturating_sub(cached)
        .saturating_sub(cache_creation);
    json!({
        "input_tokens": input,
        "output_tokens": u.output(),
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cached
    })
}

/// 消息 reasoning_content → 文本（`reasoning` / `reasoning_details[]` 别名兜底；
/// M1：reasoning_details 为 xAI Grok 等上游的数组形态 [{text:…}]）
fn extract_reasoning(message: &Value) -> String {
    if let Some(r) = message
        .get("reasoning_content")
        .and_then(|r| r.as_str())
        .or_else(|| message.get("reasoning").and_then(|r| r.as_str()))
    {
        return r.to_string();
    }
    message
        .get("reasoning_details")
        .and_then(|d| d.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 消息 content → (正文, refusal)。M20：parts 覆盖 `text`/`output_text`；
/// part 级 `refusal` 与 message 级 refusal 独立返回（与正文并存不丢）
fn extract_text_and_refusal(message: &Value) -> (String, Option<String>) {
    let mut text = String::new();
    let mut part_refusals: Vec<String> = Vec::new();
    match message.get("content") {
        Some(Value::String(s)) => text = s.clone(),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("output_text") => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("refusal") => {
                        if let Some(r) = part
                            .get("refusal")
                            .or_else(|| part.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            part_refusals.push(r.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let msg_refusal = message
        .get("refusal")
        .and_then(|r| r.as_str())
        .map(str::to_string);
    let refusal = match (part_refusals.is_empty(), &msg_refusal) {
        (false, Some(m)) => Some(format!("{}\n{m}", part_refusals.join("\n"))),
        (false, None) => Some(part_refusals.join("\n")),
        (true, Some(m)) => Some(m.clone()),
        (true, None) => None,
    };
    (text, refusal)
}

/// Chat tool_call → Anthropic tool_use 块（arguments 字符串 → JSON 对象，失败 `{}`）。
/// P1-10：与流式口径统一——id/name/arguments 任一存在即保留：缺 id 兜底
/// `tool_call_{index}`、缺 name 兜底 `unknown_tool`；完全零载荷才丢弃
///（cc-switch transform.rs:612-635 + streaming.rs:542-602 语义）
fn chat_tool_call_to_tool_use(tc: &Value, index: usize) -> Option<Value> {
    let id_raw = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").trim();
    let name_raw = tc
        .pointer("/function/name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim();
    let args_raw = tc
        .pointer("/function/arguments")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .trim();
    // 零载荷（无 id 无 name 无 arguments）→ 丢弃并计数（保留护栏 Err 语义）
    if id_raw.is_empty() && name_raw.is_empty() && args_raw.is_empty() {
        return None;
    }
    let id = if id_raw.is_empty() {
        format!("tool_call_{index}")
    } else {
        id_raw.to_string()
    };
    let name = if name_raw.is_empty() {
        "unknown_tool".to_string()
    } else {
        name_raw.to_string()
    };
    let input = serde_json::from_str::<Value>(args_raw)
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
}

fn legacy_function_call_to_tool_use(fc: &Value, message_id: &str) -> Option<Value> {
    // P1-11：空名但有 arguments 的 legacy function_call 保留（name 兜底
    // unknown_tool；cc-switch transform.rs:655-662 `!name.is_empty() || has_arguments`
    // 同款——此前整只静默丢弃，老上游工具调用消失）
    let has_arguments = fc.get("arguments").is_some();
    if !has_arguments
        && fc
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim()
            .is_empty()
    {
        return None;
    }
    let name = fc
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown_tool");
    let input = match fc.get("arguments") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({})),
        Some(Value::Object(_)) | Some(Value::Array(_)) => {
            fc.get("arguments").cloned().unwrap_or_else(|| json!({}))
        }
        _ => json!({}),
    };
    let id = fc
        .get("id")
        .and_then(|i| i.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{message_id}_function_call"));
    Some(json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_full_response() {
        let resp = json!({
            "id": "chatcmpl-1", "model": "deepseek-chat",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "It's 22C.",
                    "reasoning_content": "thinking hard",
                    "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 40}}
        });
        let out = chat_response_to_anthropic(&resp, "msg_1").unwrap();
        assert_eq!(out["id"], "msg_1");
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        // 块顺序：thinking → text → tool_use
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "thinking hard");
        assert_eq!(out["content"][1]["type"], "text");
        assert_eq!(out["content"][1]["text"], "It's 22C.");
        assert_eq!(out["content"][2]["type"], "tool_use");
        assert_eq!(out["content"][2]["id"], "call_1");
        assert_eq!(out["content"][2]["name"], "get_weather");
        assert_eq!(out["content"][2]["input"], json!({"city": "Paris"}));
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["stop_sequence"], Value::Null);
        // 三桶恒等式：100 − 40 cached − 0 = 60 fresh input
        assert_eq!(out["usage"]["input_tokens"], 60);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 40);
        assert_eq!(out["usage"]["cache_creation_input_tokens"], 0);
        assert_eq!(stop_reason_from_finish("tool_calls", false), "end_turn"); // 无有效 tool_use 块
    }

    #[test]
    fn stop_reason_matrix() {
        assert_eq!(stop_reason_from_finish("stop", false), "end_turn");
        assert_eq!(stop_reason_from_finish("length", false), "max_tokens");
        assert_eq!(stop_reason_from_finish("tool_calls", true), "tool_use");
        assert_eq!(stop_reason_from_finish("tool_calls", false), "end_turn");
        assert_eq!(stop_reason_from_finish("function_call", true), "tool_use");
        assert_eq!(stop_reason_from_finish("content_filter", false), "end_turn");
        assert_eq!(stop_reason_from_finish("weird", false), "end_turn");
        // 无 finish_reason + tool_use → tool_use
        assert_eq!(stop_reason_from_finish("", true), "tool_use");
        assert_eq!(stop_reason_from_finish("", false), "end_turn");
    }

    #[test]
    fn malformed_arguments_fallback_empty_object() {
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "function": {"name": "f", "arguments": "not-json{"}}]},
                "finish_reason": "tool_calls"}]
        });
        let out = chat_response_to_anthropic(&resp, "msg_1").unwrap();
        assert_eq!(out["content"][0]["input"], json!({}));
    }

    /// P1-10：全部工具调用无名但带载荷 → 不再整只丢弃：unknown_tool 合成
    ///（与流式 M17 口径统一；零载荷才 Err）
    #[test]
    fn nameless_tool_call_with_payload_kept_as_unknown_tool() {
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "function": {"name": "", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"}]
        });
        let out = chat_response_to_anthropic(&resp, "msg_1").unwrap();
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["name"], "unknown_tool");
        assert_eq!(out["stop_reason"], "tool_use");
    }

    /// P1-10：零载荷（无 id/name/arguments）→ 丢弃 + 无其它输出时显式 Err
    #[test]
    fn zero_payload_tool_call_only_fails_guardrail() {
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "", "function": {"name": "", "arguments": ""}}]},
                "finish_reason": "tool_calls"}]
        });
        let err = chat_response_to_anthropic(&resp, "msg_1").unwrap_err();
        assert!(err.contains("no other output"), "{err}");
    }

    /// P1-10：有文本输出时无名工具合成 unknown_tool 保留（与流式口径一致）
    #[test]
    fn nameless_tool_call_tolerated_when_text_present() {
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": "answer",
                "tool_calls": [{"id": "call_1", "function": {"name": "", "arguments": "{}"}}]},
                "finish_reason": "stop"}]
        });
        let out = chat_response_to_anthropic(&resp, "msg_1").unwrap();
        assert_eq!(out["content"][0]["text"], "answer");
        assert_eq!(out["content"][1]["name"], "unknown_tool");
        // P0-5 对齐：stop + tool_use 块 → tool_use（此前 end_turn）
        assert_eq!(out["stop_reason"], "tool_use");
    }

    /// L4：legacy 顶层 function_call 形态 → tool_use 块
    #[test]
    fn legacy_function_call_converted() {
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "function_call": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                "finish_reason": "function_call"}]
        });
        let out = chat_response_to_anthropic(&resp, "msg_9").unwrap();
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["name"], "get_weather");
        assert_eq!(out["content"][0]["id"], "msg_9_function_call");
        assert_eq!(out["content"][0]["input"], json!({"city": "Paris"}));
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn anthropic_style_cache_fields() {
        let u: ChatUsage = serde_json::from_value(json!({
            "prompt_tokens": 200, "completion_tokens": 5,
            "cache_read_input_tokens": 120, "cache_creation_input_tokens": 30
        }))
        .unwrap();
        let out = anthropic_usage(&u);
        assert_eq!(out["input_tokens"], 50); // 200 − 120 − 30
        assert_eq!(out["cache_read_input_tokens"], 120);
        assert_eq!(out["cache_creation_input_tokens"], 30);
    }

    #[test]
    fn deepseek_cache_field() {
        let u: ChatUsage = serde_json::from_value(json!({
            "prompt_tokens": 100, "completion_tokens": 5, "prompt_cache_hit_tokens": 80
        }))
        .unwrap();
        let out = anthropic_usage(&u);
        assert_eq!(out["input_tokens"], 20);
        assert_eq!(out["cache_read_input_tokens"], 80);
    }
}
