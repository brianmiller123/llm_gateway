use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::{once, Stream, StreamExt};
use serde_json::Value;
use uuid::Uuid;

use crate::error::AppError;
use crate::service::usage;
use crate::state::AppState;
use crate::store::upstream::Provider;
use crate::store::usage::{Usage, UsageMeta};

/// 代理请求的端点描述
#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    /// 对外路径（写入 usage_logs.endpoint）
    pub api: &'static str,
    /// 上游路径（拼接在 base_url 之后，base_url 约定以 /v1 结尾）
    pub upstream: &'static str,
}

/// OpenAI 兼容代理：鉴权 → 限流 → 配额 → 路由 → 转发（含降级）→ 记账
pub async fn proxy(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
) -> Result<Response, AppError> {
    let started = Instant::now();

    // 1. 鉴权
    let (user_id, key_id) = crate::service::auth::authenticate(st, &headers).await?;

    // 2. 三层限流
    crate::service::ratelimit::apply_rate_limits(st, user_id, key_id)?;

    // 3. 解析请求体
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON: {e}")))?;
    let model: String = json
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError::BadRequest("missing 'model' field".into()))?;
    let streamed = json.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // 4. 配额预检查
    if let Some(uid) = user_id {
        usage::check_quota(st, uid)?;
    }

    // 5. 路由解析 → 候选上游（主 + 降级链）
    let route = {
        let routes = st.routes.read();
        crate::service::routing::resolve_route(&routes, &model)
            .cloned()
            .ok_or_else(|| {
                AppError::BadRequest(format!("model '{model}' is not routed to any provider"))
            })?
    };
    let candidates = resolve_candidates(st, &route);
    if candidates.is_empty() {
        return Err(AppError::Internal(format!(
            "no enabled provider for route '{}'",
            route.model_pattern
        )));
    }
    // 用户访问授权：无规则=默认放行；有规则则过滤掉未授权候选（含降级链）；admin 跳过
    let candidates = match user_id {
        Some(uid) if !st.admin_ids.read().contains(&uid) => {
            let allowed: Vec<_> = candidates
                .into_iter()
                .filter(|p| crate::service::routing::user_can_use(st, uid, p.id, &model))
                .collect();
            allowed
        }
        _ => candidates,
    };
    if candidates.is_empty() {
        return Err(AppError::Forbidden(format!(
            "model '{model}' is not permitted for this user"
        )));
    }

    // 6. 构造上游请求体（流式自动注入 include_usage 以获得精确 usage；
    //    路由配置了 upstream_model 时把请求模型改写为上游实际模型名）
    let outbound = if streamed {
        let mut json = json;
        if json.get("stream_options").is_none() {
            json["stream_options"] = serde_json::json!({ "include_usage": true });
        }
        if let Some(target) = &route.upstream_model {
            json["model"] = serde_json::Value::String(target.clone());
        }
        serde_json::to_vec(&json).map_err(AppError::internal)?
    } else if let Some(target) = &route.upstream_model {
        if target != &model {
            let mut json = json;
            json["model"] = serde_json::Value::String(target.clone());
            serde_json::to_vec(&json).map_err(AppError::internal)?
        } else {
            body.to_vec()
        }
    } else {
        body.to_vec()
    };

    let request_id = Uuid::new_v4();
    let mut meta = UsageMeta {
        request_id,
        user_id,
        api_key_id: key_id,
        model: model.clone(),
        provider_id: candidates[0].id,
        endpoint: endpoint.api,
        streamed,
        client_ip: Some(client_ip),
    };

    // 7. 转发 + 记账
    if streamed {
        // 流式：仅主上游，不重试（防重复生成/重复计费），透明透传
        let provider = &candidates[0];
        let provider_key =
            crate::crypto::decrypt(&provider.api_key_encrypted, &st.cfg.master_key)
                .map_err(AppError::internal)?;
        let resp = send_upstream(st, provider, &provider_key, &outbound, endpoint, request_id, true)
            .await
            .map_err(|e| AppError::Internal(format!("upstream request failed: {e}")))?;
        let status = resp.status();
        let content_type = content_type_of(&resp);
        let latency = started.elapsed().as_millis() as i64;
        // 非 2xx：不透传 usage 计费（错误体里的 usage 不可信），按真实状态记账、token 记 0
        let capture_usage = status.is_success();
        let stream = wrap_stream(st.clone(), meta, resp.bytes_stream(), latency, status.as_u16(), capture_usage);
        tracing::info!(
            request_id = %request_id, model = %model, provider = %provider.name,
            streamed = true, status = %status, "proxying stream"
        );
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header("X-Request-Id", request_id.to_string())
            .body(Body::from_stream(stream))
            .map_err(AppError::internal)?)
    } else {
        // 非流式：429/5xx/超时/传输错误 → 按降级链重试；4xx 透明透传不重试
        let mut last_response: Option<(u16, Vec<u8>, String)> = None;
        for provider in &candidates {
            meta.provider_id = provider.id;
            let provider_key = match crate::crypto::decrypt(
                &provider.api_key_encrypted,
                &st.cfg.master_key,
            ) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "provider key decrypt failed");
                    continue;
                }
            };
            let resp = match send_upstream(
                st, provider, &provider_key, &outbound, endpoint, request_id, false,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "upstream attempt failed");
                    continue;
                }
            };
            let status = resp.status();
            let content_type = content_type_of(&resp);
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                // 可重试：记录本次响应后尝试降级
                let bytes = resp.bytes().await.unwrap_or_default().to_vec();
                tracing::warn!(provider = %provider.name, status = %status, "upstream retryable failure");
                last_response = Some((status.as_u16(), bytes, content_type));
                continue;
            }
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "upstream body read failed");
                    continue;
                }
            };
            let latency = started.elapsed().as_millis() as i64;
            let usage = parse_usage(&bytes);
            usage::record(st, &meta, usage.as_ref(), status.as_u16(), latency).await;
            tracing::info!(request_id = %request_id, model = %model, provider = %provider.name, "proxied");
            return Ok(Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .header("X-Request-Id", request_id.to_string())
                .body(Body::from(bytes))
                .map_err(AppError::internal)?);
        }
        // 全部候选失败：优先透传最后一次上游响应，否则 502
        if let Some((status, bytes, content_type)) = last_response {
            let latency = started.elapsed().as_millis() as i64;
            let usage = parse_usage(&bytes);
            usage::record(st, &meta, usage.as_ref(), status, latency).await;
            return Ok(Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
                .header(header::CONTENT_TYPE, content_type)
                .header("X-Request-Id", request_id.to_string())
                .body(Body::from(bytes))
                .map_err(AppError::internal)?);
        }
        Err(AppError::Internal("all upstream attempts failed".into()))
    }
}

/// 主上游 + 降级链（去重、仅启用）
fn resolve_candidates(st: &AppState, route: &crate::store::upstream::ModelRoute) -> Vec<Provider> {
    let providers = st.providers.read();
    let mut list: Vec<Provider> = Vec::new();
    for id in std::iter::once(route.provider_id).chain(route.fallback_ids.iter().copied()) {
        if let Some(p) = providers.iter().find(|p| p.id == id) {
            if !list.iter().any(|l| l.id == id) {
                list.push(p.clone());
            }
        }
    }
    list
}

/// 发送上游请求；timeout_ms 作用于响应头等待；非流式额外施加请求总超时（含响应体）
async fn send_upstream(
    st: &AppState,
    provider: &Provider,
    provider_key: &str,
    body: &[u8],
    endpoint: Endpoint,
    request_id: Uuid,
    streamed: bool,
) -> Result<reqwest::Response, Box<dyn Error + Send + Sync>> {
    let url = format!(
        "{}{}",
        provider.base_url.trim_end_matches('/'),
        endpoint.upstream
    );
    let mut req = st
        .client
        .post(&url)
        .header(header::AUTHORIZATION, format!("Bearer {provider_key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Request-Id", request_id.to_string())
        .body(body.to_vec());
    if streamed {
        req = req.header(header::ACCEPT, "text/event-stream");
    } else {
        // 非流式：reqwest 总超时覆盖 响应头+响应体（防慢速上游挂死连接池）
        let total = Duration::from_millis(provider.timeout_ms.max(1_000) as u64 + 30_000);
        req = req.timeout(total);
    }
    let timeout = Duration::from_millis(provider.timeout_ms.max(1_000) as u64);
    tokio::time::timeout(timeout, req.send())
        .await
        .map_err(|_| "upstream header timeout".to_string())?
        .map_err(|e| format!("upstream request failed: {e}").into())
}

fn content_type_of(resp: &reqwest::Response) -> String {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string()
}

/// 流式透传 + 流内 usage 捕获与记账：
/// - capture_usage = true（上游 2xx）：捕获到 usage 或 [DONE] 时立即记账（仅一次），字节流原样透传
/// - capture_usage = false（上游非 2xx）：错误体不解析不计量，按真实状态记一条零 token 明细
/// - 流异常中断时由 chain 兜底记账（usage 未知）
fn wrap_stream(
    st: AppState,
    meta: UsageMeta,
    upstream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    latency: i64,
    record_status: u16,
    capture_usage: bool,
) -> impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send {
    let recorded = Arc::new(AtomicBool::new(false));
    let captured: Arc<parking_lot::Mutex<Option<Usage>>> = Arc::new(parking_lot::Mutex::new(None));
    let buf: Arc<parking_lot::Mutex<Vec<u8>>> = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let main = upstream.then({
        let recorded = recorded.clone();
        let captured = captured.clone();
        let buf = buf.clone();
        let st = st.clone();
        let meta = meta.clone();
        move |chunk| {
            let recorded = recorded.clone();
            let captured = captured.clone();
            let buf = buf.clone();
            let st = st.clone();
            let meta = meta.clone();
            async move {
                let chunk = chunk.map_err(|e| -> Box<dyn Error + Send + Sync> { e.into() })?;
                let (usage, done) = if capture_usage {
                    feed_sse(&mut buf.lock(), &chunk)
                } else {
                    (None, false)
                };
                let has_usage = usage.is_some();
                if has_usage {
                    *captured.lock() = usage;
                }
                if (done || has_usage) && !recorded.swap(true, Ordering::SeqCst) {
                    let u = captured.lock().take();
                    usage::record(&st, &meta, u.as_ref(), record_status, latency).await;
                }
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(chunk)
            }
        }
    });

    let tail = once({
        let recorded = recorded.clone();
        let captured = captured.clone();
        let st = st.clone();
        let meta = meta.clone();
        async move {
            // 上游未正常结束（无 [DONE]/usage）时兜底记账
            if !recorded.swap(true, Ordering::SeqCst) {
                let u = captured.lock().take();
                usage::record(&st, &meta, u.as_ref(), record_status, latency).await;
            }
            Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::new())
        }
    });

    main.chain(tail)
}

/// 从 OpenAI 兼容响应体解析 usage
pub fn parse_usage(body: &[u8]) -> Option<Usage> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let u = value.get("usage")?;
    serde_json::from_value(u.clone()).ok()
}

/// 增量解析 SSE 字节流：返回 (本次捕获的 usage, 是否遇到 [DONE])
/// 缓冲跨块保留，事件按 "\n\n" 或 "\r\n\r\n" 切分
pub fn feed_sse(buf: &mut Vec<u8>, chunk: &[u8]) -> (Option<Usage>, bool) {
    buf.extend_from_slice(chunk);
    let mut usage = None;
    let mut done = false;
    loop {
        let Some(idx) = find_event_end(buf) else { break };
        let event: Vec<u8> = buf.drain(..idx).collect();
        let text = String::from_utf8_lossy(&event);
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data == "[DONE]" {
                    done = true;
                } else if usage.is_none() {
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(u) = v.get("usage") {
                            usage = serde_json::from_value(u.clone()).ok();
                        }
                    }
                }
            }
        }
        if done {
            break;
        }
    }
    // 防御：异常巨大的不完整事件丢弃缓冲
    if buf.len() > 1_000_000 {
        buf.clear();
    }
    (usage, done)
}

fn find_event_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| i + 2)
        .or_else(|| {
            buf.windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4)
        })
}
