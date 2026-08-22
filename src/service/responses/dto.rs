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
    /// 非 o 系列模型走 max_tokens（DeepSeek 等 Chat 上游不识别 max_completion_tokens，
    /// 静默忽略会导致输出上限丢失；cc-switch 同款模型族选择）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
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
    pub frequency_penalty: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
    /// L3：Chat 风格客户端直发 top_logprobs 时透传（cc-switch EXTRA 白名单同款）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
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
    /// 跨轮思考回传（reasoning item 附挂；DeepSeek/kimi 等 thinking 上游要求
    /// assistant tool-call 消息带非空 reasoning_content）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallOut {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionOut {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    /// L12：strict 结构化输出标志（tools 侧透传；tool_call 条目不携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<Value>,
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
    /// 上游在流中内嵌错误帧（`{"error": {...}}`，OpenAI 兼容或裸 message 形态）；
    /// 转换层据此发 response.failed 而非伪 completed（cc-switch 同款语义）
    #[serde(default)]
    pub error: Option<Value>,
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
    /// 模型拒答文本（Chat refusal；四-3：流式通道此前缺失，拒答被静默丢弃）
    #[serde(default)]
    pub refusal: Option<String>,
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
// 宽松解析（上游流式 chunk）
// ---------------------------------------------------------------------------
//
// 上游（网关中转 / 兼容层）常出现字段类型不守协议的形态：usage 数值为字符串、
// created 为浮点、reasoning 为对象、content 增量为 parts 数组……
// 若用严格 serde 反序列化，任何一个字段类型不匹配都会让整个 chunk 解析失败，
// 调用方静默丢弃（continue）→ 客户端拿到"零内容的成功响应"。
// 这里对齐 Go 版语义（json.Unmarshal 类型不匹配时仍填充其余字段）：
// JSON 合法即成功，字段按尽力而为原则提取。

/// 任意标量 → 文本（string 原样；number/bool 转字符串；对象取 text/content 字段）
fn lenient_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Object(o) => o
            .get("text")
            .or_else(|| o.get("content"))
            .and_then(|t| t.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// 任意数值形态 → i64（整数原样；浮点截断；字符串解析）
fn lenient_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

impl Usage {
    /// 宽松构造：数值字段容忍字符串/浮点形态（部分网关上游字符串化 usage）
    pub fn from_value_lenient(v: &Value) -> Usage {
        let get = |key: &str| v.get(key).and_then(lenient_i64);
        Usage {
            prompt_tokens: get("prompt_tokens"),
            completion_tokens: get("completion_tokens"),
            total_tokens: get("total_tokens"),
            input_tokens: get("input_tokens"),
            output_tokens: get("output_tokens"),
            prompt_tokens_details: v.get("prompt_tokens_details").filter(|d| d.is_object()).cloned(),
            completion_tokens_details: v.get("completion_tokens_details").filter(|d| d.is_object()).cloned(),
            input_tokens_details: v.get("input_tokens_details").filter(|d| d.is_object()).cloned(),
            cache_read_input_tokens: get("cache_read_input_tokens"),
            cache_creation_input_tokens: get("cache_creation_input_tokens"),
            prompt_cache_hit_tokens: get("prompt_cache_hit_tokens"),
        }
    }
}

impl ChatStreamChunk {
    /// 宽松解析：JSON 语法非法才返回 None；字段类型不匹配按尽力而为提取，
    /// 不让单个怪癖字段（字符串 usage / 浮点 created / 对象 reasoning）丢掉整帧
    pub fn from_json_str(data: &str) -> Option<ChatStreamChunk> {
        let v: Value = serde_json::from_str(data).ok()?;
        let choices = v
            .get("choices")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|c| {
                        // delta 缺失（如末帧仅携带 finish_reason）不丢 choice：
                        // finish_reason/usage 语义仍在；message 兜底覆盖非流式形态
                        let delta = c.get("delta").or_else(|| c.get("message"));
                        ChatStreamChoice {
                            index: c.get("index").and_then(lenient_i64).unwrap_or(0),
                            delta: delta
                                .map(|d| ChatStreamDelta {
                                    role: d.get("role").and_then(lenient_text),
                                    content: d.get("content").cloned(),
                                    reasoning_content: delta_reasoning_text(d),
                                    reasoning: None,
                                    tool_calls: d
                                        .get("tool_calls")
                                        .and_then(|t| t.as_array())
                                        .map(|arr| arr.iter().filter_map(lenient_tool_call).collect())
                                        .unwrap_or_default(),
                                    refusal: d.get("refusal").and_then(lenient_text),
                                })
                                .unwrap_or_default(),
                            finish_reason: c.get("finish_reason").and_then(lenient_text),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(ChatStreamChunk {
            id: v.get("id").and_then(lenient_text).unwrap_or_default(),
            model: v.get("model").and_then(lenient_text).unwrap_or_default(),
            created: v.get("created").and_then(lenient_i64),
            choices,
            usage: v.get("usage").filter(|u| u.is_object()).map(Usage::from_value_lenient),
            error: v.get("error").filter(|e| !e.is_null()).cloned(),
        })
    }
}

/// M4：流式 delta 的思考文本提取（cc-switch extract_reasoning_field_text 同款穷举）：
/// reasoning_content > reasoning（字符串/对象 content/text/summary）>
/// reasoning_details（字符串/数组 [{text}]，xAI Grok 形态）——此前只发
/// reasoning_details 的上游流式思考被静默丢弃（与非流式 extract_message_reasoning 不一致）
pub fn delta_reasoning_text(d: &Value) -> Option<String> {
    if let Some(s) = d.get("reasoning_content").and_then(lenient_text) {
        if !s.trim().is_empty() {
            return Some(s);
        }
    }
    if let Some(v) = d.get("reasoning") {
        if let Some(s) = lenient_text(v) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        } else if let Some(o) = v.as_object() {
            for k in ["content", "text", "summary"] {
                if let Some(s) = o.get(k).and_then(|t| t.as_str()) {
                    if !s.trim().is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    if let Some(details) = d.get("reasoning_details") {
        match details {
            Value::String(s) if !s.trim().is_empty() => return Some(s.clone()),
            Value::Array(parts) => {
                let texts: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .filter(|s| !s.trim().is_empty())
                    .collect();
                if !texts.is_empty() {
                    return Some(texts.join("\n"));
                }
            }
            _ => {}
        }
    }
    None
}

/// 流式 tool_call 帧 → 宽松 DTO（arguments 容忍对象形态，序列化为字符串）
fn lenient_tool_call(tc: &Value) -> Option<ToolCallDelta> {
    let function = tc.get("function").or_else(|| tc.get("function_call"));
    let name = function
        .and_then(|f| f.get("name"))
        .and_then(lenient_text)
        .or_else(|| tc.get("name").and_then(lenient_text));
    let arguments = function
        .and_then(|f| f.get("arguments"))
        .or_else(|| tc.get("arguments"))
        .map(|a| match a {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        });
    let has_payload = name.is_some() || arguments.is_some();
    Some(ToolCallDelta {
        index: tc.get("index").and_then(lenient_i64),
        id: tc.get("id").and_then(lenient_text),
        r#type: tc.get("type").and_then(|t| t.as_str()).map(str::to_string),
        // legacy 顶层 name/arguments 形态同样产出 function（避免下游因 function
        // 为 None 而跳过整个调用）
        function: has_payload.then(|| FunctionDelta { name, arguments }),
    })
}

/// delta.content → 增量文本。string 原样；部分上游（Gemini 兼容层等）以
/// content-parts 数组下发增量 —— 拼接 text 类 part（image/audio 等非文本忽略）。
pub fn delta_content_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("text");
                if matches!(ty, "text" | "output_text" | "input_text" | "thinking") {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        out.push_str(t);
                    }
                }
            }
            Some(out)
        }
        other => lenient_text(other),
    }
}

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
    /// Responses 上游的 input_tokens_details（L3：缓存字段优先级高于 prompt_ 镜像）
    #[serde(default)]
    pub input_tokens_details: Option<Value>,
    /// Anthropic 风格直传缓存字段（部分 Chat 网关上游会携带）
    #[serde(default)]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<i64>,
    /// DeepSeek 文档化缓存命中字段（末位兜底，cc-switch 同款解析链）
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<i64>,
}

/// 流式工具帧缺 `index` 时的 key 解析（cc-switch `resolve_tool_key_without_index`，
/// streaming_codex_chat.rs:345-368 语义）：
/// - 帧带非空 `id` 且该 id 与所有已知调用都不同 → 新调用，分配新 key
///   （max+1，`checked_add` 防畸形上游发 `index: i64::MAX` 后回绕覆盖既有调用）
/// - 帧带已知 `id` → 归回该调用的 key
/// - 无 `id` 或空 → 并入最后已知调用（空表时 key 0）
///
/// 宁可两个并行调用坍缩成一个，也不能把一个调用的续帧炸成多个 item。
pub fn resolve_no_index_tool_key(
    id: Option<&str>,
    key_for_id: Option<i64>,
    last_key: Option<i64>,
    max_key: Option<i64>,
) -> i64 {
    let id = id.map(str::trim).filter(|s| !s.is_empty());
    match id {
        Some(_) => match key_for_id {
            Some(k) => k,
            None => match max_key.or(last_key) {
                Some(k) => k.checked_add(1).unwrap_or(k),
                None => 0,
            },
        },
        None => last_key.unwrap_or(0),
    }
}

impl Usage {
    pub fn input(&self) -> i64 {
        self.prompt_tokens.or(self.input_tokens).unwrap_or(0)
    }
    pub fn output(&self) -> i64 {
        self.completion_tokens.or(self.output_tokens).unwrap_or(0)
    }
    pub fn total(&self) -> i64 {
        self.total_tokens.unwrap_or_else(|| self.input().saturating_add(self.output()))
    }

    /// 缓存命中 token（L3，cc-switch usage/parser.rs:12-26 同款优先级）：
    /// cache_read_input_tokens > input_tokens_details.cached_tokens >
    /// prompt_tokens_details.cached_tokens > prompt_cache_hit_tokens
    ///（Responses 形态 input_tokens_details 是标准位，Chat 形态 prompt_ 镜像居后）
    pub fn cached_tokens(&self) -> i64 {
        self.cache_read_input_tokens
            .or_else(|| details_i64(self.input_tokens_details.as_ref(), "cached_tokens"))
            .or_else(|| details_i64(self.prompt_tokens_details.as_ref(), "cached_tokens"))
            .or(self.prompt_cache_hit_tokens)
            .unwrap_or(0)
    }

    /// 缓存写入 token（cache_creation / cache_write 同义；优先级与 cached 一致）
    pub fn cache_write_tokens(&self) -> i64 {
        self.cache_creation_input_tokens
            .or_else(|| details_i64(self.input_tokens_details.as_ref(), "cache_write_tokens"))
            .or_else(|| details_i64(self.prompt_tokens_details.as_ref(), "cache_write_tokens"))
            .unwrap_or(0)
    }
}

fn details_i64(details: Option<&Value>, key: &str) -> Option<i64> {
    details.and_then(|d| d.get(key)).and_then(|v| v.as_i64())
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
    /// M2：恒携带（cached_tokens 至少为 0）；Chat 上游未提供 details 时合成零值
    pub input_tokens_details: Value,
    /// M2：Responses 命名的输出 details（completion_tokens_details 映射 +
    /// reasoning_tokens 缺省 0）——Codex 推理 token 统计读取此字段
    pub output_tokens_details: Value,
}

/// Chat usage → Responses usage（input/output 双语义；details 搬运 + 缓存桶归一化；
/// M2：output_tokens_details 恒在、input_tokens_details 恒在且 cached_tokens ≥ 0）
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
        input_tokens: input,
        output_tokens: output,
        input_tokens_details: input_details_with_cache(u)
            .unwrap_or_else(|| json!({"cached_tokens": 0})),
        output_tokens_details: output_details_from_chat(u),
    }
}

/// M2：`output_tokens_details`（Responses 命名）——以 completion_tokens_details 为底，
/// 缺 reasoning_tokens 补 0；上游完全未提供时输出零值对象（cc-switch
/// chat_usage_to_responses_usage :1919-1934 同款）
fn output_details_from_chat(u: &Usage) -> Value {
    let mut base = u
        .completion_tokens_details
        .clone()
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = base.as_object_mut() {
        if !obj.contains_key("reasoning_tokens") {
            obj.insert("reasoning_tokens".into(), json!(0));
        }
    }
    base
}

/// `input_tokens_details`：以 prompt/input tokens_details 为底，把多源缓存字段
/// （cached_tokens / cache_read / prompt_cache_hit / cache_write）归一化合并进来；
/// 全零则不带（与 nonzero 语义一致）
fn input_details_with_cache(u: &Usage) -> Option<Value> {
    let cached = u.cached_tokens();
    let write = u.cache_write_tokens();
    let mut base = u
        .prompt_tokens_details
        .clone()
        .or_else(|| u.input_tokens_details.clone())
        .unwrap_or_else(|| json!({}));
    let Some(obj) = base.as_object_mut() else {
        return nonzero_input_details(u.prompt_tokens_details.as_ref());
    };
    if cached > 0 {
        obj.insert("cached_tokens".into(), json!(cached));
    }
    if write > 0 {
        obj.insert("cache_write_tokens".into(), json!(write));
    }
    let any_nonzero = obj
        .values()
        .any(|v| v.as_i64().is_some_and(|n| n != 0));
    if any_nonzero {
        Some(base)
    } else {
        None
    }
}

fn zero_prompt_details() -> Value {
    json!({"cached_tokens": 0, "text_tokens": 0, "audio_tokens": 0, "image_tokens": 0})
}

fn zero_completion_details() -> Value {
    json!({"text_tokens": 0, "audio_tokens": 0, "image_tokens": 0, "reasoning_tokens": 0})
}

/// Chat `prompt_tokens_details` → Responses `input_tokens_details`（任一分量非零才携带）
/// 仅当某已知分量值非零时才携带（原实现只匹配 key 前缀，全零 details 也会被搬运）
fn nonzero_input_details(details: Option<&Value>) -> Option<Value> {
    let d = details?;
    let any_nonzero = match d {
        Value::Object(map) => map.iter().any(|(k, v)| {
            let known = k.starts_with("cached")
                || k.starts_with("text")
                || k.starts_with("audio")
                || k.starts_with("image")
                || k == "cache_write_tokens";
            known && v.as_i64().is_some_and(|n| n != 0)
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

/// Responses API 响应（完整字段面，与 new-api wire 一致；L6：不回显请求参数
/// 的字段改为 Option + 跳过序列化——此前硬编码 store:false / temperature:0 /
/// max_output_tokens:0 是误导性伪值，cc-switch envelope 亦不回显）
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    pub model: String,
    pub output: Vec<ResponsesOutputOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Value>,
    pub usage: Option<ResponsesUsageOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
                input_tokens_details: json!({"cached_tokens": 0}),
                output_tokens_details: zero_completion_details(),
            });
        }
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IncompleteDetailsOut {
    pub reason: String,
}

/// M6（cc-switch custom_tool_input_from_chat_arguments :1832-1844 同款）：
/// custom 工具降级后的参数 `{"input": <str>}` 还原为自由文本；
/// 非对象 / 无 input 字段 → 原样返回（含空串）
pub fn custom_tool_input_from_chat_arguments(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(obj)) => obj
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or(arguments)
            .to_string(),
        _ => arguments.to_string(),
    }
}

/// 输出 item（message / reasoning / function_call / custom_tool_call /
/// tool_search_call / web_search_call）
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesOutputOut {
    pub r#type: String,
    /// tool_search_call item 无 id（真实 API 同形态）；其余类型恒非空
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub status: String,
    pub role: String,
    pub content: Vec<ResponsesOutputContentOut>,
    /// reasoning item 的思维链正文（真实 Responses 形态：summary[] 而非 content[]）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<SummaryPartOut>,
    pub quality: String,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// P0-4：namespace 工具还原——function_call item 携带原始 namespace
    ///（chat 名为拍平形态 `ns__name`，客户端据 namespace+name 识别 MCP 工具；
    /// cc-switch response_function_call_item_with_namespace 同款）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// 参数 JSON 字符串（`"{\"city\":\"Paris\"}"`）；流式进行中为 `""`；
    /// tool_search_call 为解析后的对象形态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    /// L6：思考归属工具调用（cc-switch reasoning_content 字段同款）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// 四-4：web_search_call item 的查询词（桥接工具专用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// M6：custom_tool_call item 的自由文本入参（从降级参数 {"input":…} 还原）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// P0-4：tool_search_call item 的执行侧标记（真实 API 恒 "client"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesOutputContentOut {
    pub r#type: String,
    pub text: String,
    /// P1-1：refusal part 的载荷字段——真实 API 形态为 `{"type":"refusal",
    /// "refusal": <text>}`（此前载荷放 `text` 字段，SDK 读 `refusal` 拿空；
    /// cc-switch transform_codex_chat.rs:1537-1542 同款）
    #[serde(skip_serializing_if = "String::is_empty")]
    pub refusal: String,
    pub annotations: Vec<Value>,
}


#[derive(Debug, Clone, Serialize)]
pub struct SummaryPartOut {
    pub r#type: String,
    pub text: String,
    /// content_part 事件携带 `annotations: []`；summary part 省略
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Vec<Value>>,
}

/// 构造 summary/content part（annotations 省略形态）
pub fn summary_part(r#type: &str, text: &str) -> SummaryPartOut {
    SummaryPartOut {
        r#type: r#type.to_string(),
        text: text.to_string(),
        annotations: None,
    }
}

/// 定义见下方 ResponsesStreamEventOut（含 M1 聚合载荷字段）
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
        summary: Vec::new(),
        quality: String::new(),
        size: String::new(),
        call_id: None,
        name: None,
        namespace: None,
        arguments: None,
        reasoning_content: None,
        query: None,
        input: None,
        execution: None,
    }
}

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
    /// M6：response.custom_tool_call_input.done 事件的完整入参
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// M1：done 事件的聚合载荷（output_text.done 带全文；
    /// reasoning_summary_text.done 带顶层 text）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// M1：function_call_arguments.done 带 canonical 全参
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// 零值 content part 列表（Go `[]interface{}{}` → `annotations: []`）
pub fn empty_annotations() -> Vec<Value> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_json_str_tolerates_string_usage_values() {
        // Bug 回归：部分网关上游把 usage 数值字符串化。严格 serde 反序列化会让
        // 整个 chunk 失败被调用方静默丢弃 → 客户端拿到零内容的成功响应
        let data = json!({
            "id": "c1", "object": "chat.completion.chunk", "model": "m",
            "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": "12", "completion_tokens": "7", "total_tokens": "19"}
        })
        .to_string();
        let chunk = ChatStreamChunk::from_json_str(&data).expect("must parse leniently");
        assert_eq!(chunk.choices[0].delta.content.as_ref().and_then(delta_content_text).as_deref(), Some("Hello"));
        let usage = chunk.usage.as_ref().expect("usage present");
        assert_eq!(usage.input(), 12);
        assert_eq!(usage.output(), 7);
    }

    #[test]
    fn from_json_str_keeps_deltaless_finish_reason_choice() {
        // Bug 回归：末帧仅携带 finish_reason（无 delta）时不得丢弃整个 choice，
        // 否则 length 截断会被误报为 completed
        let data = json!({
            "id": "c1", "model": "m",
            "choices": [{"index": 0, "finish_reason": "length"}]
        })
        .to_string();
        let chunk = ChatStreamChunk::from_json_str(&data).expect("must parse");
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn from_json_str_synthesizes_function_for_legacy_tool_call() {
        // Bug 回归：legacy 顶层 name/arguments 形态必须产出 function，
        // 否则下游因 function 为 None 跳过整个调用
        let data = json!({
            "id": "c1", "model": "m",
            "choices": [{"index": 0, "delta": {
                "tool_calls": [{"index": 0, "id": "call_1", "name": "get_time", "arguments": "{}"}]
            }}]
        })
        .to_string();
        let chunk = ChatStreamChunk::from_json_str(&data).expect("must parse");
        let tc = &chunk.choices[0].delta.tool_calls[0];
        let f = tc.function.as_ref().expect("function synthesized");
        assert_eq!(f.name.as_deref(), Some("get_time"));
        assert_eq!(f.arguments.as_deref(), Some("{}"));
    }

    #[test]
    fn from_json_str_message_fallback_carries_role() {
        // 非流式形态（message 而非 delta）：role 从 message 提取
        let data = json!({
            "id": "c1", "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi"}, "finish_reason": "stop"}]
        })
        .to_string();
        let chunk = ChatStreamChunk::from_json_str(&data).expect("must parse");
        assert_eq!(chunk.choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(
            chunk.choices[0].delta.content.as_ref().and_then(delta_content_text).as_deref(),
            Some("Hi")
        );
    }

    #[test]
    fn from_json_str_tolerates_object_reasoning_and_float_created() {
        let data = json!({
            "id": 42, "model": "m", "created": 1700000000.5,
            "choices": [{
                "index": "0",
                "delta": {"reasoning": {"text": "hmm"}, "content": null},
                "finish_reason": null
            }]
        })
        .to_string();
        let chunk = ChatStreamChunk::from_json_str(&data).expect("must parse leniently");
        assert_eq!(chunk.id, "42");
        assert_eq!(chunk.created, Some(1700000000));
        assert_eq!(chunk.choices[0].delta.reasoning_content.as_deref(), Some("hmm"));
        assert_eq!(chunk.choices[0].delta.content, Some(Value::Null));
        // 真正的语法错误才返回 None
        assert!(ChatStreamChunk::from_json_str("{\"broken").is_none());
        assert!(ChatStreamChunk::from_json_str("").is_none());
    }

    #[test]
    fn from_json_str_tolerates_object_tool_arguments() {
        let data = json!({
            "id": "c1", "model": "m",
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_1",
                "function": {"name": "get_weather", "arguments": {"city": "Paris"}}
            }]}, "finish_reason": null}]
        })
        .to_string();
        let chunk = ChatStreamChunk::from_json_str(&data).expect("must parse leniently");
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.id.as_deref(), Some("call_1"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("get_weather"));
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"city\":\"Paris\"}")
        );
    }

    #[test]
    fn delta_content_text_forms() {
        assert_eq!(delta_content_text(&Value::Null), None);
        assert_eq!(delta_content_text(&json!("hi")), Some("hi".into()));
        assert_eq!(
            delta_content_text(&json!([{"type": "text", "text": "a"}, {"type": "output_text", "text": "b"}])),
            Some("ab".into())
        );
        // 非文本 part 忽略，不产出乱码
        assert_eq!(
            delta_content_text(&json!([
                {"type": "image_url", "image_url": {"url": "data:..."}},
                {"type": "text", "text": "t"}
            ])),
            Some("t".into())
        );
        // 空数组 → 空串（等价于无增量）
        assert_eq!(delta_content_text(&json!([])), Some(String::new()));
    }

    #[test]
    fn usage_from_value_lenient_coerces() {
        let u = Usage::from_value_lenient(&json!({
            "prompt_tokens": "10",
            "completion_tokens": 3.9,
            "prompt_tokens_details": {"cached_tokens": "6"}
        }));
        assert_eq!(u.input(), 10);
        assert_eq!(u.output(), 3);
        // details 内的字符串缓存数不参与宽松换算（as_i64 仅整数）→ 0，可接受
        assert_eq!(u.cached_tokens(), 0);
    }

    /// L8：直传缓存字段优先于 details（cc-switch transform.rs:693-711 同款）
    #[test]
    fn cached_tokens_prefer_direct_fields_over_details() {
        // 双字段并存：直传值胜出
        let u = Usage::from_value_lenient(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
            "cache_read_input_tokens": 40,
            "prompt_tokens_details": {"cached_tokens": 7}
        }));
        assert_eq!(u.cached_tokens(), 40);
        // 无直传 → details 兜底
        let u2 = Usage::from_value_lenient(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
            "prompt_tokens_details": {"cached_tokens": 7}
        }));
        assert_eq!(u2.cached_tokens(), 7);
        // 缓存写：直传 cache_creation 优先于 details cache_write
        let u3 = Usage::from_value_lenient(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
            "cache_creation_input_tokens": 9,
            "prompt_tokens_details": {"cache_write_tokens": 3}
        }));
        assert_eq!(u3.cache_write_tokens(), 9);
        let u4 = Usage::from_value_lenient(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
            "prompt_tokens_details": {"cache_write_tokens": 3}
        }));
        assert_eq!(u4.cache_write_tokens(), 3);
        // L3：input_tokens_details（Responses 标准位）优先于 prompt_tokens_details 镜像
        let u5 = Usage::from_value_lenient(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
            "prompt_tokens_details": {"cached_tokens": 7, "cache_write_tokens": 2},
            "input_tokens_details": {"cached_tokens": 12, "cache_write_tokens": 5}
        }));
        assert_eq!(u5.cached_tokens(), 12);
        assert_eq!(u5.cache_write_tokens(), 5);
    }
}
