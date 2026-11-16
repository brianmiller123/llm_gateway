//! TypeSafe System One 决策模型端点（`/v1/systemone`）。
//!
//! 协议基线：TypeSafe System One（`POST {base_url}/systemone`，单次请求返回
//! 全部问题的概率分布，无 SSE）。网关只做原生透传——`providers.api_type =
//! "systemone"` 的上游逐字转发，其余上游显式 400，**不做**任何降级转换
//! （无 Chat/Responses/Anthropic 映射管线）。因此本模块**不设** `should_convert`
//! 决策门（与 anthropic/responses 不同：没有可转换的目标方言）；配对可行性由
//! `proxy::pairing_unsupported` 双向判定，只暴露 [`provider_native_systemone`]。
//!
//! 客户端方言错误体：`{"detail": "<message>"}`，状态码与 `AppError` 的既有
//! 映射一致（[`crate::error::AppError::classify`]），429 带 `Retry-After`，
//! 全链路 `X-Request-Id`。上游错误体不经二次整形，原样透传。

use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::store::upstream::Provider;

/// 上游是否原生 System One（透传，不转换）
pub fn provider_native_systemone(p: &Provider) -> bool {
    p.api_type.eq_ignore_ascii_case("systemone")
}

/// 客户端是否请求流式：协议层无 SSE，缺失或 `false` 之外的一切取值（`true`、
/// `"true"`、数字、null……）都算请求流式——`as_bool` 会把非 bool 值静默降级
/// 成非流式，等于放行一个协议不支持的请求形态。
pub fn stream_requested(json: &Value) -> bool {
    !matches!(json.get("stream"), None | Some(Value::Bool(false)))
}

/// `stream: true` 拒绝：TypeSafe/Alibaba/Ollama/LiteLLM 四种实现均返回单次
/// JSON，协议层不存在 SSE，网关不做流式包装。
pub fn reject_stream(endpoint_systemone: bool, streamed: bool) -> Result<(), AppError> {
    if endpoint_systemone && streamed {
        return Err(AppError::BadRequest(
            "systemone does not support streaming; remove 'stream' or set it to false".into(),
        ));
    }
    Ok(())
}

/// `{"detail": "<message>"}` 序列化体：网关自身错误体与「上游违约/超时」终局
/// 整形共用同一形状（systemone 方言只有这一种错误封装）。
pub fn detail_body(message: &str) -> Vec<u8> {
    // PERF-06：单次序列化直达字节；先 `to_string()` 再 `into_bytes()` 会在
    // 错误路径多一次整包拷贝
    serde_json::to_vec(&json!({ "detail": message })).unwrap_or_default()
}

/// 网关内部错误（AppError）→ System One 方言错误响应（`/v1/systemone` handler 用）
pub fn error_response(err: &AppError) -> Response {
    // P1-6：解包请求关联 id（状态/消息/退避映射委托 classify；响应附 X-Request-Id）
    let request_id = match err {
        AppError::WithRequestId { request_id, .. } => Some(*request_id),
        _ => None,
    };
    let (status, _code, message, retry_after) = err.classify();
    let body = detail_body(&message);
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(retry) = retry_after {
        builder = builder.header("Retry-After", retry.to_string());
    }
    if let Some(id) = request_id {
        builder = builder.header("X-Request-Id", id.to_string());
    }
    builder
        .body(Body::from(body))
        .expect("static response is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// 流式拒绝：systemone 无 SSE，`stream:true` 直接 400；false/非端点放行
    #[test]
    fn reject_stream_only_when_systemone_streamed() {
        let err = reject_stream(true, true).expect_err("systemone 流式必须拒绝");
        assert!(matches!(err, AppError::BadRequest(_)));
        assert!(err.to_string().contains("does not support streaming"));
        assert!(reject_stream(true, false).is_ok());
        assert!(reject_stream(false, true).is_ok());
    }

    /// 非 bool 的 `stream` 值不得被 `as_bool` 静默降级成非流式
    #[test]
    fn stream_requested_treats_non_false_values_as_streaming() {
        assert!(!stream_requested(&json!({})), "缺失 = 非流式");
        assert!(!stream_requested(&json!({ "stream": false })));
        assert!(stream_requested(&json!({ "stream": true })));
        assert!(
            stream_requested(&json!({ "stream": "true" })),
            "字符串 true 不得绕过拒绝"
        );
        assert!(stream_requested(&json!({ "stream": 1 })));
        assert!(stream_requested(&json!({ "stream": null })));
    }

    /// 终局整形共用体：`{"detail": ...}` 序列化（网关自身错误 / 上游违约同形）
    #[test]
    fn detail_body_is_detail_only() {
        let body = detail_body("upstream returned SSE body for non-stream request");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            v,
            json!({ "detail": "upstream returned SSE body for non-stream request" })
        );
    }

    /// 方言错误体：只含 `detail`，状态码沿用 AppError 映射；429 带 Retry-After
    #[tokio::test]
    async fn error_response_emits_detail_with_status_semantics() {
        for (err, status) in [
            (AppError::BadRequest("bad".into()), StatusCode::BAD_REQUEST),
            (AppError::Auth("no key".into()), StatusCode::UNAUTHORIZED),
            (
                AppError::ServiceUnavailable("disabled".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let resp = error_response(&err);
            assert_eq!(resp.status(), status);
            assert!(
                resp.headers().get("Retry-After").is_none(),
                "非 429 不带退避头"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body");
            let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
            assert_eq!(v, json!({ "detail": err_message(&err) }));
            assert!(
                v.get("error").is_none(),
                "systemone 方言不得输出 OpenAI 错误体"
            );
        }

        let resp = error_response(&AppError::RateLimited(6.0));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get("Retry-After").unwrap(), "6");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["detail"], "Rate limit exceeded, retry after 6s");
    }

    /// 关联 id 透传：包装错误仍附 X-Request-Id（与 AppError::into_response 一致）
    #[tokio::test]
    async fn error_response_carries_request_id() {
        let id = uuid::Uuid::new_v4();
        let err = AppError::BadRequest("bad".into()).with_request_id(id);
        let resp = error_response(&err);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers()
                .get("X-Request-Id")
                .unwrap()
                .to_str()
                .unwrap(),
            id.to_string()
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["detail"], "bad", "detail 取变体内的 message");
    }

    /// 期望文案 = AppError Display（classify 取的是变体内的 message）
    fn err_message(err: &AppError) -> String {
        match err {
            AppError::BadRequest(m) | AppError::Auth(m) | AppError::ServiceUnavailable(m) => {
                m.clone()
            }
            other => panic!("unexpected variant: {other}"),
        }
    }
}
