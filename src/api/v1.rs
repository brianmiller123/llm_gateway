use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::json;
use std::net::SocketAddr;

use crate::error::AppError;
use crate::service::proxy::{self, Endpoint};
use crate::state::AppState;

/// 客户端 IP：X-Forwarded-For 首值可解析时用之（假定经可信反代部署），
/// 否则取对端地址。仅用于用量统计，非安全边界（直连场景可伪造 XFF）。
pub(crate) fn resolve_client_ip(peer: SocketAddr, headers: &axum::http::HeaderMap) -> std::net::IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next().map(str::trim))
        .and_then(|s| s.parse().ok())
        .unwrap_or(peer.ip())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/models", get(models))
}

async fn chat_completions(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers);
    proxy::proxy(
        &st,
        headers,
        body,
        Endpoint {
            api: "/v1/chat/completions",
            upstream: "/chat/completions",
        },
        ip,
    )
    .await
}

async fn completions(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers);
    proxy::proxy(
        &st,
        headers,
        body,
        Endpoint {
            api: "/v1/completions",
            upstream: "/completions",
        },
        ip,
    )
    .await
}

async fn embeddings(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers);
    proxy::proxy(
        &st,
        headers,
        body,
        Endpoint {
            api: "/v1/embeddings",
            upstream: "/embeddings",
        },
        ip,
    )
    .await
}

/// GET /v1/models：列出全部模型名（模型库 models 表 + 已启用路由的具体 pattern，去重）；
/// 有 API Key 时按用户授权过滤；通配符 pattern（不可作为模型名提交）不列出。
async fn models(State(st): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    let uid = crate::service::auth::authenticate(&st, &headers)
        .await
        .ok()
        .and_then(|(uid, _)| uid);
    let is_admin = uid
        .map(|u| st.admin_ids.read().contains(&u))
        .unwrap_or(true);
    let can_use = |provider_id: i64, model: &str| {
        match uid {
            None => true,
            Some(_) if is_admin => true,
            Some(u) => crate::service::routing::user_can_use(&st, u, provider_id, model),
        }
    };

    let mut seen: Vec<String> = Vec::new();
    let mut data: Vec<serde_json::Value> = Vec::new();
    let mut push = |name: String| {
        if seen.contains(&name) {
            return;
        }
        seen.push(name.clone());
        data.push(json!({
            "id": name,
            "object": "model",
            "owned_by": "llm-gateway",
        }));
    };

    // 1) 模型库（测试连接/手动刷新入库的模型）
    let catalog: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "SELECT m.model_id, m.provider_id FROM models m \
         JOIN providers p ON p.id = m.provider_id",
    )
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for (model, provider_id) in catalog {
        if can_use(provider_id, &model) {
            push(model);
        }
    }

    // 2) 已启用路由的具体 pattern（通配符仅用于匹配，不对外列出）
    let routes = st.routes.read();
    for r in routes.iter() {
        if r.model_pattern.ends_with('*') {
            continue;
        }
        let providers = st
            .providers
            .read()
            .iter()
            .filter(|p| p.id == r.provider_id || r.fallback_ids.contains(&p.id))
            .cloned()
            .collect::<Vec<_>>();
        if providers.is_empty() {
            continue;
        }
        if providers.iter().any(|p| can_use(p.id, &r.model_pattern)) {
            push(r.model_pattern.clone());
        }
    }
    (StatusCode::OK, Json(json!({"object": "list", "data": data}))).into_response()
}
