//! Responses 请求 → Chat Completions 请求。
//!
//! 移植自 new-api `relaykit/relayconvert/internal/oai_responses/to_oai_chat_req.go`。
//! 语义要点：
//! - `instructions` → system 消息；`input` 字符串 → user 消息；input item 数组逐项拆解
//! - `function_call` / `custom_tool_call` item 合并进最后一条 assistant 消息的 tool_calls
//! - `function_call_output` → tool 消息（call_id 缺失不报错，与 Go 一致）
//! - 有状态字段（conversation / previous_response_id / prompt / context_management）显式拒绝
//! - tools / tool_choice / text.format 结构改写为 Chat 形态

use serde_json::{json, Map, Value};

use super::dto::{
    self, arguments_string, present, value_to_string, ChatRequestOut, FunctionOut, MessageOut,
    ToolCallOut,
};

const INPUT_TYPE_FUNCTION_CALL: &str = "function_call";
const INPUT_TYPE_FUNCTION_CALL_OUTPUT: &str = "function_call_output";
const INPUT_TYPE_CUSTOM_TOOL_CALL: &str = "custom_tool_call";

/// Responses 请求 → Chat Completions 请求（JSON Value）。
/// 失败返回 BadRequest 语义的错误消息。
pub fn responses_request_to_chat(req: &Value) -> Result<Value, String> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim();
    if model.is_empty() {
        return Err("model is required".into());
    }
    validate_unsupported_fields(req)?;

    let messages = request_messages_to_chat(req)?;
    let tools = request_tools_to_chat(req.get("tools"))?;
    let tool_choice = request_tool_choice_to_chat(req.get("tool_choice"))?;
    let response_format = request_text_to_chat_response_format(req.get("text"))?;

    let out = ChatRequestOut {
        model: model.to_string(),
        messages,
        stream: req.get("stream").and_then(|s| s.as_bool()),
        stream_options: raw_present(req.get("stream_options")).cloned(),
        max_completion_tokens: req
            .get("max_output_tokens")
            .and_then(|n| n.as_u64())
            .filter(|n| *n != 0),
        temperature: req.get("temperature").and_then(|v| v.as_f64()),
        top_p: req.get("top_p").and_then(|v| v.as_f64()),
        top_logprobs: req.get("top_logprobs").and_then(|v| v.as_i64()),
        response_format,
        tools,
        tool_choice,
        parallel_tool_calls: req
            .get("parallel_tool_calls")
            .and_then(|v| v.as_bool()),
        user: raw_present(req.get("user")).cloned(),
        store: raw_present(req.get("store")).cloned(),
        metadata: raw_present(req.get("metadata")).cloned(),
        reasoning_effort: req
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|e| e.as_str())
            .filter(|e| !e.is_empty())
            .map(str::to_string),
        service_tier: req
            .get("service_tier")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_string())),
        safety_identifier: raw_present(req.get("safety_identifier")).cloned(),
        prompt_cache_retention: raw_present(req.get("prompt_cache_retention")).cloned(),
        prompt_cache_key: req
            .get("prompt_cache_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        enable_thinking: raw_present(req.get("enable_thinking")).cloned(),
        thinking_budget: raw_present(req.get("thinking_budget")).cloned(),
    };
    serde_json::to_value(out).map_err(|e| format!("serialize chat request: {e}"))
}

/// 字段存在且非 null
fn raw_present(v: Option<&Value>) -> Option<&Value> {
    if present(v) {
        v
    } else {
        None
    }
}

/// 有状态字段无法映射到无状态 Chat API：显式拒绝（对应 Go `validateResponsesRequestChatUnsupportedFields`）
fn validate_unsupported_fields(req: &Value) -> Result<(), String> {
    let mut unsupported: Vec<&str> = Vec::new();
    if present(req.get("conversation")) {
        unsupported.push("conversation");
    }
    if let Some(pid) = req.get("previous_response_id").and_then(|v| v.as_str()) {
        if !pid.trim().is_empty() {
            unsupported.push("previous_response_id");
        }
    }
    if present(req.get("prompt")) {
        unsupported.push("prompt");
    }
    if present(req.get("context_management")) {
        unsupported.push("context_management");
    }
    if !unsupported.is_empty() {
        return Err(format!(
            "responses to chat conversion does not support stateful fields: {}",
            unsupported.join(", ")
        ));
    }
    Ok(())
}

/// instructions + input → messages
fn request_messages_to_chat(req: &Value) -> Result<Vec<MessageOut>, String> {
    let mut messages: Vec<MessageOut> = Vec::new();
    if let Some(instructions) = raw_present(req.get("instructions")) {
        let text = value_to_string(instructions);
        if !text.trim().is_empty() {
            messages.push(MessageOut {
                role: "system".into(),
                content: Some(Value::String(text)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
    }

    let Some(input) = raw_present(req.get("input")) else {
        return Ok(messages);
    };
    match input {
        Value::String(s) => {
            messages.push(MessageOut {
                role: "user".into(),
                content: Some(Value::String(s.clone())),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
        Value::Array(items) => {
            for item in items {
                messages = input_item_to_chat_messages(item, messages)?;
            }
        }
        other => {
            return Err(format!(
                "unsupported responses input type {:?}",
                dto::json_type_name(other)
            ));
        }
    }
    Ok(messages)
}

/// 单个 input item → 0..n 条消息
fn input_item_to_chat_messages(item: &Value, mut messages: Vec<MessageOut>) -> Result<Vec<MessageOut>, String> {
    let item_type = item
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    match item_type {
        INPUT_TYPE_FUNCTION_CALL => {
            let tool = function_call_item_to_chat_tool_call(item)?;
            append_tool_call_to_last_assistant(&mut messages, tool);
        }
        INPUT_TYPE_CUSTOM_TOOL_CALL => {
            let tool = custom_tool_call_item_to_chat_tool_call(item)?;
            append_tool_call_to_last_assistant(&mut messages, tool);
        }
        INPUT_TYPE_FUNCTION_CALL_OUTPUT => {
            let call_id = item
                .get("call_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let content = tool_output_to_chat_content(item.get("output"));
            messages.push(MessageOut {
                role: "tool".into(),
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id),
            });
        }
        _ => {
            let role = item
                .get("role")
                .and_then(|r| r.as_str())
                .filter(|r| !r.trim().is_empty())
                .unwrap_or("user");
            let content = input_content_to_chat_content(item.get("content"))?;
            messages.push(MessageOut {
                role: role.to_string(),
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
    }
    Ok(messages)
}

/// content → Chat content（string / parts 数组 / 原样透传）
fn input_content_to_chat_content(content: Option<&Value>) -> Result<Value, String> {
    let Some(content) = content else {
        return Ok(Value::String(String::new()));
    };
    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(parts) => content_parts_to_chat_content(parts),
        // 对象/其他：Go 默认分支原样透传
        _ => Ok(content.clone()),
    }
}

/// content parts 数组 → Chat parts；纯文本时收敛为字符串
fn content_parts_to_chat_content(parts: &[Value]) -> Result<Value, String> {
    let mut chat_parts: Vec<Value> = Vec::with_capacity(parts.len());
    let mut text_only = String::new();
    let mut only_text = true;

    for raw_part in parts {
        let Some(part) = raw_part.as_object() else {
            only_text = false;
            chat_parts.push(raw_part.clone());
            continue;
        };
        let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match part_type {
            "input_text" | "output_text" | "text" => {
                let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                text_only.push_str(text);
                chat_parts.push(json!({"type": "text", "text": text}));
            }
            "input_image" => {
                only_text = false;
                chat_parts.push(json!({
                    "type": "image_url",
                    "image_url": image_part_to_chat_image_url(part)
                }));
            }
            "input_file" => {
                only_text = false;
                chat_parts.push(json!({
                    "type": "file",
                    "file": file_part_to_chat_file(part)
                }));
            }
            "input_audio" => {
                only_text = false;
                chat_parts.push(json!({
                    "type": "input_audio",
                    "input_audio": part_payload(part, "input_audio")
                }));
            }
            "input_video" => {
                only_text = false;
                chat_parts.push(json!({
                    "type": "video_url",
                    "video_url": video_part_to_chat_video_url(part)
                }));
            }
            _ => {
                only_text = false;
                chat_parts.push(Value::Object(part.clone()));
            }
        }
    }

    if only_text {
        Ok(Value::String(text_only))
    } else {
        Ok(Value::Array(chat_parts))
    }
}

/// input_image part → Chat image_url（对应 Go `responsesImagePartToChatImageURL`）
fn image_part_to_chat_image_url(part: &Map<String, Value>) -> Value {
    if let Some(image_url) = part.get("image_url") {
        return image_url.clone();
    }
    let mut image_url = Map::new();
    for key in ["url", "file_id", "detail"] {
        if let Some(v) = part.get(key) {
            image_url.insert(key.to_string(), v.clone());
        }
    }
    if image_url.is_empty() {
        Value::Object(part.clone())
    } else {
        Value::Object(image_url)
    }
}

/// input_file part → Chat file（对应 Go `responsesFilePartToChatFile`）
fn file_part_to_chat_file(part: &Map<String, Value>) -> Value {
    if let Some(file) = part.get("file") {
        return file.clone();
    }
    let mut file = Map::new();
    for key in ["file_id", "file_data", "filename", "file_url"] {
        if let Some(v) = part.get(key) {
            file.insert(key.to_string(), v.clone());
        }
    }
    if file.is_empty() {
        Value::Object(part.clone())
    } else {
        Value::Object(file)
    }
}

/// input_video part → Chat video_url（对应 Go `responsesVideoPartToChatVideoURL`）
fn video_part_to_chat_video_url(part: &Map<String, Value>) -> Value {
    if let Some(video_url) = part.get("video_url") {
        if let Some(url) = video_url.get("url").and_then(|u| u.as_str()) {
            if !url.is_empty() {
                return Value::String(url.to_string());
            }
        }
        return video_url.clone();
    }
    if let Some(url) = part.get("url").and_then(|u| u.as_str()) {
        if !url.is_empty() {
            return Value::String(url.to_string());
        }
    }
    part_payload(part, "video_url")
}

/// part 载荷：优先命名字段，否则去掉 type 后的全量（对应 Go `responsesPartPayload`）
fn part_payload(part: &Map<String, Value>, key: &str) -> Value {
    if let Some(v) = part.get(key) {
        return v.clone();
    }
    let mut payload = Map::new();
    for (k, v) in part {
        if k == "type" {
            continue;
        }
        payload.insert(k.clone(), v.clone());
    }
    Value::Object(payload)
}

/// function_call item → Chat tool_call
fn function_call_item_to_chat_tool_call(item: &Value) -> Result<ToolCallOut, String> {
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return Err("function_call item is missing name".into());
    }
    Ok(ToolCallOut {
        id: call_id(item),
        r#type: "function".into(),
        function: Some(FunctionOut {
            description: String::new(),
            name: name.to_string(),
            parameters: None,
            arguments: arguments_string(item.get("arguments").unwrap_or(&Value::Null)),
        }),
        custom: None,
    })
}

/// custom_tool_call item → Chat tool_call（custom 字段携带原始 item）
fn custom_tool_call_item_to_chat_tool_call(item: &Value) -> Result<ToolCallOut, String> {
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let arguments = arguments_string(item.get("input").unwrap_or(&Value::Null));
    let function = FunctionOut {
        description: String::new(),
        name,
        parameters: None,
        arguments,
    };
    // function 全空时省略（避免向 Chat 上游发送空 name 噪音）
    let has_function = !function.name.is_empty() || !function.arguments.is_empty();
    Ok(ToolCallOut {
        id: call_id(item),
        r#type: INPUT_TYPE_CUSTOM_TOOL_CALL.into(),
        function: has_function.then_some(function),
        custom: Some(item.clone()),
    })
}

/// 把 tool_call 合并进最后一条 assistant 消息（没有则新建）
fn append_tool_call_to_last_assistant(messages: &mut Vec<MessageOut>, tool: ToolCallOut) {
    let last_is_assistant = messages
        .last()
        .map(|m| m.role == "assistant")
        .unwrap_or(false);
    if !last_is_assistant {
        messages.push(MessageOut {
            role: "assistant".into(),
            content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    messages.last_mut().expect("assistant message exists").tool_calls.push(tool);
}

/// call_id 优先，回退 id（对应 Go `responsesCallID`）
fn call_id(item: &Value) -> String {
    item.get("call_id")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            item.get("id")
                .and_then(|i| i.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

/// tool item `output` → tool 消息 content（字符串原样，其余 JSON 序列化）
fn tool_output_to_chat_content(output: Option<&Value>) -> Value {
    match output {
        None => Value::String(String::new()),
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Null) => Value::String(String::new()),
        Some(other) => Value::String(other.to_string()),
    }
}

/// tools 数组改写：`{type:function, name, description, parameters}` → Chat 嵌套结构
fn request_tools_to_chat(tools: Option<&Value>) -> Result<Vec<ToolCallOut>, String> {
    let Some(tools) = raw_present(tools) else {
        return Ok(Vec::new());
    };
    let Value::Array(arr) = tools else {
        return Err("invalid tools".into());
    };
    let mut out = Vec::with_capacity(arr.len());
    for tool in arr {
        let Some(t) = tool.as_object() else {
            return Err("invalid tools".into());
        };
        let tool_type = t.get("type").and_then(|x| x.as_str()).unwrap_or("").trim();
        if tool_type == "function" {
            out.push(ToolCallOut {
                id: String::new(),
                r#type: "function".into(),
                function: Some(FunctionOut {
                    description: {
                        let d = t
                            .get("description")
                            .map(value_to_string)
                            .unwrap_or_default();
                        d
                    },
                    name: t.get("name")
                        .map(value_to_string)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    parameters: t
                        .get("parameters")
                        .filter(|p| !matches!(p, Value::Null))
                        .cloned(),
                    arguments: String::new(),
                }),
                custom: None,
            });
        } else {
            // 非 function 工具：custom 字段携带原始对象
            out.push(ToolCallOut {
                id: String::new(),
                r#type: tool_type.to_string(),
                function: None,
                custom: Some(Value::Object(t.clone())),
            });
        }
    }
    Ok(out)
}

/// tool_choice 改写：`{type:"function", name}` → Chat 嵌套结构；其余原样
fn request_tool_choice_to_chat(tool_choice: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(tc) = raw_present(tool_choice) else {
        return Ok(None);
    };
    if let Value::String(s) = tc {
        return Ok(Some(Value::String(s.clone())));
    }
    let Some(map) = tc.as_object() else {
        return Err("invalid tool_choice".into());
    };
    if map.get("type").and_then(|t| t.as_str()) == Some("function") {
        let name = map.get("name").and_then(|n| n.as_str()).unwrap_or("").trim();
        if !name.is_empty() {
            return Ok(Some(json!({"type": "function", "function": {"name": name}})));
        }
    }
    Ok(Some(tc.clone()))
}

/// `text.format` → Chat `response_format`（json_schema 需嵌套完整对象）
fn request_text_to_chat_response_format(text: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(t) = raw_present(text) else {
        return Ok(None);
    };
    let Some(map) = t.as_object() else {
        return Err("invalid text config".into());
    };
    let Some(format) = map.get("format").and_then(|f| f.as_object()) else {
        return Ok(None);
    };
    let format_type = format.get("type").and_then(|x| x.as_str()).unwrap_or("").trim();
    if format_type.is_empty() {
        return Ok(None);
    }
    if format_type == "json_schema" {
        Ok(Some(json!({
            "type": "json_schema",
            "json_schema": Value::Object(format.clone())
        })))
    } else {
        Ok(Some(json!({"type": format_type})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Go 黄金夹具 `request/openai_responses_to_openai.golden.json` 的等价输入
    #[test]
    fn converts_rich_responses_request_to_chat() {
        let req = json!({
            "model": "gpt-test",
            "stream": true,
            "max_output_tokens": 1024,
            "instructions": "You are a helpful assistant.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "What is in this image?"},
                    {"type": "input_image", "image_url": "https://example.com/cat.png"}
                ]},
                {"type": "function_call", "call_id": "call_abc", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_abc", "output": "15 degrees"}
            ],
            "tools": [{"type": "function", "name": "get_weather", "description": "Get weather by city",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}]
        });
        let out = responses_request_to_chat(&req).expect("convert");
        assert_eq!(out["model"], "gpt-test");
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["max_completion_tokens"], json!(1024));
        // instructions → system
        assert_eq!(out["messages"][0], json!({"role": "system", "content": "You are a helpful assistant."}));
        // 多模态 user content parts
        assert_eq!(out["messages"][1]["content"][0], json!({"type": "text", "text": "What is in this image?"}));
        assert_eq!(
            out["messages"][1]["content"][1],
            json!({"type": "image_url", "image_url": "https://example.com/cat.png"})
        );
        // function_call item → assistant tool_calls
        let assistant = &out["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null());
        assert_eq!(
            assistant["tool_calls"][0],
            json!({"id": "call_abc", "type": "function",
                   "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}})
        );
        // function_call_output → tool 消息
        assert_eq!(
            out["messages"][3],
            json!({"role": "tool", "tool_call_id": "call_abc", "content": "15 degrees"})
        );
        // tools 嵌套改写
        assert_eq!(
            out["tools"][0],
            json!({"type": "function", "function": {
                "name": "get_weather",
                "description": "Get weather by city",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
            }})
        );
    }

    #[test]
    fn input_string_becomes_user_message() {
        let out = responses_request_to_chat(&json!({"model": "m", "input": "hello"})).unwrap();
        assert_eq!(out["messages"][0], json!({"role": "user", "content": "hello"}));
    }

    #[test]
    fn rejects_stateful_fields() {
        let err = responses_request_to_chat(&json!({
            "model": "m",
            "input": "hi",
            "previous_response_id": "resp_1",
            "conversation": "conv_1"
        }))
        .unwrap_err();
        assert!(err.contains("stateful fields"), "{err}");
        assert!(err.contains("previous_response_id"));
        assert!(err.contains("conversation"));
    }

    #[test]
    fn rejects_missing_model() {
        let err = responses_request_to_chat(&json!({"input": "hi"})).unwrap_err();
        assert_eq!(err, "model is required");
    }

    #[test]
    fn maps_tool_choice_function() {
        let out = responses_request_to_chat(&json!({
            "model": "m",
            "input": "hi",
            "tool_choice": {"type": "function", "name": "get_weather"}
        }))
        .unwrap();
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
    }

    #[test]
    fn maps_text_format_json_schema() {
        let out = responses_request_to_chat(&json!({
            "model": "m",
            "input": "hi",
            "text": {"format": {"type": "json_schema", "name": "weather",
                "schema": {"type": "object"}, "strict": true}}
        }))
        .unwrap();
        assert_eq!(out["response_format"]["type"], "json_schema");
        assert_eq!(out["response_format"]["json_schema"]["name"], "weather");
        assert_eq!(out["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn maps_reasoning_effort() {
        let out = responses_request_to_chat(&json!({
            "model": "m",
            "input": "hi",
            "reasoning": {"effort": "high"}
        }))
        .unwrap();
        assert_eq!(out["reasoning_effort"], "high");
    }
}
