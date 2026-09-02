//! Responses 请求 → Chat Completions 请求。
//!
//! 移植自 new-api `relaykit/relayconvert/internal/oai_responses/to_oai_chat_req.go`，
//! 工具/思考语义对齐 cc-switch 管线①（`transform_codex_chat.rs`）：
//! - `instructions` → system 消息；`input` 字符串 → user 消息；input item 数组逐项拆解
//! - `function_call` / `custom_tool_call` item 合并进最后一条 assistant 消息的 tool_calls；
//!   custom 调用降级为 function 形态（input 包装为 `{"input": <str>}`，cc-switch 同款）
//! - `function_call_output` → tool 消息（call_id 缺失不报错）；`custom_tool_call_output` /
//!   `tool_search_output` → canonical JSON tool 消息（不再静默丢弃，H1）
//! - `reasoning` item → pending_reasoning 前向附挂到其后 assistant 消息；user 回合
//!   边界与 input 尾部回溯附挂到上一条 assistant（M1，cc-switch :764-1098 语义）
//! - 有状态字段（conversation / previous_response_id / prompt / context_management）显式拒绝
//! - tools / tool_choice / text.format 结构改写为 Chat 形态；custom 工具定义降级为
//!   function 并把原始定义嵌入 description（H1）
//! - tools 为空时剥离 tool_choice / parallel_tool_calls（M2，vLLM 严格上游 400 防护）
//! - reasoning_effort 仅对支持该参数的模型族转发，none/off/disabled 不发（M3）

use serde_json::{Value, json};

use crate::service::model_family::{effort_is_off, supports_reasoning_effort};

use super::dto::{
    self, ChatRequestOut, FunctionOut, MessageOut, ToolCallOut, arguments_string, present,
    value_to_string,
};
use super::tool_ctx::{TOOL_SEARCH_PROXY_NAME, ToolContext};

const INPUT_TYPE_FUNCTION_CALL: &str = "function_call";
const INPUT_TYPE_FUNCTION_CALL_OUTPUT: &str = "function_call_output";
const INPUT_TYPE_CUSTOM_TOOL_CALL: &str = "custom_tool_call";
const INPUT_TYPE_CUSTOM_TOOL_CALL_OUTPUT: &str = "custom_tool_call_output";
const INPUT_TYPE_TOOL_SEARCH_CALL: &str = "tool_search_call";
const INPUT_TYPE_TOOL_SEARCH_OUTPUT: &str = "tool_search_output";
const INPUT_TYPE_REASONING: &str = "reasoning";
const INPUT_TYPE_WEB_SEARCH_CALL: &str = "web_search_call";
/// custom 工具降级后的入参字段（cc-switch `CUSTOM_TOOL_INPUT_FIELD`）
const CUSTOM_TOOL_INPUT_FIELD: &str = "input";
const CUSTOM_TOOL_INPUT_DESCRIPTION: &str = "Raw string input for the original custom tool. Preserve formatting exactly and follow the original tool definition embedded in the description.";
const CUSTOM_TOOL_PRESERVED_METADATA_HEADING: &str = "Original tool definition:";

/// 空 `reasoning_content` 占位符（kimi/DeepSeek 等 thinking 上游要求 assistant
/// tool-call 消息带非空思考；cc-switch backfill_tool_call_reasoning_placeholders 同款）
const TOOL_CALL_REASONING_PLACEHOLDER: &str = "tool call";

/// Responses 请求 → Chat Completions 请求（JSON Value）。
/// `gating_model`：模型族门控用的模型名（H1 = 路由映射后的上游模型名，
/// 无映射时与客户端模型一致）——出站体 model 仍写客户端模型，由代理层
/// 在转换后统一改写（proxy build_outbound），此处仅用于能力判定。
/// 失败返回 BadRequest 语义的错误消息。
pub fn responses_request_to_chat(req: &Value, gating_model: &str) -> Result<Value, String> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim();
    if model.is_empty() {
        return Err("model is required".into());
    }
    validate_unsupported_fields(req)?;

    let tool_ctx = ToolContext::from_request(req);
    let messages = request_messages_to_chat(req, &tool_ctx)?;
    let tools = request_tools_to_chat(req.get("tools"), &tool_ctx)?;
    let has_tools = !tools.is_empty();
    let tool_choice = if has_tools {
        request_tool_choice_to_chat(req.get("tool_choice"), &tool_ctx)?
    } else {
        // tools 为空时 tool_choice/parallel_tool_calls 必须剥离（cc-switch :330-342：
        // vLLM/企业网关对「有 choice 无 tools」直接 400）
        None
    };
    // L3：text.format 映射优先；Chat 风格客户端直发顶层 response_format 时透传
    let response_format = request_text_to_chat_response_format(req.get("text"))?
        .or_else(|| raw_present(req.get("response_format")).cloned());
    let (max_completion_tokens, max_tokens) = max_tokens_fields(req, gating_model);

    // 客户端 gpt-5.1 映射到 deepseek 时不发）；
    // M9：显式关闭（none/off/disabled）保留为 "none" 下发 —— 由路由级
    // reasoning_effort_mode 决定终态（openrouter → {"reasoning":{"effort":"none"}}
    // 忠实转发；其余模式在 apply_reasoning_effort_mode 统一移除，防枚举 400）
    let reasoning_effort = req
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty())
        .filter(|_| supports_reasoning_effort(gating_model))
        .map(|e| {
            if effort_is_off(e) {
                "none".to_string()
            } else {
                e.to_string()
            }
        });

    let out = ChatRequestOut {
        model: model.to_string(),
        messages,
        stream: req.get("stream").and_then(|s| s.as_bool()),
        stream_options: raw_present(req.get("stream_options")).cloned(),
        max_completion_tokens,
        max_tokens,
        temperature: req.get("temperature").and_then(|v| v.as_f64()),
        top_p: req.get("top_p").and_then(|v| v.as_f64()),
        response_format,
        tools,
        tool_choice,
        parallel_tool_calls: has_tools
            .then(|| req.get("parallel_tool_calls").and_then(|v| v.as_bool()))
            .flatten(),
        user: raw_present(req.get("user")).cloned(),
        store: raw_present(req.get("store")).cloned(),
        metadata: raw_present(req.get("metadata")).cloned(),
        reasoning_effort,
        frequency_penalty: raw_present(req.get("frequency_penalty")).cloned(),
        logit_bias: raw_present(req.get("logit_bias")).cloned(),
        logprobs: raw_present(req.get("logprobs")).cloned(),
        top_logprobs: raw_present(req.get("top_logprobs")).cloned(),
        n: raw_present(req.get("n")).cloned(),
        presence_penalty: raw_present(req.get("presence_penalty")).cloned(),
        seed: raw_present(req.get("seed")).cloned(),
        stop: raw_present(req.get("stop")).cloned(),
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
    let mut v = serde_json::to_value(out).map_err(|e| format!("serialize chat request: {e}"))?;
    if !has_tools {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("parallel_tool_calls");
        }
    }
    Ok(v)
}

/// 输出上限字段选择（cc-switch TC:297-311 同款）：o 系列模型（o1/o3/o4…，
/// 仅接受 max_completion_tokens）→ max_completion_tokens；其余（DeepSeek 等
/// Chat 上游）→ max_tokens。`max_output_tokens` 为 0 视为未设置。
fn max_tokens_fields(req: &Value, model: &str) -> (Option<u64>, Option<u64>) {
    // L9：Chat 风格客户端直发 max_tokens / max_completion_tokens 时回退读取
    //（cc-switch :300-305 直通同款；此前输出上限静默丢失）
    let n = req
        .get("max_output_tokens")
        .and_then(|n| n.as_u64())
        .filter(|n| *n != 0)
        .or_else(|| {
            req.get("max_completion_tokens")
                .and_then(|n| n.as_u64())
                .filter(|n| *n != 0)
        });
    let n_o_series_style = n.or_else(|| {
        req.get("max_tokens")
            .and_then(|n| n.as_u64())
            .filter(|n| *n != 0)
    });
    let bare = model.rsplit('/').next().unwrap_or(model);
    let is_o_series = crate::service::model_family::is_openai_o_series(bare);
    if is_o_series {
        (n_o_series_style, None)
    } else {
        (None, n_o_series_style)
    }
}

/// 字段存在且非 null
fn raw_present(v: Option<&Value>) -> Option<&Value> {
    if present(v) { v } else { None }
}

/// L2：input item 角色归一化——仅识别 user/assistant/system；缺失/空/未知
/// 一律回退 user（cc-switch transform_codex_chat.rs:940-947 同款；未知角色
/// 原样透传会让严格 Chat 上游 400）
fn normalize_input_role(role: Option<&str>) -> String {
    match role.map(str::trim).unwrap_or("") {
        "user" | "assistant" | "system" => role.map(str::to_string).expect("matched above"),
        _ => "user".to_string(),
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
            "responses to chat conversion does not support stateful fields: {}. \
             The Chat upstream is stateless: resend the full conversation history \
             instead of referencing a previous response (codex clients: disable \
             server-side storage, e.g. 'store' 'false' in model args)",
            unsupported.join(", ")
        ));
    }
    Ok(())
}

/// instructions → 文本：string 原样；parts 数组拼接各 part 的 text（与 content 同语义）；
/// 其余类型回退为 JSON 文本。
fn instructions_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        // L1：parts 间用 "\n\n" 连接（对齐 cc-switch transform_codex_chat.rs:582-596；
        // 跨网关会话共享前缀缓存时字节形态一致）
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => other.to_string(),
    }
}

/// instructions + input → messages（含 reasoning 附挂与 tool_result 媒体抽取）
fn request_messages_to_chat(
    req: &Value,
    tool_ctx: &ToolContext,
) -> Result<Vec<MessageOut>, String> {
    let mut messages: Vec<MessageOut> = Vec::new();
    if let Some(instructions) = raw_present(req.get("instructions")) {
        // instructions 支持 string 或 content parts 数组：数组扁平化为文本
        // （原 value_to_string 会把数组序列化成字面 JSON 当 system prompt）
        let text = instructions_text(instructions);
        if !text.trim().is_empty() {
            messages.push(MessageOut {
                role: "system".into(),
                content: Some(Value::String(text)),
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
            });
        }
    }

    let Some(input) = raw_present(req.get("input")) else {
        backfill_tool_call_reasoning_placeholders(&mut messages);
        return Ok(messages);
    };
    // 跨 item 状态：待附挂思考 / 待冲刷的 tool 媒体（H1/M1）
    let mut pending_reasoning: Option<String> = None;
    let mut pending_media: Vec<Value> = Vec::new();
    match input {
        Value::String(s) => {
            messages.push(MessageOut {
                role: "user".into(),
                content: Some(Value::String(s.clone())),
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        Value::Array(items) => {
            for item in items {
                input_item_to_chat_messages(
                    item,
                    &mut messages,
                    &mut pending_reasoning,
                    &mut pending_media,
                    tool_ctx,
                )?;
            }
        }
        // M12：单个对象形态（非 string/array）按单 item 处理
        //（cc-switch :629-642 同款；此前整请求 400）
        Value::Object(_) => {
            input_item_to_chat_messages(
                input,
                &mut messages,
                &mut pending_reasoning,
                &mut pending_media,
                tool_ctx,
            )?;
        }
        other => {
            return Err(format!(
                "unsupported responses input type {:?}",
                dto::json_type_name(other)
            ));
        }
    }
    // input 尾部：冲刷残留媒体；剩余 pending reasoning 回溯附挂到最后一条 assistant
    // （其后已没有可前向附挂的目标，cc-switch :654-662 同款）
    flush_pending_media(&mut messages, &mut pending_media);
    attach_reasoning_to_last_assistant(&mut messages, pending_reasoning.take());
    backfill_tool_call_reasoning_placeholders(&mut messages);
    Ok(messages)
}

/// 单个 input item → 0..n 条消息（pending 状态由调用方持有）
fn input_item_to_chat_messages(
    item: &Value,
    messages: &mut Vec<MessageOut>,
    pending_reasoning: &mut Option<String>,
    pending_media: &mut Vec<Value>,
    tool_ctx: &ToolContext,
) -> Result<(), String> {
    let item_type = item
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    match item_type {
        INPUT_TYPE_REASONING => {
            // 思考先进入 pending，前向附挂到其后 assistant 消息 / 工具调用；
            // 不直接回溯（会把新一轮思考错拼进旧消息，cc-switch :764-773 注释同款）
            let text = reasoning_item_text(item);
            append_pending_reasoning(pending_reasoning, text);
        }
        INPUT_TYPE_FUNCTION_CALL => {
            let tool = function_call_item_to_chat_tool_call(item, tool_ctx)?;
            // M10：item 内嵌 reasoning 并入 pending（历史工具调用轮的思考）
            append_pending_reasoning(pending_reasoning, item_embedded_reasoning(item));
            // L14：新建 assistant 工具批前冲刷积压媒体（媒体贴着自己的工具回合）
            flush_pending_media(messages, pending_media);
            append_tool_call_to_last_assistant(messages, tool);
            attach_reasoning_to_last_assistant(messages, pending_reasoning.take());
        }
        INPUT_TYPE_WEB_SEARCH_CALL => {
            // 四-4：web_search_call item → assistant tool_calls（name=web_search,
            // arguments={"query":...}）+ tool 消息回执。Chat 上游无 hosted 工具，
            // 桥接为普通 function 调用往返（cc-switch 管线④反向桥接语义）
            let id = call_id(item);
            let query = item
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = serde_json::to_string(&json!({ "query": query }))
                .map_err(|e| format!("web_search_call query serialization failed: {e}"))?;
            // L14：新建 assistant 工具批前冲刷积压媒体
            flush_pending_media(messages, pending_media);
            append_tool_call_to_last_assistant(
                messages,
                ToolCallOut {
                    id,
                    r#type: "function".into(),
                    function: Some(FunctionOut {
                        description: String::new(),
                        name: "web_search".into(),
                        parameters: None,
                        strict: None,
                        arguments,
                    }),
                },
            );
            attach_reasoning_to_last_assistant(messages, pending_reasoning.take());
            // results（completed 时存在）canonical 序列化为工具回执；
            // in_progress（client 执行中）无 results → 空字符串
            let results = item
                .get("results")
                .map(crate::service::canonical::canonical_json_string)
                .unwrap_or_default();
            messages.push(MessageOut {
                role: "tool".into(),
                content: Some(Value::String(results)),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id(item)),
                reasoning_content: None,
            });
        }
        INPUT_TYPE_CUSTOM_TOOL_CALL => {
            let tool = custom_tool_call_item_to_chat_tool_call(item);
            append_pending_reasoning(pending_reasoning, item_embedded_reasoning(item));
            flush_pending_media(messages, pending_media);
            append_tool_call_to_last_assistant(messages, tool);
            attach_reasoning_to_last_assistant(messages, pending_reasoning.take());
        }
        // P0-4：tool_search_call item → 代理 function 调用（name=tool_search；
        // cc-switch responses_tool_search_call_to_chat_tool_call :1372-1391 同款）
        INPUT_TYPE_TOOL_SEARCH_CALL => {
            append_pending_reasoning(pending_reasoning, item_embedded_reasoning(item));
            flush_pending_media(messages, pending_media);
            append_tool_call_to_last_assistant(
                messages,
                tool_search_call_item_to_chat_tool_call(item),
            );
            attach_reasoning_to_last_assistant(messages, pending_reasoning.take());
        }
        INPUT_TYPE_FUNCTION_CALL_OUTPUT => {
            let call_id = item
                .get("call_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // output 里的媒体 part 抽取为相邻 user 媒体消息（Chat tool 消息不能带
            // 图片；cc-switch pipeline① :702-717 同款）
            let content = tool_output_to_chat_content(item.get("output"), &call_id, pending_media);
            messages.push(MessageOut {
                role: "tool".into(),
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id),
                reasoning_content: None,
            });
        }
        INPUT_TYPE_CUSTOM_TOOL_CALL_OUTPUT | INPUT_TYPE_TOOL_SEARCH_OUTPUT => {
            // custom 工具执行结果：canonical JSON 序列化整项（保留 type/输出结构，
            // 上游能看到工具真实回执；此前落入默认分支被静默丢弃 → 工具循环断裂）
            let call_id = item
                .get("call_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // custom/tool_search 输出同样走媒体抽取（H6：MCP 自由文本工具的
            // 截图/大文件不再留在巨型 JSON 文本里；残余 canonical 回写）
            let split = crate::service::tool_media::split_tool_output(&Value::Object(
                item.as_object().cloned().unwrap_or_default(),
            ));
            if !split.media.is_empty() {
                if !call_id.is_empty() {
                    pending_media.push(json!({
                        "type": "text",
                        "text": crate::service::tool_media::media_label(&call_id)
                    }));
                }
                pending_media.extend(split.media);
            }
            messages.push(MessageOut {
                role: "tool".into(),
                content: Some(Value::String(split.text)),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id),
                reasoning_content: None,
            });
        }
        // H5：裸 content-part item（input_text/input_image/input_file/input_audio/
        // input_video 直接作为 input 数组元素，无 role/content 包装）→ 单 part
        // 消息（cc-switch :774-805 同款；此前落入默认分支被静默丢弃 = 数据丢失）
        "input_text" | "output_text" | "input_image" | "input_file" | "input_audio"
        | "input_video" => {
            // P1-8：裸 part item 是新回合边界——先回溯附挂 pending reasoning、
            // 冲刷积压媒体（与 user 回合分支同语义；此前媒体错位 + 思考泄漏到
            // 后续工具批，cc-switch :774-821 边界冲刷同款）
            attach_reasoning_to_last_assistant(messages, pending_reasoning.take());
            flush_pending_media(messages, pending_media);
            let role = normalize_input_role(item.get("role").and_then(|r| r.as_str()));
            let content = content_parts_to_chat_content(std::slice::from_ref(item))?;
            messages.push(MessageOut {
                role: role.to_string(),
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        _ => {
            // message item（role+content）；hosted 工具 item（computer_call 等）
            // 无 content → 跳过（避免空 user 消息污染会话序）
            let Some(content) = item.get("content") else {
                return Ok(());
            };
            let role = normalize_input_role(item.get("role").and_then(|r| r.as_str()));
            let content = input_content_to_chat_content(Some(content))?;
            if role == "assistant" {
                messages.push(MessageOut {
                    role: "assistant".into(),
                    content: Some(content),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    reasoning_content: pending_reasoning.take().filter(|s| !s.trim().is_empty()),
                });
            } else {
                // user/system 回合边界：思考不允许跨 user 回合前向泄漏 —— 回溯附挂
                // 到上一条 assistant（cc-switch :1065-1098 语义）；媒体先冲刷
                attach_reasoning_to_last_assistant(messages, pending_reasoning.take());
                flush_pending_media(messages, pending_media);
                messages.push(MessageOut {
                    role: role.to_string(),
                    content: Some(content),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
        }
    }
    Ok(())
}

/// reasoning item → 文本。M10：对齐 cc-switch extract_reasoning_summary_text 全形态：
/// - `reasoning_content` / `content` / `text` 字符串直接命中
/// - `summary`：字符串形态（此前返回 None → 跨轮思考丢失）或数组形态
///   （各 part 取 text/content/裸字符串，join "\n\n"）
fn reasoning_item_text(item: &Value) -> Option<String> {
    for key in ["reasoning_content", "content", "text"] {
        if let Some(s) = item.get(key).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    match item.get("summary") {
        Some(Value::String(s)) => (!s.trim().is_empty()).then(|| s.clone()),
        Some(Value::Array(parts)) => {
            let texts: Vec<String> = parts
                .iter()
                .filter_map(|p| match p {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(o) => o
                        .get("text")
                        .or_else(|| o.get("content"))
                        .and_then(|t| t.as_str())
                        .map(str::to_string),
                    _ => None,
                })
                .filter(|s| !s.trim().is_empty())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n\n"))
        }
        _ => None,
    }
}

/// M10：item 内嵌 reasoning 字段提取（function_call / custom_tool_call / message
/// item 自带思考，cc-switch extract_reasoning_field_text :8-38 同款）——
/// 历史恢复时工具调用轮/assistant 轮的思考不再丢失
fn item_embedded_reasoning(item: &Value) -> Option<String> {
    for key in ["reasoning_content", "reasoning"] {
        match item.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.clone()),
            Some(Value::Object(o)) => {
                for k in ["content", "text", "summary"] {
                    if let Some(s) = o.get(k).and_then(|v| v.as_str()) {
                        if !s.trim().is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // reasoning_details 数组形态（xAI Grok 等）
    let details: Vec<String> = item
        .get("reasoning_details")
        .and_then(|d| d.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (!details.is_empty()).then(|| details.join("\n"))
}

fn append_pending_reasoning(pending: &mut Option<String>, text: Option<String>) {
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return;
    };
    match pending {
        Some(p) => {
            p.push('\n');
            p.push_str(&text);
        }
        None => *pending = Some(text),
    }
}

/// pending reasoning 附挂（设置或追加）到最后一条 assistant 消息；无 assistant 则丢弃
/// （保留 pending 语义由调用方决定；此处消费传入值）。
/// P1-9：回溯越过中间的 tool/user 消息定位最近 assistant（thinking 与 assistant
/// 之间隔着 tool_result 回执时不再丢失；cc-switch last_assistant_index 语义）
fn attach_reasoning_to_last_assistant(messages: &mut [MessageOut], reasoning: Option<String>) {
    let Some(reasoning) = reasoning.filter(|r| !r.trim().is_empty()) else {
        return;
    };
    let Some(last) = messages.iter_mut().rev().find(|m| m.role == "assistant") else {
        return;
    };
    match &mut last.reasoning_content {
        Some(existing) if !existing.trim().is_empty() => {
            existing.push('\n');
            existing.push_str(&reasoning);
        }
        _ => last.reasoning_content = Some(reasoning),
    }
}

/// 冲刷积压的 tool 媒体为一条 user 消息（内容为媒体 parts 数组，
/// 含 per-call 来源标注 part）
fn flush_pending_media(messages: &mut Vec<MessageOut>, pending_media: &mut Vec<Value>) {
    if pending_media.is_empty() {
        return;
    }
    let parts = std::mem::take(pending_media);
    messages.push(MessageOut {
        role: "user".into(),
        content: Some(Value::Array(parts)),
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning_content: None,
    });
}

/// assistant tool-call 消息缺 `reasoning_content` 时补占位（管线末端最终兜底；
/// cc-switch backfill_tool_call_reasoning_placeholders :1030-1063 同款，无厂商门控）
fn backfill_tool_call_reasoning_placeholders(messages: &mut [MessageOut]) {
    for m in messages.iter_mut() {
        if m.role == "assistant"
            && !m.tool_calls.is_empty()
            && m.reasoning_content
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            m.reasoning_content = Some(TOOL_CALL_REASONING_PLACEHOLDER.to_string());
        }
    }
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

/// content parts 数组 → Chat parts；纯文本时收敛为字符串。
/// L7：refusal part → text part；L8：空 text part 跳过、多段 join("\n")；
/// 媒体 part 统一走共享形状识别器（L6：URL-only file 不产出非法形态）
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
            "input_text" | "output_text" | "text" | "refusal" => {
                let text = if part_type == "refusal" {
                    part.get("refusal")
                        .or_else(|| part.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                } else {
                    part.get("text").and_then(|t| t.as_str()).unwrap_or("")
                };
                if !text.is_empty() {
                    if !text_only.is_empty() {
                        text_only.push('\n');
                    }
                    text_only.push_str(text);
                    chat_parts.push(json!({"type": "text", "text": text}));
                }
            }
            "input_image" | "input_file" | "input_audio" | "input_video" | "image_url" | "file" => {
                if let Some(media) =
                    crate::service::tool_media::chat_media_part(&Value::Object(part.clone()))
                {
                    only_text = false;
                    chat_parts.push(media);
                }
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

fn function_call_item_to_chat_tool_call(
    item: &Value,
    tool_ctx: &ToolContext,
) -> Result<ToolCallOut, String> {
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return Err("function_call item is missing name".into());
    }
    // P0-4：namespace 字段 → 拍平 chat 名（历史轮 MCP 工具调用往返一致；
    // cc-switch responses_function_call_to_chat_tool_call :1329-1351 同款）
    let namespace = item
        .get("namespace")
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let chat_name = tool_ctx.chat_name_for(name, namespace);
    // P1-19：空参数钳为 "{}"（Minimax 等严格上游对 arguments:"" 直接 400；
    // cc-switch json_canonical.rs:48-63 同款）
    let arguments = crate::service::canonical::canonicalize_json_string_if_parseable(
        &arguments_string(item.get("arguments").unwrap_or(&Value::Null)),
    );
    let arguments = if arguments.trim().is_empty() {
        "{}".to_string()
    } else {
        arguments
    };
    Ok(ToolCallOut {
        id: call_id(item),
        r#type: "function".into(),
        function: Some(FunctionOut {
            description: String::new(),
            name: chat_name,
            parameters: None,
            strict: None,
            arguments,
        }),
    })
}

/// P0-4：tool_search_call item → 代理 function tool_call
///（cc-switch responses_tool_search_call_to_chat_tool_call :1372-1391 同款）
fn tool_search_call_item_to_chat_tool_call(item: &Value) -> ToolCallOut {
    let arguments = item
        .get("arguments")
        .map(crate::service::canonical::canonical_json_string)
        .unwrap_or_else(|| "{}".to_string());
    ToolCallOut {
        id: call_id(item),
        r#type: "function".into(),
        function: Some(FunctionOut {
            description: String::new(),
            name: TOOL_SEARCH_PROXY_NAME.into(),
            parameters: None,
            strict: None,
            arguments,
        }),
    }
}

/// custom_tool_call item → Chat function tool_call（H1 修复）：
/// - wire `type` 恒为 "function"（`custom_tool_call` 非法，严格上游 400）
/// - 自由文本 input 包装为 `{"input": <str>}`（与 tools 侧降级 schema 对齐，
///   cc-switch `responses_custom_tool_call_to_chat_tool_call` :1353-1370 同款）
fn custom_tool_call_item_to_chat_tool_call(item: &Value) -> ToolCallOut {
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let input = item.get("input").cloned().unwrap_or_else(|| json!(""));
    let arguments = crate::service::canonical::canonical_json_string(&json!({
        CUSTOM_TOOL_INPUT_FIELD: input
    }));
    ToolCallOut {
        id: call_id(item),
        r#type: "function".into(),
        function: Some(FunctionOut {
            description: String::new(),
            name,
            parameters: None,
            strict: None,
            arguments,
        }),
    }
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
            reasoning_content: None,
        });
    }
    messages
        .last_mut()
        .expect("assistant message exists")
        .tool_calls
        .push(tool);
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

/// tool item `output` → tool 消息 content。H6：委托共享递归抽取器
/// （tool_media::split_tool_output）——字符串/数组/对象统一处理：JSON 字符串
/// 内嵌媒体递归抽取、残余 base64 钳制、整串 data-URL 搬运；媒体带 per-call
/// 来源标注（L15，模型可归因多工具媒体）。
fn tool_output_to_chat_content(
    output: Option<&Value>,
    call_id: &str,
    pending_media: &mut Vec<Value>,
) -> Value {
    let Some(output) = output else {
        return Value::String(String::new());
    };
    if output.is_null() {
        return Value::String(String::new());
    }
    let split = crate::service::tool_media::split_tool_output(output);
    if !split.media.is_empty() {
        if !call_id.trim().is_empty() {
            pending_media.push(json!({
                "type": "text",
                "text": crate::service::tool_media::media_label(call_id)
            }));
        }
        pending_media.extend(split.media);
    }
    Value::String(split.text)
}

/// tools 数组改写（H1 修复 + P0-4）：
/// - `{type:function,...}` → Chat 嵌套结构（parameters null 过滤）
/// - `{type:custom,...}` → 降级为 function：原始定义 canonical 嵌入 description、
///   parameters 固定 `{"input": string}`（cc-switch add_custom_tool :141-169 同款）
/// - P0-4 `{type:namespace, name, tools:[…]}` → 子 function 拍平为 `ns__name`
/// - P0-4 `{type:tool_search}` → 注入代理 function（query/limit 单参，
///   cc-switch add_tool_search_tool :171-199 同款）
/// - 其余 hosted 类型（file_search 等）防御性跳过 —— 不产出非法 wire 形态
fn request_tools_to_chat(
    tools: Option<&Value>,
    tool_ctx: &ToolContext,
) -> Result<Vec<ToolCallOut>, String> {
    let Some(tools) = raw_present(tools) else {
        return Ok(Vec::new());
    };
    let Value::Array(arr) = tools else {
        return Err("invalid tools".into());
    };
    let mut out = Vec::with_capacity(arr.len());
    for tool in arr {
        // H4：Responses 协议允许 `tools: ["apply_patch"]` 字符串简写（custom 工具），
        // cc-switch add_response_tool :220-233 同款按 custom 降级（此前整请求 400）
        if let Value::String(name) = tool {
            let name = name.trim();
            if !name.is_empty() {
                out.push(custom_tool_to_chat(name, "", tool));
            }
            continue;
        }
        let Some(t) = tool.as_object() else {
            return Err("invalid tools".into());
        };
        let tool_type = t.get("type").and_then(|x| x.as_str()).unwrap_or("").trim();
        if tool_type == "web_search" {
            // 四-4：hosted web_search 桥接为 function 工具（query 单参），
            // 上游返回的 web_search 调用由流式层映回 web_search_call item
            out.push(ToolCallOut {
                id: String::new(),
                r#type: "function".into(),
                function: Some(FunctionOut {
                    description: t
                        .get("description")
                        .map(value_to_string)
                        .unwrap_or_else(|| "Web search tool".into()),
                    name: "web_search".into(),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string", "description": "Search query"}
                        },
                        "required": ["query"]
                    })),
                    strict: None,
                    arguments: String::new(),
                }),
            });
            continue;
        }
        // P0-4：tool_search → 代理 function（cc-switch add_tool_search_tool 同款）
        if tool_type == "tool_search" {
            out.push(ToolCallOut {
                id: String::new(),
                r#type: "function".into(),
                function: Some(FunctionOut {
                    description: "Search and load Codex tools, plugins, connectors, and MCP namespaces for the current task.".into(),
                    name: TOOL_SEARCH_PROXY_NAME.into(),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query for tools or connectors to load."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of tool groups to return."
                            }
                        },
                        "required": ["query"]
                    })),
                    strict: None,
                    arguments: String::new(),
                }),
            });
            continue;
        }
        // P0-4：namespace 包装工具 → 子 function 逐个拍平（cc-switch add_namespace_tool
        // :201-218 同款；chat 名与 ToolContext 注册名一致，响应侧可还原）
        if tool_type == "namespace" {
            let Some(namespace) = t
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Some(children) = t
                .get("tools")
                .or_else(|| t.get("children"))
                .and_then(|v| v.as_array())
            else {
                continue;
            };
            for child in children {
                let child_obj = match child.as_object() {
                    Some(o) => o,
                    None => continue,
                };
                if let Some(converted) = function_tool_to_chat(child_obj, Some(namespace), tool_ctx)
                {
                    out.push(converted);
                }
            }
            continue;
        }
        if tool_type == "function" || tool_type.is_empty() {
            if let Some(converted) = function_tool_to_chat(t, None, tool_ctx) {
                out.push(converted);
            }
        } else if tool_type == "custom" {
            let name = t
                .get("name")
                .map(value_to_string)
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let original_desc = t
                .get("description")
                .map(value_to_string)
                .unwrap_or_default();
            out.push(custom_tool_to_chat(&name, &original_desc, tool));
        }
        // 其它 hosted 类型跳过（file_search / computer_use 等；原生 Responses
        // 透传路径不经过本函数，hosted 工具在那里保留原样）
    }
    Ok(out)
}

/// 单个 function 工具定义 → Chat 嵌套结构（P0-4 抽取共用：namespace 子工具与
/// 顶层 function 走同一转换，仅 chat 名按 namespace 拍平）
fn function_tool_to_chat(
    t: &serde_json::Map<String, Value>,
    namespace: Option<&str>,
    tool_ctx: &ToolContext,
) -> Option<ToolCallOut> {
    // M5：Chat 风格嵌套形态 `{type:function, function:{...}}` 的子对象优先
    //（此前只读顶层 name，嵌套形态产出空名非法工具，严格上游 400）
    let nested = t.get("function").and_then(|f| f.as_object());
    let field = |key: &str| -> Option<Value> {
        nested
            .and_then(|n| n.get(key))
            .or_else(|| t.get(key))
            .cloned()
    };
    let name = field("name")
        .map(|v| value_to_string(&v))
        .unwrap_or_default()
        .trim()
        .to_string();
    // M5：无名工具跳过（cc-switch responses_tool_name 空名 → None 整工具丢弃）
    if name.is_empty() {
        tracing::warn!("dropping function tool without name");
        return None;
    }
    let chat_name = tool_ctx.chat_name_for(&name, namespace);
    let description = field("description")
        .map(|v| value_to_string(&v))
        .unwrap_or_default();
    let parameters = normalize_function_parameters(field("parameters"));
    // L12：strict 结构化输出标志透传（嵌套子对象优先）
    let strict = nested
        .and_then(|n| n.get("strict"))
        .or_else(|| t.get("strict"))
        .cloned();
    let mut function = FunctionOut {
        description,
        name: chat_name,
        parameters: Some(parameters),
        strict: None,
        arguments: String::new(),
    };
    if let Some(strict) = strict {
        function.strict = Some(strict);
    }
    Some(ToolCallOut {
        id: String::new(),
        r#type: "function".into(),
        function: Some(function),
    })
}

/// custom 工具定义降级为 function：原始定义 canonical 嵌入 description、
/// parameters 固定 `{"input": string}`（cc-switch add_custom_tool :141-169 同款）
fn custom_tool_to_chat(name: &str, original_desc: &str, tool: &Value) -> ToolCallOut {
    let description = if original_desc.trim().is_empty() {
        format!(
            "{CUSTOM_TOOL_PRESERVED_METADATA_HEADING}\n```json\n{}```",
            crate::service::canonical::canonical_json_string(tool)
        )
    } else {
        format!(
            "{original_desc}\n\n{CUSTOM_TOOL_PRESERVED_METADATA_HEADING}\n```json\n{}```",
            crate::service::canonical::canonical_json_string(tool)
        )
    };
    ToolCallOut {
        id: String::new(),
        r#type: "function".into(),
        function: Some(FunctionOut {
            description,
            name: name.to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    CUSTOM_TOOL_INPUT_FIELD: {
                        "type": "string",
                        "description": CUSTOM_TOOL_INPUT_DESCRIPTION
                    }
                },
                "required": [CUSTOM_TOOL_INPUT_FIELD]
            })),
            strict: None,
            arguments: String::new(),
        }),
    }
}

/// M11：parameters 规范化（cc-switch normalize_function_parameters :1272-1286 同款）：
/// 缺失/null/非 object → `{"type":"object","properties":{}}`；type 非 object 强制改写。
/// 严格上游（vLLM strict 等）要求 parameters 必须为 object schema，原样透传畸形值会 400。
fn normalize_function_parameters(parameters: Option<Value>) -> Value {
    let mut v = match parameters {
        Some(v) if v.is_object() => v,
        _ => json!({"type": "object", "properties": {}}),
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("type".into(), json!("object"));
    }
    v
}

/// tool_choice 改写（P0-2 + P0-4）：
/// - `{type:"function", name[, namespace]}` → Chat 嵌套结构（namespace 拍平）
/// - `{type:"custom", name}` → Chat 嵌套结构（cc-switch :1414-1422 同款；
///   此前原样透传非法形态 → 严格上游 400）
/// - `{type:"tool_search"}` → 代理 function 名（cc-switch :1406-1413 同款）
/// - 字符串（auto/none/required）与未知对象原样透传
fn request_tool_choice_to_chat(
    tool_choice: Option<&Value>,
    tool_ctx: &ToolContext,
) -> Result<Option<Value>, String> {
    let Some(tc) = raw_present(tool_choice) else {
        return Ok(None);
    };
    if let Value::String(s) = tc {
        return Ok(Some(Value::String(s.clone())));
    }
    let Some(map) = tc.as_object() else {
        return Err("invalid tool_choice".into());
    };
    let choice_type = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let named =
        |chat_name: String| Some(json!({"type": "function", "function": {"name": chat_name}}));
    match choice_type {
        "function" => {
            let name = map
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                let namespace = map
                    .get("namespace")
                    .and_then(|n| n.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                return Ok(named(tool_ctx.chat_name_for(name, namespace)));
            }
            Ok(Some(tc.clone()))
        }
        // P0-2：custom → 同名 function 必选形态（原样透传会被严格上游 400）
        "custom" => {
            let name = map
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                return Ok(named(name.to_string()));
            }
            Ok(Some(tc.clone()))
        }
        // P0-4：tool_search → 代理 function 必选形态
        "tool_search" => Ok(named(TOOL_SEARCH_PROXY_NAME.to_string())),
        _ => Ok(Some(tc.clone())),
    }
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
    let format_type = format
        .get("type")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
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
        let out = responses_request_to_chat(&req, "gpt-test").expect("convert");
        assert_eq!(out["model"], "gpt-test");
        assert_eq!(out["stream"], json!(true));
        // 非 o 系列模型走 max_tokens（DeepSeek 等 Chat 上游兼容）
        assert_eq!(out["max_tokens"], json!(1024));
        assert!(out.get("max_completion_tokens").is_none());
        // o 系列模型改写为 max_completion_tokens
        let mut o_req = req.clone();
        o_req["model"] = json!("openai/o3-mini");
        let o_out = responses_request_to_chat(&o_req, "openai/o3-mini").unwrap();
        assert_eq!(o_out["max_completion_tokens"], json!(1024));
        assert!(o_out.get("max_tokens").is_none());
        // instructions → system
        assert_eq!(
            out["messages"][0],
            json!({"role": "system", "content": "You are a helpful assistant."})
        );
        // 多模态 user content parts
        assert_eq!(
            out["messages"][1]["content"][0],
            json!({"type": "text", "text": "What is in this image?"})
        );
        assert_eq!(
            out["messages"][1]["content"][1],
            json!({"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}})
        );
        // function_call item → assistant tool_calls（含占位 reasoning_content，M1 兜底）
        let assistant = &out["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null());
        assert_eq!(
            assistant["tool_calls"][0],
            json!({"id": "call_abc", "type": "function",
                   "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}})
        );
        assert_eq!(assistant["reasoning_content"], "tool call");
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
        let out = responses_request_to_chat(&json!({"model": "m", "input": "hello"}), "m").unwrap();
        assert_eq!(
            out["messages"][0],
            json!({"role": "user", "content": "hello"})
        );
    }

    #[test]
    fn rejects_stateful_fields() {
        let err = responses_request_to_chat(
            &json!({
                "model": "m",
                "input": "hi",
                "conversation": "conv_1",
                "previous_response_id": "resp_x"
            }),
            "m",
        )
        .unwrap_err();
        assert!(err.contains("stateful fields"), "{err}");
        assert!(err.contains("previous_response_id"));
        assert!(err.contains("conversation"));
    }

    #[test]
    fn rejects_missing_model() {
        let err = responses_request_to_chat(&json!({"input": "hi"}), "").unwrap_err();
        assert_eq!(err, "model is required");
    }

    #[test]
    fn maps_tool_choice_function() {
        let out = responses_request_to_chat(
            &json!({
                "model": "m",
                "input": "hi",
                "tools": [{"type": "function", "name": "get_weather"}],
                "tool_choice": {"type": "function", "name": "get_weather"}
            }),
            "m",
        )
        .unwrap();
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
    }

    /// M2：tools 为空时 tool_choice / parallel_tool_calls 必须剥离（vLLM 400 防护）
    #[test]
    fn empty_tools_strips_tool_choice_and_parallel_tool_calls() {
        let out = responses_request_to_chat(
            &json!({
                "model": "m",
                "input": "hi",
                "tool_choice": "auto",
                "parallel_tool_calls": true
            }),
            "m",
        )
        .unwrap();
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn maps_text_format_json_schema() {
        let out = responses_request_to_chat(
            &json!({
                "model": "m",
                "input": "hi",
                "text": {"format": {"type": "json_schema", "name": "weather",
                    "schema": {"type": "object"}, "strict": true}}
            }),
            "m",
        )
        .unwrap();
        assert_eq!(out["response_format"]["type"], "json_schema");
        assert_eq!(out["response_format"]["json_schema"]["name"], "weather");
        assert_eq!(out["response_format"]["json_schema"]["strict"], true);
    }

    /// M3：仅支持 reasoning_effort 的模型族转发；none 保留标记；不支持模型族不发
    #[test]
    fn reasoning_effort_model_family_gating() {
        let gpt5 = responses_request_to_chat(
            &json!({
                "model": "gpt-5.1", "input": "hi", "reasoning": {"effort": "high"}
            }),
            "gpt-5.1",
        )
        .unwrap();
        assert_eq!(gpt5["reasoning_effort"], "high");

        let none = responses_request_to_chat(
            &json!({
                "model": "gpt-5.1", "input": "hi", "reasoning": {"effort": "none"}
            }),
            "gpt-5.1",
        )
        .unwrap();
        assert_eq!(
            none.get("reasoning_effort"),
            Some(&json!("none")),
            "显式关闭保留 none，由路由级 effort_mode 决定终态（M9）"
        );

        let deepseek = responses_request_to_chat(
            &json!({
                "model": "deepseek-chat", "input": "hi", "reasoning": {"effort": "high"}
            }),
            "deepseek-chat",
        )
        .unwrap();
        assert!(
            deepseek.get("reasoning_effort").is_none(),
            "不支持的模型族不发"
        );
    }

    /// L3：未知参数白名单透传（cc-switch EXTRA_CHAT_PASSTHROUGH_FIELDS 子集）
    #[test]
    fn passthrough_sampling_fields() {
        let out = responses_request_to_chat(
            &json!({
                "model": "m",
                "input": "hi",
                "frequency_penalty": 0.5,
                "presence_penalty": 0.1,
                "logit_bias": {"50256": -100},
                "logprobs": true,
                "n": 2,
                "seed": 42,
                "stop": ["\n\n"]
            }),
            "m",
        )
        .unwrap();
        assert_eq!(out["frequency_penalty"], json!(0.5));
        assert_eq!(out["presence_penalty"], json!(0.1));
        assert_eq!(out["logit_bias"], json!({"50256": -100}));
        assert_eq!(out["logprobs"], json!(true));
        assert_eq!(out["n"], json!(2));
        assert_eq!(out["seed"], json!(42));
        assert_eq!(out["stop"], json!(["\n\n"]));
    }

    /// H1：custom 工具全链路 —— 定义降级为 function（定义嵌入 description）、
    /// 调用包装为 {"input": ...}、输出序列化为 tool 消息（不再丢失）
    #[test]
    fn custom_tool_round_trip() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "custom_tool_call", "call_id": "cust_1", "name": "apply_patch",
                 "input": "*** Begin Patch\n*** End Patch"},
                {"type": "custom_tool_call_output", "call_id": "cust_1", "output": "Done!"}
            ],
            "tools": [{"type": "custom", "name": "apply_patch",
                       "description": "Apply a patch", "format": {"type": "text"}}]
        });
        let out = responses_request_to_chat(&req, "gpt-test").unwrap();
        // 调用侧：合法 function wire 形态 + input 包装
        let assistant = &out["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["id"], "cust_1");
        // 精确断言（canonical JSON）
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            serde_json::Value::String(crate::service::canonical::canonical_json_string(
                &json!({"input": "*** Begin Patch\n*** End Patch"})
            ))
        );
        // 输出侧：tool 消息（canonical JSON 序列化整项），不再静默丢弃
        let tool_msg = &out["messages"][1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "cust_1");
        let content = tool_msg["content"].as_str().unwrap();
        assert!(
            content.contains("\"output\":\"Done!\""),
            "content={content}"
        );
        assert!(
            content.contains("custom_tool_call_output"),
            "保留 item 类型：{content}"
        );
        // 定义侧：降级 function + 原始定义嵌入 description
        let tool_def = &out["tools"][0];
        assert_eq!(tool_def["type"], "function");
        assert_eq!(tool_def["function"]["name"], "apply_patch");
        let desc = tool_def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("Original tool definition:"));
        assert!(desc.contains("apply_patch"));
        assert_eq!(
            tool_def["function"]["parameters"]["required"],
            json!(["input"])
        );
    }

    /// H1 媒体半边：function_call_output 中的图片抽取为相邻 user 媒体消息
    #[test]
    fn function_call_output_media_extracted() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "screenshot", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": [
                    {"type": "input_text", "text": "captured"},
                    {"type": "input_image", "image_url": "https://example.com/s.png"}
                ]}
            ]
        });
        let out = responses_request_to_chat(&req, "gpt-test").unwrap();
        let msgs = out["messages"].as_array().unwrap();
        // assistant(tool_calls) → tool(text) → user(媒体)
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(
            msgs[1]["content"],
            format!(
                "captured\n{}",
                crate::service::tool_media::MEDIA_MOVED_MARKER
            )
        );
        assert_eq!(msgs[2]["role"], "user");
        // L15：媒体前插 per-call 来源标注；image_url 字符串形态包装为 {url}
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[2]["content"][0]["type"], "text");
        assert_eq!(
            msgs[2]["content"][1],
            json!({"type": "image_url", "image_url": {"url": "https://example.com/s.png"}})
        );
    }

    /// M1：reasoning item 前向附挂到其后 function_call 的 assistant 消息
    #[test]
    fn reasoning_item_attaches_forward_to_tool_call() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "I should check weather"}]},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"}
            ]
        });
        let out = responses_request_to_chat(&req, "gpt-test").unwrap();
        assert_eq!(
            out["messages"][0]["reasoning_content"],
            "I should check weather"
        );
    }

    /// M1：user 回合边界的 pending reasoning 回溯附挂到上一条 assistant
    #[test]
    fn trailing_reasoning_backfills_to_previous_assistant() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "message", "role": "assistant", "content": "hello"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "trailing thought"}]},
                {"type": "message", "role": "user", "content": "go on"}
            ]
        });
        let out = responses_request_to_chat(&req, "gpt-test").unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "user/assistant/user 三条消息");
        assert_eq!(msgs[1]["reasoning_content"], "trailing thought");
    }

    /// 四-4：hosted web_search 工具桥接为 function；web_search_call item
    /// 往返为 tool_calls + tool 消息
    #[test]
    fn web_search_bridge_request_conversion() {
        let req = json!({
            "model": "m",
            "instructions": "s",
            "input": [
                {"type": "message", "role": "user", "content": "find it"},
                {"type": "web_search_call", "id": "ws_1", "status": "completed",
                 "query": "Rust async",
                 "results": [{"url": "https://x", "title": "t", "text": "body"}]}
            ],
            "tools": [{"type": "web_search"}]
        });
        let out = responses_request_to_chat(&req, "gpt-test").unwrap();
        // 工具降级为 function（query 单参 schema）
        assert_eq!(out["tools"][0]["function"]["name"], "web_search");
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["required"],
            json!(["query"])
        );
        // web_search_call → assistant tool_calls + tool 回执
        let msgs = out["messages"].as_array().unwrap();
        let call_msg = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && !m["tool_calls"].is_null());
        assert!(call_msg.is_some(), "assistant tool_calls 存在");
        let tc = &call_msg.unwrap()["tool_calls"][0];
        assert_eq!(tc["id"], "ws_1");
        assert_eq!(tc["function"]["name"], "web_search");
        assert_eq!(tc["function"]["arguments"], "{\"query\":\"Rust async\"}");
        let tool_msg = msgs
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool 回执");
        assert_eq!(tool_msg["tool_call_id"], "ws_1");
        assert!(tool_msg["content"].as_str().unwrap().contains("https://x"));
    }

    /// 四-4：桥接检测（流式层据此映回 web_search_call；P0-4 起并入 ToolContext）
    #[test]
    fn request_bridges_web_search_detection() {
        use crate::service::responses::tool_ctx::ToolContext;
        assert!(
            ToolContext::from_request(&json!({"tools": [{"type": "web_search"}]}))
                .bridges_web_search
        );
        assert!(
            !ToolContext::from_request(&json!({"tools": [{"type": "function", "name": "f"}]}))
                .bridges_web_search
        );
        assert!(!ToolContext::from_request(&json!({})).bridges_web_search);
    }
}
