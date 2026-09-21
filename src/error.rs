use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

/// 网关统一错误：一律输出 OpenAI 兼容错误体
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("rate limited, retry after {0}s")]
    RateLimited(f64),
    /// 并发上限（在途请求数）满：与 RateLimited 同为 429，但消息可区分
    #[error("concurrency limit exceeded, retry after {0}s")]
    ConcurrentLimited(f64),
    #[error("monthly quota exceeded")]
    QuotaExceeded,
    /// Coding Plan 周期配额耗尽且策略为拦截（429 insufficient_quota）
    #[error("{0}")]
    PlanQuotaExceeded(String, Option<u64>),
    /// 状态冲突（如重复加入同一 Coding Plan）
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
    // M26：上游错误语义映射（cc-switch error.rs:130-157 同款）——客户端据此区分
    // 可重试(504) / 网关错误(502) / 渠道耗尽(503)
    #[error("upstream request timeout: {0}")]
    UpstreamTimeout(String),
    #[error("bad gateway: {0}")]
    BadGateway(String),
    #[error("all upstream providers exhausted: {0}")]
    UpstreamExhausted(String),
    // P1-6：带请求关联 id 的包装（proxy 入口生成；响应头 X-Request-Id +
    // 日志关联早期失败）。Display/状态码语义全部委托内层。
    #[error("{inner}")]
    WithRequestId {
        inner: Box<AppError>,
        request_id: uuid::Uuid,
    },
}

impl AppError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        AppError::Internal(e.to_string())
    }

    /// P1-6：包装请求关联 id（幂等——已包装的直接换 id）
    pub fn with_request_id(self, request_id: uuid::Uuid) -> Self {
        match self {
            AppError::WithRequestId { inner, .. } => AppError::WithRequestId { inner, request_id },
            other => AppError::WithRequestId {
                inner: Box::new(other),
                request_id,
            },
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // P1-6：解包关联 id，响应统一附 X-Request-Id 头（早期失败可关联日志）
        let (request_id, self_err) = match self {
            AppError::WithRequestId { inner, request_id } => (Some(request_id), *inner),
            other => (None, other),
        };
        let (status, code, message, retry_after): (StatusCode, &str, String, Option<u64>) =
            match self_err {
                // 双层包装（理论上不出现）解包后按内层变体映射
                AppError::WithRequestId { inner, .. } => {
                    return IntoResponse::into_response(*inner);
                }
                AppError::Auth(m) => (StatusCode::UNAUTHORIZED, "invalid_api_key", m, None),
                AppError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m, None),
                AppError::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m, None),
                AppError::RateLimited(secs) => {
                    // 上限 24h：rpm<=0 的规则会产生无限等待，不能让 u64 溢出/巨值直达客户端
                    let retry = secs.ceil().max(1.0).min(86400.0) as u64;
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_exceeded",
                        format!("Rate limit exceeded, retry after {retry}s"),
                        Some(retry),
                    )
                }
                AppError::ConcurrentLimited(secs) => {
                    // 同 RateLimited：钳制 [1, 86400]，防溢出/巨值直达客户端
                    let retry = secs.ceil().max(1.0).min(86400.0) as u64;
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_exceeded",
                        format!("Concurrency limit exceeded, retry after {retry}s"),
                        Some(retry),
                    )
                }
                AppError::QuotaExceeded => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "insufficient_quota",
                    "Monthly quota exceeded, please contact administrator".to_string(),
                    None,
                ),
                AppError::PlanQuotaExceeded(m, retry) => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "insufficient_quota",
                    m,
                    retry,
                ),
                AppError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m, None),
                AppError::BadRequest(m) => {
                    (StatusCode::BAD_REQUEST, "invalid_request_error", m, None)
                }
                AppError::Internal(m) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", m, None)
                }
                AppError::ServiceUnavailable(m) => {
                    (StatusCode::SERVICE_UNAVAILABLE, "api_disabled", m, None)
                }
                AppError::UpstreamTimeout(m) => {
                    (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", m, None)
                }
                AppError::BadGateway(m) => (StatusCode::BAD_GATEWAY, "bad_gateway", m, None),
                AppError::UpstreamExhausted(m) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_exhausted",
                    m,
                    None,
                ),
            };

        let body = json!({
            "error": { "message": message, "type": code, "code": code }
        });
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
            .body(Body::from(body.to_string()))
            .expect("static response is valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// Plan 配额 429 携带 Retry-After：客户端可等到周期重置而非空转重试
    #[tokio::test]
    async fn plan_quota_response_carries_retry_after() {
        let resp =
            AppError::PlanQuotaExceeded("plan p exhausted".into(), Some(3600)).into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get("Retry-After").unwrap(), "3600");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "insufficient_quota");
        assert_eq!(v["error"]["message"], "plan p exhausted");
    }

    /// total 周期无边界 → 无 Retry-After（不承诺不确定的等待）
    #[tokio::test]
    async fn plan_quota_without_boundary_has_no_retry_after() {
        let resp = AppError::PlanQuotaExceeded("x".into(), None).into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get("Retry-After").is_none());
    }

    /// 限流 Retry-After 上限钳制（rpm<=0 产生的巨值不得直达客户端）
    #[tokio::test]
    async fn rate_limited_retry_after_is_capped() {
        let resp = AppError::RateLimited(1e9).into_response();
        assert_eq!(resp.headers().get("Retry-After").unwrap(), "86400");
    }
}
