//! Chat Completions 响应 → Responses API 响应（非流式）。
//! 补充（按 cc-switch 分析文档重构）：工具参数 canonical 化、无名工具调用护栏、
//! usage 缓存桶归一化、refusal/reasoning 别名兜底。
//!
//! 移植自 new-api `relaykit/relayconvert/internal/oai_chat/to_oai_responses_resp.go`。
//! 语义要点：
//! - `finish_reason` → `status`：length → incomplete/max_output_tokens；content_filter → incomplete/content_filter；其余 completed
//! - 文本 → message output（output_text）；reasoning_content → reasoning output（summary_text）
//! - tool_calls → function_call outputs（call_id 缺失时回退 `{id}_call_{i}`）
//! - usage 双语义搬运（input/output + prompt/completion + details）

use serde_json::{Value, json};

use super::dto::{
    IncompleteDetailsOut, ResponsesOutputContentOut, ResponsesOutputOut, ResponsesResponseOut,
    Usage, empty_annotations, output_item, usage_from_chat,
};

const FINISH_REASON_LENGTH: &str = "length";
const FINISH_REASON_CONTENT_FILTER: &str = "content_filter";
const INCOMPLETE_REASON_MAX_TOKENS: &str = "max_output_tokens";

/// Chat 响应 → Responses 响应。
/// `id`：本网关生成的响应 id（`resp_<uuid>`）；`fallback_created`：上游缺 created 时使用。
/// `tool_ctx`：请求工具上下文（P0-4：custom / namespace / tool_search 还原；
/// 代理层从原始 Responses 请求构建）
pub fn chat_response_to_responses(
    resp: &Value,
    id: &str,
    fallback_created: i64,
    tool_ctx: &super::tool_ctx::ToolContext,
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
        .filter(|u| u.is_object())
        .map(Usage::from_value_lenient);

    let finish_reason = resp
        .pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let (mut status, incomplete_details) = finish_reason_to_status(&finish_reason);

    let message = resp.pointer("/choices/0/message");
    // P1-2：content parts 数组里的 refusal part 载荷不再丢失（与 message 级
    // refusal 一并产出独立 refusal part；cc-switch transform_codex_chat.rs
    // :1527-1536 同款）
    let (mut text, part_refusals) = message
        .map(extract_message_text_and_refusals)
        .unwrap_or_default();
    let mut reasoning = message.map(extract_message_reasoning).unwrap_or_default();
    // M1：非流式正文开头的 <think>…</think> 分离为 reasoning（与流式路径
    // InlineThinkState 行为对齐；此前思考污染 output_text 且计费口径失真）
    if let Some((think, rest)) = crate::service::inline_think::split_leading_think(&text) {
        reasoning = if reasoning.is_empty() {
            think
        } else {
            format!("{reasoning}\n{think}")
        };
        text = rest.trim().to_string();
    }
    let mut output: Vec<ResponsesOutputOut> = Vec::new();
    let output_status = if status == "incomplete" {
        "incomplete"
    } else {
        "completed"
    };

    // L1：输出顺序 reasoning → message → tools（与真实生成序一致，
    // cc-switch transform_codex_chat.rs:1460-1470 reasoning_pos < message_pos）
    if !reasoning.is_empty() {
        let mut item = output_item(
            "reasoning",
            format!("{id}_reasoning_0"),
            output_status,
            Vec::new(),
        );
        item.summary = vec![super::dto::summary_part("summary_text", &reasoning)];
        output.push(item);
    }
    if !text.is_empty() || !part_refusals.is_empty() {
        // L2 / P1-1：refusal part 使用真实 API 的 `refusal` 载荷字段
        //（此前载荷放 `text` 字段，SDK 读 `refusal` 拿空）；message 级与
        // part 级 refusal 各产出独立 part
        let mut parts = vec![ResponsesOutputContentOut {
            r#type: "output_text".into(),
            text,
            refusal: String::new(),
            annotations: empty_annotations(),
        }];
        if let Some(r) = message
            .and_then(|m| m.get("refusal"))
            .and_then(|r| r.as_str())
            .filter(|r| !r.trim().is_empty())
        {
            parts.push(ResponsesOutputContentOut {
                r#type: "refusal".into(),
                text: String::new(),
                refusal: r.to_string(),
                annotations: empty_annotations(),
            });
        }
        for r in part_refusals {
            parts.push(ResponsesOutputContentOut {
                r#type: "refusal".into(),
                text: String::new(),
                refusal: r,
                annotations: empty_annotations(),
            });
        }
        output.push(output_item(
            "message",
            format!("{id}_msg_0"),
            output_status,
            parts,
        ));
        // message 输出带 role（与 Go 一致：message output 有 role，reasoning/tool 无）
        if let Some(item) = output.last_mut() {
            item.role = "assistant".into();
        }
    }
    // 工具调用护栏（cc-switch #4341）：缺函数名的 tool_call 无法执行，静默保留会让
    // agent loop 拿到"零工具调用的 completed"而无声卡死 —— 丢弃并在全部被丢弃且
    // 无其他输出时报错（length 截断豁免：那是截断而非畸形数据）
    let mut dropped_tools = 0usize;
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for (i, tc) in tool_calls.iter().enumerate() {
            let name = tc
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match name {
                // P1-5：非流式工具 item 同样附挂 message 级思考（与流式路径
                // L6 对齐；cc-switch :1715-1730 传 reasoning 同款）
                Some(_) => output.push(chat_tool_call_to_responses_output(
                    tc,
                    id,
                    i,
                    output_status,
                    (!reasoning.trim().is_empty()).then(|| reasoning.trim()),
                    tool_ctx,
                )),
                None => {
                    dropped_tools += 1;
                    tracing::warn!(
                        index = i,
                        "dropping upstream tool_call without function name"
                    );
                }
            }
        }
    }
    // L3：legacy `message.function_call` 兜底（无 tool_calls 时；与流式路径
    // lenient_tool_call 兼容对齐，老式上游工具调用不再静默丢失）
    if output
        .iter()
        .all(|o| o.r#type != "function_call" && o.r#type != "custom_tool_call")
    {
        if let Some(fc) = message.and_then(|m| m.get("function_call")) {
            let name = fc
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(name) = name {
                let arguments = fc
                    .get("arguments")
                    .map(|a| match a {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                let legacy = json!({
                    "id": fc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                });
                output.push(chat_tool_call_to_responses_output(
                    &legacy,
                    id,
                    output.len(),
                    output_status,
                    (!reasoning.trim().is_empty()).then(|| reasoning.trim()),
                    tool_ctx,
                ));
            }
        }
    }
    if dropped_tools > 0 && output.is_empty() && status != "incomplete" {
        status = "failed".into();
    }

    let error = if status == "failed" {
        Some(json!({
            "code": "upstream_tool_call_dropped",
            "message": "upstream returned tool_calls without function name and no other output"
        }))
    } else {
        None
    };

    let out = ResponsesResponseOut {
        id: id.to_string(),
        object: "response",
        created_at: created,
        status,
        error,
        incomplete_details,
        instructions: None,
        // L6：不回显请求参数（此前硬编码 0/false 是误导性伪值）
        max_output_tokens: None,
        model,
        output,
        parallel_tool_calls: None,
        previous_response_id: None,
        reasoning: None,
        store: None,
        temperature: None,
        tool_choice: None,
        tools: None,
        top_p: None,
        truncation: None,
        usage: usage.map(|u| usage_from_chat(&u)),
        user: None,
        metadata: None,
    };
    serde_json::to_value(out.with_zero_usage())
        .map_err(|e| format!("serialize responses response: {e}"))
}

/// finish_reason → (status, incomplete_details)
pub fn finish_reason_to_status(finish_reason: &str) -> (String, Option<IncompleteDetailsOut>) {
    match finish_reason.trim() {
        // 兼容部分供应商的非标准截断变体（max_tokens/token_limit）
        FINISH_REASON_LENGTH | "max_tokens" | "token_limit" => (
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

/// 消息 content → (正文, part 级 refusal 载荷列表)。
/// string 原样；parts 数组拼接 text part（P1-2：refusal part 载荷独立返回，
/// 不再随 text 丢弃）；content 为空时退回 message 级 refusal 作正文兜底。
pub fn extract_message_text_and_refusals(message: &Value) -> (String, Vec<String>) {
    let mut refusals = Vec::new();
    let from_content = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("output_text") => {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            out.push_str(text);
                        }
                    }
                    Some("refusal") => {
                        if let Some(r) = part
                            .get("refusal")
                            .or_else(|| part.get("text"))
                            .and_then(|t| t.as_str())
                            .filter(|r| !r.trim().is_empty())
                        {
                            refusals.push(r.to_string());
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    };
    if from_content.is_empty() {
        // content 为空时退回 message 级 refusal（cc-switch 同款：refusal → 文本输出）
        let fallback = message
            .get("refusal")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        (fallback, refusals)
    } else {
        (from_content, refusals)
    }
}

/// 消息 reasoning_content → 纯文本（`reasoning` / `reasoning_details[]` 别名兜底；
/// M1：reasoning_details 为 xAI Grok 等上游的数组形态 [{text:…}]）
pub fn extract_message_reasoning(message: &Value) -> String {
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

/// Chat tool_call → Responses function_call / custom_tool_call / tool_search_call /
/// namespace function_call output。
/// P0-4：按请求工具上下文还原——custom 名单 → custom_tool_call（ctc_ 前缀、
/// input 从降级参数还原）；tool_search 代理名 → tool_search_call item；
/// namespace 拍平名 → 还原原始短名 + namespace 字段
///（cc-switch response_tool_call_item_from_chat_name :1752-1781 同款）
fn chat_tool_call_to_responses_output(
    tc: &Value,
    response_id: &str,
    index: usize,
    status: &str,
    reasoning: Option<&str>,
    tool_ctx: &super::tool_ctx::ToolContext,
) -> ResponsesOutputOut {
    use super::tool_ctx::ToolKind;

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
    // arguments 以 JSON 字符串输出（`"{\"city\":\"Paris\"}"`）；
    // 部分上游发 object 形态，用 arguments_string 序列化；可解析时 canonical 化
    //（键序稳定，cc-switch canonicalize_tool_arguments_str 同款——保上游前缀缓存）
    let arguments = tc
        .pointer("/function/arguments")
        .map(super::dto::arguments_string)
        .map(|s| crate::service::canonical::canonicalize_json_string_if_parseable(&s))
        .unwrap_or_default();
    let reasoning_content = reasoning
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string);

    if let Some(spec) = tool_ctx.lookup(&name) {
        match spec.kind {
            ToolKind::Custom => {
                // M6：请求声明的 custom 工具 → custom_tool_call item
                return ResponsesOutputOut {
                    r#type: "custom_tool_call".into(),
                    id: format!("ctc_{call_id}"),
                    status: status.to_string(),
                    role: String::new(),
                    content: Vec::new(),
                    summary: Vec::new(),
                    quality: String::new(),
                    size: String::new(),
                    call_id: Some(call_id),
                    name: Some(name),
                    namespace: None,
                    arguments: None,
                    reasoning_content,
                    query: None,
                    input: Some(super::dto::custom_tool_input_from_chat_arguments(
                        &arguments,
                    )),
                    execution: None,
                };
            }
            ToolKind::ToolSearch => {
                // P0-4：tool_search 代理调用 → tool_search_call item
                //（execution 恒 client；arguments 解析为对象形态，
                // cc-switch response_tool_search_call_item :1783-1799 同款）
                return ResponsesOutputOut {
                    r#type: "tool_search_call".into(),
                    id: String::new(),
                    status: status.to_string(),
                    role: String::new(),
                    content: Vec::new(),
                    summary: Vec::new(),
                    quality: String::new(),
                    size: String::new(),
                    call_id: Some(call_id),
                    name: None,
                    namespace: None,
                    arguments: Some(parse_tool_arguments_object(&arguments)),
                    reasoning_content,
                    query: None,
                    input: None,
                    execution: Some("client".into()),
                };
            }
            ToolKind::Namespace => {
                // P0-4：拍平名 → 还原原始短名 + namespace 字段
                return ResponsesOutputOut {
                    r#type: "function_call".into(),
                    id: format!("fc_{call_id}"),
                    status: status.to_string(),
                    role: String::new(),
                    content: Vec::new(),
                    summary: Vec::new(),
                    quality: String::new(),
                    size: String::new(),
                    call_id: Some(call_id),
                    name: Some(spec.name.clone()),
                    namespace: spec.namespace.clone(),
                    arguments: Some(Value::String(arguments)),
                    reasoning_content,
                    query: None,
                    input: None,
                    execution: None,
                };
            }
            ToolKind::Function => {}
        }
    }

    ResponsesOutputOut {
        r#type: if tool_type.is_empty() || tool_type == "function" {
            "function_call".into()
        } else {
            tool_type.to_string()
        },
        // L4：与真实 API id 形态一致（fc_ 前缀；call_id 保持原值可寻址）
        id: format!("fc_{call_id}"),
        status: status.to_string(),
        role: String::new(),
        content: Vec::new(),
        summary: Vec::new(),
        quality: String::new(),
        size: String::new(),
        call_id: Some(call_id),
        name: Some(name),
        namespace: None,
        arguments: Some(Value::String(arguments)),
        reasoning_content,
        query: None,
        input: None,
        execution: None,
    }
}

/// tool_search 参数字符串 → 对象形态（空 → {}；非对象 → {"query": <str>}）
///（P0-4：非流式与流式共用，stream.rs 同款调用）
pub(crate) fn parse_tool_arguments_object(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({ "query": arguments }))
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
        let out = chat_response_to_responses(
            &chat_resp(),
            "resp_fixed",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["id"], "resp_fixed");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["created_at"], 1700000000);
        // L1：输出顺序 reasoning → message（真实生成序）
        assert_eq!(out["output"][0]["type"], "reasoning");
        assert_eq!(out["output"][0]["summary"][0]["text"], "Deep thought.");
        assert_eq!(out["output"][1]["type"], "message");
        assert_eq!(out["output"][1]["role"], "assistant");
        assert_eq!(out["output"][1]["id"], "resp_fixed_msg_0");
        assert_eq!(out["output"][1]["content"][0]["type"], "output_text");
        assert_eq!(out["output"][1]["content"][0]["text"], "The answer is 42.");
        assert_eq!(out["output"][1]["content"][0]["annotations"], json!([]));
        // reasoning output（L1：已位于 output[0]）
        assert_eq!(out["output"][0]["id"], "resp_fixed_reasoning_0");
        assert_eq!(out["output"][0]["summary"][0]["type"], "summary_text");
        assert_eq!(out["output"][0]["summary"][0]["text"], "Deep thought.");
        assert!(out["output"][0]["content"].as_array().unwrap().is_empty());
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
        // 标准字段面（SDK 兼容）；L6：不回显请求参数 → store/instructions 等缺失
        assert!(out.get("store").is_none(), "L6：store 不再回显");
        assert!(out["instructions"].is_null());
        assert!(out["previous_response_id"].is_null());
    }

    #[test]
    fn maps_finish_reason_length_to_incomplete() {
        let mut resp = chat_resp();
        resp["choices"][0]["finish_reason"] = json!("length");
        resp["choices"][0]["message"]["content"] = json!("partial");
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
        // incomplete 时输出 item 状态同步
        assert_eq!(out["output"][0]["status"], "incomplete");
    }

    #[test]
    fn fallback_call_id_and_arguments() {
        let mut resp = chat_resp();
        resp["choices"][0]["message"]["tool_calls"][0]
            .as_object_mut()
            .unwrap()
            .remove("id");
        resp["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!("");
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["output"][2]["id"], "fc_resp_x_call_0");
        assert_eq!(out["output"][2]["call_id"], "resp_x_call_0");
        assert_eq!(out["output"][2]["arguments"], json!(""));
    }

    #[test]
    fn empty_choices_still_valid() {
        let resp = json!({"id": "x", "object": "chat.completion", "model": "m",
                          "choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}});
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"], json!([]));
        assert_eq!(out["usage"]["input_tokens"], 1);
    }

    #[test]
    fn tool_calls_finish_reason_not_overridden_by_stop() {
        // finish_reason=tool_calls → completed（Go 语义：仅 length/content_filter 映射 incomplete）
        let out = chat_response_to_responses(
            &chat_resp(),
            "resp_fixed",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["status"], "completed");
    }

    #[test]
    fn all_tool_calls_dropped_without_name_yields_failed() {
        // 上游畸形：tool_call 缺函数名且无任何其他输出 → failed（cc-switch #4341 护栏）
        let resp = json!({
            "id": "chatcmpl-bad", "model": "m",
            "choices": [{
                "message": {"role": "assistant", "content": null,
                    "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["status"], "failed");
        assert_eq!(out["error"]["code"], "upstream_tool_call_dropped");
        assert_eq!(out["output"], json!([]));
    }

    #[test]
    fn dropped_tools_tolerated_when_text_present() {
        // 有文本输出时静默丢弃无名工具调用，不报 failed
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{
                "message": {"role": "assistant", "content": "answer",
                    "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "", "arguments": "{}"}}]},
                "finish_reason": "stop"
            }],
            "usage": null
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"].as_array().unwrap().len(), 1);
        assert_eq!(out["output"][0]["type"], "message");
    }

    #[test]
    fn usage_cache_buckets_normalized() {
        // 多源缓存字段归一化：prompt_tokens_details.cached_tokens 直传
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5,
                      "prompt_tokens_details": {"cached_tokens": 60}}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 60);

        // DeepSeek 风格 prompt_cache_hit_tokens（无 details）→ 合成 cached_tokens
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5, "prompt_cache_hit_tokens": 40}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 40);

        // Anthropic 风格直传 cache_read/cache_creation
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5,
                      "cache_read_input_tokens": 70, "cache_creation_input_tokens": 10}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 70);
        assert_eq!(
            out["usage"]["input_tokens_details"]["cache_write_tokens"],
            10
        );
    }

    #[test]
    fn refusal_fallback_when_content_empty() {
        let resp = json!({
            "id": "c", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null, "refusal": "cannot help"}, "finish_reason": "stop"}]
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["output"][0]["content"][0]["text"], "cannot help");
    }

    /// M6：custom 工具还原 —— 命中请求名单的调用输出 custom_tool_call + input 还原
    #[test]
    fn custom_tool_call_restored_from_downgraded_function() {
        let resp = json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_9", "type": "function",
                    "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** patch ***\"}"}}]},
                "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::from_request(
                &serde_json::json!({"tools": [{"type": "custom", "name": "apply_patch"}]}),
            ),
        )
        .unwrap();
        assert_eq!(out["output"][0]["type"], "custom_tool_call");
        assert_eq!(out["output"][0]["id"], "ctc_call_9");
        assert_eq!(out["output"][0]["call_id"], "call_9");
        assert_eq!(out["output"][0]["name"], "apply_patch");
        assert_eq!(out["output"][0]["input"], "*** patch ***");
        assert!(
            out["output"][0].get("arguments").is_none(),
            "custom item 无 arguments 字段"
        );
        // 未命中名单的同名调用保持 function_call
        let out2 = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out2["output"][0]["type"], "function_call");
        assert_eq!(
            out2["output"][0]["arguments"],
            json!("{\"input\":\"*** patch ***\"}")
        );
    }

    /// M1：非流式正文开头的 <think> 块分离为 reasoning，正文剥离
    #[test]
    fn leading_think_block_split_nonstream() {
        let resp = json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"message": {"role": "assistant",
                "content": "<think>step by step</think>The answer."}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        let types: Vec<&str> = out["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["reasoning", "message"]);
        assert_eq!(out["output"][0]["summary"][0]["text"], "step by step");
        assert_eq!(out["output"][1]["content"][0]["text"], "The answer.");
    }

    /// M1：reasoning_details 数组别名（xAI Grok 形态）提取
    #[test]
    fn reasoning_details_alias_extracted() {
        let resp = json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "grok-4",
            "choices": [{"message": {"role": "assistant", "content": "hi",
                "reasoning_details": [{"type": "reasoning_text", "text": "ponder"}]},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = chat_response_to_responses(
            &resp,
            "resp_x",
            0,
            &crate::service::responses::tool_ctx::ToolContext::default(),
        )
        .unwrap();
        assert_eq!(out["output"][0]["type"], "reasoning");
        assert_eq!(out["output"][0]["summary"][0]["text"], "ponder");
    }

    /// P0-4：namespace 工具全链路——请求拍平 `ns__name`、响应还原原始短名 +
    /// namespace 字段（cc-switch response_function_call_item_with_namespace 同款）
    #[test]
    fn namespace_tool_round_trips() {
        let req = json!({
            "model": "m",
            "tools": [{"type": "namespace", "name": "mcp__s", "tools": [
                {"type": "function", "name": "q", "parameters": {"type": "object"}}
            ]}]
        });
        let ctx = crate::service::responses::tool_ctx::ToolContext::from_request(&req);
        let chat =
            crate::service::responses::convert_req::responses_request_to_chat(&req, "m").unwrap();
        assert_eq!(chat["tools"][0]["function"]["name"], "mcp__s__q");
        let resp = json!({
            "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "mcp__s__q", "arguments": "{\"k\":1}"}}]},
                "finish_reason": "tool_calls"}]
        });
        let out = chat_response_to_responses(&resp, "r1", 0, &ctx).unwrap();
        assert_eq!(out["output"][0]["type"], "function_call");
        assert_eq!(out["output"][0]["name"], "q");
        assert_eq!(out["output"][0]["namespace"], "mcp__s");
        assert_eq!(out["output"][0]["id"], "fc_c1");
    }

    /// P0-4：tool_search 代理调用 → tool_search_call item（execution=client、
    /// arguments 对象形态；cc-switch response_tool_search_call_item 同款）
    #[test]
    fn tool_search_proxy_call_restored() {
        let req = json!({"model": "m", "tools": [{"type": "tool_search"}]});
        let ctx = crate::service::responses::tool_ctx::ToolContext::from_request(&req);
        let resp = json!({
            "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "c2", "type": "function",
                    "function": {"name": "tool_search", "arguments": "{\"query\":\"gmail\"}"}}]},
                "finish_reason": "tool_calls"}]
        });
        let out = chat_response_to_responses(&resp, "r2", 0, &ctx).unwrap();
        assert_eq!(out["output"][0]["type"], "tool_search_call");
        assert_eq!(out["output"][0]["execution"], "client");
        assert_eq!(out["output"][0]["arguments"], json!({"query": "gmail"}));
        assert_eq!(out["output"][0]["call_id"], "c2");
    }

    /// P0-2：tool_choice custom 形态改写（此前原样透传非法形态 → 严格上游 400）
    #[test]
    fn tool_choice_custom_mapped_to_function() {
        let req = json!({
            "model": "m",
            "tools": [{"type": "custom", "name": "apply_patch"}],
            "tool_choice": {"type": "custom", "name": "apply_patch"}
        });
        let chat =
            crate::service::responses::convert_req::responses_request_to_chat(&req, "m").unwrap();
        assert_eq!(
            chat["tool_choice"],
            json!({"type": "function", "function": {"name": "apply_patch"}})
        );
    }
}
