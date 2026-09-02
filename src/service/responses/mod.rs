//! Responses API ↔ Chat Completions 降级转换。
//!
//! 移植自 new-api（third/new-api/relaykit）：`/v1/responses` 客户端请求经
//! 本模块转换为 Chat Completions 请求转发给上游（convert_req），上游响应再转换回
//! Responses 形态（convert_resp 非流式 / stream 流式）。
//!
//! 决策门：provider `api_type = "openai-responses"` 时原生透传（不转换）；
//! 其余（默认 `openai` 等）一律走本模块降级转换。

pub mod convert_req;
pub mod convert_resp;
pub mod dto;
pub mod history;
pub mod stream;
pub mod tool_ctx;

use serde_json::Value;

use crate::store::upstream::Provider;

/// 上游是否原生支持 /v1/responses（透传，不做降级转换）
pub fn provider_native_responses(p: &Provider) -> bool {
    p.api_type.eq_ignore_ascii_case("openai-responses")
}

/// 降级转换决策：/v1/responses 端点 + 非原生上游 → 转换
pub fn should_convert(endpoint_responses: bool, provider: &Provider) -> bool {
    endpoint_responses && !provider_native_responses(provider)
}

/// 请求转换入口：Responses 请求 JSON → Chat 请求 JSON。
/// `gating_model`：模型族门控用模型名（H1 = 路由映射后的上游模型）
pub fn convert_request(req: &Value, gating_model: &str) -> Result<Value, String> {
    convert_req::responses_request_to_chat(req, gating_model)
}

/// 上游 200 + Chat 错误体（或无 choices 的异常体）→ OpenAI 错误形状。
/// Responses 客户端 SDK 只认 `{"error":{message,type,code}}` + 非 2xx 状态码才报错；
/// 透传原始 Chat 错误体 + 200 会被 SDK 当作 Response 对象解析 —— 全字段缺失、
/// output_text 为空串（静默空响应）。type 统一 upstream_error，附原始状态码。
///
/// M9：message 截断 1800 字符（上游 nginx HTML 错误页不再整页下发）、
/// code 恒为字符串枚举（数值回退改为 `upstream_http_<status>`，客户端可稳定分支）。
/// M3：结构化排障字段（cc-switch codex_proxy_error_json 同款）——
/// upstream_status / body_type / provider / model / request_id 独立字段，
/// ` [provider=…]` 后缀保留在 message 内兼容既有日志检索。
pub fn reshape_upstream_error_for_responses(
    status: u16,
    body: &[u8],
    ctx: &crate::service::proxy::UpstreamErrorContext,
) -> Vec<u8> {
    const MAX_MESSAGE_CHARS: usize = 1800;
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            let text = String::from_utf8_lossy(&body[..body.len().min(1024)])
                .trim()
                .to_string();
            let message = if text.is_empty() {
                format!("upstream returned HTTP {status} with non-JSON body")
            } else {
                text
            };
            let resp = serde_json::json!({
                "error": {
                    "message": compact_error_message(
                        &format!("{message}{}", ctx.message_suffix()),
                        MAX_MESSAGE_CHARS
                    ),
                    "type": "upstream_error",
                    "code": format!("upstream_http_{status}"),
                    "upstream_status": status,
                    "body_type": crate::service::proxy::classify_upstream_body(body),
                    "provider": ctx.provider,
                    "model": ctx.model,
                    "request_id": ctx.request_id.to_string(),
                }
            });
            return serde_json::to_vec(&resp).unwrap_or_default();
        }
    };
    // P1-4：MiniMax base_resp 包装层（message/code 嵌在 base_resp 内）优先解析
    //（cc-switch transform_codex_chat.rs:1963-2010 同款）；数值 code 保留数值
    // 形态（此前强制字符串化丢失分支语义）
    let base = v.get("base_resp");
    let message = base
        .and_then(|b| {
            b.get("message")
                .or_else(|| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
        })
        .map(str::to_string)
        .unwrap_or_else(|| {
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .or_else(|| v.get("message").and_then(|m| m.as_str()))
                .or_else(|| v.get("error").and_then(|e| e.as_str()))
                .or_else(|| v.get("detail").and_then(|d| d.as_str()))
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!("upstream returned HTTP {status} with unconvertible body")
                })
        });
    // 字符串 code 优先；数值 code 原样保留（`"code": 4001` 而非转字符串）
    let code: serde_json::Value = base
        .and_then(|b| b.get("code"))
        .or_else(|| v.pointer("/error/code"))
        .filter(|c| c.is_string() || c.is_i64() || c.is_u64())
        .cloned()
        .unwrap_or_else(|| serde_json::Value::String(format!("upstream_http_{status}")));
    // P1-4：`param` 透传（结构化输出违规定位用）
    let param: Option<serde_json::Value> = base
        .and_then(|b| b.get("param"))
        .or_else(|| v.pointer("/error/param"))
        .cloned();
    let mut err = serde_json::Map::new();
    err.insert(
        "message".into(),
        serde_json::Value::String(compact_error_message(
            &format!("{message}{}", ctx.message_suffix()),
            MAX_MESSAGE_CHARS,
        )),
    );
    err.insert(
        "type".into(),
        serde_json::Value::String("upstream_error".into()),
    );
    err.insert("code".into(), code);
    // M3：结构化排障字段
    err.insert("upstream_status".into(), serde_json::json!(status));
    err.insert(
        "body_type".into(),
        serde_json::json!(crate::service::proxy::classify_upstream_body(body)),
    );
    err.insert("provider".into(), serde_json::json!(ctx.provider));
    err.insert("model".into(), serde_json::json!(ctx.model));
    err.insert(
        "request_id".into(),
        serde_json::json!(ctx.request_id.to_string()),
    );
    if let Some(param) = param {
        err.insert("param".into(), param);
    }
    serde_json::to_vec(&serde_json::json!({"error": err})).unwrap_or_default()
}

/// 错误消息压缩：空白归一化 + 截断（cc-switch compact_error_message :2032 同款）
pub(crate) fn compact_error_message(message: &str, max_chars: usize) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    normalized
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string()
}
/// 非流式响应转换入口：Chat 响应 JSON → Responses 响应 JSON。
/// `tool_ctx`：请求工具上下文（P0-4：custom / namespace / tool_search 还原）
pub fn convert_response(
    resp: &Value,
    id: &str,
    fallback_created: i64,
    tool_ctx: &tool_ctx::ToolContext,
) -> Result<Value, String> {
    convert_resp::chat_response_to_responses(resp, id, fallback_created, tool_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tctx() -> crate::service::proxy::UpstreamErrorContext<'static> {
        crate::service::proxy::UpstreamErrorContext {
            provider: "p",
            model: "m",
            request_id: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn reshape_error_json_body() {
        let out = reshape_upstream_error_for_responses(
            200,
            br#"{"error": {"code": "mock_error", "message": "boom requested"}}"#,
            &tctx(),
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap()
                .starts_with("boom requested")
        );
        assert_eq!(
            v.pointer("/error/code").and_then(|c| c.as_str()),
            Some("mock_error")
        );
        assert_eq!(
            v.pointer("/error/type").and_then(|t| t.as_str()),
            Some("upstream_error")
        );
        // M3：结构化排障字段
        assert_eq!(
            v.pointer("/error/provider").and_then(|p| p.as_str()),
            Some("p")
        );
        assert_eq!(
            v.pointer("/error/model").and_then(|m| m.as_str()),
            Some("m")
        );
        assert_eq!(
            v.pointer("/error/upstream_status").and_then(|s| s.as_u64()),
            Some(200)
        );
        assert_eq!(
            v.pointer("/error/body_type").and_then(|b| b.as_str()),
            Some("json")
        );
    }

    #[test]
    fn reshape_error_plain_and_empty_bodies() {
        let out = reshape_upstream_error_for_responses(200, b"not json at all", &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap()
                .starts_with("not json at all")
        );
        // M9：code 恒为字符串枚举（不再回退数值状态码）
        assert_eq!(
            v.pointer("/error/code").and_then(|c| c.as_str()),
            Some("upstream_http_200")
        );
        assert_eq!(
            v.pointer("/error/body_type").and_then(|b| b.as_str()),
            Some("text")
        );

        let out = reshape_upstream_error_for_responses(200, b"", &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap();
        assert!(msg.contains("200"));
        assert_eq!(
            v.pointer("/error/body_type").and_then(|b| b.as_str()),
            Some("empty")
        );

        let out = reshape_upstream_error_for_responses(
            200,
            json!({"choices": []}).to_string().as_bytes(),
            &tctx(),
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap()
                .contains("unconvertible")
        );
    }

    /// M9：context 拼进 message；超长 message 截断
    #[test]
    fn reshape_error_context_and_truncation() {
        // 短 message：context 完整保留
        let out =
            reshape_upstream_error_for_responses(502, br#"{"error":{"message":"boom"}}"#, &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap();
        assert!(msg.contains("provider=p"), "{msg}");
        assert!(msg.contains("request_id=00000000"), "{msg}");
        // 超长 message：截断
        let big = "x".repeat(5000);
        let out = reshape_upstream_error_for_responses(
            502,
            format!(r#"{{"error":{{"message":"{big}"}}}}"#).as_bytes(),
            &tctx(),
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap();
        assert!(msg.chars().count() <= 1800, "len={}", msg.len());
    }
}
