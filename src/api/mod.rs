pub mod admin_config;
pub mod console;
pub mod v1;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::services::ServeDir;

use crate::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let web_dir = state.cfg.web_dir.clone();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(root))
        .merge(v1::routes())
        .merge(console::routes(state.clone()))
        .merge(admin_config::routes(state.clone()))
        // 控制台前端静态资源（Vue dist）；目录不存在时 ServeDir 自然 404
        .nest_service("/assets", ServeDir::new(web_dir.join("assets")))
        // SPA 深层路由（浏览器导航）→ index.html；API 未匹配路径 → JSON 404
        .fallback(spa_fallback)
        // LLM 请求体（长上下文 prompt）可远超 axum 默认 2MB 限制，放宽到 32MB
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
}

async fn healthz() -> Response {
    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
}

async fn root(State(st): State<AppState>) -> Response {
    // 前端已构建：`/` 返回控制台；否则返回服务信息 JSON（纯 API 部署）
    match tokio::fs::read(st.cfg.web_dir.join("index.html")).await {
        Ok(bytes) => html_response(bytes),
        Err(_) => (
            StatusCode::OK,
            Json(json!({
                "service": "llm_gateway",
                "version": env!("CARGO_PKG_VERSION"),
                "openai_compat": ["/v1/chat/completions", "/v1/responses", "/v1/completions", "/v1/embeddings", "/v1/models"]
            })),
        )
            .into_response(),
    }
}

/// 未匹配路由：SPA 导航请求回 index.html；/api /v1 前缀与机器客户端一律 JSON 404
async fn spa_fallback(State(st): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html"))
        .unwrap_or(false);
    if wants_html && !path.starts_with("/api/") && !path.starts_with("/v1/") {
        if let Ok(bytes) = tokio::fs::read(st.cfg.web_dir.join("index.html")).await {
            return html_response(bytes);
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {"code": "not_found", "message": "not found", "type": "not_found"}
        })),
    )
        .into_response()
}

fn html_response(bytes: Vec<u8>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        bytes,
    )
        .into_response()
}
