use axum::{
    body::Body,
    http::{header, StatusCode},
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
    #[error("monthly quota exceeded")]
    QuotaExceeded,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        AppError::Internal(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, retry_after): (StatusCode, &str, String, Option<u64>) =
            match self {
                AppError::Auth(m) => (
                    StatusCode::UNAUTHORIZED,
                    "invalid_api_key",
                    m,
                    None,
                ),
                AppError::Unauthorized(m) => (
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    m,
                    None,
                ),
                AppError::Forbidden(m) => (
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    m,
                    None,
                ),
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
                AppError::QuotaExceeded => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "insufficient_quota",
                    "Monthly quota exceeded, please contact administrator".to_string(),
                    None,
                ),
                AppError::BadRequest(m) => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    m,
                    None,
                ),
                AppError::Internal(m) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
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
        builder
            .body(Body::from(body.to_string()))
            .expect("static response is valid")
    }
}
