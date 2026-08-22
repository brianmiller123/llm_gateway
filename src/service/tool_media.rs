//! tool_result 媒体抽取（cc-switch `proxy/tool_media.rs` 同款思想的共享实现）。
//!
//! 职责：
//! 1. **形状识别**：把两条链路（Responses / Anthropic）各自的内容块形态
//!    （Chat part、Anthropic source 块、MCP mimeType+data、loose data-URL 对象）
//!    统一识别为 Chat 媒体 part（image_url / file / input_audio）。
//! 2. **递归抽取**：工具输出常被 MCP 框架 JSON.stringify 成字符串 —— 对可解析
//!    JSON 的字符串递归抽取媒体后 canonical 回写；数组/对象内任意位置同样递归。
//! 3. **尺寸钳制**：媒体搬走后残余文本中的巨型 base64/data-URL（≥16KiB）
//!    钳制为省略标记（防上游 413/上下文爆炸）；小图标（<8KiB data-URL）保留。
//! 4. **来源标注**：搬运媒体带 per-call 标签文本 part，模型可归因多工具媒体。

use serde_json::{json, Map, Value};

/// 整串 data-URL 视为媒体搬运的下限（小于此值保留为文本）
pub const WHOLE_DATA_URL_MIN_BYTES: usize = 8 * 1024;
/// base64 样式残余文本的钳制下限
pub const BASE64ISH_MIN_BYTES: usize = 16 * 1024;
/// 媒体搬运后在 tool 文本中留下的标记（cc-switch TOOL_RESULT_MEDIA_MOVED_MARKER 同款语义）
pub const MEDIA_MOVED_MARKER: &str =
    "[gateway: tool result media moved to the following user message]";
/// 搬运媒体前的来源标注（cc-switch queue_chat_tool_output_media 标签同款语义）
pub fn media_label(call_id: &str) -> String {
    format!("[gateway: media output of tool call {call_id}]")
}
/// 递归深度上限（防畸形自引用体无限递归；cc-switch 同款 32）
const MAX_DEPTH: usize = 32;

/// 文本片段整体是图片 data-URL 且 ≥ 8KiB → 转 Chat image_url part（搬运）；
/// 其余（短 data-URL / 非 base64 / 非图片）保留为文本
pub fn whole_string_image_data_url_to_part(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.len() < WHOLE_DATA_URL_MIN_BYTES {
        return None;
    }
    let comma = trimmed.find(',')?;
    let header = trimmed[..comma].to_ascii_lowercase();
    if !(header.starts_with("data:image/") && header.ends_with(";base64")) {
        return None;
    }
    Some(json!({"type": "image_url", "image_url": {"url": trimmed}}))
}

/// base64 载荷样式判定：≥ 16KiB 且全部为 base64 字母表字符（无空白/标点）
fn looks_like_base64_payload(value: &str) -> bool {
    value.len() >= BASE64ISH_MIN_BYTES
        && value
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'/' | b'='))
}

/// 钳制巨型 data:/base64 文本 → 省略标记（仅在已确认媒体搬运后调用，
/// 普通长文本不受影响；cc-switch clamp_base64ish_strings 同款）
pub fn clamp_base64ish(text: &str) -> String {
    let trimmed = text.trim();
    if looks_like_base64_payload(trimmed)
        || (trimmed.starts_with("data:")
            && trimmed.len() >= BASE64ISH_MIN_BYTES
            && trimmed.contains(";base64,"))
    {
        format!("[gateway: {} bytes of base64 media payload omitted]", trimmed.len())
    } else {
        text.to_string()
    }
}

/// 媒体被搬走后，tool 文本追加搬移标记（原文为空时标记即全文，
/// 保持 tool 消息 content 非空便于上游与客户端排查）
pub fn with_moved_marker(text: &str) -> String {
    if text.trim().is_empty() {
        MEDIA_MOVED_MARKER.to_string()
    } else {
        format!("{text}\n{MEDIA_MOVED_MARKER}")
    }
}

// ---------------------------------------------------------------------------
// 形状识别（cc-switch tool_media_kind / typed_image_url / chat_file_from_input_file）
// ---------------------------------------------------------------------------

/// 识别结构化内容块 / part 并归一为 Chat 媒体 part。覆盖：
/// - Chat part：`input_image` / `image_url` / `input_file` / `input_audio`
/// - Anthropic 块：`image`（source.base64/url 与 MCP mimeType+data 两种形态）、
///   `document`（source.base64）、`audio`（source.base64 → input_audio）
/// - loose：无有效 type 但带 data:URL `url` 字段的对象
/// 非媒体形态返回 None（由调用方按文本/未知块处理）。
pub fn chat_media_part(block: &Value) -> Option<Value> {
    let obj = block.as_object()?;
    let ty = obj.get("type").and_then(|t| t.as_str()).unwrap_or("").trim();
    match ty {
        "input_image" | "image_url" => image_like_part(obj, &["image_url", "url", "file_id"]),
        "image" => anthropic_image_part(obj),
        "input_file" | "file" => file_part(obj),
        "document" => document_part(obj),
        "input_audio" => Some(json!({
            "type": "input_audio",
            "input_audio": part_payload(obj, "input_audio")
        })),
        "audio" => anthropic_audio_part(obj),
        _ => loose_data_url_part(obj),
    }
}

/// input_image / image_url part → `{"type":"image_url","image_url":{"url",…}}`。
/// `image_url` 字符串形态包装为 `{url}`（部分客户端发裸字符串）；仅保留 url/detail。
fn image_like_part(obj: &Map<String, Value>, keys: &[&str]) -> Option<Value> {
    let mut image_url = Map::new();
    if let Some(iu) = obj.get("image_url") {
        match iu {
            Value::String(s) if !s.trim().is_empty() => {
                image_url.insert("url".into(), json!(s));
            }
            Value::Object(m) => {
                for k in ["url", "detail"] {
                    if let Some(v) = m.get(k) {
                        image_url.insert(k.to_string(), v.clone());
                    }
                }
            }
            _ => {}
        }
    }
    if image_url.get("url").is_none() {
        for k in keys {
            if let Some(v) = obj.get(*k) {
                if let Some(s) = v.as_str() {
                    if !s.trim().is_empty() {
                        image_url.insert("url".into(), json!(s));
                        break;
                    }
                }
            }
        }
    }
    if let Some(url) = image_url.get("url").and_then(|u| u.as_str()) {
        if !url.trim().is_empty() {
            return Some(json!({"type": "image_url", "image_url": Value::Object(image_url)}));
        }
    }
    None
}

/// Anthropic `image` 块：source.{type:base64|url} 或 MCP 形态（mimeType/mime_type + data，
/// 无 source）。media_type 缺省 image/png（cc-switch typed_image_url 同款）。
fn anthropic_image_part(obj: &Map<String, Value>) -> Option<Value> {
    let mut detail = Map::new();
    if let Some(d) = obj.get("detail") {
        detail.insert("detail".into(), d.clone());
    }
    if let Some(source) = obj.get("source").and_then(|s| s.as_object()) {
        let mime = source
            .get("media_type")
            .or_else(|| source.get("mime_type"))
            .and_then(|m| m.as_str())
            .unwrap_or("image/png");
        match source.get("type").and_then(|t| t.as_str()) {
            Some("base64") => {
                let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                if data.is_empty() {
                    return None;
                }
                let url = if mime.starts_with("data:") {
                    // data 已是 data:URL 形态则直通（cc-switch 同款）
                    data.to_string()
                } else {
                    format!("data:{mime};base64,{data}")
                };
                detail.insert("url".into(), json!(url));
                return Some(json!({"type": "image_url", "image_url": Value::Object(detail)}));
            }
            Some("url") => {
                let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
                if url.trim().is_empty() {
                    return None;
                }
                detail.insert("url".into(), json!(url));
                return Some(json!({"type": "image_url", "image_url": Value::Object(detail)}));
            }
            _ => return None,
        }
    }
    // MCP 形态：{type:"image", mimeType|mime_type, data}
    let mime = obj
        .get("mimeType")
        .or_else(|| obj.get("mime_type"))
        .and_then(|m| m.as_str())
        .unwrap_or("image/png");
    let data = obj.get("data").and_then(|d| d.as_str()).unwrap_or("");
    if data.is_empty() {
        return None;
    }
    let url = if data.starts_with("data:") {
        data.to_string()
    } else {
        format!("data:{mime};base64,{data}")
    };
    detail.insert("url".into(), json!(url));
    Some(json!({"type": "image_url", "image_url": Value::Object(detail)}))
}

/// input_file / file part → `{"type":"file","file":{file_id|file_data,filename}}`。
/// 仅 file_id / file_data 存在才生成（URL-only / filename-only 形态非法，丢弃；
/// cc-switch chat_file_from_input_file :149-152 同款）。
fn file_part(obj: &Map<String, Value>) -> Option<Value> {
    if let Some(file) = obj.get("file").and_then(|f| f.as_object()) {
        if file.get("file_id").is_some() || file.get("file_data").is_some() {
            return Some(json!({"type": "file", "file": Value::Object(file.clone())}));
        }
        return None;
    }
    let has_id = obj
        .get("file_id")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let has_data = obj
        .get("file_data")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    if !has_id && !has_data {
        return None;
    }
    let mut file = Map::new();
    for k in ["file_id", "file_data", "filename"] {
        if let Some(v) = obj.get(k) {
            file.insert(k.to_string(), v.clone());
        }
    }
    Some(json!({"type": "file", "file": Value::Object(file)}))
}

/// Anthropic `document` 块（source.type=base64）→ file part
fn document_part(obj: &Map<String, Value>) -> Option<Value> {
    let source = obj.get("source")?.as_object()?;
    if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
        return None;
    }
    let media_type = source
        .get("media_type")
        .and_then(|m| m.as_str())
        .unwrap_or("application/pdf");
    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
    if data.is_empty() {
        return None;
    }
    let filename = obj
        .get("filename")
        .and_then(|f| f.as_str())
        .unwrap_or("document");
    Some(json!({
        "type": "file",
        "file": {"filename": filename, "file_data": format!("data:{media_type};base64,{data}")}
    }))
}

/// Anthropic `audio` 块（source.type=base64）→ Chat input_audio part
fn anthropic_audio_part(obj: &Map<String, Value>) -> Option<Value> {
    let source = obj.get("source")?.as_object()?;
    if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
        return None;
    }
    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
    if data.is_empty() {
        return None;
    }
    let format = source
        .get("media_type")
        .or_else(|| source.get("mime_type"))
        .and_then(|m| m.as_str())
        .or_else(|| obj.get("mimeType").and_then(|m| m.as_str()))
        .unwrap_or("wav");
    Some(json!({
        "type": "input_audio",
        "input_audio": {"data": data, "format": format}
    }))
}

/// 无有效 type 但携带 data:URL `url` 字段的 loose 对象（cc-switch loose 形态）
fn loose_data_url_part(obj: &Map<String, Value>) -> Option<Value> {
    let url = obj.get("url").and_then(|u| u.as_str())?;
    let trimmed = url.trim();
    if trimmed.len() >= WHOLE_DATA_URL_MIN_BYTES && trimmed.starts_with("data:") {
        return Some(json!({"type": "image_url", "image_url": {"url": trimmed}}));
    }
    None
}

/// part 载荷：优先命名字段，否则去掉 type 后的全量
fn part_payload(obj: &Map<String, Value>, key: &str) -> Value {
    if let Some(v) = obj.get(key) {
        return v.clone();
    }
    let mut payload = Map::new();
    for (k, v) in obj {
        if k == "type" {
            continue;
        }
        payload.insert(k.clone(), v.clone());
    }
    Value::Object(payload)
}

// ---------------------------------------------------------------------------
// 递归抽取（cc-switch strip_media_from_tool_value / strip_and_clamp 同款）
// ---------------------------------------------------------------------------

/// 工具输出切分结果：`text` 为残余文本（媒体搬走后钳制 + 标记），
/// `media` 为搬运出的 Chat 媒体 part，`moved` 表示是否发生搬运。
#[derive(Debug, Default)]
pub struct ToolMediaSplit {
    pub text: String,
    pub media: Vec<Value>,
}

/// 工具输出（任意 JSON 形态）→ (残余文本, 媒体 parts)。
/// - 字符串：整串 data-URL 搬运；可解析 JSON 递归后再判定；否则原样文本
/// - 数组：逐元素识别（媒体形状 / text part / 其他递归）
/// - 对象：媒体形状搬运；含 content 字段的包装对象递归；否则 canonical JSON 文本
/// 无媒体时文本保持原始表示（字符串原样 / 其余 canonical），与 cc-switch 语义一致。
pub fn split_tool_output(output: &Value) -> ToolMediaSplit {
    let mut media: Vec<Value> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let text = strip_at_depth(output, 0, &mut media, &mut texts);
    if media.is_empty() {
        return ToolMediaSplit { text, media };
    }
    let clamped: Vec<String> = texts.iter().map(|t| clamp_base64ish(t)).collect();
    ToolMediaSplit {
        text: with_moved_marker(&clamped.join("\n")),
        media,
    }
}

/// 递归抽取；返回该值剥离媒体后的文本表示（无媒体时保持原始表示）
fn strip_at_depth(v: &Value, depth: usize, media: &mut Vec<Value>, texts: &mut Vec<String>) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    match v {
        Value::String(s) => {
            if let Some(part) = whole_string_image_data_url_to_part(s) {
                media.push(part);
                return String::new();
            }
            // MCP 框架常把工具结果 JSON.stringify 成字符串：可解析则递归抽取。
            // P1-13：不再按 8KiB 门槛跳过——短 JSON 字符串内的图片/文件同样
            // 抽取（cc-switch tool_media.rs:316-333 无门槛同款；递归深度由
            // MAX_DEPTH 限制防炸）
            if let Ok(parsed) = serde_json::from_str::<Value>(s.trim()) {
                if parsed.is_object() || parsed.is_array() {
                    let before_media = media.len();
                    let inner = strip_at_depth(&parsed, depth + 1, media, texts);
                    if media.len() > before_media {
                        // 抽到了媒体：残余以 canonical JSON 回写（cc-switch 同款）
                        return inner;
                    }
                    // 无媒体：保持原始字符串表示
                    return s.clone();
                }
            }
            texts.push(s.clone());
            s.clone()
        }
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                if let Some(part) = chat_media_part(item) {
                    media.push(part);
                    continue;
                }
                match item {
                    Value::String(_) => {
                        let repr = strip_at_depth(item, depth + 1, media, texts);
                        parts.push(repr);
                    }
                    // text part（Responses/Chat 两命名）→ 纯文本
                    Value::Object(o)
                        if matches!(
                            o.get("type").and_then(|t| t.as_str()),
                            Some("text") | Some("output_text") | Some("input_text")
                        ) =>
                    {
                        let t = o.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !t.trim().is_empty() {
                            texts.push(t.to_string());
                            parts.push(t.to_string());
                        }
                    }
                    other => parts.push(strip_at_depth(other, depth + 1, media, texts)),
                }
            }
            let non_empty: Vec<&String> = parts.iter().filter(|p| !p.trim().is_empty()).collect();
            if non_empty.is_empty() && media.is_empty() {
                return String::new();
            }
            non_empty.into_iter().cloned().collect::<Vec<_>>().join("\n")
        }
        Value::Object(_) => {
            if let Some(part) = chat_media_part(v) {
                media.push(part);
                return String::new();
            }
            // content 包装对象：{content: [...]} / {content: "..."} 递归
            if let Some(inner) = v.get("content") {
                if inner.is_array() || inner.is_string() {
                    let before_media = media.len();
                    let repr = strip_at_depth(inner, depth + 1, media, texts);
                    if media.len() > before_media {
                        return repr;
                    }
                }
            }
            crate::service::canonical::canonical_json_string(v)
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn big_b64() -> String {
        "A".repeat(WHOLE_DATA_URL_MIN_BYTES + 100)
    }

    #[test]
    fn whole_data_url_moved() {
        let s = format!("data:image/png;base64,{}", big_b64());
        let split = split_tool_output(&Value::String(s.clone()));
        assert_eq!(split.media.len(), 1);
        assert!(split.text.contains(MEDIA_MOVED_MARKER));
    }

    #[test]
    fn small_data_url_kept_as_text() {
        let s = "data:image/png;base64,QUJD".to_string();
        let split = split_tool_output(&Value::String(s.clone()));
        assert!(split.media.is_empty());
        assert_eq!(split.text, s);
    }

    #[test]
    fn json_string_with_embedded_media_is_parsed() {
        // MCP 框架把工具结果 JSON.stringify：字符串内嵌 base64 图片应抽出
        let inner = json!({
            "content": [
                {"type": "text", "text": "screenshot attached"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big_b64()}}
            ]
        });
        let s = Value::String(inner.to_string());
        let split = split_tool_output(&s);
        assert_eq!(split.media.len(), 1);
        assert_eq!(split.media[0]["type"], "image_url");
        assert!(split.text.contains("screenshot attached"));
    }

    #[test]
    fn mcp_image_form_recognized() {
        let block = json!({
            "type": "image", "mimeType": "image/jpeg", "data": big_b64()
        });
        let part = chat_media_part(&block).unwrap();
        assert_eq!(part["type"], "image_url");
        assert!(part["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn anthropic_source_url_form() {
        let block = json!({
            "type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}
        });
        let part = chat_media_part(&block).unwrap();
        assert_eq!(part["image_url"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn media_type_defaults_to_png() {
        let block = json!({
            "type": "image", "source": {"type": "base64", "data": "QUJD"}
        });
        let part = chat_media_part(&block).unwrap();
        assert!(part["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    }

    #[test]
    fn file_part_requires_id_or_data() {
        assert!(chat_media_part(&json!({"type": "input_file", "filename": "a.txt"})).is_none());
        assert!(chat_media_part(&json!({"type": "input_file", "file_url": "https://x"})).is_none());
        let part = chat_media_part(&json!({"type": "input_file", "file_id": "f1"})).unwrap();
        assert_eq!(part["file"]["file_id"], "f1");
    }

    #[test]
    fn input_audio_part_passthrough() {
        let part = chat_media_part(&json!({
            "type": "input_audio", "input_audio": {"data": "QUJD", "format": "wav"}
        }))
        .unwrap();
        assert_eq!(part["input_audio"]["format"], "wav");
    }

    #[test]
    fn anthropic_audio_block() {
        let part = chat_media_part(&json!({
            "type": "audio", "source": {"type": "base64", "media_type": "mp3", "data": "QUJD"}
        }))
        .unwrap();
        assert_eq!(part["type"], "input_audio");
        assert_eq!(part["input_audio"]["format"], "mp3");
    }

    #[test]
    fn unknown_blocks_in_array_kept_as_canonical_text() {
        // search_result 等非媒体块不应被静默清空（cc-switch 保留 canonical JSON）
        let arr = json!([
            {"type": "text", "text": "results:"},
            {"type": "search_result", "title": "t", "url": "https://x"}
        ]);
        let split = split_tool_output(&arr);
        assert!(split.media.is_empty());
        assert!(split.text.contains("results:"));
        assert!(split.text.contains("search_result"));
    }

    #[test]
    fn residual_base64_clamped_after_move() {
        let arr = json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big_b64()}},
            {"type": "text", "text": format!("data:image/png;base64,{}", "Q".repeat(BASE64ISH_MIN_BYTES))}
        ]);
        let split = split_tool_output(&arr);
        assert_eq!(split.media.len(), 1);
        assert!(split.text.contains("omitted"));
    }

    #[test]
    fn object_without_media_kept_canonical() {
        let obj = json!({"b": 2, "a": 1});
        let split = split_tool_output(&obj);
        assert!(split.media.is_empty());
        assert_eq!(split.text, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn loose_data_url_object() {
        let block = json!({"url": format!("data:image/png;base64,{}", big_b64())});
        let part = chat_media_part(&block).unwrap();
        assert_eq!(part["type"], "image_url");
    }

    #[test]
    fn image_url_string_form_wrapped() {
        let part = chat_media_part(&json!({
            "type": "input_image", "image_url": "https://example.com/a.png"
        }))
        .unwrap();
        assert_eq!(part["image_url"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn label_format() {
        assert_eq!(media_label("call_1"), "[gateway: media output of tool call call_1]");
    }

    #[test]
    fn clamps_data_url_prefix_text() {
        let text = format!("data:image/png;base64,{}", "Q".repeat(BASE64ISH_MIN_BYTES));
        assert!(clamp_base64ish(&text).starts_with("[gateway:"));
    }
}
