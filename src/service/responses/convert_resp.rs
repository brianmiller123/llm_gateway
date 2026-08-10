//! Chat Completions 响应 → Responses API 响应（非流式）。
//!
//! 移植自 new-api `relaykit/relayconvert/internal/oai_chat/to_oai_responses_resp.go`。
//! 语义要点：
//! - `finish_reason` → `status`：length → incomplete/max_output_tokens；content_filter → incomplete/content_filter；其余 completed
//! - 文本 → message output（output_text）；reasoning_content → reasoning output（summary_text）
//! - tool_calls → function_call outputs（call_id 缺失时回退 `{id}_call_{i}`）
//! - usage 双语义搬运（input/output + prompt/completion + details）

use serde_json::Value;

use super::dto::{
    empty_annotations, output_item, usage_from_chat, IncompleteDetailsOut, ResponsesOutputContentOut,
    ResponsesOutputOut, ResponsesResponseOut, Usage,
};

const FINISH_REASON_LENGTH: &str = "length";
const FINISH_REASON_CONTENT_FILTER: &str = "content_filter";
const INCOMPLETE_REASON_MAX_TOKENS: &str = "max_output_tokens";

/// Chat 响应 → Responses 响应。
/// `id`：本网关生成的响应 id（`resp_<uuid>`）；`fallback_created`：上游缺 created 时使用。
pub fn chat_response_to_responses(
    resp: &Value,
    id: &str,
    fallback_created: i64,
) -> Result<Value, String> {
    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let created = resp
        .get("created")
        .and_then(|c| c.as_i64())
        .unwrap_or(fallback_created);
    let usage: Option<Usage> = resp
        .get("usage")
        .and_then(|u| serde_json::from_value(u.clone()).ok());

    let (status, incomplete_details) = finish_reason_to_status(
        resp.pointer("/choices/0/finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or(""),
    );

    let message = resp.pointer("/choices/0/message");
    let text = message.map(extract_message_text).unwrap_or_default();
    let reasoning = message
        .map(extract_message_reasoning)
        .unwrap_or_default();

    let mut output: Vec<ResponsesOutputOut> = Vec::new();
    let output_status = if status == "incomplete" { "incomplete" } else { "completed" };

    if !text.is_empty() {
        output.push(output_item(
            "message",
            format!("{id}_msg_0"),
            output_status,
            vec![ResponsesOutputContentOut {
                r#type: "output_text".into(),
                text,
                annotations: empty_annotations(),
            }],
        ));
        // message 输出带 role（与 Go 一致：message output 有 role，reasoning/tool 无）
        if let Some(item) = output.last_mut() {
            item.role = "assistant".into();
        }
    }
    if !reasoning.is_empty() {
        output.push(output_item(
            "reasoning",
            format!("{id}_reasoning_0"),
            output_status,
            vec![ResponsesOutputContentOut {
                r#type: "summary_text".into(),
                text: reasoning,
                annotations: empty_annotations(),
            }],
        ));
    }
    if let Some(tool_calls) = message.and_then(|m| m.get("tool_calls")).and_then(|t| t.as_array()) {
        for (i, tc) in tool_calls.iter().enumerate() {
            output.push(chat_tool_call_to_responses_output(tc, id, i, output_status));
        }
    }

    let out = ResponsesResponseOut {
        id: id.to_string(),
        object: "response",
        created_at: created,
        status,
        error: None,
        incomplete_details,
        instructions: None,
        max_output_tokens: 0,
        model,
        output,
        parallel_tool_calls: false,
        previous_response_id: None,
        reasoning: None,
        store: false,
        temperature: 0.0,
        tool_choice: None,
        tools: None,
        top_p: 0.0,
        truncation: None,
        usage: usage.map(|u| usage_from_chat(&u)),
        user: None,
        metadata: None,
    };
    serde_json::to_value(out.with_zero_usage()).map_err(|e| format!("serialize responses response: {e}"))
}

/// finish_reason → (status, incomplete_details)
pub fn finish_reason_to_status(finish_reason: &str) -> (String, Option<IncompleteDetailsOut>) {
    match finish_reason.trim() {
        FINISH_REASON_LENGTH => (
            "incomplete".into(),
            Some(IncompleteDetailsOut {
                reason: INCOMPLETE_REASON_MAX_TOKENS.into(),
            }),
        ),
        FINISH_REASON_CONTENT_FILTER => (
            "incomplete".into(),
            Some(IncompleteDetailsOut {
                reason: FINISH_REASON_CONTENT_FILTER.into(),
            }),
        ),
        _ => ("completed".into(), None),
    }
}

/// 消息 content → 纯文本（string 原样；parts 数组拼接 text part；其余忽略）
pub fn extract_message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        out.push_str(text);
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 消息 reasoning_content → 纯文本
pub fn extract_message_reasoning(message: &Value) -> String {
    message
        .get("reasoning_content")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string()
}

/// Chat tool_call → Responses function_call output
fn chat_tool_call_to_responses_output(tc: &Value, response_id: &str, index: usize, status: &str) -> ResponsesOutputOut {
    let call_id = tc
        .get("id")
        .and_then(|i| i.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{response_id}_call_{index}"));
    let tool_type = tc
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("function");
    let name = tc
        .pointer("/function/name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    // arguments 以 JSON 字符串输出（`"{\"city\":\"Paris\"}"`）
    let arguments = tc
        .pointer("/function/arguments")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();

    ResponsesOutputOut {
        r#type: if tool_type.is_empty() || tool_type == "function" {
            "function_call".into()
        } else {
            tool_type.to_string()
        },
        id: call_id.clone(),
        status: status.to_string(),
        role: String::new(),
        content: Vec::new(),
        quality: String::new(),
        size: String::new(),
        call_id: Some(call_id),
        name: Some(name),
        arguments: Some(Value::String(arguments)),
    }
}

/// Chat usage → Responses usage（供流式终态/非流式共用）

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chat_resp() -> Value {
        json!({
            "id": "chatcmpl-fixed",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning_content": "Deep thought.",
                    "tool_calls": [{"id": "call_abc", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })
    }

    #[test]
    fn converts_chat_response_to_responses() {
        let out = chat_response_to_responses(&chat_resp(), "resp_fixed", 0).unwrap();
        assert_eq!(out["id"], "resp_fixed");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["created_at"], 1700000000);
        // message output（含 role、annotations 空数组）
        assert_eq!(out["output"][0]["type"], "message");
        assert_eq!(out["output"][0]["role"], "assistant");
        assert_eq!(out["output"][0]["id"], "resp_fixed_msg_0");
        assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(out["output"][0]["content"][0]["text"], "The answer is 42.");
        assert_eq!(out["output"][0]["content"][0]["annotations"], json!([]));
        // reasoning output
        assert_eq!(out["output"][1]["type"], "reasoning");
        assert_eq!(out["output"][1]["id"], "resp_fixed_reasoning_0");
        assert_eq!(out["output"][1]["content"][0]["type"], "summary_text");
        assert_eq!(out["output"][1]["content"][0]["text"], "Deep thought.");
        // function_call output
        assert_eq!(out["output"][2]["type"], "function_call");
        assert_eq!(out["output"][2]["call_id"], "call_abc");
        assert_eq!(out["output"][2]["name"], "get_weather");
        assert_eq!(out["output"][2]["arguments"], json!("{\"city\":\"Paris\"}"));
        // usage 双语义
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 5);
        assert_eq!(out["usage"]["prompt_tokens"], 10);
        assert_eq!(out["usage"]["completion_tokens"], 5);
        assert_eq!(out["usage"]["total_tokens"], 15);
        // 标准字段面（SDK 兼容）
        assert_eq!(out["store"], false);
        assert!(out["instructions"].is_null());
        assert!(out["previous_response_id"].is_null());
    }

    #[test]
    fn maps_finish_reason_length_to_incomplete() {
        let mut resp = chat_resp();
        resp["choices"][0]["finish_reason"] = json!("length");
        resp["choices"][0]["message"]["content"] = json!("partial");
        let out = chat_response_to_responses(&resp, "resp_x", 0).unwrap();
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
        // incomplete 时输出 item 状态同步
        assert_eq!(out["output"][0]["status"], "incomplete");
    }

    #[test]
    fn fallback_call_id_and_arguments() {
        let mut resp = chat_resp();
        resp["choices"][0]["message"]["tool_calls"][0].as_object_mut().unwrap().remove("id");
        resp["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!("");
        let out = chat_response_to_responses(&resp, "resp_x", 0).unwrap();
        assert_eq!(out["output"][2]["id"], "resp_x_call_0");
        assert_eq!(out["output"][2]["call_id"], "resp_x_call_0");
        assert_eq!(out["output"][2]["arguments"], json!(""));
    }

    #[test]
    fn empty_choices_still_valid() {
        let resp = json!({"id": "x", "object": "chat.completion", "model": "m",
                          "choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}});
        let out = chat_response_to_responses(&resp, "resp_x", 0).unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"], json!([]));
        assert_eq!(out["usage"]["input_tokens"], 1);
    }

    #[test]
    fn tool_calls_finish_reason_not_overridden_by_stop() {
        // finish_reason=tool_calls → completed（Go 语义：仅 length/content_filter 映射 incomplete）
        let out = chat_response_to_responses(&chat_resp(), "resp_fixed", 0).unwrap();
        assert_eq!(out["status"], "completed");
    }
}
