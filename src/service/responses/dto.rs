//! Responses API ↔ Chat Completions 转换的 DTO。
//!
//! 移植自 new-api relaykit（third/new-api/relaykit/dto + relayconvert/internal/oai_*），
//! 仅保留降级路径（/v1/responses 客户端 → Chat Completions 上游）需要的结构。
//! 宽松解析：请求/上游响应多数字段按 `Option<Value>` 透传，只对需要转换的字段做强类型。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 字段存在且非 JSON null（对应 Go `rawJSONPresent`）
pub fn present(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Null) | None) == false
}

/// 任意 JSON 值 → 字符串（对应 Go `Interface2String`：字符串原样，其余序列化为 JSON 文本）
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// 工具参数 → 字符串（对应 Go `responsesArgumentsString`：字符串原样，其余 JSON 序列化）
pub fn arguments_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// JSON 类型名（对应 Go `GetJsonType`）
pub fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Chat Completions 请求（转换输出）
// ---------------------------------------------------------------------------

/// Chat Completions 请求（`GeneralOpenAIRequest` 的转换子集，omitempty 语义）
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequestOut {
    pub model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolCallOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageOut {
    pub role: String,
    /// string 或 content part 数组；空时输出 null（与 Chat API 兼容）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallOut {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionOut>,
    /// 非 function 工具（custom_tool_call 等）：原始 item 原样携带
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionOut {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// Chat Completions 流式 chunk（上游响应解析）
// ---------------------------------------------------------------------------

/// 上游 Chat Completions 流式 chunk（`chat.completion.chunk`）
#[derive(Debug, Clone, Deserialize)]
pub struct ChatStreamChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub created: Option<i64>,
    #[serde(default)]
    pub choices: Vec<ChatStreamChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatStreamChoice {
    /// 仅解析保留（多候选未使用，转换取第一个）
    #[allow(dead_code)]
    #[serde(default)]
    pub index: i64,
    #[serde(default)]
    pub delta: ChatStreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatStreamDelta {
    /// 仅解析保留（转换不依赖 role）
    #[allow(dead_code)]
    #[serde(default)]
    pub role: Option<String>,
    /// string 或 null（部分上游发 content part 数组，转换时忽略）
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 部分上游用 `reasoning` 字段（Go `GetReasoningContent` 同时支持两者）
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: Option<i64>,
    #[serde(default)]
    pub id: Option<String>,
    /// 仅解析保留（转换统一输出 function_call 类型）
    #[allow(dead_code)]
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// Usage：双形态
// ---------------------------------------------------------------------------

/// 上游 usage（Chat 与 Responses 双形态；缺失字段 None）
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<i64>,
    #[serde(default)]
    pub completion_tokens: Option<i64>,
    #[serde(default)]
    pub total_tokens: Option<i64>,
    #[serde(default)]
    pub input_tokens: Option<i64>,
    #[serde(default)]
    pub output_tokens: Option<i64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<Value>,
    #[serde(default)]
    pub completion_tokens_details: Option<Value>,
    /// 仅解析保留（Responses 上游的 input_tokens_details；记账走 input()/output()）
    #[allow(dead_code)]
    #[serde(default)]
    pub input_tokens_details: Option<Value>,
}

impl Usage {
    pub fn input(&self) -> i64 {
        self.prompt_tokens.or(self.input_tokens).unwrap_or(0)
    }
    pub fn output(&self) -> i64 {
        self.completion_tokens.or(self.output_tokens).unwrap_or(0)
    }
    pub fn total(&self) -> i64 {
        self.total_tokens.unwrap_or_else(|| self.input() + self.output())
    }
}

/// 输出到客户端的 Responses usage（双语义 + details 搬运，对应 Go `UsageFromChatUsage`）
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesUsageOut {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub prompt_tokens_details: Value,
    pub completion_tokens_details: Value,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// null 当 Chat 上游未提供 details
    pub input_tokens_details: Option<Value>,
}

/// Chat usage → Responses usage（input/output 双语义；details 搬运）
pub fn usage_from_chat(u: &Usage) -> ResponsesUsageOut {
    let input = u.input();
    let output = u.output();
    let total = u.total();
    ResponsesUsageOut {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        prompt_tokens_details: u
            .prompt_tokens_details
            .clone()
            .unwrap_or_else(zero_prompt_details),
        completion_tokens_details: u
            .completion_tokens_details
            .clone()
            .unwrap_or_else(zero_completion_details),
        input_tokens: if input != 0 { input } else { 0 },
        output_tokens: if output != 0 { output } else { 0 },
        input_tokens_details: nonzero_input_details(u.prompt_tokens_details.as_ref()),
    }
}

fn zero_prompt_details() -> Value {
    json!({"cached_tokens": 0, "text_tokens": 0, "audio_tokens": 0, "image_tokens": 0})
}

fn zero_completion_details() -> Value {
    json!({"text_tokens": 0, "audio_tokens": 0, "image_tokens": 0, "reasoning_tokens": 0})
}

/// Chat `prompt_tokens_details` → Responses `input_tokens_details`（任一分量非零才携带）
fn nonzero_input_details(details: Option<&Value>) -> Option<Value> {
    let d = details?;
    let any_nonzero = match d {
        Value::Object(map) => map.iter().any(|(k, v)| {
            k.starts_with("cached")
                || k.starts_with("text")
                || k.starts_with("audio")
                || k.starts_with("image")
                || k == "cache_write_tokens"
                || v.as_i64().is_some_and(|n| n != 0)
        }),
        _ => false,
    };
    if any_nonzero {
        Some(d.clone())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Responses 响应（输出到客户端）
// ---------------------------------------------------------------------------

/// Responses API 响应（完整字段面，与 new-api wire 一致：缺失字段输出 null/零值）
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesResponseOut {
    pub id: String,
    pub object: &'static str,
    pub created_at: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetailsOut>,
    pub instructions: Option<Value>,
    pub max_output_tokens: u64,
    pub model: String,
    pub output: Vec<ResponsesOutputOut>,
    pub parallel_tool_calls: bool,
    pub previous_response_id: Option<Value>,
    pub reasoning: Option<Value>,
    pub store: bool,
    pub temperature: f64,
    pub tool_choice: Option<Value>,
    pub tools: Option<Value>,
    pub top_p: f64,
    pub truncation: Option<Value>,
    pub usage: Option<ResponsesUsageOut>,
    pub user: Option<Value>,
    pub metadata: Option<Value>,
}

impl ResponsesResponseOut {
    /// 零 usage（上游未提供时输出空对象，避免 SDK 解析失败）
    pub fn with_zero_usage(mut self) -> Self {
        if self.usage.is_none() {
            self.usage = Some(ResponsesUsageOut {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                prompt_tokens_details: zero_prompt_details(),
                completion_tokens_details: zero_completion_details(),
                input_tokens: 0,
                output_tokens: 0,
                input_tokens_details: None,
            });
        }
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IncompleteDetailsOut {
    pub reason: String,
}

/// 输出 item（message / reasoning / function_call / custom_tool_call）
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesOutputOut {
    pub r#type: String,
    pub id: String,
    pub status: String,
    pub role: String,
    pub content: Vec<ResponsesOutputContentOut>,
    pub quality: String,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 参数 JSON 字符串（`"{\"city\":\"Paris\"}"`）；流式进行中为 `""`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesOutputContentOut {
    pub r#type: String,
    pub text: String,
    pub annotations: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryPartOut {
    pub r#type: String,
    pub text: String,
}

/// 流式事件（`event: {type}\ndata: {json}\n\n` 的 data 载荷）
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesStreamEventOut {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponsesResponseOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<ResponsesOutputOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<SummaryPartOut>,
}

/// 事件 → SSE 帧（与 new-api `ResponseChunkData` 输出一致）
pub fn sse_frame(ev: &ResponsesStreamEventOut) -> String {
    let data = serde_json::to_string(ev).expect("responses event serialization cannot fail");
    format!("event: {}\ndata: {}\n\n", ev.r#type, data)
}

/// 构造通用输出 item（message/reasoning/function_call 共用的字段补全）
pub fn output_item(r#type: &str, id: String, status: &str, content: Vec<ResponsesOutputContentOut>) -> ResponsesOutputOut {
    ResponsesOutputOut {
        r#type: r#type.to_string(),
        id,
        status: status.to_string(),
        role: String::new(),
        content,
        quality: String::new(),
        size: String::new(),
        call_id: None,
        name: None,
        arguments: None,
    }
}

/// 零值 content part 列表（Go `[]interface{}{}` → `annotations: []`）
pub fn empty_annotations() -> Vec<Value> {
    Vec::new()
}
