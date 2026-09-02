//! Anthropic Messages 请求 → Chat Completions 请求（cc-switch `transform.rs` :140 移植）。
//!
//! 字段级映射：
//! - system（string / `[{type:text,text}]` 数组）→ system 消息（剥 billing header）
//! - messages[].content（string / 块数组）：
//!   text → text part；image → image_url；tool_use → tool_calls（canonical args）；
//!   tool_result → 独立 role:"tool" 消息；thinking → reasoning_content
//! - max_tokens 直传；stop_sequences → stop；temperature/top_p 直传；top_k 丢弃
//! - tools：input_schema → parameters（根强制 object）；tool_choice：
//!   any→"required"（字符串）、{type:tool,name}→{type:function,function:{name}}；
//!   未知块 canonical JSON 保留（不再静默清空）
//! - reasoning_content 仅对要求该字段的厂商（DeepSeek/MiMo，模型名或 base_url
//!   命中）注入；纯 thinking 回合整条丢弃（M14，防严格上游 400）
//! - tool_result 媒体走共享递归抽取器（tool_media::split_tool_output），
//!   搬运媒体带 per-call 来源标注

use serde_json::{Map, Value, json};

use crate::service::canonical::canonical_json_string;

/// 要求 assistant tool-call 消息带非空 reasoning_content 的厂商（cc-switch
/// REASONING_VENDOR_HINTS；Moonshot/Kimi 已按厂商要求移除）
const REASONING_VENDOR_HINTS: [&str; 3] = ["deepseek", "mimo", "xiaomimimo"];

/// 空思考占位符（DeepSeek 拒绝空 reasoning_content 的 tool-call 消息）
const THINKING_PLACEHOLDER: &str = "tool call";

/// Claude Code 注入的动态计费头（每请求变化，不剥会破坏上游前缀缓存，cc-switch #2350）
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// Anthropic 请求 → Chat 请求。失败返回错误消息（BadRequest 语义）。
/// `gating_model`：模型族门控用的模型名（H1 = 路由映射后的上游模型，
/// 无映射时与客户端一致）——出站体 model 仍写客户端模型，由代理层改写。
/// `base_url`：渠道 base URL（M19：DeepSeek/MiMo 厂商 quirk 判定的兜底信号，
/// 模型映射为自定义名但端点是厂商端点时仍能命中）。
pub fn anthropic_request_to_chat(
    req: &Value,
    gating_model: &str,
    base_url: &str,
) -> Result<Value, String> {
    let mut out = Map::new();

    // model / stream / 采样参数
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .ok_or("missing 'model' field")?
        .to_string();
    out.insert("model".into(), Value::String(model.clone()));
    if let Some(stream) = req.get("stream").and_then(|s| s.as_bool()) {
        out.insert("stream".into(), Value::Bool(stream));
    }
    for key in ["temperature", "top_p"] {
        if let Some(v) = req.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    // H2：thinking / output_config.effort → reasoning_effort（此前完全丢弃，
    // Claude Code 扩展思考意图经网关后丢失）。cc-switch resolve_reasoning_effort
    // （transform.rs:94-124）+ supports_reasoning_effort 门控（:71-81）同款：
    // 仅支持该参数的模型族注入（H1：按映射后上游模型判定，claude-* 客户端
    // 路由到 gpt-5 系上游时思考意图不再丢失；反向映射到 deepseek 时不发）
    if crate::service::model_family::supports_reasoning_effort(gating_model) {
        if let Some(effort) = resolve_reasoning_effort(req) {
            out.insert("reasoning_effort".into(), json!(effort));
        }
    }
    // Anthropic max_tokens 必填；缺失按上游兼容交给 Chat 端默认（不注入）。
    // o 系列模型（o1/o3/o4…）只认 max_completion_tokens（cc-switch T:58-64 同款；
    // H1：按映射后上游模型判定，避免 claude-* 映射到 o 系时输出上限静默丢失）
    if let Some(max) = req
        .get("max_tokens")
        .and_then(|m| m.as_u64())
        .filter(|m| *m > 0)
    {
        let bare = gating_model.rsplit('/').next().unwrap_or(gating_model);
        let is_o_series = crate::service::model_family::is_openai_o_series(bare);
        let key = if is_o_series {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        out.insert(key.into(), json!(max));
    }
    // L10：Anthropic 规范字段为 metadata.user_id（此前读 /metadata/user 永远取不到）
    if let Some(user) = req
        .pointer("/metadata/user_id")
        .or_else(|| req.pointer("/metadata/user"))
        .and_then(|u| u.as_str())
    {
        out.insert("user".into(), json!(user));
    }
    if let Some(stops) = req_stop_sequences(req) {
        out.insert("stop".into(), stops);
    }
    // system → system 消息（剥 billing header 前缀行）
    let mut messages: Vec<Value> = Vec::new();
    // H1：DeepSeek/MiMo 厂商 quirk 按映射后上游模型判定（claude-* 客户端
    // 路由到 deepseek 上游时 tool-call 消息同样需要非空思考占位）
    let vendor_requires_reasoning = requires_reasoning_content(gating_model, base_url);
    if let Some(system_text) = system_to_text(req.get("system")) {
        if !system_text.is_empty() {
            messages.push(json!({"role": "system", "content": system_text}));
        }
    }

    // messages → chat messages
    let Some(arr) = req.get("messages").and_then(|m| m.as_array()) else {
        return Err("missing 'messages' field".into());
    };
    for msg in arr {
        convert_message_to_chat(msg, &mut messages, vendor_requires_reasoning)?;
    }
    out.insert("messages".into(), Value::Array(messages));

    // tools
    if let Some(tools) = convert_tools(req.get("tools")) {
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
            if let Some(choice) = convert_tool_choice(req.get("tool_choice")) {
                out.insert("tool_choice".into(), choice);
            }
            // 四-1：disable_parallel_tool_use → Chat parallel_tool_calls:false
            //（cc-switch transform_codex_anthropic.rs:427-435 反方向映射）
            if let Some(obj) = req.get("tool_choice").and_then(|t| t.as_object()) {
                if obj
                    .get("disable_parallel_tool_use")
                    .and_then(|v| v.as_bool())
                    == Some(true)
                {
                    out.insert("parallel_tool_calls".into(), json!(false));
                }
            }
        }
    }

    Ok(Value::Object(out))
}

/// Anthropic thinking / output_config.effort → OpenAI reasoning_effort
/// （cc-switch transform.rs:94-124 同款）：
/// 1. `output_config.effort` 显式优先：low/medium/high 1:1，max→xhigh，未知忽略
/// 2. `thinking` 回退：adaptive→xhigh；enabled 按预算分档（<4000 low /
///    4000–15999 medium / ≥16000 high；无预算 high）；disabled/缺失 → None
fn resolve_reasoning_effort(req: &Value) -> Option<&'static str> {
    if let Some(effort) = req
        .pointer("/output_config/effort")
        .and_then(|v| v.as_str())
    {
        return match effort {
            "low" => Some("low"),
            "medium" => Some("medium"),
            "high" => Some("high"),
            "max" => Some("xhigh"),
            _ => None,
        };
    }
    let thinking = req.get("thinking")?;
    match thinking.get("type").and_then(|t| t.as_str()) {
        Some("adaptive") => Some("xhigh"),
        Some("enabled") => {
            let budget = thinking.get("budget_tokens").and_then(|b| b.as_u64());
            match budget {
                Some(b) if b < 4_000 => Some("low"),
                Some(b) if b < 16_000 => Some("medium"),
                Some(_) => Some("high"),
                None => Some("high"),
            }
        }
        _ => None,
    }
}
fn req_stop_sequences(req: &Value) -> Option<Value> {
    let arr = req.get("stop_sequences")?.as_array()?;
    let stops: Vec<&str> = arr
        .iter()
        .filter_map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    (!stops.is_empty()).then(|| json!(stops))
}

/// system → 文本：string 原样；数组拼接 text 块（L18：join("\n")，与 cc-switch
/// normalize_openai_system_messages 的字节形态一致，跨网关前缀缓存对齐）；
/// 每段剥 billing header
fn system_to_text(system: Option<&Value>) -> Option<String> {
    let system = system?;
    let text = match system {
        Value::String(s) => strip_billing_header(s),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(strip_billing_header)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    (!text.trim().is_empty()).then_some(text)
}
/// 剥离文本开头的 x-anthropic-billing-header 行（L19：仅当以该前缀开头才剥，
/// 中段出现的字面行保留——cc-switch "Later occurrences are kept" 同款）
fn strip_billing_header(text: &str) -> String {
    let trimmed_start = text.trim_start();
    let Some(rest) = trimmed_start.strip_prefix(BILLING_HEADER_PREFIX) else {
        return text.to_string();
    };
    // 跳过前缀所在的整行（保留后续行的真实 system 内容）；
    // 前缀行后无换行 = 整段只有计费头 → 空
    match rest.find(['\n', '\r']) {
        Some(idx) => rest[idx..].trim_start_matches(['\r', '\n']).to_string(),
        None => String::new(),
    }
}

/// 模型名或渠道 base_url 是否命中"要求 tool-call 消息带 reasoning_content"的厂商
///（cc-switch should_preserve_reasoning_content_for_openai_chat：模型名或
/// base_url 任一命中 vendor hint，防模型映射改名后 quirk 失效）
fn requires_reasoning_content(model: &str, base_url: &str) -> bool {
    let model_hit = REASONING_VENDOR_HINTS
        .iter()
        .any(|h| model.to_lowercase().contains(h));
    let url_hit = base_url
        .to_lowercase()
        .split('/')
        .any(|seg| REASONING_VENDOR_HINTS.iter().any(|h| seg.contains(h)));
    model_hit || url_hit
}

/// 单条 Anthropic message → 0..n 条 Chat 消息。
/// tool_result 产出独立 tool 消息；tool_use 汇入 assistant tool_calls；
/// thinking 汇入 assistant reasoning_content（vendor hint 时空思考补占位符）
fn convert_message_to_chat(
    msg: &Value,
    messages: &mut Vec<Value>,
    vendor_requires_reasoning: bool,
) -> Result<(), String> {
    let role = msg
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("user")
        .to_string();

    match msg.get("content") {
        // 纯文本 content
        Some(Value::String(s)) => {
            messages.push(json!({"role": role, "content": s}));
            return Ok(());
        }
        Some(Value::Array(blocks)) => {
            convert_blocks_to_chat(&role, blocks, messages, vendor_requires_reasoning)
        }
        // content 缺失：占位 null（Chat 端允许 content:null）
        None => {
            messages.push(json!({"role": role, "content": null}));
            Ok(())
        }
        // P1-12：非法标量 content（number/bool）原样透传交给上游裁决，而非静默
        // 丢整条消息（cc-switch transform.rs:512-513 同款——无提示语义丢失比
        // 上游 400 更危险）
        Some(other) => {
            messages.push(json!({"role": role, "content": other}));
            Ok(())
        }
    }
}

/// content 块数组 → Chat 消息（user 块 + tool 消息 + assistant tool_calls）。
/// H3：tool_result content 中的 image/document 块抽取为相邻 user 媒体消息
/// （Chat tool 消息不能带媒体；cc-switch plan/queue/flush_pending 同款）。
fn convert_blocks_to_chat(
    role: &str,
    blocks: &[Value],
    messages: &mut Vec<Value>,
    vendor_requires_reasoning: bool,
) -> Result<(), String> {
    let mut text_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut reasoning: Vec<String> = Vec::new();
    let mut pending_media: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !t.trim().is_empty() {
                        text_parts.push(json!({"type": "text", "text": t}));
                    }
                }
            }
            // M18/L17：image/document/audio 块统一走共享形状识别器
            //（source 双形态、MCP mimeType+data、media_type 缺省等）
            "image" | "document" | "audio" => {
                if let Some(part) = media_block_to_chat_part(block) {
                    text_parts.push(part);
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() || name.is_empty() {
                    return Err("tool_use block missing id or name".into());
                }
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = canonical_json_string(&input);
                tool_calls.push(json!({
                    "id": id, "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }));
            }
            "tool_result" => {
                // tool_result → 独立 tool 消息（先于本条消息的文本部分压入，保持 tool 序）。
                // content 走共享递归抽取（M18）：JSON 字符串内嵌媒体、未知块 canonical
                // 保留；媒体带 per-call 来源标注（L15）
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                let (content, media) =
                    tool_result_content_with_media(block.get("content"), &tool_use_id);
                pending_media.extend(media);
                messages.push(json!({
                    "role": "tool", "tool_call_id": tool_use_id, "content": content
                }));
            }
            "thinking" => {
                if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                    if !t.is_empty() {
                        reasoning.push(t.to_string());
                    }
                }
            }
            "redacted_thinking" => {
                reasoning.push("[redacted thinking]".into());
            }
            _ => {
                // M18：未知块（search_result / server_tool_use 等）不再静默丢弃——
                // canonical JSON 保留进文本（模型能看到结构化数据），cc-switch 同款
                text_parts.push(json!({
                    "type": "text",
                    "text": canonical_json_string(block)
                }));
            }
        }
    }

    // assistant：文本 + tool_calls（+ reasoning_content vendor quirk）
    if role == "assistant" {
        let has_tools = !tool_calls.is_empty();
        // M14：纯 thinking 回合（无文本无工具调用）整条丢弃——content:null 且无
        // tool_calls 的 assistant 消息会被部分上游拒收，思考也无法在 Chat 协议
        // 中单独成消息（cc-switch thinking-only 丢弃同款，测试 :1118-1131）
        if text_parts.is_empty() && !has_tools {
            return Ok(());
        }
        let content: Value = if text_parts.is_empty() {
            Value::Null
        } else if text_parts
            .iter()
            .all(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
        {
            // 全文本块 → 纯字符串（Chat 端最通用形态）
            Value::String(
                text_parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
            )
        } else {
            Value::Array(text_parts.clone())
        };
        let mut m = Map::new();
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), content);
        if has_tools {
            m.insert("tool_calls".into(), Value::Array(tool_calls));
            // M14：reasoning_content 仅对要求该字段的厂商注入（cc-switch
            // preserve 语义：非厂商上游发未知字段会被 GLM/Qwen 等严格校验 400）
            if vendor_requires_reasoning {
                let rc = if reasoning.is_empty() {
                    THINKING_PLACEHOLDER.to_string()
                } else {
                    reasoning.join("\n")
                };
                m.insert("reasoning_content".into(), json!(rc));
            }
        }
        messages.push(Value::Object(m));
        return Ok(());
    }

    // user：媒体消息先冲刷（与 tool 消息相邻、先于本回合普通文本），
    // 再压入剩余文本/媒体块（tool_result 已独立成 tool 消息）
    if !pending_media.is_empty() {
        messages.push(
            json!({"role": "user", "content": Value::Array(std::mem::take(&mut pending_media))}),
        );
    }
    if !text_parts.is_empty() {
        let content: Value = if text_parts.len() == 1
            && text_parts[0].get("type").and_then(|t| t.as_str()) == Some("text")
        {
            text_parts[0].get("text").cloned().unwrap_or(Value::Null)
        } else {
            Value::Array(text_parts)
        };
        messages.push(json!({"role": "user", "content": content}));
    }
    Ok(())
}

/// 内容块（image/document/audio 等）→ Chat 媒体 part。
/// 委托共享形状识别器（M18/L17：Anthropic source 双形态、MCP mimeType+data、
/// media_type 缺省、detail 透传等统一在 tool_media::chat_media_part 处理）
fn media_block_to_chat_part(block: &Value) -> Option<Value> {
    crate::service::tool_media::chat_media_part(block).filter(|part| {
        // 本函数只处理媒体 part；纯文本块由调用方处理
        matches!(
            part.get("type").and_then(|t| t.as_str()),
            Some("image_url") | Some("file") | Some("input_audio")
        )
    })
}

/// tool_result content → (残余文本, 带来源标注的媒体 parts)。
/// M18：委托共享递归抽取器——JSON 字符串内嵌媒体、数组嵌套、未知块
/// （search_result 等）以 canonical JSON 保留，不再静默清空；
/// L15：每个 tool call 的媒体前插来源标注 text part（模型可归因）。
fn tool_result_content_with_media(
    content: Option<&Value>,
    tool_use_id: &str,
) -> (String, Vec<Value>) {
    let Some(content) = content else {
        return (String::new(), Vec::new());
    };
    let split = crate::service::tool_media::split_tool_output(content);
    if split.media.is_empty() {
        return (split.text, Vec::new());
    }
    let mut media = Vec::with_capacity(split.media.len() + 1);
    if !tool_use_id.trim().is_empty() {
        media.push(json!({
            "type": "text",
            "text": crate::service::tool_media::media_label(tool_use_id)
        }));
    }
    media.extend(split.media);
    (split.text, media)
}
/// Anthropic tool_choice → Chat tool_choice。
/// 字符串或 `{"type": "auto"|"any"|"none"|"tool"}` 对象形态均可；
/// `{"type": "tool", "name"}` → Chat 嵌套 function 形态。
/// L12：未知 mode 原样透传（此前强制 auto；cc-switch transform.rs:285-306 同款）
fn convert_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    match choice {
        Value::String(s) => map_tool_choice_mode(s),
        Value::Object(obj) => match obj.get("type").and_then(|t| t.as_str()) {
            Some("tool") => {
                let name = obj.get("name").and_then(|n| n.as_str())?;
                Some(json!({"type": "function", "function": {"name": name}}))
            }
            Some("auto" | "none" | "any") => {
                map_tool_choice_mode(obj.get("type").and_then(|t| t.as_str()).unwrap_or(""))
            }
            // L12：未知 type 的对象整体透传（上游裁决）；无 type 亦透传
            _ => Some(choice.clone()),
        },
        _ => Some(choice.clone()),
    }
}

fn map_tool_choice_mode(mode: &str) -> Option<Value> {
    match mode {
        // H3：Chat tool_choice 的对象形态仅允许 {type:function,...}，
        // "required" 必须以字符串形态下发（cc-switch map_tool_choice_to_chat 同款）
        "any" => Some(json!("required")),
        // L12：未知值原样透传（此前强制 auto）
        _ => Some(json!(mode)),
    }
}
/// Anthropic tools → Chat tools（input_schema → parameters，根强制 object）
fn convert_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let arr = tools?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        // L11：Claude Code 批处理工具标记不进 Chat 上游（cc-switch transform.rs:219
        // 同款过滤 type=="BatchTool"；Chat 工具协议无对应物）
        if t.get("type").and_then(|x| x.as_str()) == Some("BatchTool") {
            continue;
        }
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        // L7：Chat 工具名上限 64 字符（Anthropic 允许 128）——超长拍平
        let name = flatten_chat_tool_name(name);
        let mut function = Map::new();
        function.insert("name".into(), json!(name));
        if let Some(desc) = t.get("description").and_then(|d| d.as_str()) {
            function.insert("description".into(), json!(desc));
        }
        // 把客户端 properties 全部丢掉；改为保留内容、仅确保 type=object
        //（cc-switch clean_schema/normalize_function_parameters 语义），并递归
        // 剥离部分严格上游拒收的 format:"uri"
        let mut schema = t.get("input_schema").cloned().unwrap_or_else(|| json!({}));
        if !schema.is_object() {
            schema = json!({"type": "object", "properties": {}});
        } else {
            if let Some(obj) = schema.as_object_mut() {
                if obj.get("type").and_then(|x| x.as_str()) != Some("object") {
                    obj.insert("type".into(), json!("object"));
                }
                if !obj.contains_key("properties") {
                    obj.insert("properties".into(), json!({}));
                }
            }
            strip_uri_formats(&mut schema);
        }
        function.insert("parameters".into(), schema);
        out.push(json!({"type": "function", "function": Value::Object(function)}));
    }
    Some(out)
}

/// L7：Chat 工具名拍平（cc-switch `flatten_namespace_tool_name`，
/// transform_codex_chat.rs:1223-1240 同款；网关无 namespace 概念，
/// 仅处理超长名字）。> 64 字符时截断为前缀 + `__` + 16 位十六进制 SHA-256
///（8 字节摘要，防截断碰撞）。
fn flatten_chat_tool_name(name: &str) -> String {
    const CHAT_TOOL_NAME_MAX_LEN: usize = 64;
    if name.len() <= CHAT_TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(name.as_bytes());
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let suffix = format!("__{hash}");
    let prefix_len = CHAT_TOOL_NAME_MAX_LEN.saturating_sub(suffix.len());
    let mut prefix = String::new();
    for ch in name.chars() {
        if prefix.len() + ch.len_utf8() > prefix_len {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix}{suffix}")
}

/// 递归剥离 `"format": "uri"`（部分上游 JSON Schema 校验器拒收；值保留）
fn strip_uri_formats(v: &mut Value) {
    match v {
        Value::Object(obj) => {
            if obj.get("format").and_then(|f| f.as_str()) == Some("uri") {
                obj.remove("format");
            }
            for (_, child) in obj.iter_mut() {
                strip_uri_formats(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                strip_uri_formats(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn anthropic_req() -> Value {
        json!({
            "model": "deepseek-chat",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "You are helpful."},
                       {"type": "text", "text": "x-anthropic-billing-header: cch=abc\nExtra rules"}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "What's the weather?"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "user wants weather"},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                     "input": {"city": "Paris", "unit": "c"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1",
                     "content": [{"type": "text", "text": "22C sunny"}]},
                    {"type": "text", "text": "Thanks!"}
                ]}
            ],
            "tools": [{"name": "get_weather", "description": "Get weather",
                       "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
            "tool_choice": {"type": "auto"},
            "stop_sequences": ["\n\nHuman:"],
            "temperature": 0.7
        })
    }

    #[test]
    fn converts_full_request() {
        let out = anthropic_request_to_chat(&anthropic_req(), "deepseek-chat", "").unwrap();
        assert_eq!(out["model"], "deepseek-chat");
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["temperature"], 0.7);
        assert_eq!(out["stop"], json!(["\n\nHuman:"]));

        let msgs = out["messages"].as_array().unwrap();
        // system（billing header 剥离后拼接）
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful.\nExtra rules");
        // user 文本 + 图片
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert_eq!(
            msgs[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aGk="
        );
        // assistant tool_calls + reasoning_content（deepseek vendor hint）
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], Value::Null);
        let tc = &msgs[2]["tool_calls"][0];
        assert_eq!(tc["id"], "toolu_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        // canonical args（键已排序）
        assert_eq!(
            tc["function"]["arguments"],
            r#"{"city":"Paris","unit":"c"}"#
        );
        assert_eq!(msgs[2]["reasoning_content"], "user wants weather");
        // tool_result → tool 消息先于 user 文本
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "toolu_1");
        assert_eq!(msgs[3]["content"], "22C sunny");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "Thanks!");
        // tools / tool_choice
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tool_choice"], "auto");
    }

    /// H2：thinking 配置 → reasoning_effort（模型族门控）
    #[test]
    fn thinking_maps_to_reasoning_effort_when_supported() {
        let req = json!({
            "model": "gpt-5.1", "max_tokens": 100,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["reasoning_effort"], "high");

        let req = json!({
            "model": "o4-mini", "max_tokens": 100,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["reasoning_effort"], "low");

        let req = json!({
            "model": "gpt-5.1", "max_tokens": 100,
            "output_config": {"effort": "max"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["reasoning_effort"], "xhigh");

        // adaptive → xhigh
        let req = json!({
            "model": "grok-4.5", "max_tokens": 100,
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["reasoning_effort"], "xhigh");

        // 不支持的模型族不发（防未知参数 400）
        let req = json!({
            "model": "deepseek-chat", "max_tokens": 100,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert!(out.get("reasoning_effort").is_none());
    }

    /// H3：tool_result 内图片抽取为相邻 user 媒体消息
    #[test]
    fn tool_result_media_extracted_to_user_message() {
        let req = json!({
            "model": "m", "max_tokens": 100,
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1",
                     "content": [
                        {"type": "text", "text": "captured"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}}
                     ]},
                    {"type": "text", "text": "Thanks!"}
                ]}
            ]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(
            msgs[0]["content"],
            format!(
                "captured\n{}",
                crate::service::tool_media::MEDIA_MOVED_MARKER
            )
        );
        assert_eq!(msgs[1]["role"], "user", "媒体在 tool 消息之后");
        // L15：媒体前插 per-call 来源标注
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert_eq!(
            msgs[1]["content"][1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}})
        );
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"], "Thanks!");
    }

    /// L10：metadata.user_id 读取（Anthropic 规范字段）
    #[test]
    fn metadata_user_id_mapped() {
        let req = json!({
            "model": "m", "max_tokens": 10,
            "metadata": {"user_id": "u42"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["user"], "u42");
    }

    /// L5：根 schema 保留 properties；format:uri 递归剥离
    #[test]
    fn input_schema_normalized_without_losing_properties() {
        let req = json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "f", "input_schema": {
                "properties": {"u": {"type": "string", "format": "uri"}, "n": {"type": "integer"}},
                "required": ["u"]
            }}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        let params = &out["tools"][0]["function"]["parameters"];
        assert_eq!(params["type"], "object", "根 type 强制 object");
        assert_eq!(
            params["properties"]["n"]["type"], "integer",
            "properties 保留"
        );
        assert!(
            params["properties"]["u"].get("format").is_none(),
            "format:uri 剥离"
        );
        assert_eq!(params["required"], json!(["u"]));
    }

    #[test]
    fn vendor_hint_injects_placeholder_for_tool_calls_without_thinking() {
        let req = json!({
            "model": "deepseek-chat", "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]}
            ]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["messages"][1]["reasoning_content"], "tool call");
    }

    #[test]
    fn non_vendor_model_omits_placeholder() {
        let req = json!({
            "model": "qwen-max", "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]}
            ]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert!(out["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn system_string_and_billing_strip() {
        let req = json!({
            "model": "m", "max_tokens": 10,
            "system": "x-anthropic-billing-header: cch=xyz\nBe terse.",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["messages"][0]["content"], "Be terse.");
    }

    #[test]
    fn tool_choice_mapping() {
        assert_eq!(
            convert_tool_choice(Some(&json!("any"))),
            Some(json!("required"))
        );
        assert_eq!(
            convert_tool_choice(Some(&json!({"type": "tool", "name": "f"}))),
            Some(json!({"type": "function", "function": {"name": "f"}}))
        );
    }

    #[test]
    fn string_content_shortcut() {
        let req = json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["messages"][0]["content"], "hello");
    }

    #[test]
    fn missing_model_rejected() {
        let err = anthropic_request_to_chat(&json!({"messages": []}), "", "").unwrap_err();
        assert!(err.contains("model"));
    }

    #[test]
    fn empty_tools_omits_tool_choice() {
        let req = json!({
            "model": "m", "max_tokens": 10,
            "tool_choice": {"type": "tool", "name": "f"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
    }

    /// L12：未知 tool_choice 原样透传（此前强制 auto）
    #[test]
    fn tool_choice_unknown_passthrough() {
        assert_eq!(
            convert_tool_choice(Some(&json!("future_mode"))),
            Some(json!("future_mode"))
        );
        let obj = json!({"type": "future_mode", "extra": 1});
        assert_eq!(convert_tool_choice(Some(&obj)), Some(obj));
        // 无 type 的对象原样透传
        let no_type = json!({"name": "x"});
        assert_eq!(convert_tool_choice(Some(&no_type)), Some(no_type));
        // 已知形态不变
        assert_eq!(
            convert_tool_choice(Some(&json!("auto"))),
            Some(json!("auto"))
        );
        assert_eq!(
            convert_tool_choice(Some(&json!("any"))),
            Some(json!("required"))
        );
    }

    /// 四-1：disable_parallel_tool_use → parallel_tool_calls:false
    #[test]
    fn disable_parallel_tool_use_maps_to_parallel_tool_calls_false() {
        let req = json!({
            "model": "deepseek-chat", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true}
        });
        let out = anthropic_request_to_chat(&req, req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["parallel_tool_calls"], false);
        // 未声明时不注入
        let req2 = json!({
            "model": "deepseek-chat", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"}
        });
        assert!(
            anthropic_request_to_chat(&req2, req2["model"].as_str().unwrap_or(""), "")
                .unwrap()
                .get("parallel_tool_calls")
                .is_none()
        );
    }

    /// L7：超长工具名拍平为 64 字符内前缀 + 16 位十六进制 SHA-256 后缀
    #[test]
    fn long_tool_name_flattened_with_hash() {
        let name = "x".repeat(100);
        let flat = flatten_chat_tool_name(&name);
        assert_eq!(flat.len(), 64);
        // 后缀 = "__" + 16 位哈希（结尾不是 "__"，此前断言误）
        let (prefix, hash) = flat.split_once("__").unwrap();
        assert_eq!(prefix.len(), 46);
        assert_eq!(hash.len(), 16, "8 字节 SHA-256 十六进制");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // 短名原样
        assert_eq!(flatten_chat_tool_name("get_weather"), "get_weather");
        // 确定性
        assert_eq!(flatten_chat_tool_name(&name), flat);
    }

    #[test]
    fn o_series_uses_max_completion_tokens() {
        let o_req = json!({
            "model": "o3", "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out =
            anthropic_request_to_chat(&o_req, o_req["model"].as_str().unwrap_or(""), "").unwrap();
        assert_eq!(out["max_completion_tokens"], 4096);
        assert!(out.get("max_tokens").is_none());

        let plain_req = json!({
            "model": "deepseek-chat", "max_tokens": 2048,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out =
            anthropic_request_to_chat(&plain_req, plain_req["model"].as_str().unwrap_or(""), "")
                .unwrap();
        assert_eq!(out["max_tokens"], 2048);
        assert!(out.get("max_completion_tokens").is_none());
    }

    /// H1：门控用映射后上游模型 —— claude-* 客户端 + gpt-5 上游：thinking 意图保留；
    /// 反向映射到 deepseek：不发 reasoning_effort，且 max_tokens 按键正确
    #[test]
    fn gating_uses_mapped_upstream_model() {
        let req = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4096,
            "thinking": {"type": "enabled", "budget_tokens": 16_000},
            "messages": [{"role": "user", "content": "hi"}]
        });
        // 映射到 gpt-5 系：reasoning_effort 按预算分档 high；max_tokens 键
        let out = anthropic_request_to_chat(&req, "gpt-5.1", "").unwrap();
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["max_tokens"], 4096);
        assert!(out.get("max_completion_tokens").is_none());
        // 映射到 deepseek：不发 reasoning_effort（不支持该参数）
        let out = anthropic_request_to_chat(&req, "deepseek-chat", "").unwrap();
        assert!(out.get("reasoning_effort").is_none());
        // 映射到 o 系：max_completion_tokens 键
        let out = anthropic_request_to_chat(&req, "o4-mini", "").unwrap();
        assert_eq!(out["max_completion_tokens"], 4096);
        assert!(out.get("max_tokens").is_none());
    }

    /// L11：BatchTool 类型工具过滤（cc-switch transform.rs:219 同款）
    #[test]
    fn batch_tool_filtered() {
        let req = json!({
            "model": "deepseek-chat",
            "tools": [
                {"name": "BatchTool", "type": "BatchTool", "input_schema": {"type": "object"}},
                {"name": "keep", "input_schema": {"type": "object"}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_request_to_chat(&req, "deepseek-chat", "").unwrap();
        let names: Vec<&str> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["keep"]);
    }

    /// M5：tool_result 媒体搬走 → 搬移标记；残余巨型 base64 钳制为省略标记
    #[test]
    fn tool_result_media_clamped_with_marker() {
        let huge_b64 = "Abc+/123=".repeat(2_000);
        let (content, media) = tool_result_content_with_media(
            Some(&json!([
                {"type": "text", "text": huge_b64},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
            ])),
            "toolu_1",
        );
        // L15：媒体带 per-call 来源标注
        assert_eq!(media.len(), 2, "标注 + 图片");
        assert_eq!(media[0]["type"], "text");
        assert!(media[0]["text"].as_str().unwrap().contains("toolu_1"));
        assert!(
            content.contains(crate::service::tool_media::MEDIA_MOVED_MARKER),
            "搬移标记存在: {content}"
        );
        assert!(content.contains("omitted"), "残余 base64 被钳制: {content}");
        assert!(!content.contains("Abc+/"), "原始 base64 不残留");
        // 无媒体时文本原样
        let (content, media) = tool_result_content_with_media(
            Some(&json!([
                {"type": "text", "text": "plain"}
            ])),
            "toolu_1",
        );
        assert!(media.is_empty());
        assert_eq!(content, "plain");
    }
}
