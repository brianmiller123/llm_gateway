use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
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

/// 流式上游 body 空闲超时:连续 120s 无任何 chunk 即终止。
/// 上游发完响应头后停流(半开连接/中间设备丢包)时,请求不再无限挂起,
/// 由网关主动断流,避免客户端长时间无响应。
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// 对流式上游响应体施加空闲超时;超时/上游错误统一映射为 Box<dyn Error> 流
pub fn with_idle_timeout<S>(
    inner: S,
    idle: Duration,
) -> impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    IdleTimeoutStream {
        inner: Box::pin(inner),
        idle,
        wait: None,
        timed_out: false,
    }
}

/// 空闲超时包装流：每个 chunk 到达后重置 120s 计时器；超时以错误项终止
struct IdleTimeoutStream<S> {
    inner: Pin<Box<S>>,
    idle: Duration,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
    timed_out: bool,
}

impl<S> Stream for IdleTimeoutStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<Bytes, Box<dyn Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 全字段 Unpin（Pin<Box<S>> 亦 Unpin），get_mut 安全
        let this = self.get_mut();
        if this.timed_out {
            return Poll::Ready(None);
        }
        if this.wait.is_none() {
            this.wait = Some(Box::pin(tokio::time::sleep(this.idle)));
        }
        if let Some(wait) = &mut this.wait {
            if wait.as_mut().poll(cx).is_ready() {
                this.timed_out = true;
                return Poll::Ready(Some(Err(Box::<dyn Error + Send + Sync>::from(
                    "upstream stream idle timeout",
                ))));
            }
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // 收到数据：重置空闲窗口
                this.wait = Some(Box::pin(tokio::time::sleep(this.idle)));
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.wait = Some(Box::pin(tokio::time::sleep(this.idle)));
                Poll::Ready(Some(Err(Box::<dyn Error + Send + Sync>::from(e))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// 代理请求的端点描述
#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    /// 对外路径（写入 usage_logs.endpoint）
    pub api: &'static str,
    /// 上游路径（拼接在 base_url 之后，base_url 约定以 /v1 结尾）
    pub upstream: &'static str,
    /// 是否 /v1/responses 端点（触发降级转换决策）
    pub responses: bool,
}

/// 工具归一化：仅保留 function 类型工具。codex/gpt-5 客户端会声明
/// `web_search` 等内置工具，DeepSeek 等上游反序列化时直接 400 ——
/// 在网关层剥离，请求退化为纯函数调用（客户端可感知）。
fn normalize_tools(json: &mut Value) -> bool {
    let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let before = tools.len();
    tools.retain(|t| {
        matches!(t.get("type").and_then(|x| x.as_str()), None | Some("function"))
    });
    before != tools.len()
}

/// OpenAI 兼容归一化：gpt-5/codex 等客户端发送 `developer` role，多数上游
/// （DeepSeek 等）不支持 → 改写为 `system`。同时覆盖 Chat `messages[]` 与
/// Responses `input[]` 两种格式。返回是否发生改写（决定透传分支是否需重序列化）。
fn normalize_chat_roles(json: &mut Value) -> bool {
    let mut changed = false;
    if let Some(arr) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for m in arr.iter_mut() {
            if m.get("role").and_then(|r| r.as_str()) == Some("developer") {
                m["role"] = Value::String("system".into());
                changed = true;
            }
        }
    }
    if let Some(arr) = json.get_mut("input").and_then(|m| m.as_array_mut()) {
        for item in arr.iter_mut() {
            if item.get("type").and_then(|t| t.as_str()) == Some("message")
                && item.get("role").and_then(|r| r.as_str()) == Some("developer")
            {
                item["role"] = Value::String("system".into());
                changed = true;
            }
        }
    }
    changed
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

    // 3. 解析请求体（先做 role 归一化，所有下游分支共用改写后的 json）
    let mut json: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON: {e}")))?;
    let mut roles_rewritten = normalize_chat_roles(&mut json);
    roles_rewritten |= normalize_tools(&mut json);
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

    // 6. 构造上游请求体（按 provider 决策：/v1/responses 端点对非原生上游降级转换为
    //    Chat Completions；流式自动注入 include_usage 以获得精确 usage；
    //    路由配置了 upstream_model 时把请求模型改写为上游实际模型名）
    let build_outbound = |provider: &Provider| -> Result<(Vec<u8>, &'static str), AppError> {
        let converted = crate::service::responses::should_convert(endpoint.responses, provider);
        if converted {
            // B：降级转换 Responses 请求 → Chat 请求（上游只支持 /v1/chat/completions）
            let mut chat = crate::service::responses::convert_request(&json).map_err(AppError::BadRequest)?;
            if streamed && chat.get("stream_options").is_none() {
                chat["stream_options"] = serde_json::json!({ "include_usage": true });
            }
            if let Some(target) = &route.upstream_model {
                chat["model"] = serde_json::Value::String(target.clone());
            }
            Ok((serde_json::to_vec(&chat).map_err(AppError::internal)?, "/chat/completions"))
        } else if streamed && !endpoint.responses {
            let mut json = json.clone();
            if json.get("stream_options").is_none() {
                json["stream_options"] = serde_json::json!({ "include_usage": true });
            }
            if let Some(target) = &route.upstream_model {
                json["model"] = serde_json::Value::String(target.clone());
            }
            Ok((serde_json::to_vec(&json).map_err(AppError::internal)?, endpoint.upstream))
        } else if let Some(target) = &route.upstream_model {
            if target != &model {
                let mut json = json.clone();
                json["model"] = serde_json::Value::String(target.clone());
                Ok((serde_json::to_vec(&json).map_err(AppError::internal)?, endpoint.upstream))
            } else if roles_rewritten {
                Ok((serde_json::to_vec(&json).map_err(AppError::internal)?, endpoint.upstream))
            } else {
                Ok((body.to_vec(), endpoint.upstream))
            }
        } else if roles_rewritten {
            Ok((serde_json::to_vec(&json).map_err(AppError::internal)?, endpoint.upstream))
        } else {
            Ok((body.to_vec(), endpoint.upstream))
        }
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
        // 流式：仅主上游，不重试（防重复生成/重复计费）
        let provider = &candidates[0];
        let converted = crate::service::responses::should_convert(endpoint.responses, provider);
        let (outbound, upstream_path) = build_outbound(provider)?;
        let provider_key =
            crate::crypto::decrypt(&provider.api_key_encrypted, &st.cfg.master_key)
                .map_err(AppError::internal)?;
        let resp = send_upstream(
            st,
            provider,
            &provider_key,
            &outbound,
            Endpoint { upstream: upstream_path, ..endpoint },
            request_id,
            true,
        )
        .await
        .map_err(|e| AppError::Internal(format!("upstream request failed: {e}")))?;
        let status = resp.status();
        let content_type = content_type_of(&resp);
        let latency = started.elapsed().as_millis() as i64;
        // 降级转换流（上游 Chat SSE → 客户端 Responses SSE）：
        // 转换器内部在 [DONE]/流尾补发终态事件并记账；仅 SSE content-type 才转换
        // （200 + 非 SSE 错误体走原 wrap_stream 透传，避免错误被吞成空 completed 响应）
        if converted && status.is_success() && content_type.contains("text/event-stream") {
            let stream = crate::service::responses::stream::wrap_chat_stream_to_responses(
                st.clone(),
                meta,
                with_idle_timeout(resp.bytes_stream(), STREAM_IDLE_TIMEOUT),
                latency,
                status.as_u16(),
                format!("resp_{request_id}"),
                model.clone(),
            );
            tracing::info!(
                request_id = %request_id, model = %model, provider = %provider.name,
                streamed = true, status = %status, "proxying responses stream (converted)"
            );
            return Ok(Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .header("X-Request-Id", request_id.to_string())
                .body(Body::from_stream(stream))
                .map_err(AppError::internal)?);
        }
        // 非 2xx：不透传 usage 计费（错误体里的 usage 不可信），按真实状态记账、token 记 0
        let capture_usage = status.is_success();
        let stream = wrap_stream(
            st.clone(),
            meta,
            with_idle_timeout(resp.bytes_stream(), STREAM_IDLE_TIMEOUT),
            latency,
            status.as_u16(),
            capture_usage,
        );
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
            let converted = crate::service::responses::should_convert(endpoint.responses, provider);
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
            let (outbound, upstream_path) = match build_outbound(provider) {
                Ok(v) => v,
                Err(e) => return Err(e),
            };
            let resp = match send_upstream(
                st,
                provider,
                &provider_key,
                &outbound,
                Endpoint { upstream: upstream_path, ..endpoint },
                request_id,
                false,
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
            let response_body = if converted {
                // B：上游 Chat 响应 → Responses 响应（仅 2xx 成功体转换；错误体/异常体透传）
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(v) if status.is_success() && v.get("choices").is_some() => {
                        match crate::service::responses::convert_response(
                            &v,
                            &format!("resp_{request_id}"),
                            chrono::Utc::now().timestamp(),
                        ) {
                            Ok(out) => serde_json::to_vec(&out)
                                .unwrap_or_else(|e| {
                                    tracing::warn!(request_id = %request_id, error = %e, "responses convert serialize failed");
                                    bytes.to_vec()
                                }),
                            Err(e) => {
                                tracing::warn!(request_id = %request_id, error = %e, "responses convert failed");
                                bytes.to_vec()
                            }
                        }
                    }
                    _ => bytes.to_vec(),
                }
            } else {
                bytes.to_vec()
            };
            return Ok(Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .header("X-Request-Id", request_id.to_string())
                .body(Body::from(response_body))
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

/// 客户端中断/流被丢弃时的兜底记账（与 tail 通过 recorded 标志互斥，只记一次）
struct BillingOnDrop {
    st: AppState,
    meta: UsageMeta,
    recorded: Arc<AtomicBool>,
    captured: Arc<parking_lot::Mutex<Option<Usage>>>,
    status: u16,
    latency: i64,
}

impl Drop for BillingOnDrop {
    fn drop(&mut self) {
        if !self.recorded.swap(true, Ordering::SeqCst) {
            let st = self.st.clone();
            let meta = self.meta.clone();
            let usage = self.captured.lock().take();
            let status = self.status;
            let latency = self.latency;
            // 尽力而为：客户端中断后无法确认上游实际消耗，按已捕获 usage 记账
            tokio::spawn(async move {
                tracing::warn!(request_id = %meta.request_id, "stream dropped before completion; billing best-effort");
                usage::record(&st, &meta, usage.as_ref(), status, latency).await;
            });
        }
    }
}

/// 带兜底记账的透传流（BillingOnDrop 与流同生命周期）
struct BillingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>,
    _billing: BillingOnDrop,
}

impl Stream for BillingStream {
    type Item = Result<Bytes, Box<dyn Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // BillingStream 全字段 Unpin，get_mut 安全
        self.get_mut().inner.as_mut().poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// 流式透传 + 流内 usage 捕获与记账：
/// - capture_usage = true（上游 2xx）：捕获到 usage 或 [DONE] 时立即记账（仅一次），字节流原样透传
/// - capture_usage = false（上游非 2xx）：错误体不解析不计量，按真实状态记一条零 token 明细
/// - 流异常中断时由 chain 兜底记账（usage 未知）；客户端断开由 BillingOnDrop 兜底
fn wrap_stream(
    st: AppState,
    meta: UsageMeta,
    upstream: impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send + 'static,
    latency: i64,
    record_status: u16,
    capture_usage: bool,
) -> BillingStream {
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
                let chunk = chunk?;
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

    BillingStream {
        inner: Box::pin(main.chain(tail)),
        _billing: BillingOnDrop {
            st,
            meta,
            recorded,
            captured,
            status: record_status,
            latency,
        },
    }
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
                } else if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if usage.is_none() {
                        // Chat 流：usage 在顶层
                        if let Some(u) = v.get("usage") {
                            usage = serde_json::from_value(u.clone()).ok();
                        }
                        // Responses 终态事件：usage 嵌在 response 对象内
                        if usage.is_none() {
                            if let Some(u) = v.get("response").and_then(|r| r.get("usage")) {
                                usage = serde_json::from_value(u.clone()).ok();
                            }
                        }
                    }
                    // Responses 流无 [DONE] 哨兵：usage 在 response.completed 事件内，
                    // 以终态事件类型判定结束
                    if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
                        if matches!(
                            ty,
                            "response.completed"
                                | "response.incomplete"
                                | "response.failed"
                                | "response.done"
                        ) {
                            done = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Chat 流：usage 顶层 + [DONE] 哨兵
    #[test]
    fn feed_sse_chat_stream() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(
            &mut buf,
            b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
        );
        assert!(done);
        let u = usage.expect("usage captured");
        assert_eq!(u.input(), 4);
        assert_eq!(u.output(), 2);
    }

    /// Responses 流：无 [DONE]，usage 嵌在 response 内，终态事件判定结束
    #[test]
    fn feed_sse_responses_stream() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(
            &mut buf,
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":7,\"total_tokens\":19}}}\n\n",
        );
        assert!(done, "response.completed 应判定结束");
        let u = usage.expect("usage captured from response.completed");
        assert_eq!(u.input(), 12);
        assert_eq!(u.output(), 7);
    }

    /// Responses 流：response.done 也能判定结束（即便 usage 缺失）
    #[test]
    fn feed_sse_responses_done_without_usage() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(&mut buf, b"data: {\"type\":\"response.done\"}\n\n");
        assert!(done);
        assert!(usage.is_none());
    }

    /// 跨块缓冲 + usage 双形态（Responses 顶层 usage 也支持）
    #[test]
    fn feed_sse_cross_chunk_and_top_level_responses_usage() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(
            &mut buf,
            b"data: {\"type\":\"response.completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1},\"response\":{}}\n\n",
        );
        assert!(done);
        let u = usage.expect("usage");
        assert_eq!(u.input(), 3);
        assert_eq!(u.output(), 1);
        // 跨块：半截事件
        let mut buf = Vec::new();
        let (_, done) = feed_sse(&mut buf, b"data: {\"a\":1}");
        assert!(!done);
        let (_, done) = feed_sse(&mut buf, b"\n\ndata: [DONE]\n\n");
        assert!(done);
    }
}
