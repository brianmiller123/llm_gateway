//! P0-8 / P0-9：上游 4xx 反应式请求整流（同上游单次重试）。
//!
//! cc-switch `media_sanitizer.rs`（反应式路径 :55-118）与 `thinking_rectifier.rs`
//! 同款语义：上游以 400/413/415/422 拒绝携带图片 / thinking 块的请求时，把出站体
//! 中的对应内容替换或剥离后重发一次同上游，对话不中断：
//! - 媒体降级：错误文案表明"仅文本 / 不支持图片" → 图片 part 替换为
//!   `[Unsupported Image]` 文本标记（覆盖 Chat `image_url`、Anthropic `image`
//!   块、Responses `input_image` item 三种出站方言）
//! - thinking 整流：原生 Anthropic 上游报签名/thinking 块错误 → 剥离
//!   thinking / redacted_thinking 块、非 thinking 块上的 signature 字段；
//!   thinking 启用且历史 assistant 不以 thinking 开头时连顶层 thinking 一起移除
//!
//! 网关无渠道级模态注册表（REF 的 proactive 路径依赖 per-provider modalities
//! 配置），故只实现反应式路径——错误自证，不误伤支持多模态的渠道。

use serde_json::Value;

/// 图片降级标记（cc-switch media_sanitizer 同款文案）
const UNSUPPORTED_IMAGE_MARKER: &str = "[Unsupported Image]";

/// 反应式整流可命中的上游状态码（cc-switch is_unsupported_image_error 同款集合）
const RECTIFIABLE_STATUSES: [u16; 4] = [400, 413, 415, 422];

/// 是否为可整流的状态码
pub fn is_rectifiable_status(status: u16) -> bool {
    RECTIFIABLE_STATUSES.contains(&status)
}

/// 出站体（Chat / Anthropic / Responses 方言）+ 上游错误体 → 整流后的新出站体。
/// 无可整流内容（错误不匹配 / 出站体无对应载荷）返回 None（调用方按常规错误返回）。
pub fn rectify_outbound(
    anthropic_native: bool,
    outbound: &[u8],
    error_body: &[u8],
    status: u16,
) -> Option<Vec<u8>> {
    if !is_rectifiable_status(status) {
        return None;
    }
    let message = extract_error_text(error_body);
    let mut body: Value = match serde_json::from_slice(outbound) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let mut changed = false;
    if is_unsupported_image_error(&message) {
        changed |= replace_image_parts(&mut body) > 0;
    }
    if is_thinking_error(&message) && anthropic_native {
        changed |= strip_thinking_blocks(&mut body) > 0;
        if remove_top_level_thinking_if_broken(&body) {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("thinking");
                changed = true;
            }
        }
    }
    // P1-4：thinking budget 约束错误（cc-switch thinking_budget_rectifier.rs 同款）：
    // 上游要求 budget_tokens >= 阈值 / max_tokens > budget → 抬到合规值后同渠道重试
    if anthropic_native && is_thinking_budget_error(&message) {
        changed |= raise_thinking_budget(&mut body);
    }
    if changed {
        serde_json::to_vec(&body).ok()
    } else {
        None
    }
}

/// P1-4：错误文案是否为 thinking budget 约束类
///（cc-switch thinking_budget_rectifier.rs:24-48 同款形状集：
/// "budget_tokens is less than 14000" / "max_tokens must be greater than
/// thinking.budget_tokens" / "thinking.budget_tokens must be >= 1024" 等）
fn is_thinking_budget_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    (m.contains("budget_tokens") || m.contains("thinking budget") || m.contains("budget tokens"))
        && (m.contains("must be")
            || m.contains("greater than")
            || m.contains("less than")
            || m.contains("minimum")
            || m.contains("invalid")
            || m.contains("exceeds"))
}

/// P1-4：把 Anthropic 出站体 thinking budget 抬到合规值
///（cc-switch thinking_budget_rectifier.rs:50-117 同款）：
/// - thinking.type=enabled 且 budget_tokens < 32000 → 32000
/// - max_tokens < 64000 → 64000（防 max_tokens 与 budget 冲突）
/// 返回是否发生修改。
fn raise_thinking_budget(body: &mut Value) -> bool {
    let mut changed = false;
    let thinking_enabled = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        == Some("enabled");
    if !thinking_enabled {
        return changed;
    }
    match body.pointer("/thinking/budget_tokens").and_then(|b| b.as_u64()) {
        Some(b) if b < 32_000 => {
            body["thinking"]["budget_tokens"] = serde_json::json!(32_000);
            changed = true;
        }
        Some(_) => {}
        None => {
            body["thinking"]["budget_tokens"] = serde_json::json!(32_000);
            changed = true;
        }
    }
    match body.get("max_tokens").and_then(|m| m.as_u64()) {
        Some(m) if m < 64_000 => {
            body["max_tokens"] = serde_json::json!(64_000);
            changed = true;
        }
        Some(_) => {}
        None => {
            body["max_tokens"] = serde_json::json!(64_000);
            changed = true;
        }
    }
    changed
}

/// H6：主动式媒体降级——发前把出站体（Chat `image_url` / Anthropic `image` /
/// Responses `input_image` 三方言）中的图片 part 替换为文本标记
///（cc-switch forwarder.rs apply_media_prevention + media_sanitizer 同款动机：
/// 纯文本上游不再每轮对话首请求必失败一次）。无图片载荷时返回 None（原样发送）。
pub fn strip_media_outbound(outbound: &[u8]) -> Option<Vec<u8>> {
    let mut body: Value = serde_json::from_slice(outbound).ok()?;
    let replaced = replace_image_parts(&mut body);
    if replaced == 0 {
        return None;
    }
    serde_json::to_vec(&body).ok()
}

/// 从错误体提取文本（JSON 的 message/error.message/detail 或原文）
fn extract_error_text(body: &[u8]) -> String {
    let raw = String::from_utf8_lossy(body);
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => {
            let msg = v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .or_else(|| v.get("message").and_then(|m| m.as_str()))
                .or_else(|| v.get("detail").and_then(|m| m.as_str()))
                .unwrap_or(raw.as_ref());
            msg.to_string()
        }
        Err(_) => raw.into_owned(),
    }
}

/// P0-8：错误文案表明"仅文本 / 不支持图片"
///（cc-switch media_sanitizer.rs:55-118 同款模式，含火山方舟 "Model only
/// support text input" 这类不出现 image 字样的自证性表述）
fn is_unsupported_image_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    // 自证性表述：短语本身断言"仅接受文本"——无需再要求提到 image
    const TEXT_ONLY_SELF_EVIDENT: &[&str] = &["only support text", "only supports text"];
    if TEXT_ONLY_SELF_EVIDENT.iter().any(|h| message.contains(h)) {
        return true;
    }
    let mentions_image = message.contains("image")
        || message.contains("vision")
        || message.contains("multimodal")
        || message.contains("multi-modal")
        || message.contains("modality")
        || message.contains("modalities")
        || message.contains("media")
        || message.contains("attachment");
    if !mentions_image {
        return false;
    }
    const UNSUPPORTED_HINTS: &[&str] = &[
        "unsupported",
        "not supported",
        "does not support",
        "doesn't support",
        "do not support",
        "don't support",
        "text only",
        "text-only",
        "invalid content type",
        "invalid message content",
        "unknown variant",
        "unknown content type",
        "unrecognized content type",
        "cannot process",
        "cannot handle",
        "can't process",
        "can't handle",
        "unable to process",
    ];
    UNSUPPORTED_HINTS.iter().any(|h| message.contains(h))
}

/// P0-9：thinking 签名/块相关错误
///（cc-switch thinking_rectifier.rs:26-109 同款模式集）
fn is_thinking_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    if lower.contains("invalid")
        && lower.contains("signature")
        && lower.contains("thinking")
        && lower.contains("block")
    {
        return true;
    }
    if lower.contains("thought signature")
        && (lower.contains("not valid") || lower.contains("invalid"))
    {
        return true;
    }
    if lower.contains("must start with a thinking block") {
        return true;
    }
    if lower.contains("expected")
        && (lower.contains("thinking") || lower.contains("redacted_thinking"))
        && lower.contains("found")
        && lower.contains("tool_use")
    {
        return true;
    }
    if lower.contains("signature") && lower.contains("field required") {
        return true;
    }
    if lower.contains("signature") && lower.contains("extra inputs are not permitted") {
        return true;
    }
    if (lower.contains("thinking") || lower.contains("redacted_thinking"))
        && lower.contains("cannot be modified")
    {
        return true;
    }
    // 与 CCH 对齐的兜底（第三方渠道通用 invalid request 文案）
    lower.contains("非法请求") || lower.contains("illegal request") || lower.contains("invalid request")
}

/// 图片 part → 文本标记（Chat `image_url` / Anthropic `image` / Responses
/// `input_image` 三种出站方言统一处理）。返回替换数。
fn replace_image_parts(body: &mut Value) -> usize {
    let mut count = 0;
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                let mut rewritten = Vec::with_capacity(content.len());
                let mut msg_changed = false;
                for part in content.iter_mut() {
                    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if part_type == "image_url" || part_type == "image" {
                        msg_changed = true;
                        count += 1;
                        rewritten.push(serde_json::json!({
                            "type": "text",
                            "text": UNSUPPORTED_IMAGE_MARKER
                        }));
                    } else {
                        rewritten.push(part.clone());
                    }
                }
                if msg_changed {
                    *content = rewritten;
                }
            }
        }
    }
    // Responses 方言（原生 openai-responses 透传出站）：input[] 里的裸 input_image item
    if let Some(input) = body.get_mut("input").and_then(|i| i.as_array_mut()) {
        let mut rewritten = Vec::with_capacity(input.len());
        let mut changed = false;
        for item in input.iter_mut() {
            if item.get("type").and_then(|t| t.as_str()) == Some("input_image") {
                changed = true;
                count += 1;
                rewritten.push(serde_json::json!({
                    "type": "input_text",
                    "text": UNSUPPORTED_IMAGE_MARKER
                }));
            } else {
                rewritten.push(item.clone());
            }
        }
        if changed {
            *input = rewritten;
        }
    }
    count
}

/// 剥离 thinking / redacted_thinking 块与非 thinking 块上的 signature 字段
///（cc-switch rectify_anthropic_request 同款）。返回剥离数。
fn strip_thinking_blocks(body: &mut Value) -> usize {
    let mut removed = 0usize;
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };
    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let mut rewritten = Vec::with_capacity(content.len());
        let mut changed = false;
        for block in content.iter() {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("thinking") | Some("redacted_thinking") => {
                    removed += 1;
                    changed = true;
                    continue;
                }
                _ => {}
            }
            if block.get("signature").is_some() {
                let mut b = block.clone();
                if let Some(obj) = b.as_object_mut() {
                    obj.remove("signature");
                    removed += 1;
                    changed = true;
                    rewritten.push(b);
                    continue;
                }
            }
            rewritten.push(block.clone());
        }
        if changed {
            *content = rewritten;
        }
    }
    removed
}

/// 顶层 thinking 是否应移除：thinking.type=enabled 且存在 assistant 消息但
/// 最后一条不以 thinking 块开头（剥离后必然如此）
fn remove_top_level_thinking_if_broken(body: &Value) -> bool {
    let thinking_enabled = body
        .pointer("/thinking/type")
        .and_then(|t| t.as_str())
        == Some("enabled");
    if !thinking_enabled {
        return false;
    }
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let Some(last_assistant) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
    else {
        return false;
    };
    // 无 content 数组（纯文本 assistant）也视为不以 thinking 开头
    !last_assistant
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| blocks.first())
        .is_some_and(|b| {
            matches!(
                b.get("type").and_then(|t| t.as_str()),
                Some("thinking") | Some("redacted_thinking")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err(msg: &str) -> Vec<u8> {
        json!({"error": {"message": msg}}).to_string().into_bytes()
    }

    #[test]
    fn media_downgrade_replaces_image_parts() {
        let outbound = json!({
            "model": "text-only",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,xxx"}}
                ]}
            ]
        })
        .to_string()
        .into_bytes();
        let fixed = rectify_outbound(false, &outbound, &err("Image input is not supported for this model"), 400)
            .expect("rectifiable");
        let v: Value = serde_json::from_slice(&fixed).unwrap();
        assert_eq!(v["messages"][0]["content"][1]["type"], "text");
        assert_eq!(v["messages"][0]["content"][1]["text"], UNSUPPORTED_IMAGE_MARKER);
    }

    #[test]
    fn text_only_self_evident_error_triggers_without_image_word() {
        // 火山方舟风格："Model only support text input"（不出现 image 字样）
        let outbound = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": "https://x/y.png"}}
            ]}]
        })
        .to_string()
        .into_bytes();
        assert!(rectify_outbound(false, &outbound, &err("Model only support text input"), 400).is_some());
    }

    #[test]
    fn unrelated_error_not_rectified() {
        let outbound = json!({"model": "m", "messages": []}).to_string().into_bytes();
        assert!(rectify_outbound(false, &outbound, &err("invalid model name"), 400).is_none());
        // 错误匹配但请求无图片 → 不重试
        let no_image = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
            .to_string()
            .into_bytes();
        assert!(rectify_outbound(false, &no_image, &err("image not supported"), 400).is_none());
    }

    #[test]
    fn thinking_strip_for_anthropic_native_only() {
        let outbound = json!({
            "model": "claude-x",
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]}
            ]
        })
        .to_string()
        .into_bytes();
        let fixed = rectify_outbound(true, &outbound, &err("Invalid 'signature' in 'thinking' block"), 400)
            .expect("rectifiable");
        let v: Value = serde_json::from_slice(&fixed).unwrap();
        let content = v["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        // 剥离后 assistant 不以 thinking 开头且 thinking 仍 enabled → 顶层一并移除
        assert!(v.get("thinking").is_none());
        // 同一出站体走非 anthropic 路径不触发 thinking 整流
        assert!(rectify_outbound(false, &outbound, &err("Invalid 'signature' in 'thinking' block"), 400).is_none());
    }

    #[test]
    fn must_start_with_thinking_error_strips_blocks() {
        let outbound = json!({
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "prev"}
                ]}
            ]
        })
        .to_string()
        .into_bytes();
        let fixed = rectify_outbound(true, &outbound, &err("messages: text content blocks must start with a thinking block"), 400)
            .expect("rectifiable");
        let v: Value = serde_json::from_slice(&fixed).unwrap();
        assert!(v.get("thinking").is_none());
    }

    /// P1-4：thinking budget 约束错误 → 抬到合规值重试（cc-switch 同款）
    #[test]
    fn thinking_budget_rectifier_raises_budget() {
        let outbound = json!({
            "model": "claude-x",
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "q"}]
        })
        .to_string()
        .into_bytes();
        let fixed = rectify_outbound(
            true,
            &outbound,
            &err("thinking.budget_tokens must be at least 14000"),
            400,
        )
        .expect("budget rectifiable");
        let v: Value = serde_json::from_slice(&fixed).unwrap();
        assert_eq!(v["thinking"]["budget_tokens"], 32000);
        assert_eq!(v["max_tokens"], 64000);
        // 非 anthropic 路径不触发
        assert!(rectify_outbound(
            false,
            &outbound,
            &err("thinking.budget_tokens must be at least 14000"),
            400
        )
        .is_none());
        // 已合规 → 无修改
        let ok_outbound = json!({
            "model": "claude-x",
            "thinking": {"type": "enabled", "budget_tokens": 32000},
            "max_tokens": 64000,
            "messages": [{"role": "user", "content": "q"}]
        })
        .to_string()
        .into_bytes();
        assert!(rectify_outbound(
            true,
            &ok_outbound,
            &err("thinking.budget_tokens must be at least 14000"),
            400
        )
        .is_none(), "已合规不重试");
    }

    /// P1-4：budget 错误形状识别
    #[test]
    fn thinking_budget_error_shape_matching() {
        assert!(is_thinking_budget_error("max_tokens must be greater than thinking.budget_tokens"));
        assert!(is_thinking_budget_error("thinking.budget_tokens must be >= 1024"));
        assert!(is_thinking_budget_error("budget_tokens is less than the minimum allowed value"));
        assert!(!is_thinking_budget_error("invalid signature in thinking block"));
        assert!(!is_thinking_budget_error("budget_tokens"));
    }
}
