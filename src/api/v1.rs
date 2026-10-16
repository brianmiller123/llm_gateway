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
pub(crate) fn resolve_client_ip(
    peer: SocketAddr,
    headers: &axum::http::HeaderMap,
) -> std::net::IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next().map(str::trim))
        .and_then(|s| s.parse().ok())
        .unwrap_or(peer.ip())
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/models", get(models))
        // 全局自定义响应头（设置页）：附加到 /v1/* 所有响应（含错误整形路径）；
        // insert 语义覆盖同名头，保存时已过校验，此处解析失败防御性跳过
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            append_custom_response_headers,
        ))
}

async fn append_custom_response_headers(
    axum::extract::State(st): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    let custom = st.custom_headers.read();
    if !custom.response.is_empty() {
        for (k, v) in custom.response.iter() {
            if let Some(s) = v.as_str() {
                if let (Ok(name), Ok(val)) = (
                    k.parse::<axum::http::HeaderName>(),
                    axum::http::HeaderValue::from_str(s),
                ) {
                    resp.headers_mut().insert(name, val);
                }
            }
        }
    }
    resp
}

/// POST /v1/responses：原生透传（上游 api_type=openai-responses）或
/// 降级转换为 Chat Completions（其余上游），由 proxy 按 provider 决策
async fn responses(
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
            api: "/v1/responses",
            upstream: "/responses",
            responses: true,
            anthropic: false,
        },
        ip,
    )
    .await
}

/// POST /v1/messages：Anthropic Messages API。上游 api_type=anthropic 透传；
/// 其余（默认 openai 等 Chat 兼容上游）降级转换 Anthropic ↔ Chat。
/// 网关内部错误整形为 Anthropic 单错误对象形状（客户端方言）。
async fn messages(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers);
    match proxy::proxy(
        &st,
        headers,
        body,
        Endpoint {
            api: "/v1/messages",
            upstream: "/messages",
            responses: false,
            anthropic: true,
        },
        ip,
    )
    .await
    {
        Ok(resp) => Ok(resp),
        Err(e) => Ok(crate::service::anthropic::error_response(&e)),
    }
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
            responses: false,
            anthropic: false,
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
            responses: false,
            anthropic: false,
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
            responses: false,
            anthropic: false,
        },
        ip,
    )
    .await
}

/// GET /v1/models：列出全部模型名（模型库 models 表 + 已启用路由的具体 pattern，去重）；
/// 有 API Key 时按用户授权过滤；通配符 pattern（不可作为模型名提交）不列出。
/// 模型库中设置了 context_length 的模型同值下发三个别名字段
/// （context_length / context_window / max_model_len——分别覆盖 one-api /
/// Anthropic / vLLM 生态客户端的读取约定），未设置则不下发，客户端回落自身默认。

/// 同名模型出现在多个供应商时的上下文长度合并：取已设置值的最小值——
/// 请求可能路由到任一候选渠道，最小值才是对客户端保证可用的安全上下文。
fn merge_context_length(acc: Option<i64>, next: Option<i64>) -> Option<i64> {
    match (acc, next) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// 同名模型多渠道能力合并：取最弱（未标注 < text < vision < multimodal）——
/// 请求可能路由到任一候选渠道，最弱才是对客户端保证成立的能力；
/// 任一候选未标注 → 整体未标注（不下发）。
fn merge_capability<'a>(acc: Option<&'a str>, next: Option<&'a str>) -> Option<&'a str> {
    fn rank(c: Option<&str>) -> u8 {
        match c {
            None => 0,
            Some("text") => 1,
            Some("vision") => 2,
            Some(_) => 3,
        }
    }
    if rank(acc) <= rank(next) {
        acc
    } else {
        next
    }
}

async fn models(State(st): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    let uid = crate::service::auth::authenticate(&st, &headers)
        .await
        .ok()
        .and_then(|(uid, _)| uid);
    let is_admin = uid
        .map(|u| st.admin_ids.read().contains(&u))
        .unwrap_or(true);
    let can_use = |provider_id: i64, model: &str| match uid {
        None => true,
        Some(_) if is_admin => true,
        Some(u) => crate::service::routing::user_can_use(&st, u, provider_id, model),
    };

    let mut seen: Vec<String> = Vec::new();
    let mut data: Vec<serde_json::Value> = Vec::new();
    let mut push = |name: String, ctx: Option<i64>, capability: Option<&str>| {
        if seen.contains(&name) {
            return;
        }
        seen.push(name.clone());
        let mut obj = json!({
            "id": name,
            "object": "model",
            "owned_by": "llm-gateway",
        });
        if let Some(n) = ctx {
            obj["context_length"] = json!(n);
            obj["context_window"] = json!(n);
            obj["max_model_len"] = json!(n);
        }
        if let Some(cap) = capability {
            obj["capability"] = json!(cap);
        }
        data.push(obj);
    };

    // 1) 模型库（测试连接/手动刷新入库的模型）；仅列启用供应商下已启用的模型。
    //    聚合与授权过滤同集合：只对该用户可路由的供应商取最小值——更精确
    //    （不因无权渠道的更小设置而低估可用上下文），且仍不高估。
    let catalog: Vec<(String, i64, Option<i64>, Option<String>)> =
        sqlx::query_as::<_, (String, i64, Option<i64>, Option<String>)>(
            "SELECT m.model_id, m.provider_id, m.context_length, m.capability FROM models m \
             JOIN providers p ON p.id = m.provider_id AND p.enabled \
             WHERE m.enabled",
        )
        .fetch_all(&st.pool)
        .await
        .unwrap_or_default();
    let mut catalog_ctx: std::collections::HashMap<&str, Option<i64>> =
        std::collections::HashMap::new();
    let mut catalog_cap: std::collections::HashMap<&str, Option<&str>> =
        std::collections::HashMap::new();
    for (model, provider_id, ctx, cap) in &catalog {
        if can_use(*provider_id, model) {
            let entry = catalog_ctx.entry(model.as_str()).or_insert(None);
            *entry = merge_context_length(*entry, *ctx);
            // 种子取首个候选的原始值而非 None：None 在 merge_capability 里是
            // “未标注=最弱”，作单位元会让首个已标注候选被吞掉（永不下发）
            let cap_entry = catalog_cap.entry(model.as_str()).or_insert(cap.as_deref());
            *cap_entry = merge_capability(*cap_entry, cap.as_deref());
        }
    }
    for (model, provider_id, _, _) in &catalog {
        if can_use(*provider_id, model) {
            let cap = catalog_cap.get(model.as_str()).copied().flatten();
            push(model.clone(), catalog_ctx.get(model.as_str()).copied().flatten(), cap);
        }
    }

    // 2) 已启用路由的具体 pattern（通配符仅用于匹配，不对外列出）；
    //    路由指向的供应商若在模型库中禁用了该（上游侧）模型则不计入
    let routes = st.routes.read();
    for r in routes.iter() {
        if r.model_pattern.ends_with('*') {
            continue;
        }
        let upstream_model = r.upstream_model.as_deref().unwrap_or(&r.model_pattern);
        let providers = st
            .providers
            .read()
            .iter()
            .filter(|p| p.id == r.provider_id || r.fallback_ids.contains(&p.id))
            .filter(|p| !crate::service::routing::model_disabled(&st, p.id, upstream_model))
            .cloned()
            .collect::<Vec<_>>();
        if providers.is_empty() {
            continue;
        }
        if providers.iter().any(|p| can_use(p.id, &r.model_pattern)) {
            // 路由 pattern 与模型库同名时已在前一步 push（含上下文/能力），此处仅补
            // 模型库未收录的路由模型——无 DB 行，无上下文/能力可下发
            push(r.model_pattern.clone(), None, None);
        }
    }
    (
        StatusCode::OK,
        Json(json!({"object": "list", "data": data})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{merge_capability, merge_context_length};

    #[test]
    fn merge_context_length_takes_min_of_set_values() {
        assert_eq!(merge_context_length(None, None), None, "双方未设置 → 不下发");
        assert_eq!(merge_context_length(None, Some(384_000)), Some(384_000));
        assert_eq!(merge_context_length(Some(1_000_000), None), Some(1_000_000));
        assert_eq!(
            merge_context_length(Some(1_000_000), Some(384_000)),
            Some(384_000),
            "取最小：请求可能落到任一候选渠道"
        );
        assert_eq!(merge_context_length(Some(128_000), Some(384_000)), Some(128_000));
    }

    #[test]
    fn merge_capability_takes_weakest_of_candidates() {
        assert_eq!(merge_capability(None, None), None, "双方未标注 → 不下发");
        assert_eq!(merge_capability(None, Some("vision")), None, "任一未标注 → 整体未标注");
        assert_eq!(merge_capability(Some("vision"), Some("vision")), Some("vision"));
        assert_eq!(
            merge_capability(Some("vision"), Some("multimodal")),
            Some("vision"),
            "取最弱：请求可能落到任一候选渠道"
        );
        assert_eq!(merge_capability(Some("multimodal"), Some("text")), Some("text"));
        assert_eq!(merge_capability(Some("text"), Some("vision")), Some("text"));
    }

    /// 回归：handler 折叠以首个候选的原始值为种子（而非 None 单位元）——
    /// 种子用 None 会让首个已标注候选被吞掉，capability 永不下发
    #[test]
    fn capability_fold_seeds_with_first_candidate() {
        fn fold<'a>(caps: &[Option<&'a str>]) -> Option<&'a str> {
            let mut acc = caps[0];
            for c in &caps[1..] {
                acc = merge_capability(acc, *c);
            }
            acc
        }
        assert_eq!(fold(&[Some("vision")]), Some("vision"), "单渠道已标注 → 下发");
        assert_eq!(fold(&[None]), None);
        assert_eq!(fold(&[Some("vision"), Some("multimodal")]), Some("vision"));
        assert_eq!(fold(&[None, Some("vision")]), None, "任一未标注 → 不下发");
        assert_eq!(fold(&[Some("vision"), None]), None);
    }
}
