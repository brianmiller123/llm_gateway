//! Anthropic Messages API ↔ Chat Completions 转换（cc-switch `transform.rs`/`streaming.rs` 移植）。
//!
//! `/v1/messages` 客户端（Claude Code / Claude SDK）请求经本模块转换为 Chat
//! Completions 请求转发给上游（transform），上游响应再转换回 Anthropic 形态
//! （convert_resp 非流式 / stream 流式 SSE 状态机）。
//!
//! 决策门：provider `api_type = "anthropic"` 时原生透传（不转换）；其余一律转换。
//!
//! 关键语义（对应 cc-switch 分析文档 §2）：
//! - system（string/数组）→ system 消息；billing header 剥离（Claude Code 前缀缓存）
//! - tool_use → tool_calls（arguments canonical JSON）；tool_result → role:"tool" 消息
//! - thinking → reasoning_content（DeepSeek/MiMo 厂商要求 tool-call 消息带非空思考，
//!   空时注入占位符 "tool call"）
//! - usage 三桶恒等式：input + cache_read + cache_creation == prompt_tokens
//! - finish_reason ↔ stop_reason 映射；多 finish_reason chunk 去重
//! - 错误整形为 Anthropic 单错误对象形状（`{"type":"error","error":{...}}`）

pub mod convert_resp;
pub mod stream;
pub mod transform;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::store::upstream::Provider;

/// 上游是否原生 Anthropic /v1/messages（透传，不转换）
pub fn provider_native_anthropic(p: &Provider) -> bool {
    p.api_type.eq_ignore_ascii_case("anthropic")
}

/// 降级转换决策：/v1/messages 端点 + 非原生上游 → 转换
pub fn should_convert(endpoint_anthropic: bool, provider: &Provider) -> bool {
    endpoint_anthropic && !provider_native_anthropic(provider)
}

/// 请求转换入口：Anthropic Messages 请求 JSON → Chat Completions 请求 JSON。
/// `gating_model`：模型族门控用模型名（H1 = 路由映射后的上游模型）
/// `base_url`：渠道 base URL（M19：厂商 quirk 判定兜底）
pub fn convert_request(req: &Value, gating_model: &str, base_url: &str) -> Result<Value, String> {
    transform::anthropic_request_to_chat(req, gating_model, base_url)
}

/// 非流式响应转换入口：Chat 响应 JSON → Anthropic Messages 响应 JSON
pub fn convert_response(resp: &Value, id: &str) -> Result<Value, String> {
    convert_resp::chat_response_to_anthropic(resp, id)
}

// ---------------------------------------------------------------------------
// 错误整形（cc-switch error.rs：对 Anthropic 客户端输出单错误对象形状）
// ---------------------------------------------------------------------------

/// HTTP 状态 → Anthropic error type
pub fn error_type_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500..=599 => "api_error",
        _ => "api_error",
    }
}

/// Anthropic 错误体：`{"type":"error","error":{"type","message"}}`
pub fn anthropic_error_body(err_type: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "error": {"type": err_type, "message": message}
    })
}

/// 网关内部错误（AppError）→ Anthropic 错误响应（/v1/messages handler 用）
pub fn error_response(err: &AppError) -> Response {
    // P1-6：解包请求关联 id（状态/类型映射委托内层；响应附 X-Request-Id）
    let (request_id, err) = match err {
        AppError::WithRequestId { inner, request_id } => (Some(*request_id), &**inner),
        other => (None, other),
    };
    let (status, err_type, message) = match err {
        // 双层包装（理论上不出现）按内层变体映射
        AppError::WithRequestId { inner, .. } => {
            return error_response(inner);
        }
        AppError::Auth(m) | AppError::Unauthorized(m) => {
            (StatusCode::UNAUTHORIZED, "authentication_error", m.clone())
        }
        AppError::Forbidden(m) => (StatusCode::FORBIDDEN, "permission_error", m.clone()),
        AppError::RateLimited(secs) => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            format!(
                "Rate limit exceeded, retry after {}s",
                secs.ceil().max(1.0) as u64
            ),
        ),
        AppError::QuotaExceeded | AppError::PlanQuotaExceeded(_) => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "quota exceeded, please contact administrator".into(),
        ),
        AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "invalid_request_error", m.clone()),
        AppError::Conflict(m) => (StatusCode::CONFLICT, "invalid_request_error", m.clone()),
        AppError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, "api_error", m.clone()),
        AppError::ServiceUnavailable(m) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            m.clone(),
        ),
        AppError::UpstreamTimeout(m) => (StatusCode::GATEWAY_TIMEOUT, "api_error", m.clone()),
        AppError::BadGateway(m) => (StatusCode::BAD_GATEWAY, "api_error", m.clone()),
        AppError::UpstreamExhausted(m) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            m.clone(),
        ),
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if matches!(err, AppError::RateLimited(_)) {
        let retry = match err {
            AppError::RateLimited(secs) => secs.ceil().max(1.0).min(86400.0) as u64,
            _ => 1,
        };
        builder = builder.header("Retry-After", retry.to_string());
    }
    if let Some(id) = request_id {
        builder = builder.header("X-Request-Id", id.to_string());
    }
    builder
        .body(Body::from(
            anthropic_error_body(err_type, &message).to_string(),
        ))
        .expect("static response is valid")
}

/// 上游非 2xx 错误体 → Anthropic 错误体（状态码透传，message 尽力提取）。
/// M9：` [provider=…]` 后缀拼进 message 便于排障关联；message 截断 1800 字符
/// （上游 nginx HTML 错误页不再整页下发）。
/// M3：结构化排障字段（cc-switch codex_proxy_error_json 同款）——error 对象内
/// 附加 upstream_status / body_type / provider / model / request_id
pub fn reshape_upstream_error(
    status: u16,
    body: &[u8],
    ctx: &crate::service::proxy::UpstreamErrorContext,
) -> Vec<u8> {
    const MAX_MESSAGE_CHARS: usize = 1800;
    let err_type = error_type_for_status(status);
    let message = extract_upstream_error_message(body)
        .unwrap_or_else(|| format!("upstream error (status {status})"));
    let message = crate::service::responses::compact_error_message(
        &format!("{message}{}", ctx.message_suffix()),
        MAX_MESSAGE_CHARS,
    );
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": err_type,
            "message": message,
            "upstream_status": status,
            "body_type": crate::service::proxy::classify_upstream_body(body),
            "provider": ctx.provider,
            "model": ctx.model,
            "request_id": ctx.request_id.to_string(),
        }
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// 从上游 JSON 错误体提取 message：error.message > message > error(str) > detail
fn extract_upstream_error_message(body: &[u8]) -> Option<String> {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    if let Some(v) = parsed {
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .or_else(|| v.get("message").and_then(|m| m.as_str()))
            .or_else(|| v.get("error").and_then(|e| e.as_str()))
            .or_else(|| v.get("detail").and_then(|d| d.as_str()))
            .map(str::to_string)
            .or_else(|| {
                v.pointer("/base_resp/status_msg")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            });
        if let Some(m) = msg {
            return Some(m);
        }
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(1000).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tctx() -> crate::service::proxy::UpstreamErrorContext<'static> {
        crate::service::proxy::UpstreamErrorContext {
            provider: "p",
            model: "m",
            request_id: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn error_type_mapping() {
        assert_eq!(error_type_for_status(400), "invalid_request_error");
        assert_eq!(error_type_for_status(401), "authentication_error");
        assert_eq!(error_type_for_status(429), "rate_limit_error");
        assert_eq!(error_type_for_status(502), "api_error");
    }

    #[test]
    fn reshapes_openai_error_body() {
        let upstream =
            br#"{"error":{"message":"quota exceeded","type":"insufficient_quota","code":429}}"#;
        let out = reshape_upstream_error(429, upstream, &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("quota exceeded")
        );
        // M3：结构化排障字段
        assert_eq!(v["error"]["provider"], "p");
        assert_eq!(v["error"]["model"], "m");
        assert_eq!(v["error"]["upstream_status"], 429);
    }

    #[test]
    fn reshapes_non_json_body() {
        let out = reshape_upstream_error(500, b"gateway timeout", &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["error"]["type"], "api_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("gateway timeout")
        );
        assert_eq!(v["error"]["body_type"], "text");
    }

    #[test]
    fn reshapes_minimax_base_resp() {
        let upstream =
            br#"{"data":{},"base_resp":{"status_code":1004,"status_msg":"invalid api key"}}"#;
        let out = reshape_upstream_error(401, upstream, &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid api key")
        );
    }

    /// M9：context 拼进 message；超长 message 截断
    #[test]
    fn reshape_error_context_and_truncation() {
        // 短 message：context 完整保留
        let out = reshape_upstream_error(502, br#"{"message":"boom"}"#, &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("provider=p"), "{msg}");
        assert!(msg.contains("request_id=00000000"), "{msg}");
        // 超长 message：截断
        let big = "y".repeat(5000);
        let out =
            reshape_upstream_error(502, format!(r#"{{"message":"{big}"}}"#).as_bytes(), &tctx());
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.chars().count() <= 1800, "len={}", msg.len());
    }
}

/// 上游流内错误帧 → message 文本（供 stream 转换发 error 事件）
pub(crate) fn extract_stream_error_message(err: &Value) -> String {
    match err {
        Value::String(s) => s.clone(),
        Value::Object(_) => err
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .or_else(|| err.get("message").and_then(|m| m.as_str()))
            .or_else(|| err.get("detail").and_then(|d| d.as_str()))
            .unwrap_or("upstream stream error")
            .to_string(),
        _ => "upstream stream error".into(),
    }
}
