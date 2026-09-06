use std::error::Error;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::{Stream, StreamExt, once};
use serde_json::Value;
use uuid::Uuid;

use crate::error::AppError;
use crate::service::usage;
use crate::state::AppState;
use crate::store::upstream::Provider;
use crate::store::usage::{Usage, UsageMeta};

/// 流式超时默认值（秒）：0 = 禁用。实际值取自 AppConfig
///（GATEWAY_STREAM_FIRST_BYTE_TIMEOUT / GATEWAY_STREAM_IDLE_TIMEOUT），
/// 与 cc-switch ProxyConfig 同款（首字节 60 / 静默 120 默认）。

/// Chat 上游流式链路的心跳间隔：连续静默超过该间隔向下游发 `: ping` 注释行
///（SSE 注释行被 OpenAI/Anthropic 客户端解析器忽略，仅保活中间设备与客户端超时）
pub const STREAM_PING_INTERVAL: Duration = Duration::from_secs(15);

/// 对流式上游响应体施加首字节 + 静默超时；超时/上游错误统一映射为
/// Box<dyn Error> 流。None = 该阶段不超时。
pub fn with_stream_timeouts<S>(
    inner: S,
    first_byte: Option<Duration>,
    idle: Option<Duration>,
) -> impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    StreamTimeoutStream {
        inner: Box::pin(inner),
        first_byte,
        idle,
        first: true,
        wait: None,
        timed_out: false,
    }
}

/// 超时包装流：首 chunk 用 first_byte 计时，其后每个 chunk 重置 idle 计时；
/// 超时以错误项终止。L9（cc-switch response_processor.rs:700-733 语义）
struct StreamTimeoutStream<S> {
    inner: Pin<Box<S>>,
    first_byte: Option<Duration>,
    idle: Option<Duration>,
    first: bool,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
    timed_out: bool,
}

impl<S> Stream for StreamTimeoutStream<S>
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
        let window = if this.first {
            this.first_byte
        } else {
            this.idle
        };
        if this.wait.is_none() {
            this.wait = window.map(|d| Box::pin(tokio::time::sleep(d)));
        }
        if let Some(wait) = &mut this.wait {
            if wait.as_mut().poll(cx).is_ready() {
                this.timed_out = true;
                let what = if this.first { "first byte" } else { "idle" };
                return Poll::Ready(Some(Err(Box::<dyn Error + Send + Sync>::from(format!(
                    "upstream stream {what} timeout"
                )))));
            }
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // 收到数据：首字节窗口关闭，此后重置静默窗口
                this.first = false;
                this.wait = this.idle.map(|d| Box::pin(tokio::time::sleep(d)));
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.first = false;
                this.wait = this.idle.map(|d| Box::pin(tokio::time::sleep(d)));
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

/// 从 AppConfig 解析流式超时（0 = 禁用该阶段）
fn stream_timeouts(st: &AppState) -> (Option<Duration>, Option<Duration>) {
    let first_byte = (st.cfg.stream_first_byte_timeout_secs > 0)
        .then_some(Duration::from_secs(st.cfg.stream_first_byte_timeout_secs));
    let idle = (st.cfg.stream_idle_timeout_secs > 0)
        .then_some(Duration::from_secs(st.cfg.stream_idle_timeout_secs));
    (first_byte, idle)
}

/// 心跳包装流：上游连续静默超过 `interval` 时向下游产出一条 `: ping` SSE
/// 注释行（不进入转换器状态机，客户端解析器忽略）。四-2：Chat→X 流式链路
/// 双方均无心跳，长思考静默期只能靠客户端自身超时容忍 —— 网关补发保活。
pub fn with_ping_keepalive<S>(
    inner: S,
    interval: Duration,
) -> impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send
where
    S: Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send + 'static,
{
    PingKeepaliveStream {
        inner: Box::pin(inner),
        interval,
        wait: Box::pin(tokio::time::sleep(interval)),
        done: false,
    }
}

struct PingKeepaliveStream<S> {
    inner: Pin<Box<S>>,
    interval: Duration,
    wait: Pin<Box<tokio::time::Sleep>>,
    done: bool,
}

impl<S> Stream for PingKeepaliveStream<S>
where
    S: Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>>,
{
    type Item = Result<Bytes, Box<dyn Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                // 事件流动：重置心跳窗口
                this.wait
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.interval);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if this.wait.as_mut().poll(cx).is_ready() {
                    this.wait
                        .as_mut()
                        .reset(tokio::time::Instant::now() + this.interval);
                    Poll::Ready(Some(Ok(Bytes::from_static(b": ping\n\n"))))
                } else {
                    Poll::Pending
                }
            }
        }
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
    /// 是否 /v1/messages 端点（Anthropic Messages → Chat 降级转换；
    /// api_type=anthropic 的上游透传）
    pub anthropic: bool,
}

/// 客户端方言 × 上游方言配对支持性（H5）。
/// 网关当前只有「X → Chat」降级与原生透传两类能力：
/// - Responses 客户端 + Anthropic 上游：需管线③（未移植）→ 不支持
/// - Anthropic 客户端 + Responses 上游：需管线④（未移植）→ 不支持
/// - Chat 客户端 + 非 Chat 上游：需 Chat→X 转换（cc-switch 亦无）→ 不支持
/// 不支持的交集显式拒绝（400），不再静默转发到不存在的端点（404/静默坏）。
fn pairing_unsupported(endpoint: Endpoint, provider: &Provider) -> Option<String> {
    let native_responses = provider.api_type.eq_ignore_ascii_case("openai-responses");
    let native_anthropic = provider.api_type.eq_ignore_ascii_case("anthropic");
    if endpoint.anthropic {
        if native_responses {
            return Some(
                "anthropic client (/v1/messages) → openai-responses upstream is not supported"
                    .into(),
            );
        }
    } else if endpoint.responses {
        if native_anthropic {
            return Some(
                "responses client (/v1/responses) → anthropic upstream is not supported".into(),
            );
        }
    } else if native_responses || native_anthropic {
        return Some("chat completions client → non-chat upstream is not supported".into());
    }
    None
}

/// 工具归一化：保留 function / custom 工具（H1：custom 是用户自定义工具，转换层
/// 会降级为 function；此前被误剥离导致自定义工具全链路断裂）。codex/gpt-5 客户端
/// 声明的 `web_search` 等内置 hosted 工具继续剥离（DeepSeek 等上游反序列化 400）。
fn normalize_tools(json: &mut Value) -> bool {
    let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let before = tools.len();
    tools.retain(|t| {
        matches!(
            t.get("type").and_then(|x| x.as_str()),
            None | Some("function") | Some("custom")
        )
    });
    before != tools.len()
}

/// OpenAI 兼容归一化：gpt-5/codex 等客户端发送 `developer` role，多数上游
/// （DeepSeek 等）不支持 → 改写为 `system`；codex 内部 `latest_reminder` →
/// `user`（L1，cc-switch transform_codex_chat.rs:945 同款）。同时覆盖 Chat
/// `messages[]` 与 Responses `input[]` 两种格式。返回是否发生改写（决定透传
/// 分支是否需重序列化）。
fn normalize_chat_roles(json: &mut Value) -> bool {
    let mut changed = false;
    if let Some(arr) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for m in arr.iter_mut() {
            match m.get("role").and_then(|r| r.as_str()) {
                Some("developer") => {
                    m["role"] = Value::String("system".into());
                    changed = true;
                }
                Some("latest_reminder") => {
                    m["role"] = Value::String("user".into());
                    changed = true;
                }
                _ => {}
            }
        }
    }
    if let Some(arr) = json.get_mut("input").and_then(|m| m.as_array_mut()) {
        for item in arr.iter_mut() {
            if item.get("type").and_then(|t| t.as_str()) != Some("message") {
                continue;
            }
            match item.get("role").and_then(|r| r.as_str()) {
                Some("developer") => {
                    item["role"] = Value::String("system".into());
                    changed = true;
                }
                Some("latest_reminder") => {
                    item["role"] = Value::String("user".into());
                    changed = true;
                }
                _ => {}
            }
        }
    }
    changed
}

/// extra_body 深合并的网关保留字段：模型名由路由映射管理、流式语义由
/// 网关记账管理（stream_options.include_usage），配置中出现即被忽略（写入端同样拒绝）
const EXTRA_MANAGED_KEYS: [&str; 3] = ["model", "stream", "stream_options"];

/// 深合并：src 覆盖 dst 同名键；双方均为对象时递归合并（保留双方非冲突键），
/// 数组/标量直接替换。任一侧非对象则不合并。
fn merge_deep(dst: &mut Value, src: &Value) {
    let (Some(d), Some(s)) = (dst.as_object_mut(), src.as_object()) else {
        return;
    };
    for (k, sv) in s {
        match d.get_mut(k) {
            Some(dv) if dv.is_object() && sv.is_object() => merge_deep(dv, sv),
            _ => {
                d.insert(k.clone(), sv.clone());
            }
        }
    }
}

/// 计算某次转发的有效 extra_body：渠道级为底、路由级（模型级）覆盖同名叶键；
/// 剥离网关保留字段；结果为空对象 → None（出站保持原始字节直传快路径）
fn effective_extra(route_extra: &Value, provider_extra: &Value) -> Option<Value> {
    let mut merged = provider_extra.clone();
    merge_deep(&mut merged, route_extra);
    let obj = merged.as_object_mut()?;
    for k in EXTRA_MANAGED_KEYS {
        obj.remove(k);
    }
    (!obj.is_empty()).then_some(merged)
}

/// L1：模型级开关启用时，把 Chat 请求 `messages` 中全部 system 消息收拢到
/// 头部（多条时按 `merge` 决定拼接为单条或保持多条独立）。默认关闭，
/// 不影响常规上游。返回是否改写。
fn apply_system_head(body: &mut serde_json::Value, enabled: bool, merge: bool) -> bool {
    if !enabled {
        return false;
    }
    match body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(messages) => crate::service::system_head::collect_system_to_head(messages, merge),
        None => false,
    }
}

/// M9：非流式上游响应体读取上限（cc-switch hyper_client MAX_RESPONSE_BODY_BYTES 同款）
const MAX_UPSTREAM_BODY_BYTES: usize = 128 * 1024 * 1024;

/// 带上限的响应体读取：超出上限报错（防恶意上游 OOM 网关）
async fn read_body_capped(
    resp: reqwest::Response,
    cap: usize,
) -> Result<Bytes, Box<dyn Error + Send + Sync>> {
    let mut total = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("upstream body read failed: {e}").into()
        })?;
        total = total.saturating_add(chunk.len());
        if total > cap {
            return Err(format!("upstream response body exceeds {cap} bytes").into());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// L8：递归剥离 `_` 前缀私有字段（cc-switch body_filter.rs:68-133 同款；
/// 深度上限 32 防畸形自引用体无限递归）。
/// M23：JSON Schema 名字图（properties/patternProperties/definitions/$defs）
/// 内的键为属性名豁免——工具 schema `parameters.properties._id` 不再被腐蚀
///（cc-switch matches_schema_name_map :97-136 同款）
fn strip_underscore_fields(v: &mut Value) {
    strip_underscore_at_depth(v, 0, false);
}

fn strip_underscore_at_depth(v: &mut Value, depth: usize, in_schema_map: bool) {
    if depth > 32 {
        return;
    }
    match v {
        Value::Object(map) => {
            if !in_schema_map {
                map.retain(|k, _| !k.starts_with('_'));
            }
            for (k, val) in map.iter_mut() {
                strip_underscore_at_depth(
                    val,
                    depth + 1,
                    matches!(
                        k.as_str(),
                        "properties" | "patternProperties" | "definitions" | "$defs"
                    ),
                );
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_underscore_at_depth(item, depth + 1, in_schema_map);
            }
        }
        _ => {}
    }
}

/// 是否存在 `_` 前缀字段（快路径是否需要重序列化的判定；与 strip 同豁免语义）
fn has_underscore_fields(v: &Value) -> bool {
    has_underscore_at_depth(v, 0, false)
}

fn has_underscore_at_depth(v: &Value, depth: usize, in_schema_map: bool) -> bool {
    if depth > 32 {
        return false;
    }
    match v {
        Value::Object(map) => {
            (!in_schema_map && map.keys().any(|k| k.starts_with('_')))
                || map.iter().any(|(k, val)| {
                    has_underscore_at_depth(
                        val,
                        depth + 1,
                        matches!(
                            k.as_str(),
                            "properties" | "patternProperties" | "definitions" | "$defs"
                        ),
                    )
                })
        }
        Value::Array(items) => items
            .iter()
            .any(|item| has_underscore_at_depth(item, depth + 1, in_schema_map)),
        _ => false,
    }
}
/// H3：路由级 reasoning_effort 钳制模式应用到出站体。
/// M9：无配置也必须跑一遍——转换层下发的显式关闭标记 "none" 需统一移除
///（openrouter 模式下忠实转发为 reasoning:{effort:none}），防枚举 400。
fn apply_effort_mode(
    body: &mut Value,
    route: &crate::store::upstream::ModelRoute,
    gating_model: &str,
) {
    let mode = route.reasoning_effort_mode.as_deref().unwrap_or("");
    // P1-10：zen 模式按 gating model 查档位表（GATEWAY_ZEN_EFFORT_TABLE）
    crate::service::model_family::apply_reasoning_effort_mode_for_model(
        body,
        mode,
        Some(gating_model),
    );
}

/// H3：effort 钳制 + thinking 形态统一应用（所有出站 Chat 体路径共用）
fn apply_reasoning_config(
    body: &mut Value,
    route: &crate::store::upstream::ModelRoute,
    gating_model: &str,
) {
    apply_effort_mode(body, route, gating_model);
    crate::service::model_family::apply_thinking_form(body, route.thinking_form.as_deref());
}

/// H2：Responses 方言专属字段默认剥离（cc-switch EXTRA_CHAT_PASSTHROUGH_FIELDS
/// 白名单制——这些字段仅对 Responses 方言有意义，严格 Chat 上游对未知字段
/// 400）；路由级 `responses_passthrough_fields` 白名单可逐字段透传。
const RESPONSES_DIALECT_ONLY_FIELDS: [&str; 4] = [
    "store",
    "safety_identifier",
    "prompt_cache_retention",
    "prompt_cache_key",
];

fn apply_dialect_field_gate(body: &mut Value, route: &crate::store::upstream::ModelRoute) {
    let allowed = route.passthrough_field_list();
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for field in RESPONSES_DIALECT_ONLY_FIELDS {
        if !allowed.iter().any(|a| a == field) {
            obj.remove(field);
        }
    }
}

/// L3：客户端会话 id（cc-switch session.rs 同款提取链，日志/用量关联用）
fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    for name in ["x-claude-code-session-id", "session_id", "x-grok-conv-id"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.chars().take(128).collect());
            }
        }
    }
    None
}

/// P1-8：Anthropic `metadata.user_id` 的 `_session_` 后缀 → 会话 id
///（cc-switch session.rs:202-215 同款；仅 /v1/messages 请求体适用）
fn session_id_from_metadata(json: &Value, anthropic_endpoint: bool) -> Option<String> {
    if !anthropic_endpoint {
        return None;
    }
    let user_id = json
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())?;
    let (_, session) = user_id.rsplit_once("_session_")?;
    let session = session.trim();
    if session.is_empty() {
        return None;
    }
    Some(session.chars().take(128).collect())
}

/// P1-2：流式请求强制 `stream_options.include_usage = true`（合并语义，
/// cc-switch inject_openai_stream_include_usage transform.rs:253-268 同款）——
/// 客户端已带 stream_options（含 include_usage:false 或仅其他键）时不再丢失
/// usage 捕获（此前 is_none() 才整体注入，显式 false 会导致记账 0 token）。
fn ensure_include_usage(body: &mut Value, streamed: bool) {
    if !streamed {
        return;
    }
    match body.get_mut("stream_options") {
        Some(Value::Object(opts)) => {
            opts.insert("include_usage".into(), Value::Bool(true));
        }
        _ => {
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }
    }
}

/// L3：活跃请求计数 guard（进入代理管线 +1，结束/异常 -1；状态页暴露）
struct ActiveRequestGuard(std::sync::Arc<std::sync::atomic::AtomicI64>);

impl ActiveRequestGuard {
    fn enter(counter: &std::sync::Arc<std::sync::atomic::AtomicI64>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// OpenAI 兼容代理入口：鉴权 → 管线
pub async fn proxy(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
) -> Result<Response, AppError> {
    let _active = ActiveRequestGuard::enter(&st.active_requests);
    // P1-6：request_id 前移到入口——鉴权/限流/配额/解析等早期失败也能关联，
    // 且下游转发沿用同一 id（日志-响应头一致）。错误统一附 X-Request-Id 头。
    let request_id = Uuid::new_v4();
    let auth = crate::service::auth::authenticate(st, &headers).await;
    let (user_id, key_id) = match auth {
        Ok(v) => v,
        Err(e) => return Err(fail_with_request_id(e, request_id)),
    };
    proxy_authed(
        st, headers, body, endpoint, client_ip, user_id, key_id, false, request_id,
    )
    .await
    .map_err(|e| fail_with_request_id(e, request_id))
}

/// P1-6：早期失败日志 + 请求 id 包装（响应头由 IntoResponse 附带）
fn fail_with_request_id(e: AppError, request_id: Uuid) -> AppError {
    tracing::warn!(request_id = %request_id, error = %e, "proxy request failed before a response was produced");
    e.with_request_id(request_id)
}

/// 管理员测试入口：跳过鉴权与用户/Key 限流上下文，且不记账
///（test_call → 不写 usage_logs，不污染状态页错误率与用量统计）
pub async fn proxy_test(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
) -> Result<Response, AppError> {
    let _active = ActiveRequestGuard::enter(&st.active_requests);
    let request_id = Uuid::new_v4();
    proxy_authed(
        st, headers, body, endpoint, client_ip, None, None, true, request_id,
    )
    .await
    .map_err(|e| fail_with_request_id(e, request_id))
}
fn apply_extra(body: &mut Value, extra: Option<&Value>) {
    let Some(extra) = extra else { return };
    let Some(dst) = body.as_object_mut() else {
        return;
    };
    let Some(src) = extra.as_object() else { return };
    for (k, sv) in src {
        if EXTRA_MANAGED_KEYS.contains(&k.as_str()) {
            continue;
        }
        match dst.get_mut(k) {
            Some(dv) if dv.is_object() && sv.is_object() => merge_deep(dv, sv),
            _ => {
                dst.insert(k.clone(), sv.clone());
            }
        }
    }
}

/// 已鉴权代理管线：端点开关 → 限流 → 配额 → 路由 → 转发（含降级）→ 记账
async fn proxy_authed(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
    user_id: Option<i64>,
    key_id: Option<i64>,
    test_call: bool,
    request_id: Uuid,
) -> Result<Response, AppError> {
    let started = Instant::now();

    // 0. API 端点开关：管理员可在控制台停用单个 API；另一 API 不受影响。
    // 真实请求与测试请求都走此检查（测试停用状态本身是合法的测试意图）
    {
        let eps = st.api_endpoints.read();
        if endpoint.responses && !eps.responses_enabled {
            return Err(AppError::ServiceUnavailable(
                "the Responses API (/v1/responses) is currently disabled by the administrator"
                    .into(),
            ));
        }
        if endpoint.anthropic && !eps.messages_enabled {
            return Err(AppError::ServiceUnavailable(
                "the Anthropic Messages API (/v1/messages) is currently disabled by the administrator"
                    .into(),
            ));
        }
    }

    // 2. 解析请求体（M24：先按 Content-Encoding 解压，再解析 JSON；
    //    后做 role 归一化，所有下游分支共用改写后的 json；
    //    Anthropic 方言跳过 OpenAI 归一化）
    let mut json: Value = {
        let decompressed = decompress_request_body(
            &body,
            headers
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
        )
        .map_err(AppError::BadRequest)?;
        serde_json::from_slice(&decompressed)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON: {e}")))?
    };
    let mut roles_rewritten = false;
    // P0-6：请求带 Content-Encoding（gzip/br/zstd…）时零改写快路径不能直传
    // 原始压缩字节（上游按 Content-Type: application/json 解析必失败）→ 记录
    // 并强制走重序列化路径
    let had_content_encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| !v.trim().is_empty() && !v.eq_ignore_ascii_case("identity"))
        .unwrap_or(false);
    if !endpoint.anthropic {
        roles_rewritten |= normalize_chat_roles(&mut json);
        // P0-3：仅 Chat 客户端在代理层剥离 hosted 工具。Responses 客户端：
        // - 转换路径（Chat 上游）：hosted 工具由 convert_request 处理——
        //   web_search 桥接此前被前置剥离恒不触发；其余 hosted 类型在转换层丢弃
        // - 原生 openai-responses 透传：hosted 工具逐字保留
        //（cc-switch transform_codex_responses_xai_sanitize.rs 注释同款语义）
        if !endpoint.responses {
            roles_rewritten |= normalize_tools(&mut json);
        }
    }
    let mut model: String = json
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError::BadRequest("missing 'model' field".into()))?;
    let streamed = json
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // 3. 三层限流（模型限定规则按客户端模型名精确匹配，独立桶计量；
    //    置于模型解析后执行——缺 model 字段/坏 JSON 的请求在上方解析即 400）
    crate::service::ratelimit::apply_rate_limits(st, user_id, key_id, &model)?;

    // 4b. Coding Plan 配额：超出按策略拦截(429)/降级改写模型/仅告警放行。
    // 降级改写 json["model"] 并标记 plan_downgraded——零改写快路径直传原始
    // 客户端字节，必须强制走重建路径才能让改写生效。
    // decision.plan 即检查期按当前模型解析出的生效 Plan（模型作用域感知），
    // 直接作为记账载荷——避免下方二次解析在作用域过滤下漏记/错记。
    let mut plan_downgraded = false;
    let mut active_plan: Option<crate::store::plans::PlanRuntime> = None;
    if let Some(uid) = user_id {
        usage::check_quota(st, uid)?;
        let decision = crate::service::plans::check_plan(st, uid, &model)?;
        active_plan = decision.plan;
        if let Some(downgraded) = decision.downgrade_to {
            tracing::info!(user_id = uid, from = %model, to = %downgraded, "plan overage: downgraded model");
            json["model"] = serde_json::Value::String(downgraded.clone());
            model = downgraded;
            plan_downgraded = true;
        }
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
    // H5：方言配对校验 —— 过滤掉客户端×上游不可行的候选；全部不可行 → 显式 400
    let (supported, unsupported): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|p| pairing_unsupported(endpoint, p).is_none());
    if supported.is_empty() {
        let reason = unsupported
            .first()
            .and_then(|p| pairing_unsupported(endpoint, p))
            .unwrap_or_default();
        return Err(AppError::BadRequest(format!(
            "no provider supports this client/upstream dialect pairing for model '{model}': {reason}"
        )));
    }
    if !unsupported.is_empty() {
        tracing::warn!(
            model = %model,
            skipped = unsupported.len(),
            "skipping candidates with unsupported dialect pairing"
        );
    }
    // M3：熔断器过滤放行窗口外的候选；全部熔断时回退全量（避免整体 502 倒挂）。
    // H7：回退后循环内不再二次检查（此前全开回退被循环内 allow() 逐个击穿，
    // 最终 500 "all upstream attempts failed"——cc-switch 全开 → 显式 503 语义）
    let (candidates, breaker_bypassed) = match st.breaker.filter_candidates(&supported, |p| p.id) {
        Some(list) => (list, false),
        None => {
            tracing::warn!(model = %model, "all candidates circuit-open; falling back to full set");
            (supported, true)
        }
    };
    // H7/M32：单候选链旁路熔断检查（cc-switch forwarder.rs:442-443 同款——
    // 熔断器不该阻塞唯一渠道的全部请求）
    let check_breaker = !breaker_bypassed && candidates.len() > 1;
    let extra_enabled = *st.extra_body_enabled.read();
    let gating_model: String = route
        .upstream_model
        .clone()
        .unwrap_or_else(|| model.clone());
    // P1-20：Claude Code 长上下文 `[1m]` 后缀剥离（路由未配置映射时防上游
    // 404/400；cc-switch model_mapper.rs:163-186 同款）
    let outbound_model: String = route.upstream_model.clone().unwrap_or_else(|| {
        model
            .strip_suffix("[1m]")
            .map(str::to_string)
            .unwrap_or_else(|| model.clone())
    });
    // P1-3：客户端模型带 `[1m]` 后缀（无论是否被剥离映射）→ 原生 Anthropic
    // 上游需要 context-1m-2025-08-07 beta 才真正启用 1M 上下文
    let wants_1m = model.ends_with("[1m]") || outbound_model.ends_with("[1m]");
    // L1：system 收拢的最终裁决——路由开关优先，MiniMax 模型族强制收拢
    //（多条 system 必 400，合并为单条是唯一可行形态）；merge 决定多条
    // system 收拢后拼接为单条（MiniMax 强制 true）还是保持多条独立前移
    //（qwen3 类「system must be at the beginning」上游）
    let system_head =
        route.strict_system_head || crate::service::model_family::is_mini_max(&gating_model);
    let system_head_merge =
        route.system_head_merge || crate::service::model_family::is_mini_max(&gating_model);
    let build_outbound_raw = |provider: &Provider| -> Result<(Vec<u8>, &'static str), AppError> {
        // extra_body 透传：渠道级 + 路由级（模型级覆盖同名叶键）深合并进出站体；
        // 全局开关关闭时保留配置但不合并（临时停用不丢配置）
        let extra = if extra_enabled {
            effective_extra(&route.extra_body, &provider.extra_body)
        } else {
            None
        };
        // B-anthropic：/v1/messages + 非原生 anthropic 上游 → Anthropic 请求转 Chat
        //（system/tool_use/tool_result/thinking 映射见 service::anthropic::transform）
        if crate::service::anthropic::should_convert(endpoint.anthropic, provider) {
            let mut chat = crate::service::anthropic::convert_request(
                &json,
                &gating_model,
                &provider.base_url,
            )
            .map_err(AppError::BadRequest)?;
            ensure_include_usage(&mut chat, streamed);
            apply_extra(&mut chat, extra.as_ref());
            // P1-7：MiniMax 系模型强制收拢 system 到头部（多条 system 400 防护，
            // cc-switch 无条件收拢的具名动机；其余上游维持路由开关语义）
            apply_system_head(&mut chat, system_head, system_head_merge);
            apply_reasoning_config(&mut chat, &route, &gating_model);
            return Ok((
                serde_json::to_vec(&chat).map_err(AppError::internal)?,
                "/chat/completions",
            ));
        }
        let converted = crate::service::responses::should_convert(endpoint.responses, provider);
        if converted {
            // B：降级转换 Responses 请求 → Chat 请求（上游只支持 /v1/chat/completions）
            // P0-1：previous_response_id 桥接——命中网关历史则把先前 output items
            // 前插进 input（原生 openai-responses 透传不走此分支，引用原样转发）
            let mut enriched = json.clone();
            crate::service::responses::history::enrich_request_with_history(
                &st.responses_history,
                &mut enriched,
            )
            .map_err(AppError::BadRequest)?;
            let mut chat = crate::service::responses::convert_request(&enriched, &gating_model)
                .map_err(AppError::BadRequest)?;
            ensure_include_usage(&mut chat, streamed);
            apply_extra(&mut chat, extra.as_ref());
            apply_system_head(&mut chat, system_head, system_head_merge);
            apply_dialect_field_gate(&mut chat, &route);
            apply_reasoning_config(&mut chat, &route, &gating_model);
            chat["model"] = serde_json::Value::String(outbound_model.clone());
            Ok((
                serde_json::to_vec(&chat).map_err(AppError::internal)?,
                "/chat/completions",
            ))
        } else if streamed && !endpoint.responses && !endpoint.anthropic {
            let mut json = json.clone();
            ensure_include_usage(&mut json, streamed);
            apply_extra(&mut json, extra.as_ref());
            apply_system_head(&mut json, system_head, system_head_merge);
            apply_reasoning_config(&mut json, &route, &gating_model);
            // L8：剥离客户端 `_` 前缀私有字段（cc-switch body_filter 同款）
            strip_underscore_fields(&mut json);
            json["model"] = serde_json::Value::String(outbound_model.clone());
            Ok((
                serde_json::to_vec(&json).map_err(AppError::internal)?,
                endpoint.upstream,
            ))
        } else {
            // extra_body 合并 / role 归一化 / 模型名映射 / 私有字段剥离 → 需重建请求体；
            // 全都未发生时保持原始字节直传（零改写快路径）
            let needs_rebuild = extra.is_some()
                || roles_rewritten
                || had_content_encoding
                || system_head
                || outbound_model != model
                || has_underscore_fields(&json)
                // Plan 降级改写了 json["model"]：快路径直传原始字节会绕过改写
                || plan_downgraded;
            if needs_rebuild {
                let mut json = json.clone();
                apply_extra(&mut json, extra.as_ref());
                apply_system_head(&mut json, system_head, system_head_merge);
                apply_reasoning_config(&mut json, &route, &gating_model);
                strip_underscore_fields(&mut json);
                json["model"] = serde_json::Value::String(outbound_model.clone());
                Ok((
                    serde_json::to_vec(&json).map_err(AppError::internal)?,
                    endpoint.upstream,
                ))
            } else {
                Ok((body.to_vec(), endpoint.upstream))
            }
        }
    };
    let build_outbound = |provider: &Provider| -> Result<(Vec<u8>, &'static str), AppError> {
        let (bytes, upstream_path) = build_outbound_raw(provider)?;
        // H6：渠道声明不支持图像 → 发前主动降级图片 part 为文本标记
        //（纯文本上游不再每轮首请求必失败一次；rectify 反应式路径保留兜底）
        if !provider.supports_images {
            if let Some(stripped) = crate::service::rectify::strip_media_outbound(&bytes) {
                tracing::debug!(provider = %provider.name, "proactively stripped media from outbound body");
                return Ok((stripped, upstream_path));
            }
        }
        Ok((bytes, upstream_path))
    };

    // Coding Plan 计量载荷：检查期已按当前模型解析（模型作用域感知），
    // 此处仅换算周期窗口；None = 该用户此刻无生效 Plan（不限额不计量）
    let plan_bill = active_plan.map(|rt| {
        let now = chrono::Utc::now();
        crate::store::usage::PlanBill {
            plan_id: rt.plan_id,
            period_start: rt.period_start(now),
            period_key: rt.period_key(now),
        }
    });

    let mut meta = UsageMeta {
        request_id,
        user_id,
        api_key_id: key_id,
        model: model.clone(),
        provider_id: candidates[0].id,
        endpoint: endpoint.api,
        streamed,
        client_ip: Some(client_ip),
        // P1-8：header 缺失时回退解析 Anthropic metadata.user_id 的 `_session_`
        // 后缀（cc-switch session.rs:202-215 同款；Claude Code 无 header 场景）
        session_id: extract_session_id(&headers)
            .or_else(|| session_id_from_metadata(&json, endpoint.anthropic)),
        first_token_ms: None,
        test_call,
        // M33：计价候选 = 客户端模型名 → 上游映射模型名（依次查价）
        pricing_models: std::iter::once(model.clone())
            .chain(route.upstream_model.clone())
            .collect(),
        plan: plan_bill,
    };

    // 7. 转发 + 记账
    // P0-4：请求工具上下文（custom / namespace / tool_search / web_search 桥接
    // 的响应侧还原依据；仅 Responses 客户端需要构建）
    let tool_ctx = if endpoint.responses {
        crate::service::responses::tool_ctx::ToolContext::from_request(&json)
    } else {
        Default::default()
    };
    // M4：降级链尝试上限（GATEWAY_MAX_ATTEMPTS 可配，默认主渠道 + 3 个降级；
    // cc-switch max_retries 可配同款——长链串行打满全渠道的尾延迟不可接受）
    let max_attempts = candidates.len().min(st.cfg.max_attempts.max(1) as usize);
    if streamed {
        // 流式：M7 —— 主上游失败/可切换状态码（429/5xx/408/401/403）时按降级链换下一候选；
        // 尚未向客户端写出任何字节（响应流在候选选定后才返回），无重复生成风险
        //（cc-switch 流式首块失败可重试同款）。选定候选后不再重试（防重复计费）。
        let mut last_failure: Option<(u16, Vec<u8>, String, UpstreamHeaders)> = None;
        // L5：实际最后失败的候选（尾段错误上下文归属；默认主候选）
        let mut last_failed: Option<&Provider> = None;
        let mut saw_timeout = false;
        let mut saw_connect_error = false;
        'candidates: for provider in candidates.iter().take(max_attempts) {
            // M4/H5：记账与错误归属当前实际候选（此前流式固定 candidates[0]，
            // 降级切换后渠道报表/错误上下文归属错渠道）
            meta.provider_id = provider.id;
            // M3/H7：熔断器跳过放行窗口外的候选（全开回退/单候选旁路时跳过检查）
            if check_breaker && !st.breaker.allow(provider.id) {
                tracing::warn!(provider = %provider.name, "circuit breaker open; skipping stream candidate");
                continue;
            }
            let anthropic_converted =
                crate::service::anthropic::should_convert(endpoint.anthropic, provider);
            let converted = crate::service::responses::should_convert(endpoint.responses, provider);
            let (outbound, upstream_path) = build_outbound(provider)?;
            let provider_key = match crate::crypto::decrypt(
                &provider.api_key_encrypted,
                &st.cfg.master_key,
            ) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "provider key decrypt failed");
                    st.breaker.record_failure(provider.id);
                    continue;
                }
            };
            let sent = send_with_rectify(
                st,
                provider,
                &provider_key,
                &outbound,
                Endpoint {
                    upstream: upstream_path,
                    ..endpoint
                },
                request_id,
                true,
                &headers,
                wants_1m,
            )
            .await;
            // P0-8/P0-9 整流未命中的 4xx：错误体已预读，直接按方言整形返回
            if let Ok(SentOutcome::PreRead {
                status,
                content_type,
                bytes,
                headers: rl,
            }) = &sent
            {
                let latency = started.elapsed().as_millis() as i64;
                st.breaker.record_success(provider.id);
                usage::record(st, &meta, None, status.as_u16(), latency).await;
                let ctx = error_context(provider, &model, request_id);
                if anthropic_converted {
                    return Ok(attach_upstream_headers(
                        Response::builder()
                            .status(if status.is_success() {
                                StatusCode::BAD_GATEWAY
                            } else {
                                *status
                            })
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("X-Request-Id", request_id.to_string())
                            .body(Body::from(
                                crate::service::anthropic::reshape_upstream_error(
                                    status.as_u16(),
                                    bytes,
                                    &ctx,
                                ),
                            ))
                            .map_err(AppError::internal)?,
                        rl,
                    ));
                }
                if converted {
                    return Ok(attach_upstream_headers(
                        Response::builder()
                            .status(
                                StatusCode::from_u16(status.as_u16())
                                    .unwrap_or(StatusCode::BAD_GATEWAY),
                            )
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("X-Request-Id", request_id.to_string())
                            .body(Body::from(
                                crate::service::responses::reshape_upstream_error_for_responses(
                                    status.as_u16(),
                                    bytes,
                                    &ctx,
                                ),
                            ))
                            .map_err(AppError::internal)?,
                        rl,
                    ));
                }
                // 透传（Chat / 原生 Responses / 原生 Anthropic 客户端）：原样回错误体
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(*status)
                        .header(header::CONTENT_TYPE, content_type.clone())
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from(bytes.to_vec()))
                        .map_err(AppError::internal)?,
                    rl,
                ));
            }
            let resp = match sent {
                Ok(SentOutcome::Response(r)) => r,
                Ok(SentOutcome::PreRead { .. }) => unreachable!("handled above"),
                Err(e) => {
                    // M26：超时 vs 连接错误分类（全部候选失败后 504 / 502 / 503）
                    if e.to_string().contains("header timeout") {
                        saw_timeout = true;
                    } else {
                        saw_connect_error = true;
                    }
                    tracing::warn!(provider = %provider.name, error = %e, "upstream stream attempt failed");
                    st.breaker.record_failure(provider.id);
                    last_failed = Some(provider);
                    continue;
                }
            };
            let status = resp.status();
            let content_type = content_type_of(&resp);
            // P0-7：白名单限流/退避头（供本候选各返回出口附加）
            let rl = rate_limit_headers(&resp);
            if is_stream_retryable(status) {
                let bytes = read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                tracing::warn!(provider = %provider.name, status = %status, "upstream stream retryable failure");
                last_failure = Some((status.as_u16(), bytes, content_type, rl.clone()));
                st.breaker.record_failure(provider.id);
                last_failed = Some(provider);
                continue;
            }
            // P0-2：熔断 success 复位延后——priming 窗口内错误（传输错误/错误
            // envelope/零产出断流）计失败并换家，不得先记 success 稀释熔断窗口
            // 非可重试状态 = 上游有响应（永久性错误也说明渠道活着）
            // H8 + P0-2：priming 预读（cc-switch forwarder.rs:2466-2546 同款语义，
            // 窗口从"仅首块"扩展到"首个产出性输出/合法终态"）：缓冲至多 256KiB，
            // 输出或终态 → 提交（缓冲块与剩余流拼接后进入转换/透传）。
            // 此前仅校验首个 chunk——"200 + 首块 role delta + 次块错误 envelope"
            // 的上游会把 failed 透传给客户端且熔断器不记故障
            let is_sse = content_type.contains("text/event-stream");
            let will_stream = is_sse || (!anthropic_converted && !converted);
            let mut resp = Some(resp);
            let mut stream_opt: Option<
                Pin<Box<dyn Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>,
            > = None;
            if will_stream {
                let (first_byte, idle) = stream_timeouts(st);
                let stream = Box::pin(with_stream_timeouts(
                    resp.take().expect("resp present").bytes_stream(),
                    first_byte,
                    idle,
                ));
                let mut stream = Box::pin(stream);
                let mut buffered: Vec<Bytes> = Vec::new();
                let mut buffered_bytes = 0usize;
                let mut primer = PrimeState::default();
                let mut committed = false;
                while !committed {
                    if buffered_bytes >= STREAM_PRIME_MAX_BYTES {
                        break;
                    }
                    match stream.as_mut().next().await {
                        Some(Ok(chunk)) => {
                            if buffered.is_empty() {
                                // L3：首块到达 → 首 token 延迟（cc-switch first_token_ms 同款）
                                meta.first_token_ms = Some(started.elapsed().as_millis() as i64);
                            }
                            // M1：错误 envelope 校验逐块进行（含 SSE data 帧内错误）
                            if let Some(msg) = first_chunk_error(&chunk) {
                                tracing::warn!(
                                    provider = %provider.name,
                                    "upstream stream error envelope within priming window; failing over"
                                );
                                last_failed = Some(provider);
                                last_failure = Some((
                                    502,
                                    upstream_error_envelope(&msg),
                                    "application/json".to_string(),
                                    Vec::new(),
                                ));
                                st.breaker.record_failure(provider.id);
                                continue 'candidates;
                            }
                            buffered_bytes += chunk.len();
                            let signal = primer.feed(&chunk);
                            buffered.push(chunk);
                            match signal {
                                // P0-2：帧内错误 envelope（多帧合并 chunk 的次帧）
                                // → 记失败走降级链（不把错误帧透传给转换器/客户端）
                                PrimeSignal::Error(msg) => {
                                    tracing::warn!(
                                        provider = %provider.name,
                                        "upstream stream error frame within priming window; failing over"
                                    );
                                    last_failed = Some(provider);
                                    last_failure = Some((
                                        502,
                                        upstream_error_envelope(&msg),
                                        "application/json".to_string(),
                                        Vec::new(),
                                    ));
                                    st.breaker.record_failure(provider.id);
                                    continue 'candidates;
                                }
                                PrimeSignal::Productive | PrimeSignal::Terminal => {
                                    committed = true;
                                }
                                PrimeSignal::Neutral => {}
                            }
                        }
                        Some(Err(e)) => {
                            let msg =
                                format!("upstream stream failed before productive output: {e}");
                            tracing::warn!(provider = %provider.name, error = %e, "stream priming transport error");
                            last_failed = Some(provider);
                            last_failure = Some((
                                502,
                                upstream_error_envelope(&msg),
                                "application/json".to_string(),
                                Vec::new(),
                            ));
                            st.breaker.record_failure(provider.id);
                            if e.to_string().contains("header timeout") {
                                saw_timeout = true;
                            } else {
                                saw_connect_error = true;
                            }
                            continue 'candidates;
                        }
                        None => {
                            let msg = if buffered.is_empty() {
                                "upstream stream ended before first byte".to_string()
                            } else {
                                "upstream stream ended before productive output".to_string()
                            };
                            tracing::warn!(provider = %provider.name, "{msg}");
                            last_failed = Some(provider);
                            last_failure = Some((
                                502,
                                upstream_error_envelope(&msg),
                                "application/json".to_string(),
                                Vec::new(),
                            ));
                            st.breaker.record_failure(provider.id);
                            continue 'candidates;
                        }
                    }
                }
                // 提交：熔断记成功 + 缓冲块与剩余流拼接（字节顺序不变）
                st.breaker.record_success(provider.id);
                stream_opt = Some(Box::pin(
                    futures_util::stream::iter(buffered.into_iter().map(Ok)).chain(stream),
                ));
            }
            if !will_stream {
                // 非流缓冲路径（200 非 SSE / 非 2xx JSON 体整形）：渠道有响应即健康
                st.breaker.record_success(provider.id);
            }
            let latency = started.elapsed().as_millis() as i64;
            // Anthropic 降级转换流（上游 Chat SSE → 客户端 Anthropic SSE）：
            // 转换器内部在 [DONE]/流尾收尾（message_delta/message_stop）并记账；
            // 仅 SSE content-type 才转换，非 SSE / 非 2xx 错误体缓冲后整形为
            // Anthropic 错误 JSON 返回（不伪装成流，cc-switch 错误方言整形同款）
            if anthropic_converted
                && status.is_success()
                && content_type.contains("text/event-stream")
            {
                let stream = crate::service::anthropic::stream::wrap_chat_stream_to_anthropic(
                    st.clone(),
                    meta.clone(),
                    stream_opt.take().expect("stream preread present"),
                    latency,
                    status.as_u16(),
                    request_id.to_string(),
                    model.clone(),
                );
                let stream = with_ping_keepalive(stream, STREAM_PING_INTERVAL);
                tracing::info!(
                    request_id = %request_id, model = %model, provider = %provider.name,
                    streamed = true, status = %status, "proxying anthropic stream (converted)"
                );
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        // P1-5：SSE 响应禁缓存（cc-switch handlers.rs:531-545 同款；
                        // 经缓存型中间代理部署时事件流不被存储）
                        .header(header::CACHE_CONTROL, "no-cache")
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from_stream(stream))
                        .map_err(AppError::internal)?,
                    &rl,
                ));
            }
            if anthropic_converted {
                // 非 2xx / 200 非 SSE：缓冲错误体（SSE 形态从预读流收集，
                // 防 resp 已被 preread 消费）整形为 Anthropic 错误 JSON 返回
                let bytes = collect_stream_or_body(&mut stream_opt, &mut resp).await;
                let response_status = if status.is_success() {
                    // 状态码统一（对齐 Responses 链路 502）：上游 200 + 不可转换体
                    // 属上游故障语义；422 会让客户端误判为请求错误重发同样的请求
                    StatusCode::BAD_GATEWAY
                } else {
                    status
                };
                tracing::warn!(
                    request_id = %request_id, status = %status,
                    content_type = %content_type, "anthropic stream upstream error"
                );
                usage::record(st, &meta, None, status.as_u16(), latency).await;
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(response_status)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from(
                            crate::service::anthropic::reshape_upstream_error(
                                status.as_u16(),
                                &bytes,
                                &error_context(provider, &model, request_id),
                            ),
                        ))
                        .map_err(AppError::internal)?,
                    &rl,
                ));
            }
            // 降级转换流（上游 Chat SSE → 客户端 Responses SSE）：
            // 转换器内部在 [DONE]/流尾补发终态事件并记账；仅 SSE content-type 才转换
            // （200 + 非 SSE 错误体走原 wrap_stream 透传，避免错误被吞成空 completed 响应）
            if converted && status.is_success() && content_type.contains("text/event-stream") {
                let stream = crate::service::responses::stream::wrap_chat_stream_to_responses(
                    st.clone(),
                    meta.clone(),
                    stream_opt.take().expect("stream preread present"),
                    latency,
                    status.as_u16(),
                    format!("resp_{request_id}"),
                    model.clone(),
                    tool_ctx.clone(),
                );
                // L14：链路 A 转换流同样发 ping 保活（与链路 B/透传路径对齐；
                // cc-switch 管线①② 双方原缺失，网关两侧统一）
                let stream = with_ping_keepalive(stream, STREAM_PING_INTERVAL);
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, content_type)
                        // P1-5：SSE 响应禁缓存
                        .header(header::CACHE_CONTROL, "no-cache")
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from_stream(stream))
                        .map_err(AppError::internal)?,
                    &rl,
                ));
            }
            if converted && status.is_success() {
                // 200 但响应体不是 SSE：上游用 JSON 报错（或忽略 stream 参数直接回了
                // 完整 Chat completion）。透传原始体会让 Responses SSE 客户端拿到无法
                let bytes =
                    read_body_capped(resp.take().expect("resp present"), MAX_UPSTREAM_BODY_BYTES)
                        .await
                        .unwrap_or_default();
                let usage = parse_usage(&bytes);
                usage::record(st, &meta, usage.as_ref(), status.as_u16(), latency).await;
                if let Some(sse) = crate::service::responses::stream::synthesize_sse_from_chat_body(
                    &bytes,
                    &format!("resp_{request_id}"),
                    &model,
                    &tool_ctx,
                ) {
                    tracing::warn!(
                        request_id = %request_id, content_type = %content_type,
                        "responses stream: upstream returned JSON body for stream request; synthesized SSE"
                    );
                    return Ok(attach_upstream_headers(
                        Response::builder()
                            .status(status)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            // P1-5：SSE 响应禁缓存
                            .header(header::CACHE_CONTROL, "no-cache")
                            .header("X-Request-Id", request_id.to_string())
                            .body(Body::from(sse))
                            .map_err(AppError::internal)?,
                        &rl,
                    ));
                }
                tracing::warn!(
                    request_id = %request_id, status = %status, content_type = %content_type,
                    "responses stream upstream returned non-SSE body"
                );
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from(
                            crate::service::responses::reshape_upstream_error_for_responses(
                                status.as_u16(),
                                &bytes,
                                &error_context(provider, &model, request_id),
                            ),
                        ))
                        .map_err(AppError::internal)?,
                    &rl,
                ));
            }
            if converted {
                // P0 关联修复：converted 路径的非 2xx（此前落入下方透传分支，
                // stream_opt 仅在 SSE 时创建 → JSON 错误体触发 expect panic）。
                // 整形为 Responses 错误 JSON 返回；SSE 形态错误体从预读流收集。
                let bytes = collect_stream_or_body(&mut stream_opt, &mut resp).await;
                usage::record(st, &meta, None, status.as_u16(), latency).await;
                tracing::warn!(
                    request_id = %request_id, status = %status,
                    content_type = %content_type, "responses stream upstream error"
                );
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(
                            StatusCode::from_u16(status.as_u16())
                                .unwrap_or(StatusCode::BAD_GATEWAY),
                        )
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from(
                            crate::service::responses::reshape_upstream_error_for_responses(
                                status.as_u16(),
                                &bytes,
                                &error_context(provider, &model, request_id),
                            ),
                        ))
                        .map_err(AppError::internal)?,
                    &rl,
                ));
            }
            // 非 2xx：不透传 usage 计费（错误体里的 usage 不可信），按真实状态记账、token 记 0
            let capture_usage = status.is_success();
            let upstream = stream_opt.take().expect("stream preread present");
            let stream = wrap_stream(
                st.clone(),
                meta,
                upstream,
                latency,
                status.as_u16(),
                capture_usage,
            );
            // 四-2：SSE 链路静默期向下游发 `: ping` 注释保活（长思考上游不中断连接）
            let stream = if content_type.contains("text/event-stream") {
                Body::from_stream(with_ping_keepalive(stream, STREAM_PING_INTERVAL))
            } else {
                Body::from_stream(stream)
            };
            tracing::info!(
                request_id = %request_id, model = %model, provider = %provider.name,
                streamed = true, status = %status, "proxying stream"
            );
            let mut builder = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type.clone())
                .header("X-Request-Id", request_id.to_string());
            // P1-5：SSE 响应禁缓存（非 SSE 字节流不加，保持语义精确）
            if content_type.contains("text/event-stream") {
                builder = builder.header(header::CACHE_CONTROL, "no-cache");
            }
            return Ok(attach_upstream_headers(
                builder.body(stream).map_err(AppError::internal)?,
                &rl,
            ));
        }

        // 全部候选失败/不可用：优先透传最后一次上游失败响应（按客户端方言整形）
        if let Some((status, bytes, content_type, rl)) = last_failure {
            let latency = started.elapsed().as_millis() as i64;
            usage::record(st, &meta, None, status, latency).await;
            // L5：错误上下文归属实际最后失败的候选（此前固定 candidates[0]）
            let ctx = error_context(last_failed.unwrap_or(&candidates[0]), &model, request_id);
            let (status_out, body, ct) = if endpoint.anthropic {
                (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    crate::service::anthropic::reshape_upstream_error(status, &bytes, &ctx),
                    "application/json".to_string(),
                )
            } else if endpoint.responses {
                (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    crate::service::responses::reshape_upstream_error_for_responses(
                        status, &bytes, &ctx,
                    ),
                    "application/json".to_string(),
                )
            } else {
                (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    bytes,
                    content_type,
                )
            };
            return Ok(attach_upstream_headers(
                Response::builder()
                    .status(status_out)
                    .header(header::CONTENT_TYPE, ct)
                    .header("X-Request-Id", request_id.to_string())
                    .body(Body::from(body))
                    .map_err(AppError::internal)?,
                &rl,
            ));
        }
        // M26：全部候选耗尽且无上游响应可透传 → 504（曾超时）/ 502（连接失败）/ 503（耗尽）
        if saw_timeout {
            Err(AppError::UpstreamTimeout(
                "all upstream stream attempts timed out".into(),
            ))
        } else if saw_connect_error {
            Err(AppError::BadGateway(
                "all upstream stream attempts failed at connection".into(),
            ))
        } else {
            Err(AppError::UpstreamExhausted(
                "all upstream stream attempts failed".into(),
            ))
        }
    } else {
        let mut last_response: Option<(u16, Vec<u8>, String, UpstreamHeaders)> = None;
        // L5：实际最后失败的候选（尾段错误上下文归属）
        let mut last_failed: Option<&Provider> = None;
        for provider in candidates.iter().take(max_attempts) {
            // M4：记账归属当前实际候选（与流式循环对齐）
            meta.provider_id = provider.id;
            // M3：熔断器跳过放行窗口外的候选（全部熔断已在候选过滤时回退）
            if check_breaker && !st.breaker.allow(provider.id) {
                tracing::warn!(provider = %provider.name, "circuit breaker open; skipping candidate");
                continue;
            }
            let anthropic_converted =
                crate::service::anthropic::should_convert(endpoint.anthropic, provider);
            let converted = crate::service::responses::should_convert(endpoint.responses, provider);
            let provider_key = match crate::crypto::decrypt(
                &provider.api_key_encrypted,
                &st.cfg.master_key,
            ) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "provider key decrypt failed");
                    st.breaker.record_failure(provider.id);
                    last_failed = Some(provider);
                    continue;
                }
            };
            let (outbound, upstream_path) = match build_outbound(provider) {
                Ok(v) => v,
                Err(e) => return Err(e),
            };
            let sent = send_with_rectify(
                st,
                provider,
                &provider_key,
                &outbound,
                Endpoint {
                    upstream: upstream_path,
                    ..endpoint
                },
                request_id,
                false,
                &headers,
                wants_1m,
            )
            .await;
            // P0-8/P0-9 整流未命中的 4xx：错误体已预读，直接按方言整形返回
            //（此前非流式循环对 PreRead 走 unreachable!——非流式 4xx 会 panic）
            if let Ok(SentOutcome::PreRead {
                status,
                content_type,
                bytes,
                headers: rl,
            }) = &sent
            {
                let latency = started.elapsed().as_millis() as i64;
                st.breaker.record_success(provider.id);
                usage::record(st, &meta, None, status.as_u16(), latency).await;
                let ctx = error_context(provider, &model, request_id);
                if anthropic_converted {
                    return Ok(attach_upstream_headers(
                        Response::builder()
                            .status(if status.is_success() {
                                StatusCode::BAD_GATEWAY
                            } else {
                                *status
                            })
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("X-Request-Id", request_id.to_string())
                            .body(Body::from(
                                crate::service::anthropic::reshape_upstream_error(
                                    status.as_u16(),
                                    bytes,
                                    &ctx,
                                ),
                            ))
                            .map_err(AppError::internal)?,
                        rl,
                    ));
                }
                if converted {
                    return Ok(attach_upstream_headers(
                        Response::builder()
                            .status(
                                StatusCode::from_u16(status.as_u16())
                                    .unwrap_or(StatusCode::BAD_GATEWAY),
                            )
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("X-Request-Id", request_id.to_string())
                            .body(Body::from(
                                crate::service::responses::reshape_upstream_error_for_responses(
                                    status.as_u16(),
                                    bytes,
                                    &ctx,
                                ),
                            ))
                            .map_err(AppError::internal)?,
                        rl,
                    ));
                }
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(*status)
                        .header(header::CONTENT_TYPE, content_type.clone())
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from(bytes.to_vec()))
                        .map_err(AppError::internal)?,
                    rl,
                ));
            }
            let resp = match sent {
                Ok(SentOutcome::Response(r)) => r,
                Ok(SentOutcome::PreRead { .. }) => unreachable!("handled above"),
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "upstream attempt failed");
                    st.breaker.record_failure(provider.id);
                    last_failed = Some(provider);
                    continue;
                }
            };
            let status = resp.status();
            let content_type = content_type_of(&resp);
            // P0-7：白名单限流/退避头（供本候选各返回出口附加）
            let rl = rate_limit_headers(&resp);
            if status_retryable(status) {
                // 可重试：记录本次响应后尝试降级（M3：计为熔断失败）
                let bytes = read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                tracing::warn!(provider = %provider.name, status = %status, "upstream retryable failure");
                last_response = Some((status.as_u16(), bytes, content_type, rl.clone()));
                st.breaker.record_failure(provider.id);
                last_failed = Some(provider);
                continue;
            }
            // M34：先完整读体再记 success（读体失败=可重试传输错误，熔断器不得
            // 误标健康；cc-switch forwarder.rs:2387-2396 同款）
            let bytes = match read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "upstream body read failed");
                    st.breaker.record_failure(provider.id);
                    last_failed = Some(provider);
                    continue;
                }
            };
            // 非可重试状态 = 上游有响应且体已完整读取 → 复位熔断
            st.breaker.record_success(provider.id);
            // M9：响应体读取上限 128MiB（cc-switch MAX_RESPONSE_BODY_BYTES 同款）
            let latency = started.elapsed().as_millis() as i64;
            // L11 / P1-15：上游违反 stream:false 回了 SSE 体 → 整形 502 而非把 SSE
            // 字节当 JSON 返回（content-type 说谎时按字节前缀嗅探兜底，
            // cc-switch handlers.rs:2285-2290 同款）
            if content_type.contains("text/event-stream") || looks_like_sse(&bytes) {
                tracing::warn!(
                    request_id = %request_id, status = %status,
                    "non-stream request received SSE body from upstream; reshaping to error"
                );
                usage::record(st, &meta, None, status.as_u16(), latency).await;
                let ctx = error_context(provider, &model, request_id);
                let body = if endpoint.anthropic {
                    crate::service::anthropic::reshape_upstream_error(
                        status.as_u16(),
                        b"upstream returned SSE body for non-stream request",
                        &ctx,
                    )
                } else if endpoint.responses {
                    crate::service::responses::reshape_upstream_error_for_responses(
                        status.as_u16(),
                        b"upstream returned SSE body for non-stream request",
                        &ctx,
                    )
                } else {
                    crate::service::responses::reshape_upstream_error_for_responses(
                        status.as_u16(),
                        b"upstream returned SSE body for non-stream request",
                        &ctx,
                    )
                };
                return Ok(attach_upstream_headers(
                    Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("X-Request-Id", request_id.to_string())
                        .body(Body::from(body))
                        .map_err(AppError::internal)?,
                    &rl,
                ));
            }
            let usage = parse_usage(&bytes);
            usage::record(st, &meta, usage.as_ref(), status.as_u16(), latency).await;
            let mut response_status = status;
            let ctx = error_context(provider, &model, request_id);
            let response_body = if anthropic_converted {
                // B-anthropic：上游 Chat 响应 → Anthropic 响应；错误体（含 200 + JSON
                // 错误）一律整形为 Anthropic 单错误对象形状（客户端方言，cc-switch 同款）
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(v) if status.is_success() && has_usable_choices(&v) => {
                        match crate::service::anthropic::convert_response(
                            &v,
                            &format!("msg_{request_id}"),
                        ) {
                            Ok(out) => serde_json::to_vec(&out).unwrap_or_else(|e| {
                                tracing::warn!(request_id = %request_id, error = %e, "anthropic convert serialize failed");
                                response_status = StatusCode::BAD_GATEWAY;
                                crate::service::anthropic::reshape_upstream_error(
                                    502,
                                    b"response serialize failed",
                                    &ctx,
                                )
                            }),
                            Err(e) => {
                                tracing::warn!(request_id = %request_id, error = %e, "anthropic convert failed");
                                response_status = StatusCode::BAD_GATEWAY;
                                crate::service::anthropic::reshape_upstream_error(502, e.as_bytes(), &ctx)
                            }
                        }
                    }
                    _ if status.is_success() => {
                        // 200 + 不可转换体（无 choices 的错误 JSON 等）：不得以 200 返回
                        // —— Anthropic SDK 会把错误体当 Message 解析成空消息（静默空
                        // 响应）；对齐 Responses 链路统一 502（上游故障语义；422 会让
                        // 客户端误判为请求错误重发同样的请求）
                        tracing::warn!(
                            request_id = %request_id,
                            "anthropic convert: upstream 200 body without usable choices; reshaping to error"
                        );
                        response_status = StatusCode::BAD_GATEWAY;
                        crate::service::anthropic::reshape_upstream_error(502, &bytes, &ctx)
                    }
                    _ => crate::service::anthropic::reshape_upstream_error(status.as_u16(), &bytes, &ctx),
                }
            } else if converted {
                // B：上游 Chat 响应 → Responses 响应。2xx 成功体转换；2xx 但无
                // 可用 choices 的异常体（含 200 + JSON 错误）不得透传 —— Responses
                // SDK 会把它当 Response 对象解析成全空输出（静默空响应），统一整形
                // 为 OpenAI 错误形状并以 502 返回；非 2xx 原样透传（状态码已表达错误）
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(v) if status.is_success() && has_usable_choices(&v) => {
                        match crate::service::responses::convert_response(
                            &v,
                            &format!("resp_{request_id}"),
                            chrono::Utc::now().timestamp(),
                            &tool_ctx,
                        ) {
                            Ok(out) => {
                                // P0-1：转换成功的响应 tee 进历史库（下一轮
                                // previous_response_id 引用据此恢复上下文）
                                if let Ok(value) = serde_json::to_value(&out) {
                                    if let (Some(id), Some(output)) = (
                                        value.get("id").and_then(|i| i.as_str()),
                                        value.get("output").cloned(),
                                    ) {
                                        st.responses_history.record(id, output);
                                    }
                                }
                                serde_json::to_vec(&out).unwrap_or_else(|e| {
                                    tracing::warn!(request_id = %request_id, error = %e, "responses convert serialize failed");
                                    bytes.to_vec()
                                })
                            }
                            Err(e) => {
                                tracing::warn!(request_id = %request_id, error = %e, "responses convert failed");
                                bytes.to_vec()
                            }
                        }
                    }
                    _ if status.is_success() => {
                        tracing::warn!(
                            request_id = %request_id,
                            "responses convert: upstream 200 body without usable choices; reshaping to error"
                        );
                        response_status = StatusCode::BAD_GATEWAY;
                        crate::service::responses::reshape_upstream_error_for_responses(
                            status.as_u16(),
                            &bytes,
                            &ctx,
                        )
                    }
                    _ => bytes.to_vec(),
                }
            } else {
                bytes.to_vec()
            };
            return Ok(attach_upstream_headers(
                Response::builder()
                    .status(response_status)
                    .header(header::CONTENT_TYPE, content_type)
                    .header("X-Request-Id", request_id.to_string())
                    .body(Body::from(response_body))
                    .map_err(AppError::internal)?,
                &rl,
            ));
        }
        // 全部候选失败：优先透传最后一次上游响应，否则 502
        if let Some((status, bytes, content_type, rl)) = last_response {
            let latency = started.elapsed().as_millis() as i64;
            let usage = parse_usage(&bytes);
            usage::record(st, &meta, usage.as_ref(), status, latency).await;
            // L5：错误上下文归属实际最后失败的候选（此前固定 candidates[0]）
            let ctx = error_context(last_failed.unwrap_or(&candidates[0]), &model, request_id);
            let body = if endpoint.anthropic {
                crate::service::anthropic::reshape_upstream_error(status, &bytes, &ctx)
            } else if endpoint.responses {
                crate::service::responses::reshape_upstream_error_for_responses(
                    status, &bytes, &ctx,
                )
            } else {
                bytes
            };
            let content_type = if endpoint.anthropic || endpoint.responses {
                "application/json".to_string()
            } else {
                content_type
            };
            return Ok(attach_upstream_headers(
                Response::builder()
                    .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
                    .header(header::CONTENT_TYPE, content_type)
                    .header("X-Request-Id", request_id.to_string())
                    .body(Body::from(body))
                    .map_err(AppError::internal)?,
                &rl,
            ));
        }
        // M26：全部候选耗尽且无上游响应可透传 → 503（cc-switch MaxRetriesExceeded）
        Err(AppError::UpstreamExhausted(
            "all upstream attempts failed".into(),
        ))
    }
}

/// M24：按请求 Content-Encoding 解压请求体（cc-switch handlers.rs:725-741 +
/// content_encoding.rs 同款）：gzip/x-gzip / deflate（zlib 容器优先，失败回退
/// raw）/ br / zstd / zst；identity/缺失直通；不支持编码返回错误信息（调用方 400）。
/// L1：逗号分隔的堆叠编码按逆序解码（cc-switch content_encoding.rs:126-131 同款），
/// 且解压输出受预算约束（防 zip bomb——解压后超限直接报错）。
fn decompress_request_body(body: &[u8], content_encoding: Option<&str>) -> Result<Vec<u8>, String> {
    let Some(enc) = content_encoding.map(|s| s.trim().to_ascii_lowercase()) else {
        return Ok(body.to_vec());
    };
    if enc.is_empty() || enc == "identity" {
        return Ok(body.to_vec());
    }
    let encodings: Vec<&str> = enc
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if encodings.is_empty() {
        return Ok(body.to_vec());
    }
    let mut data = body.to_vec();
    // 堆叠编码：应用顺序 a,b,c 表示原始数据先经 a 编码；还原需逆序 c→b→a
    for enc in encodings.iter().rev() {
        let mut out = Vec::with_capacity(data.len().min(MAX_DECOMPRESSED_BODY_BYTES) * 2);
        decode_single_coding(&data, enc, &mut out)?;
        data = out;
    }
    Ok(data)
}

/// L1：解压输出预算（防 zip bomb；客户端原始体 200MB 上限下留充足裕量）
const MAX_DECOMPRESSED_BODY_BYTES: usize = 512 * 1024 * 1024;

/// 单一编码解码到 out（带输出预算；超限报错）。`input` 为已按前置编码解出的数据。
fn decode_single_coding(input: &[u8], enc: &str, out: &mut Vec<u8>) -> Result<(), String> {
    use std::io::Read;
    let limited: &mut dyn Read = &mut input.take((MAX_DECOMPRESSED_BODY_BYTES + 1) as u64);
    match enc {
        "gzip" | "x-gzip" => flate2::read::GzDecoder::new(limited)
            .read_to_end(out)
            .map(|_| ())
            .map_err(|e| format!("gzip decode failed: {e}")),
        "deflate" => {
            // zlib 容器优先，失败回退 raw deflate（部分客户端发裸流）
            if flate2::read::ZlibDecoder::new(limited)
                .read_to_end(out)
                .is_err()
            {
                out.clear();
                let raw_limited: &mut dyn Read =
                    &mut input.take((MAX_DECOMPRESSED_BODY_BYTES + 1) as u64);
                flate2::read::DeflateDecoder::new(raw_limited)
                    .read_to_end(out)
                    .map(|_| ())
                    .map_err(|e| format!("deflate decode failed: {e}"))?;
            }
            Ok(())
        }
        "br" => brotli::Decompressor::new(limited, 4096)
            .read_to_end(out)
            .map(|_| ())
            .map_err(|e| format!("brotli decode failed: {e}")),
        "zstd" | "zst" => zstd::stream::read::Decoder::new(limited)
            .map_err(|e| format!("zstd decode init failed: {e}"))
            .and_then(|mut d| {
                d.read_to_end(out)
                    .map(|_| ())
                    .map_err(|e| format!("zstd decode failed: {e}"))
            }),
        other => Err(format!(
            "unsupported content-encoding: {other} (supported: gzip, deflate, br, zstd)"
        )),
    }?;
    if out.len() > MAX_DECOMPRESSED_BODY_BYTES {
        return Err(format!(
            "decompressed body exceeds {} bytes limit",
            MAX_DECOMPRESSED_BODY_BYTES
        ));
    }
    Ok(())
}

/// 流式/非流式共用的可重试状态码判定。M25：黑名单制——仅协议性/语义性错误
/// 不可重试（cc-switch categorize_proxy_error :2721-2723 同款集合），其余含
/// 404/409/402/451 等均可切换（换一家 provider 可能持有不同的 key、配额或模型映射）。
/// 2xx 成功恒不可重试（黑名单翻转引入的回归：200 被当作可重试失败，
/// 单候选时成功响应被尾段整形为 "unconvertible body" 错误）。
fn status_retryable(status: StatusCode) -> bool {
    if status.is_success() {
        return false;
    }
    !matches!(
        status.as_u16(),
        400 | 405 | 406 | 413 | 414 | 415 | 422 | 501
    )
}
/// 流式首块失败可切换判定（复用同一集合）
fn is_stream_retryable(status: StatusCode) -> bool {
    status_retryable(status)
}

/// 上游限流/退避头列表（白名单转发用）
type UpstreamHeaders = Vec<(axum::http::HeaderName, axum::http::HeaderValue)>;

/// 发送结果：正常响应，或「整流未命中的 4xx」——错误体已预读（响应对象已消费），
/// 携带状态/内容类型/限流头供调用方直接整形返回
enum SentOutcome {
    Response(reqwest::Response),
    PreRead {
        status: StatusCode,
        content_type: String,
        bytes: Bytes,
        headers: Vec<(axum::http::HeaderName, axum::http::HeaderValue)>,
    },
}

/// P0-8/P0-9：发送 + 反应式整流重试。4xx（400/413/415/422）时读错误体，
/// 命中媒体降级 / thinking 剥离（service::rectify）→ 修复出站体后同上游
/// 重发一次（cc-switch media_sanitizer / thinking_rectifier 反应式路径同款）；
/// 未命中 → PreRead 返回错误体。
/// H6：GATEWAY_RECTIFY_ENABLED=false 时整流整体关闭（4xx 错误体不再预读，
/// 由调用方常规错误路径处理；对畸形上游无意外重发副作用）。
async fn send_with_rectify(
    st: &AppState,
    provider: &Provider,
    provider_key: &str,
    outbound: &[u8],
    endpoint: Endpoint,
    request_id: Uuid,
    streamed: bool,
    client_headers: &HeaderMap,
    wants_1m: bool,
) -> Result<SentOutcome, Box<dyn Error + Send + Sync>> {
    let resp = send_upstream(
        st,
        provider,
        provider_key,
        outbound,
        endpoint,
        request_id,
        streamed,
        client_headers,
        wants_1m,
    )
    .await?;
    if !st.cfg.rectify_enabled
        || !crate::service::rectify::is_rectifiable_status(resp.status().as_u16())
    {
        return Ok(SentOutcome::Response(resp));
    }
    let status = resp.status();
    let content_type = content_type_of(&resp);
    let rl = rate_limit_headers(&resp);
    let bytes = read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES)
        .await
        .unwrap_or_default();
    let anthropic_native = provider.api_type.eq_ignore_ascii_case("anthropic");
    match crate::service::rectify::rectify_outbound(
        anthropic_native,
        outbound,
        &bytes,
        status.as_u16(),
    ) {
        Some(fixed) => {
            tracing::info!(
                provider = %provider.name, status = %status.as_u16(),
                "rectified outbound body (media downgrade / thinking strip); retrying same provider once"
            );
            let retried = send_upstream(
                st,
                provider,
                provider_key,
                &fixed,
                endpoint,
                request_id,
                streamed,
                client_headers,
                wants_1m,
            )
            .await?;
            Ok(SentOutcome::Response(retried))
        }
        None => Ok(SentOutcome::PreRead {
            status,
            content_type,
            bytes,
            headers: rl,
        }),
    }
}
/// P0-7/P1-5：上游响应头转发白名单（429 透传时客户端 SDK 据此退避；
/// cc-switch handlers.rs:1866-1876 复制上游头同款动机）：
/// - retry-after / x-ratelimit-* / anthropic-ratelimit-*：限流与退避
/// - openai-organization / openai-processing-ms：组织路由回显与上游耗时诊断
fn rate_limit_headers(
    resp: &reqwest::Response,
) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    resp.headers()
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n == "retry-after"
                || n == "openai-organization"
                || n == "openai-processing-ms"
                || n.starts_with("x-ratelimit-")
                || n.starts_with("anthropic-ratelimit-")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
/// 从「预读流（SSE 错误体）或未消费响应（JSON 错误体）」收集错误体字节
///（转换路径错误整形共用；防 resp 已被流式预读消费后的 expect panic）
async fn collect_stream_or_body(
    stream_opt: &mut Option<
        Pin<Box<dyn Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>,
    >,
    resp: &mut Option<reqwest::Response>,
) -> Vec<u8> {
    if let Some(mut stream) = stream_opt.take() {
        let mut buf = Vec::new();
        while let Some(chunk) = StreamExt::next(&mut stream).await {
            match chunk {
                Ok(b) => buf.extend_from_slice(&b),
                Err(_) => break,
            }
        }
        return buf;
    }
    match resp.take() {
        Some(r) => read_body_capped(r, MAX_UPSTREAM_BODY_BYTES)
            .await
            .unwrap_or_default()
            .to_vec(),
        None => Vec::new(),
    }
}

/// P1-15：字节前缀嗅探 SSE 体（content-type 说谎的上游；BOM/空白容忍）。
/// L2：前缀集对齐 cc-switch body_looks_like_sse（handlers.rs:2285-2292）——
/// data:/event:/id:/retry:/`:` 注释行
fn looks_like_sse(bytes: &[u8]) -> bool {
    let trimmed: &[u8] = {
        let mut s = bytes;
        while let Some((first, rest)) = s.split_first() {
            if matches!(first, b' ' | b'\t' | b'\r' | b'\n' | 0xEF) {
                s = rest;
            } else {
                break;
            }
        }
        s
    };
    trimmed.starts_with(b"data:")
        || trimmed.starts_with(b"event:")
        || trimmed.starts_with(b"id:")
        || trimmed.starts_with(b"retry:")
        || trimmed.starts_with(b":")
}

/// M3：错误整形用的结构化排障上下文（cc-switch codex_proxy_error_json 同款——
/// provider/model/request_id/upstream_status 独立字段，客户端可程序化排障；
/// message 内保留 ` [provider=…]` 后缀兼容既有日志检索）
#[derive(Debug, Clone, Copy)]
pub(crate) struct UpstreamErrorContext<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub request_id: Uuid,
}

impl<'a> UpstreamErrorContext<'a> {
    /// message 后缀（provider/model/request_id 进 message，M9）
    pub fn message_suffix(&self) -> String {
        format!(
            " [provider={}, model={}, request_id={}]",
            self.provider, self.model, self.request_id
        )
    }
}

fn error_context<'a>(
    provider: &'a Provider,
    model: &'a str,
    request_id: Uuid,
) -> UpstreamErrorContext<'a> {
    UpstreamErrorContext {
        provider: &provider.name,
        model,
        request_id,
    }
}

/// M3：上游错误体字节分类（cc-switch classify_body_for_diagnostics 同款；
/// 只分类不泄露内容，供结构化错误字段 body_type 用）
pub(crate) fn classify_upstream_body(body: &[u8]) -> &'static str {
    if body.is_empty() {
        return "empty";
    }
    if serde_json::from_slice::<Value>(body).is_ok() {
        return "json";
    }
    if looks_like_sse(body) {
        return "sse";
    }
    let head = &body[..body.len().min(512)];
    if !head
        .iter()
        .all(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
    {
        return "binary";
    }
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    if s.contains("<html") || s.contains("<!doctype") {
        "html"
    } else {
        "text"
    }
}

/// M1：流式首块是否携带错误 envelope（200 + 错误体不换家的修正）。
/// 识别 JSON 错误体（{"error":{…}}）与 Anthropic 方言（{"type":"error"}），
/// 以及 SSE `data:` 帧内嵌的同类错误。返回压缩后的错误消息；非错误 → None。
fn first_chunk_error(bytes: &[u8]) -> Option<String> {
    let trimmed: &[u8] = {
        let mut s = bytes;
        while let Some((first, rest)) = s.split_first() {
            if matches!(first, b' ' | b'\t' | b'\r' | b'\n' | 0xEF) {
                s = rest;
            } else {
                break;
            }
        }
        s
    };
    // SSE 注释行 / 空帧：跳过
    if trimmed.is_empty() || trimmed.starts_with(b":") {
        return None;
    }
    let payload: &[u8] = if let Some(rest) = trimmed.strip_prefix(b"data:") {
        // 取首个 data 行（到 \n 为止）
        let line_end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        &rest[..line_end]
    } else {
        trimmed
    };
    let payload = {
        let mut s = payload;
        while let Some((first, rest)) = s.split_first() {
            if matches!(first, b' ' | b'\t' | b'\r') {
                s = rest;
            } else {
                break;
            }
        }
        s
    };
    if payload.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_slice(payload).ok()?;
    if let Some(err) = v.get("error") {
        if err.is_object() && (err.get("message").is_some() || err.get("type").is_some()) {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error");
            return Some(format!(
                "upstream returned error payload in first chunk: {msg}"
            ));
        }
    }
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error");
        return Some(format!(
            "upstream returned error payload in first chunk: {msg}"
        ));
    }
    None
}

/// P0-2：priming 窗口缓冲上限（cc-switch forwarder.rs STREAM_PRIME_LIMIT 同款）
const STREAM_PRIME_MAX_BYTES: usize = 256 * 1024;

/// P0-2：单个 chunk 的信号
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrimeSignal {
    Neutral,
    Productive,
    Terminal,
    /// 帧内错误 envelope（多帧合并 chunk 的次帧错误；携带压缩消息）
    Error(String),
}

/// P0-2：priming 窗口内错误/降级链复用的 OpenAI 错误 envelope 字节
fn upstream_error_envelope(msg: &str) -> Vec<u8> {
    serde_json::json!({
        "error": {"message": msg, "type": "upstream_error", "code": "upstream_error"}
    })
    .to_string()
    .into_bytes()
}

/// P0-2：priming 窗口的跨 chunk SSE 事件分析器。事件可能被 TCP 块切开，
/// 故维护跨块缓冲；对每个完整事件的 data 载荷判定「产出性输出 / 终态 / 中性」：
/// - Chat 方言：delta 带 content（非空 string/parts）/ reasoning_content /
///   tool_calls → 产出；finish_reason 非空或 usage 出现 → 终态
/// - Anthropic 方言：content_block_delta（text/thinking/input_json）→ 产出；
///   message_stop → 终态（message_start/content_block_start/ping 为中性；
///   type:error 已由 first_chunk_error 拦截）
/// - Responses 方言：`response.output*` / `response.function_call_arguments.*`
///   → 产出；response.completed/incomplete/failed → 终态（created/in_progress 中性）
/// - `data: [DONE]` → 终态
#[derive(Default)]
struct PrimeState {
    buf: Vec<u8>,
}

impl PrimeState {
    fn feed(&mut self, chunk: &[u8]) -> PrimeSignal {
        self.buf.extend_from_slice(chunk);
        let mut signal = PrimeSignal::Neutral;
        loop {
            let Some(idx) = find_event_end(&self.buf) else {
                break;
            };
            let event: Vec<u8> = self.buf.drain(..idx).collect();
            let text = String::from_utf8_lossy(&event);
            let event_name = text
                .lines()
                .find_map(|l| l.strip_prefix("event:"))
                .map(|v| v.trim().to_string());
            for line in text.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    signal = signal.max(PrimeSignal::Terminal);
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                // P0-2：帧内错误 envelope（{"error":{…}} 或具名 event: error 帧）
                // ——多帧合并 chunk 时 first_chunk_error 只查首行，此处逐帧兜底。
                // 错误帧优先级最高（覆盖同 chunk 后续 [DONE] 的提交信号）
                let frame_error =
                    frame_error_message(&v, event_name.as_deref()).map(PrimeSignal::Error);
                if let Some(e) = frame_error {
                    signal = e;
                    break;
                }
                signal = signal.max(prime_signal_of(&v, event_name.as_deref()));
            }
        }
        // 防御：不完整的巨型事件（> 窗口上限）直接判产出提交，不再等待
        if self.buf.len() > STREAM_PRIME_MAX_BYTES {
            return signal.max(PrimeSignal::Productive);
        }
        signal
    }
}

impl PartialOrd for PrimeSignal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrimeSignal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(s: &PrimeSignal) -> u8 {
            match s {
                PrimeSignal::Neutral => 0,
                PrimeSignal::Terminal => 1,
                PrimeSignal::Productive => 2,
                PrimeSignal::Error(_) => 3,
            }
        }
        rank(self).cmp(&rank(other))
    }
}

/// 帧级错误 envelope 提取：{"error":{message|type}} / {"type":"error"} /
/// 具名 `event: error` 帧（data 无 error 字段）→ Some(压缩消息)
fn frame_error_message(v: &Value, event_name: Option<&str>) -> Option<String> {
    if let Some(err) = v.get("error") {
        if err.is_object() && (err.get("message").is_some() || err.get("type").is_some()) {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error");
            return Some(format!("upstream returned error payload in stream: {msg}"));
        }
    }
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error");
        return Some(format!("upstream returned error payload in stream: {msg}"));
    }
    if event_name == Some("error") {
        return Some("upstream stream emitted a named error event".to_string());
    }
    None
}

fn prime_signal_of(v: &Value, event_name: Option<&str>) -> PrimeSignal {
    // Anthropic / Responses 方言优先按 type 字段判定
    if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
        // Anthropic 方言
        if ty == "content_block_delta" {
            return PrimeSignal::Productive;
        }
        if ty == "message_stop" {
            return PrimeSignal::Terminal;
        }
        if ty.starts_with("response.") {
            // Responses 方言
            if ty.starts_with("response.output")
                || ty.starts_with("response.function_call_arguments")
                || ty.starts_with("response.reasoning")
                || ty.starts_with("response.custom_tool_call_input")
            {
                return PrimeSignal::Productive;
            }
            if matches!(
                ty,
                "response.completed" | "response.incomplete" | "response.failed" | "response.done"
            ) {
                return PrimeSignal::Terminal;
            }
            return PrimeSignal::Neutral;
        }
    }
    let _ = event_name;
    // Chat 方言
    if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
                return PrimeSignal::Terminal;
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content") {
                    let has_content = match content {
                        Value::String(s) => !s.is_empty(),
                        Value::Array(a) => !a.is_empty(),
                        _ => false,
                    };
                    if has_content {
                        return PrimeSignal::Productive;
                    }
                }
                if delta
                    .get("reasoning_content")
                    .and_then(|r| r.as_str())
                    .is_some_and(|s| !s.is_empty())
                {
                    return PrimeSignal::Productive;
                }
                if delta
                    .get("tool_calls")
                    .and_then(|t| t.as_array())
                    .is_some_and(|a| !a.is_empty())
                {
                    return PrimeSignal::Productive;
                }
            }
        }
    }
    if v.get("usage").is_some() {
        return PrimeSignal::Terminal;
    }
    PrimeSignal::Neutral
}
fn attach_upstream_headers(
    mut response: Response,
    upstream: &[(axum::http::HeaderName, axum::http::HeaderValue)],
) -> Response {
    for (name, value) in upstream {
        response.headers_mut().insert(name.clone(), value.clone());
    }
    response
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

/// M27：透传客户端的请求头白名单（cc-switch forwarder.rs:1953-1998 全量转发
/// 子集；OpenRouter 计费归因、OpenAI 组织路由、遥测归因等）。
/// M2：补充 idempotency-key（Anthropic 官方重试幂等）、
/// anthropic-dangerous-direct-browser-access（SDK CORS 豁免头）、
/// x-claude-code-session-id（会话关联）。`x-stainless-*` 前缀族另行前缀匹配透传。
const CLIENT_HEADER_PASSTHROUGH: [&str; 13] = [
    "anthropic-version",
    "anthropic-beta",
    "user-agent",
    "x-app",
    "http-referer",
    "x-title",
    "openai-organization",
    "openai-project",
    "originator",
    "session_id",
    "idempotency-key",
    "anthropic-dangerous-direct-browser-access",
    "x-claude-code-session-id",
];

/// M30：日志脱敏——reqwest Error Display 含完整 URL，聚合器风格 base_url
/// （https://gw/<KEY>/v1）会把密钥打进日志；落日志前替换密钥子串
fn redact_secrets(message: String, provider_key: &str) -> String {
    if provider_key.trim().is_empty() {
        return message;
    }
    message.replace(provider_key, "***")
}
/// 发送上游请求；timeout_ms 作用于响应头等待；非流式额外施加请求总超时（含响应体）。
/// H6：原生 anthropic 上游注入 anthropic-version（客户端值优先，缺省 2023-06-01）；
/// 认证形态支持 x-api-key / bearer。M10：客户端头白名单透传 + 渠道级 extra_headers
/// （最高优先级，覆盖同名透传头）。
/// P1-3：原生 anthropic 上游重建 anthropic-beta——透传客户端值并按需追加
/// `claude-code-20250219`（GATEWAY_ANTHROPIC_INJECT_CLAUDE_CODE_BETA）与
/// `context-1m-2025-08-07`（客户端模型带 `[1m]` 后缀时；cc-switch
/// forwarder.rs:2148-2162 + context-1m 同款动机）。
async fn send_upstream(
    st: &AppState,
    provider: &Provider,
    provider_key: &str,
    body: &[u8],
    endpoint: Endpoint,
    request_id: Uuid,
    streamed: bool,
    client_headers: &HeaderMap,
    wants_1m: bool,
) -> Result<reqwest::Response, Box<dyn Error + Send + Sync>> {
    // M31 + P1-14：base_url 拼接加固——误填完整端点不双拼；origin-only 带尾
    // `/v1` 时去重；比较大小写不敏感；query/fragment 剥离
    //（cc-switch codex.rs:943-972 + forwarder.rs:2943-2957 同款）
    let base = provider
        .base_url
        .trim()
        .trim_end_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let base_lower = base.to_ascii_lowercase();
    let upstream_lower = endpoint.upstream.to_ascii_lowercase();
    let url = if base_lower.ends_with(&upstream_lower) {
        base.to_string()
    } else if base_lower.ends_with("/v1") && upstream_lower.starts_with("/v1/") {
        format!("{base}{}", &endpoint.upstream[3..])
    } else {
        format!("{base}{}", endpoint.upstream)
    };
    let mut req = st.client.post(&url);
    // 认证形态：x-api-key（Anthropic 原生风格）或 Authorization: Bearer（默认）
    if provider
        .auth_scheme
        .trim()
        .eq_ignore_ascii_case("x-api-key")
    {
        req = req.header("x-api-key", provider_key);
    } else {
        req = req.header(header::AUTHORIZATION, format!("Bearer {provider_key}"));
    }
    req = req
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Request-Id", request_id.to_string());
    // H6：Anthropic 官方 API 强制要求 anthropic-version；透传客户端值、缺省补默认
    let anthropic_native = provider.api_type.eq_ignore_ascii_case("anthropic");
    if anthropic_native {
        let version = client_headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.trim().is_empty())
            .unwrap_or("2023-06-01");
        req = req.header("anthropic-version", version);
    }
    // M10/M27：客户端头白名单透传 + x-stainless-* 前缀族。
    // P1-3：anthropic-beta 对原生 Anthropic 上游改走重建路径（见下），此处跳过
    for name in CLIENT_HEADER_PASSTHROUGH {
        if anthropic_native && name == "anthropic-beta" {
            continue;
        }
        if let Some(v) = client_headers.get(name) {
            req = req.header(name, v);
        }
    }
    // P1-3：原生 Anthropic 上游重建 anthropic-beta（客户端值 + 标记注入）
    if anthropic_native {
        let mut betas: Vec<String> = client_headers
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if st.cfg.anthropic_inject_claude_code_beta
            && !betas.iter().any(|b| b == "claude-code-20250219")
        {
            betas.push("claude-code-20250219".into());
        }
        if wants_1m && !betas.iter().any(|b| b == "context-1m-2025-08-07") {
            betas.push("context-1m-2025-08-07".into());
        }
        if !betas.is_empty() {
            req = req.header("anthropic-beta", betas.join(","));
        }
    }
    for (name, v) in client_headers.iter() {
        if name.as_str().starts_with("x-stainless-") {
            req = req.header(name, v);
        }
    }
    // 全局自定义上游请求头（设置页）。与渠道级 extra_headers 同为追加语义：
    // 同名时并存为多值头（reqwest .header() append）。保存时已过合法性/
    // 黑名单校验，此处解析失败防御性跳过
    {
        let custom = st.custom_headers.read();
        if !custom.upstream.is_empty() {
            for (k, v) in custom.upstream.iter() {
                if let Some(s) = v.as_str() {
                    if let (Ok(name), Ok(val)) = (
                        k.parse::<axum::http::HeaderName>(),
                        axum::http::HeaderValue::from_str(s),
                    ) {
                        req = req.header(name, val);
                    }
                }
            }
        }
    }
    if let Some(map) = provider.extra_headers.as_object() {
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                req = req.header(k, s);
            }
        }
    }
    req = req.body(body.to_vec());
    if streamed {
        req = req.header(header::ACCEPT, "text/event-stream");
    } else {
        // M27：非流式透传客户端 Accept（缺省 application/json）
        if let Some(accept) = client_headers.get(header::ACCEPT) {
            req = req.header(header::ACCEPT, accept);
        } else {
            req = req.header(header::ACCEPT, "application/json");
        }
        // M5：非流式总超时独立可配（GATEWAY_NON_STREAM_TIMEOUT，默认 600s；
        // 0 = 禁用）——与 header 超时（provider.timeout_ms）解耦，长推理生成
        // 不再被 header 等待时限 +30s 掐断
        if st.cfg.non_stream_timeout_secs > 0 {
            req = req.timeout(Duration::from_secs(st.cfg.non_stream_timeout_secs));
        }
    }
    let timeout = Duration::from_millis(provider.timeout_ms.max(1_000) as u64);
    tokio::time::timeout(timeout, req.send())
        .await
        .map_err(|_| "upstream header timeout".to_string())?
        .map_err(|e| redact_secrets(format!("upstream request failed: {e}"), provider_key).into())
}

fn content_type_of(resp: &reqwest::Response) -> String {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string()
}

/// 上游 Chat 响应体是否含可转换的 choices（非空数组；choices:null/[] 均不可转换）
fn has_usable_choices(v: &Value) -> bool {
    v.get("choices")
        .and_then(|c| c.as_array())
        .is_some_and(|a| !a.is_empty())
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
        let Some(idx) = find_event_end(buf) else {
            break;
        };
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
                        // P1-18：Anthropic message_start 的 usage 嵌在 message 对象内
                        //（原生 anthropic 透传流）→ 顶层捕获不到、缓存桶丢失
                        if usage.is_none() {
                            if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                                usage = serde_json::from_value(u.clone()).ok();
                            }
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
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// extra_body 深合并：对象递归合并、标量/数组直接覆盖、非对象不合并
    #[test]
    fn merge_deep_semantics() {
        let mut dst = serde_json::json!({
            "chat_template_kwargs": { "thinking": false, "keep": 1 },
            "top_p": 0.9,
            "stop": ["a"]
        });
        let src = serde_json::json!({
            "chat_template_kwargs": { "thinking": true, "reasoning_effort": "max" },
            "top_p": 0.5,
            "stop": ["b"],
            "top_k": 20
        });
        merge_deep(&mut dst, &src);
        assert_eq!(
            dst,
            serde_json::json!({
                "chat_template_kwargs": { "thinking": true, "keep": 1, "reasoning_effort": "max" },
                "top_p": 0.5,
                "stop": ["b"],
                "top_k": 20
            })
        );
        // 标量目标 + 对象来源 → 整体替换；非对象来源 → 不动
        let mut scalar = serde_json::json!(1);
        merge_deep(&mut scalar, &serde_json::json!({ "a": 1 }));
        assert_eq!(scalar, serde_json::json!(1));
        let mut obj = serde_json::json!({ "a": 1 });
        merge_deep(&mut obj, &serde_json::json!([1, 2]));
        assert_eq!(obj, serde_json::json!({ "a": 1 }));
    }

    /// 有效 extra：路由级覆盖渠道级、保留字段剥离、空对象 → None
    #[test]
    fn effective_extra_precedence() {
        let provider = serde_json::json!({
            "chat_template_kwargs": { "thinking": true, "reasoning_effort": "medium" },
            "top_k": 20
        });
        let route = serde_json::json!({ "chat_template_kwargs": { "reasoning_effort": "max" } });
        let eff = effective_extra(&route, &provider).expect("merged");
        assert_eq!(
            eff,
            serde_json::json!({
                "chat_template_kwargs": { "thinking": true, "reasoning_effort": "max" },
                "top_k": 20
            })
        );
        // 网关保留字段被剥离
        let provider =
            serde_json::json!({ "model": "x", "stream": true, "stream_options": {}, "top_k": 1 });
        let eff = effective_extra(&serde_json::json!({}), &provider).expect("merged");
        assert_eq!(eff, serde_json::json!({ "top_k": 1 }));
        // 全空 / 全为保留字段 → None（出站走原始字节快路径）
        assert!(effective_extra(&serde_json::json!({}), &serde_json::json!({})).is_none());
        assert!(
            effective_extra(&serde_json::json!({}), &serde_json::json!({ "model": "x" })).is_none()
        );
    }

    /// 出站体合并：配置覆盖客户端同名叶键、客户端独有字段保留、保留字段不生效
    #[test]
    fn apply_extra_preserves_client_fields() {
        let mut body = serde_json::json!({
            "model": "client-model",
            "messages": [{ "role": "user", "content": "hi" }],
            "chat_template_kwargs": { "user_keep": true },
            "temperature": 0.7
        });
        let extra = serde_json::json!({
            "chat_template_kwargs": { "thinking": true },
            "model": "evil-override",
            "temperature": 0.2
        });
        apply_extra(&mut body, Some(&extra));
        assert_eq!(body["model"], serde_json::json!("client-model"));
        assert_eq!(
            body["chat_template_kwargs"],
            serde_json::json!({ "user_keep": true, "thinking": true })
        );
        assert_eq!(body["temperature"], serde_json::json!(0.2));
        // None → 不动
        apply_extra(&mut body, None);
        assert_eq!(body["temperature"], serde_json::json!(0.2));
    }

    /// H5：方言配对判定 —— 仅三种不支持交集返回原因
    #[test]
    fn pairing_unsupported_matrix() {
        let mk = |api_type: &str| Provider {
            id: 1,
            name: "p".into(),
            api_type: api_type.into(),
            base_url: "http://x/v1".into(),
            api_key_encrypted: "k".into(),
            timeout_ms: 1000,
            extra_body: serde_json::json!({}),
            auth_scheme: "bearer".into(),
            extra_headers: serde_json::json!({}),
            supports_images: true,
        };
        let chat = Endpoint {
            api: "/v1/chat/completions",
            upstream: "/chat/completions",
            responses: false,
            anthropic: false,
        };
        let resp = Endpoint {
            api: "/v1/responses",
            upstream: "/responses",
            responses: true,
            anthropic: false,
        };
        let msg = Endpoint {
            api: "/v1/messages",
            upstream: "/messages",
            responses: false,
            anthropic: true,
        };
        // 原生透传：全部支持
        assert!(pairing_unsupported(chat, &mk("openai")).is_none());
        assert!(pairing_unsupported(resp, &mk("openai-responses")).is_none());
        assert!(pairing_unsupported(msg, &mk("anthropic")).is_none());
        // 降级转换：全部支持
        assert!(pairing_unsupported(resp, &mk("openai")).is_none());
        assert!(pairing_unsupported(msg, &mk("openai")).is_none());
        // 不支持的交集：显式原因
        assert!(pairing_unsupported(resp, &mk("anthropic")).is_some());
        assert!(pairing_unsupported(msg, &mk("openai-responses")).is_some());
        assert!(pairing_unsupported(chat, &mk("anthropic")).is_some());
        assert!(pairing_unsupported(chat, &mk("openai-responses")).is_some());
    }

    /// M3：流式可重试状态码集合（401/403/408 可切换，4xx 其余不切）
    /// M25：可重试状态码黑名单制（cc-switch categorize_proxy_error 同款集合）
    #[test]
    fn stream_retryable_status_codes() {
        assert!(is_stream_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_stream_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_stream_retryable(StatusCode::UNAUTHORIZED));
        assert!(is_stream_retryable(StatusCode::FORBIDDEN));
        assert!(
            is_stream_retryable(StatusCode::REQUEST_TIMEOUT),
            "408 可切换"
        );
        assert!(
            is_stream_retryable(StatusCode::NOT_FOUND),
            "M25：404 可切换（换家有该模型）"
        );
        assert!(!is_stream_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_stream_retryable(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(
            !is_stream_retryable(StatusCode::NOT_IMPLEMENTED),
            "501 不可重试"
        );
    }

    /// L1：latest_reminder → user、developer → system（messages[] 与 input[] 双形态）
    #[test]
    fn normalize_chat_roles_maps_codex_internal_roles() {
        let mut json = serde_json::json!({
            "messages": [
                {"role": "developer", "content": "be terse"},
                {"role": "latest_reminder", "content": "keep it short"},
                {"role": "user", "content": "hi"}
            ]
        });
        assert!(normalize_chat_roles(&mut json));
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][2]["role"], "user");
        let mut input = serde_json::json!({
            "input": [
                {"type": "message", "role": "developer", "content": "d"},
                {"type": "message", "role": "latest_reminder", "content": "r"}
            ]
        });
        assert!(normalize_chat_roles(&mut input));
        assert_eq!(input["input"][0]["role"], "system");
        assert_eq!(input["input"][1]["role"], "user");
        // 无变化 → false
        assert!(!normalize_chat_roles(&mut serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}]
        })));
    }

    /// L8：`_` 前缀私有字段递归剥离 + 存在性判定
    #[test]
    fn underscore_fields_stripped_recursively() {
        let mut v = serde_json::json!({
            "model": "m",
            "_internal": "debug",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi", "_meta": 1}]}]
        });
        assert!(has_underscore_fields(&v));
        strip_underscore_fields(&mut v);
        assert!(v.get("_internal").is_none());
        assert!(v["messages"][0]["content"][0].get("_meta").is_none());
        assert!(!has_underscore_fields(&v));
        // 无下划线字段 → 判定 false
        assert!(!has_underscore_fields(&serde_json::json!({"a": 1})));
    }

    /// H1：normalize_tools 保留 custom 工具（用户自定义），剥离 hosted 内置工具
    #[test]
    fn normalize_tools_keeps_custom() {
        let mut json = serde_json::json!({
            "tools": [
                {"type": "function", "name": "f"},
                {"type": "custom", "name": "apply_patch", "format": {"type": "text"}},
                {"type": "web_search"},
                {"name": "no-type"}
            ]
        });
        assert!(normalize_tools(&mut json), "web_search 被剥离");
        let tools = json["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[1]["type"], "custom", "custom 工具必须保留");
    }

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

    /// P1-18：Anthropic message_start 的 usage 嵌在 message 对象内——原生
    /// anthropic 透传流的缓存桶不再丢失（此前仅顶层 usage 被捕获）
    #[test]
    fn feed_sse_captures_nested_anthropic_message_usage() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(
            &mut buf,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\
              \"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":5,\
              \"cache_read_input_tokens\":7}}}\n\n",
        );
        assert!(!done);
        let u = usage.expect("nested usage captured");
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.cache_write_tokens, Some(5));
        assert_eq!(u.cache_read_tokens, Some(7));
        // 顶层 Chat usage 形态仍优先
        let mut buf2 = Vec::new();
        let (usage2, _) = feed_sse(
            &mut buf2,
            b"data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n",
        );
        assert_eq!(usage2.expect("top-level usage").prompt_tokens, Some(3));
    }

    /// P1-2：include_usage 合并语义——客户端自带 stream_options 时保留兄弟键
    #[test]
    fn ensure_include_usage_merges_existing_stream_options() {
        let mut body = serde_json::json!({
            "stream": true,
            "stream_options": { "include_usage": false, "other_key": 1 }
        });
        ensure_include_usage(&mut body, true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream_options"]["other_key"], 1, "兄弟键保留");
        // 无 stream_options → 整体注入
        let mut body = serde_json::json!({ "stream": true });
        ensure_include_usage(&mut body, true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // 非流式不动
        let mut body = serde_json::json!({ "stream": false });
        ensure_include_usage(&mut body, false);
        assert!(body.get("stream_options").is_none());
        // 非对象 stream_options → 整体替换
        let mut body = serde_json::json!({ "stream": true, "stream_options": "garbage" });
        ensure_include_usage(&mut body, true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    /// P1-8：metadata.user_id `_session_` 后缀提取（仅 anthropic 端点）
    #[test]
    fn session_id_from_metadata_parses_suffix() {
        let json = serde_json::json!({
            "metadata": { "user_id": "user_abc123_session_9f8e7d6c" }
        });
        assert_eq!(
            session_id_from_metadata(&json, true).as_deref(),
            Some("9f8e7d6c")
        );
        // 非 anthropic 端点不解析
        assert!(session_id_from_metadata(&json, false).is_none());
        // 无 _session_ → None
        assert!(
            session_id_from_metadata(&serde_json::json!({"metadata": {"user_id": "plain"}}), true)
                .is_none()
        );
        // 空后缀 → None
        assert!(
            session_id_from_metadata(
                &serde_json::json!({"metadata": {"user_id": "x_session_"}}),
                true
            )
            .is_none()
        );
    }

    /// P0-2：priming 窗口信号判定——三方言产出性输出 / 终态 / 中性
    #[test]
    fn prime_signal_detects_productive_output() {
        // Chat：role-only delta → 中性；content delta → 产出
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n"),
            PrimeSignal::Neutral
        );
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n"),
            PrimeSignal::Productive
        );
        // Chat：finish_reason → 终态
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"),
            PrimeSignal::Terminal
        );
        // Chat：[DONE] → 终态
        let mut p = PrimeState::default();
        assert_eq!(p.feed(b"data: [DONE]\n\n"), PrimeSignal::Terminal);
        // Anthropic：message_start 中性；content_block_delta 产出；message_stop 终态
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n"),
            PrimeSignal::Neutral
        );
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n"),
            PrimeSignal::Productive
        );
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            PrimeSignal::Terminal
        );
        // Responses：response.created 中性；output_text.delta 产出；completed 终态
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n"),
            PrimeSignal::Neutral
        );
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n"),
            PrimeSignal::Productive
        );
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n"),
            PrimeSignal::Terminal
        );
        // 跨块：事件被 TCP 切开仍正确判定
        let mut p = PrimeState::default();
        assert_eq!(
            p.feed(b"data: {\"choices\":[{\"delta\":{\"con"),
            PrimeSignal::Neutral
        );
        assert_eq!(p.feed(b"tent\":\"Hi\"}}]}\n\n"), PrimeSignal::Productive);
        // 多帧合并 chunk：role delta + 错误 envelope + [DONE] → Error 优先
        //（first_chunk_error 只查首行，帧级检测必须兜住次帧错误）
        let mut p = PrimeState::default();
        assert!(matches!(
            p.feed(
                b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                  data: {\"error\":{\"message\":\"boom second\"}}\n\n\
                  data: [DONE]\n\n"
            ),
            PrimeSignal::Error(_)
        ));
        // 具名 event: error 帧
        let mut p = PrimeState::default();
        assert!(matches!(
            p.feed(b"event: error\ndata: {\"foo\":1}\n\n"),
            PrimeSignal::Error(_)
        ));
    }

    /// P0-2：错误 envelope 帧识别跨块（复用 first_chunk_error 语义）
    #[test]
    fn first_chunk_error_detects_error_envelope_in_sse_frame() {
        assert!(first_chunk_error(b"data: {\"error\":{\"message\":\"boom\"}}\n\n").is_some());
        assert!(
            first_chunk_error(b"data: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}\n\n")
                .is_some()
        );
        assert!(first_chunk_error(b"{\"error\":{\"message\":\"boom\"}}").is_some());
        // 正常 delta 不误判
        assert!(
            first_chunk_error(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
                .is_none()
        );
    }

    /// P0-2：错误 envelope 字节
    #[test]
    fn upstream_error_envelope_shape() {
        let bytes = upstream_error_envelope("boom");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["message"], "boom");
        assert_eq!(v["error"]["code"], "upstream_error");
    }
}
