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
use crate::service::breaker::Breaker;
use crate::service::usage;
use crate::state::AppState;
use crate::store::upstream::Provider;
use crate::store::usage::{Usage, UsageMeta};

/// 流式超时默认值（秒）：0 = 禁用。实际值取自 AppConfig
///（GATEWAY_STREAM_FIRST_BYTE_TIMEOUT / GATEWAY_STREAM_IDLE_TIMEOUT），
/// 默认 300 / 300——与客户端首事件/空闲看门狗对齐（原 cc-switch 60 / 120
/// 会把合法长预填充/长思考判成失败）。

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

impl<S> StreamTimeoutStream<S> {
    /// 窗口切换/重置到 idle：优先 reset 复用既有 Sleep——逐 chunk 重建计时器
    /// 在 1000 chunk/s 量级即每秒千次堆分配+定时轮注册；语义与重建一致：
    /// idle 禁用 → 撤销计时器，尚无计时器 → 新建。
    fn arm_idle(&mut self) {
        match (self.idle, &mut self.wait) {
            (Some(d), Some(wait)) => wait.as_mut().reset(tokio::time::Instant::now() + d),
            (Some(d), None) => self.wait = Some(Box::pin(tokio::time::sleep(d))),
            (None, _) => self.wait = None,
        }
    }
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
                this.arm_idle();
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.first = false;
                this.arm_idle();
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

/// priming 墙钟上限（GATEWAY_STREAM_PRIME_TIMEOUT_MS；0 = 不限）
fn stream_prime_deadline(st: &AppState) -> Option<Duration> {
    (st.cfg.stream_prime_timeout_ms > 0)
        .then(|| Duration::from_millis(st.cfg.stream_prime_timeout_ms))
}

/// priming 阶段把「下一个流项」与「墙钟 deadline」合流的小结构：构造一次、
/// 跨 chunk 复用同一个 `Sleep`（逐 chunk 重建计时器 = 每帧一次堆分配 + 定时器
/// 注册），并且每次轮询都**先**轮询 deadline（`biased`）。
///
/// 先轮询 deadline 是防饥饿的唯一保障：上游若把大批中性帧（role delta / `: ping`）
/// 一次性压进 hyper 读缓冲，item 每轮轮询都同步 Ready；若先轮询 item，`Sleep`
/// 的 waker 永不注册，墙钟上限会被帧洪水饿死（RCA S3 的形态）。
struct PrimeDeadline {
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

/// `PrimeDeadline::next` 的两条出口：拿到流项（含流结束的 `None`），或墙钟到期。
enum PrimePoll<T> {
    Item(T),
    Deadline,
}

impl PrimeDeadline {
    fn new(after: Option<Duration>) -> Self {
        Self {
            sleep: after.map(|d| Box::pin(tokio::time::sleep(d))),
        }
    }

    /// 撤销 deadline（非 SSE 响应到期后回到无 deadline 的等待）
    fn disarm(&mut self) {
        self.sleep = None;
    }

    /// 取下一个流项；deadline 先到期则返回 `PrimePoll::Deadline`。
    /// `StreamExt::next` 取消安全：deadline 分支胜出时被丢弃的 future 不丢 chunk。
    async fn next<S>(&mut self, stream: &mut S) -> PrimePoll<Option<S::Item>>
    where
        S: Stream + Unpin,
    {
        let Some(sleep) = self.sleep.as_mut() else {
            return PrimePoll::Item(stream.next().await);
        };
        let mut next = stream.next();
        tokio::select! {
            biased;
            _ = sleep.as_mut() => PrimePoll::Deadline,
            item = &mut next => PrimePoll::Item(item),
        }
    }
}

/// 既有超时文案（header 等待 / 首字节 / 静默）
fn timeout_text(text: &str) -> bool {
    text.contains("header timeout")
        || text.contains("stream first byte timeout")
        || text.contains("stream idle timeout")
}

fn is_timeout_node(e: &(dyn Error + 'static)) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .is_some_and(reqwest::Error::is_timeout)
        || timeout_text(&e.to_string())
}

/// 判定错误是否为上游超时（header 等待 / 首字节 / 静默）：沿 `source()` 链同时
/// 检查错误类型与既有超时文案。
///
/// 文案匹配是**必需**的判定，不是兜底：网关自己产生的超时错误全是字符串——
/// - `StreamTimeoutStream` 的首字节/静默超时是
///   `Box::from(format!("upstream stream {{first byte|idle}} timeout"))`（`String`），
///   `downcast_ref::<reqwest::Error>` 永不命中；
/// - header 等待超时由 `send_upstream` 生成 `"upstream header timeout".to_string()`，
///   连接/请求失败也被 `redact_secrets(…).into()` 压平成字符串（`source()` 链为空）。
///
/// 仅内层 reqwest 错误（`Box::from(e)` 透传，如响应体解码错误）保留原类型，
/// `reqwest::Error::is_timeout()` 才可靠。删掉文案匹配会让网关侧超时退回
/// 502/连接错误分类。
fn is_timeout_error(err: &(dyn Error + Send + Sync + 'static)) -> bool {
    let mut cur: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if is_timeout_node(e) {
            return true;
        }
        cur = e.source();
    }
    false
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
        at_line_start: true,
    }
}

struct PingKeepaliveStream<S> {
    inner: Pin<Box<S>>,
    interval: Duration,
    wait: Pin<Box<tokio::time::Sleep>>,
    done: bool,
    /// 上游最后字节是否收在 '\n'（行边界）。ping 只在行边界注入：SSE 注释行
    /// 楔进两行之间无害，楔进半行 data 载荷中间会把一条事件撕成两条坏帧
    at_line_start: bool,
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
                if let Ok(b) = &item {
                    this.at_line_start = b.last() == Some(&b'\n');
                }
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
                    if this.at_line_start {
                        Poll::Ready(Some(Ok(Bytes::from_static(b": ping\n\n"))))
                    } else {
                        // 半行中间不楔入：保留静默，等上游下一字节到边界再发
                        Poll::Pending
                    }
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

/// 带上限与截止时间的响应体读取：超出上限报错（防恶意上游 OOM 网关）；
/// deadline 兜底僵死流——响应头已回但体永不结束/停止推进时不再无限挂起
/// （重试体预读 / 降级转换体收集同理）。超时错误文案
/// `"upstream body read timeout"`：候选失败分类据此归因 body_timeout。
async fn read_body_capped(
    resp: reqwest::Response,
    cap: usize,
    deadline: Duration,
) -> Result<Bytes, Box<dyn Error + Send + Sync>> {
    tokio::time::timeout(deadline, async move {
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
    })
    .await
    .unwrap_or_else(|_| Err("upstream body read timeout".into()))
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

/// 按用户×模型实时并发 guard：鉴权并解析出模型后 +1，响应真正结束时 -1，
/// 归零移除条目。模型取客户端请求体原始 model（后续改写不影响已挂计数键），
/// 缺 model 时落空串键——该请求随即在管线 400，条目只存活毫秒级。
struct UserActiveGuard {
    map: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<(i64, String), i64>>>,
    key: (i64, String),
}

impl UserActiveGuard {
    fn enter(
        map: &std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<(i64, String), i64>>>,
        user_id: i64,
        model: Option<String>,
    ) -> Self {
        let key = (user_id, model.unwrap_or_default());
        *map.lock().entry(key.clone()).or_insert(0) += 1;
        Self {
            map: map.clone(),
            key,
        }
    }
}

impl Drop for UserActiveGuard {
    fn drop(&mut self) {
        let mut m = self.map.lock();
        if let Some(c) = m.get_mut(&self.key) {
            *c -= 1;
        }
        // 归零移除：防止表随历史用户×模型组合无限增长
        m.retain(|_, c| *c > 0);
    }
}

/// 响应级 guard 组：handler 返回 ≠ 响应结束（SSE 流可远长于首字节），
/// 计数 guard 必须活到响应体流尽/客户端断开，否则流式请求严重少计。
struct ResponseGuards {
    active: ActiveRequestGuard,
    user_active: Option<UserActiveGuard>,
    /// 并发上限名额（请求管线下发前占坑，响应体流尽/断开归还；None = 测试调用不限）
    concurrency: Option<crate::service::ratelimit::ConcurrencyGuard>,
}

/// 把 guard 绑定到响应体生命周期：逐帧透传，流尽时释放；
/// 客户端中途断开时 body 连同 unfold 状态一起 drop，同样释放。
fn bind_guards_to_body(resp: Response, guards: ResponseGuards) -> Response {
    let (parts, body) = resp.into_parts();
    let bound = futures_util::stream::unfold(
        (body.into_data_stream(), Some(guards)),
        |state| async move {
            let (mut stream, guards) = state;
            match futures_util::StreamExt::next(&mut stream).await {
                Some(item) => Some((item, (stream, guards))),
                None => {
                    drop(guards);
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(bound))
}

/// 连接加固：全局在途并发兜底规则合成。`cap > 0` 时在规则快照上追加一条
/// global 作用域、不限模型的规则——`matched_rules` 将其映射为与 DB 全局规则
/// 相同的 "global" 桶键，`apply_concurrency_limits` 按键去重取最严（min），
/// all-or-nothing acquire 一并覆盖全局上限。`cap == 0`（关闭）→ 原样返回。
fn with_global_cap(
    mut rules: Vec<crate::store::rules::RateRule>,
    cap: usize,
) -> Vec<crate::store::rules::RateRule> {
    if cap == 0 {
        return rules;
    }
    rules.push(crate::store::rules::RateRule {
        scope: "global".to_string(),
        scope_id: None,
        model: None,
        concurrency: cap.min(i32::MAX as usize) as i32,
    });
    rules
}

/// OpenAI 兼容代理入口：鉴权 → 管线
pub async fn proxy(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
) -> Result<Response, AppError> {
    let active = ActiveRequestGuard::enter(&st.active_requests);
    // P1-6：request_id 前移到入口——鉴权/限流/配额/解析等早期失败也能关联，
    // 且下游转发沿用同一 id（日志-响应头一致）。错误统一附 X-Request-Id 头。
    let request_id = Uuid::new_v4();
    let auth = crate::service::auth::authenticate(st, &headers).await;
    let (user_id, key_id) = match auth {
        Ok(v) => v,
        Err(e) => {
            drop(active);
            return Err(fail_with_request_id(e, request_id));
        }
    };
    // 端点开关与请求体解析从 proxy_authed 前置到此处（保持原有次序：开关拒绝
    // 优先于 body 400）：解析只做一次，解析出的 JSON 全管线复用，模型维度
    // （客户端原始 model）在挂载用户并发 guard 前即可取得
    ensure_endpoint_enabled(st, endpoint).map_err(|e| fail_with_request_id(e, request_id))?;
    let json =
        parse_request_json(&body, &headers).map_err(|e| fail_with_request_id(e, request_id))?;
    let model = requested_model(&json);
    // 并发上限（管线唯一的限流维度）：guard 绑定响应体生命周期，SSE 流尽或
    // 客户端断开才归还。拒绝时并发未占坑（try_acquire 全有或全无）。
    // 连接加固：cfg.global_max_concurrency > 0 时合成一条 global 规则并入规则
    // 快照（与 DB 全局规则同键、apply_concurrency_limits 去重取最严），超出
    // 全局上限同样走既有 429 拒绝路径。经 AppState 副本注入（Arc 浅克隆 +
    // 新 rules Arc），不改共享规则表；guard 持同一计数表引用，释放语义不变。
    let conc = if st.cfg.global_max_concurrency > 0 {
        let mut effective = st.clone();
        let rules =
            with_global_cap(effective.rules.read().clone(), st.cfg.global_max_concurrency);
        effective.rules = Arc::new(parking_lot::RwLock::new(rules));
        crate::service::ratelimit::apply_concurrency_limits(
            &effective,
            user_id,
            key_id,
            model.as_deref().unwrap_or_default(),
        )
    } else {
        crate::service::ratelimit::apply_concurrency_limits(
            st,
            user_id,
            key_id,
            model.as_deref().unwrap_or_default(),
        )
    };
    let conc = match conc {
        Ok(g) => g,
        Err(e) => {
            drop(active);
            return Err(fail_with_request_id(e, request_id));
        }
    };
    let guards = ResponseGuards {
        active,
        user_active: user_id.map(|uid| UserActiveGuard::enter(&st.active_by_user, uid, model)),
        concurrency: Some(conc),
    };
    proxy_authed(
        st, headers, body, json, endpoint, client_ip, user_id, key_id, false, request_id,
    )
    .await
    .map(|resp| bind_guards_to_body(resp, guards))
    .map_err(|e| fail_with_request_id(e, request_id))
}

/// API 端点运行时开关检查（原 proxy_authed 步骤 0）：管理员停用的端点直接拒绝。
/// 真实请求与测试请求都走此检查（测试停用状态本身是合法的测试意图）
fn ensure_endpoint_enabled(st: &AppState, endpoint: Endpoint) -> Result<(), AppError> {
    let eps = st.api_endpoints.read();
    if endpoint.responses && !eps.responses_enabled {
        return Err(AppError::ServiceUnavailable(
            "the Responses API (/v1/responses) is currently disabled by the administrator".into(),
        ));
    }
    if endpoint.anthropic && !eps.messages_enabled {
        return Err(AppError::ServiceUnavailable(
            "the Anthropic Messages API (/v1/messages) is currently disabled by the administrator"
                .into(),
        ));
    }
    Ok(())
}

/// M24：请求体解析（先按 Content-Encoding 解压，再解析 JSON）。
/// 在入口挂用户并发 guard 前调用一次，结果传入管线复用——整份请求体
/// 不做第二次解压/解析。
fn parse_request_json(body: &Bytes, headers: &HeaderMap) -> Result<Value, AppError> {
    let decompressed = decompress_request_body(
        body,
        headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
    )
    .map_err(AppError::BadRequest)?;
    serde_json::from_slice(&decompressed)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON: {e}")))
}

/// 客户端请求的原始模型名（空/缺失 → None）。仅用于用户并发观测维度；
/// 管线内对缺 model 的正式校验仍在下游（错误语义不变）
fn requested_model(json: &Value) -> Option<String> {
    json.get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// P1-6：早期失败日志 + 请求 id 包装（响应头由 IntoResponse 附带）
fn fail_with_request_id(e: AppError, request_id: Uuid) -> AppError {
    tracing::warn!(request_id = %request_id, error = %e, "proxy request failed before a response was produced");
    e.with_request_id(request_id)
}

/// 管理员测试入口：跳过鉴权与用户/Key 限流上下文，且不记账
/// （test_call → 不写 usage_logs，不污染状态页错误率与用量统计）
pub async fn proxy_test(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
) -> Result<Response, AppError> {
    let guards = ResponseGuards {
        active: ActiveRequestGuard::enter(&st.active_requests),
        user_active: None,
        // 管理员测试调用不占并发名额（诊断流量，与 user_active 同宽免）
        concurrency: None,
    };
    let request_id = Uuid::new_v4();
    ensure_endpoint_enabled(st, endpoint).map_err(|e| fail_with_request_id(e, request_id))?;
    let json =
        parse_request_json(&body, &headers).map_err(|e| fail_with_request_id(e, request_id))?;
    proxy_authed(
        st, headers, body, json, endpoint, client_ip, None, None, true, request_id,
    )
    .await
    .map(|resp| bind_guards_to_body(resp, guards))
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

/// 已鉴权代理管线：Plan 配额 → 路由 → 转发（含降级）→ 记账
/// （并发上限已在入口 proxy 占坑、guard 绑定响应体；proxy_test 不占坑）。
/// 端点开关检查与请求体解压/解析已在入口（proxy / proxy_test）完成，
/// `json` 为解析结果、`body` 为原始字节（零改写快路径直传用）。
async fn proxy_authed(
    st: &AppState,
    headers: HeaderMap,
    body: Bytes,
    mut json: Value,
    endpoint: Endpoint,
    client_ip: std::net::IpAddr,
    user_id: Option<i64>,
    key_id: Option<i64>,
    test_call: bool,
    request_id: Uuid,
) -> Result<Response, AppError> {
    let started = Instant::now();

    // 2. 请求体归一化（解压/解析已在入口完成）：role 归一化后所有下游分支
    //    共用改写后的 json；Anthropic 方言跳过 OpenAI 归一化
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

    // 3. Coding Plan 配额：超出按策略拦截(429)/降级改写模型/仅告警放行。
    // 降级改写 json["model"] 并标记 plan_downgraded——零改写快路径直传原始
    // 客户端字节，必须强制走重建路径才能让改写生效。
    // decision.plan 即检查期按当前模型解析出的生效 Plan（模型作用域感知），
    // 直接作为记账载荷——避免下方二次解析在作用域过滤下漏记/错记。
    let mut plan_downgraded = false;
    let mut active_plan: Option<crate::store::plans::PlanRuntime> = None;
    if let Some(uid) = user_id {
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
    // P1-20：Claude Code 长上下文 `[1m]` 后缀剥离（路由未配置映射时防上游
    // 404/400；cc-switch model_mapper.rs:163-186 同款）
    let outbound_model: String = route.upstream_model.clone().unwrap_or_else(|| {
        model
            .strip_suffix("[1m]")
            .map(str::to_string)
            .unwrap_or_else(|| model.clone())
    });
    // 模型库启停：模型库记录的是上游侧模型名，按出站模型名剔除已禁用的
    // (供应商, 模型) 候选——主上游被禁则走降级链，全部被禁显式 503
    let candidates = {
        let before = candidates.len();
        let kept: Vec<_> = candidates
            .into_iter()
            .filter(|p| !crate::service::routing::model_disabled(st, p.id, &outbound_model))
            .collect();
        if kept.len() < before {
            tracing::info!(
                model = %model,
                upstream_model = %outbound_model,
                skipped = before - kept.len(),
                "skipping candidates whose model is disabled in the model catalog"
            );
        }
        kept
    };
    if candidates.is_empty() {
        return Err(AppError::ServiceUnavailable(format!(
            "model '{model}' is disabled by the administrator"
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
        return Err(AppError::Forbidden(
            st.message_templates.read().model_denied(&model),
        ));
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
            // 出站模型名统一在此改写（路由映射 upstream_model / [1m] 剥离）——
            // transform 只写客户端模型，四个出站分支都必须补写，漏写会把
            // claude-* 原名发给 Chat 上游（400/404 model-not-found）
            chat["model"] = serde_json::Value::String(outbound_model.clone());
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
                // 原生 anthropic 透传体是 Anthropic 方言：apply_thinking_form 会
                // 无条件剥 thinking 再按 Chat 形态重造（未配置 form 时不还原），
                // 客户端的 thinking 配置被静默丢弃——方言改写仅对 Chat 体执行
                let native_anthropic =
                    endpoint.anthropic && provider.api_type.eq_ignore_ascii_case("anthropic");
                if !native_anthropic {
                    apply_reasoning_config(&mut json, &route, &gating_model);
                }
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
        started,
        test_call,
        // M33：计价候选 = 客户端模型名 → 上游映射模型名（依次查价）
        pricing_models: std::iter::once(model.clone())
            .chain(route.upstream_model.clone())
            .collect(),
        plan: plan_bill,
        // 失败归因（连接加固）：仅候选失败行经 usage::record_failure_row 填写
        failure_class: None,
        upstream_status: None,
        attempt: None,
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
        // last_failure 元组里的状态码可能是网关合成的 502（priming 错误帧/
        // 提前断流）；归因行需要真实上游状态（200 流内错误帧 / 重试状态原值）
        let mut last_failure_upstream: Option<u16> = None;
        // L5：实际最后失败的候选（尾段错误上下文归属；默认主候选）
        let mut last_failed: Option<&Provider> = None;
        // 失败归因行的 attempt 列（0 = 主渠道，1.. = 降级链）
        let mut last_failed_idx: Option<usize> = None;
        let mut saw_timeout = false;
        let mut saw_connect_error = false;
        // 候选循环内响应体预读/错误体收集的统一截止时间（僵死流兜底）
        let body_read_deadline = Duration::from_secs(st.cfg.stream_idle_timeout_secs.max(1));
        'candidates: for (attempt_idx, provider) in
            candidates.iter().enumerate().take(max_attempts)
        {
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
            // F1：出站体构建失败（方言转换/序列化错误）不是渠道健康信号——此前
            // `?` 直接上抛，已占用的半开探测名额被永久占位，该渠道窗口到期后
            // 再也无法 allow 放行。只归还探测名额（record_failure 会把半开渠道
            // 重开整个窗口，且这不是线路故障）后换下一候选
            let (outbound, upstream_path) = match build_outbound(provider) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "provider outbound build failed");
                    st.breaker.release_probe(provider.id);
                    continue;
                }
            };
            let provider_key = match crate::crypto::decrypt(
                &provider.api_key_encrypted,
                &st.cfg.master_key,
            ) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "provider key decrypt failed");
                    if st.breaker.record_failure(provider.id) {
                        st.metrics.record_breaker_open();
                    }
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
                // 终局透传上游错误体：非重试失败（客户端拿到真实状态码）——
                // 上游错误指标 + 失败归因随记账行落库
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
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
                    if is_timeout_error(e.as_ref()) {
                        saw_timeout = true;
                    } else {
                        saw_connect_error = true;
                    }
                    tracing::warn!(provider = %provider.name, error = %e, "upstream stream attempt failed");
                    // 连接加固：候选失败三件套——熔断失败（开闸跃迁计指标）、
                    // 上游错误计数、失败归因行（类别 + 候选序号）
                    if st.breaker.record_failure(provider.id) {
                        st.metrics.record_breaker_open();
                    }
                    let (class, status_out, header_timeout) = classify_send_failure(e.as_ref());
                    st.metrics.record_upstream_error(provider.id);
                    usage::record_failure_row(
                        st,
                        &meta,
                        class,
                        None,
                        Some(attempt_idx),
                        status_out,
                        started.elapsed().as_millis() as i64,
                    )
                    .await;
                    // 连接加固：header 等待超时默认判终局——请求体已完整写出，
                    // 上游可能已在生成，换候选重发有双份生成/计费风险；
                    // GATEWAY_FAILOVER_ON_HEADER_TIMEOUT=true 保留旧 failover 行为
                    if header_timeout && !st.cfg.failover_on_header_timeout {
                        return header_timeout_terminal_response(
                            provider,
                            &model,
                            request_id,
                            endpoint,
                        );
                    }
                    last_failed = Some(provider);
                    last_failed_idx = Some(attempt_idx);
                    continue;
                }
            };
            let status = resp.status();
            let content_type = content_type_of(&resp);
            // P0-7：白名单限流/退避头（供本候选各返回出口附加）
            let rl = rate_limit_headers(&resp);
            if is_stream_retryable(status) {
                // 连接加固：429/503 带明确 Retry-After 时换候选前先等待（钳 2s
                // ——长窗口退避交由客户端 SDK；候选循环久等会拖死整条降级链）
                let wait_secs = retry_after_secs(
                    resp.headers(),
                    matches!(
                        status,
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                    ),
                );
                let bytes = read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES, body_read_deadline)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                tracing::warn!(provider = %provider.name, status = %status, "upstream stream retryable failure");
                last_failure = Some((status.as_u16(), bytes, content_type, rl.clone()));
                last_failure_upstream = Some(status.as_u16());
                if st.breaker.record_failure(provider.id) {
                    st.metrics.record_breaker_open();
                }
                // 连接加固：上游错误指标 + 失败归因行（真实上游状态）
                st.metrics.record_upstream_error(provider.id);
                usage::record_failure_row(
                    st,
                    &meta,
                    "upstream_status",
                    Some(status.as_u16()),
                    Some(attempt_idx),
                    status.as_u16(),
                    started.elapsed().as_millis() as i64,
                )
                .await;
                if let Some(secs) = wait_secs {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
                last_failed = Some(provider);
                last_failed_idx = Some(attempt_idx);
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
            // 连接加固：所有 SSE 链路的熔断信号都由流终止结算（透传 = `SseErrorTail`；
            // 转换链 = 各自 `wrap_chat_stream_to_*` 内部在传输错误/终态/干净 EOF
            // 时结算），提交处不再预记 success——"2xx 后必断流 / 缺终态帧"的
            // 供应商两种形态都按真实结果计数。判定只在这里计算一次，提交点与
            // 错误收尾上下文安装点共用——避免两处各自推演条件而漂移
            let tail_settles = is_sse;
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
                // W1：priming 墙钟 deadline——上游 2xx 后迟迟没有首个“产出性”
                // 事件（长预填充 / 长思考 / 中性帧洪水）时，到点即提交并进入流式，
                // 客户端立刻拿到响应头 + 15s 心跳，不再被误判 502。
                // 代价：deadline 之后到达的 200 + 错误帧已随流透传，不再触发
                // 候选切换（换取有界首字节与不再“无响应头”）。
                let mut prime_deadline = PrimeDeadline::new(stream_prime_deadline(st));
                while !committed {
                    if buffered_bytes >= STREAM_PRIME_MAX_BYTES {
                        break;
                    }
                    let item = match prime_deadline.next(&mut stream).await {
                        PrimePoll::Deadline => {
                            // SSE 响应到期即提交：缓冲为空或仅有中性帧（role delta /
                            // `: ping`）同样提交，提交时缓冲与剩余流按原序重放；
                            // 非 SSE 保持「等 JSON 体 / 零产出」判定不变（上游 200
                            // 但忽略 stream 参数的识别路径）
                            if is_sse {
                                tracing::info!(
                                    request_id = %request_id,
                                    model = %model,
                                    provider = %provider.name,
                                    buffered_bytes,
                                    "stream priming deadline reached; committing to client"
                                );
                                committed = true;
                                continue;
                            }
                            prime_deadline.disarm();
                            continue;
                        }
                        PrimePoll::Item(item) => item,
                    };
                    match item {
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
                                last_failed_idx = Some(attempt_idx);
                                last_failure = Some((
                                    502,
                                    upstream_error_envelope(&msg),
                                    "application/json".to_string(),
                                    Vec::new(),
                                ));
                                last_failure_upstream = Some(status.as_u16());
                                if st.breaker.record_failure(provider.id) {
                                    st.metrics.record_breaker_open();
                                }
                                st.metrics.record_upstream_error(provider.id);
                                usage::record_failure_row(
                                    st,
                                    &meta,
                                    "upstream_status",
                                    Some(status.as_u16()),
                                    Some(attempt_idx),
                                    502,
                                    started.elapsed().as_millis() as i64,
                                )
                                .await;
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
                                    last_failed_idx = Some(attempt_idx);
                                    last_failure = Some((
                                        502,
                                        upstream_error_envelope(&msg),
                                        "application/json".to_string(),
                                        Vec::new(),
                                    ));
                                    last_failure_upstream = Some(status.as_u16());
                                    if st.breaker.record_failure(provider.id) {
                                        st.metrics.record_breaker_open();
                                    }
                                    st.metrics.record_upstream_error(provider.id);
                                    usage::record_failure_row(
                                        st,
                                        &meta,
                                        "upstream_status",
                                        Some(status.as_u16()),
                                        Some(attempt_idx),
                                        502,
                                        started.elapsed().as_millis() as i64,
                                    )
                                    .await;
                                    continue 'candidates;
                                }
                                PrimeSignal::Productive | PrimeSignal::Terminal => {
                                    committed = true;
                                }
                                PrimeSignal::Neutral => {}
                            }
                        }
                        Some(Err(e)) => {
                            // 客户端错误体（全候选失败后经 last_failure 透传）与日志
                            // 同源：错误 Display 在聚合器风格 base_url 下可能含渠道
                            // 密钥，落响应体/日志前统一脱敏
                            let msg = redact_secrets(
                                format!("upstream stream failed before productive output: {e}"),
                                &provider_key,
                            );
                            tracing::warn!(provider = %provider.name, error = %msg, "stream priming transport error");
                            last_failed = Some(provider);
                            last_failed_idx = Some(attempt_idx);
                            // 超时（首字节/静默）→ 504；传输错误与零产出断流 → 502
                            let timed_out = is_timeout_error(e.as_ref());
                            last_failure = Some((
                                if timed_out { 504 } else { 502 },
                                upstream_error_envelope(&msg),
                                "application/json".to_string(),
                                Vec::new(),
                            ));
                            // 连接加固：流阶段超时归因 body_timeout（真实上游
                            // 状态 = 已收到的 200），其余按连接错误
                            let (class, status_out) = if timed_out {
                                ("body_timeout", 504u16)
                            } else {
                                ("connect_error", 502u16)
                            };
                            last_failure_upstream = Some(status.as_u16());
                            if st.breaker.record_failure(provider.id) {
                                st.metrics.record_breaker_open();
                            }
                            st.metrics.record_upstream_error(provider.id);
                            usage::record_failure_row(
                                st,
                                &meta,
                                class,
                                Some(status.as_u16()),
                                Some(attempt_idx),
                                status_out,
                                started.elapsed().as_millis() as i64,
                            )
                            .await;
                            if timed_out {
                                saw_timeout = true;
                            } else {
                                saw_connect_error = true;
                            }
                            continue 'candidates;
                        }
                        None => {
                            // 上游 200 但忽略 stream 参数直接回了完整 JSON 体（非 SSE）：
                            // 按「零产出断流」故障转移会白白重放请求并 502 掉一次成功
                            // 生成（转换路径 1445 同场景已合成处理，透传按 JSON 响应透出）
                            if !buffered.is_empty() && status.is_success() && !is_sse {
                                st.breaker.record_success(provider.id);
                                let mut body = Vec::with_capacity(buffered_bytes);
                                for chunk in &buffered {
                                    body.extend_from_slice(chunk);
                                }
                                let mut usage = parse_usage(&body);
                                if endpoint.anthropic
                                    && provider.api_type.eq_ignore_ascii_case("anthropic")
                                {
                                    if let Some(u) = usage.as_mut() {
                                        u.fold_anthropic_input();
                                    }
                                }
                                let latency = started.elapsed().as_millis() as i64;
                                usage::record(st, &meta, usage.as_ref(), status.as_u16(), latency)
                                    .await;
                                tracing::warn!(
                                    request_id = %request_id, provider = %provider.name,
                                    status = %status,
                                    "stream request: upstream returned JSON body ignoring stream; passing through as non-stream"
                                );
                                return Ok(attach_upstream_headers(
                                    Response::builder()
                                        .status(status)
                                        .header(header::CONTENT_TYPE, content_type.clone())
                                        .header("X-Request-Id", request_id.to_string())
                                        .body(Body::from(body))
                                        .map_err(AppError::internal)?,
                                    &rl,
                                ));
                            }
                            let msg = if buffered.is_empty() {
                                "upstream stream ended before first byte".to_string()
                            } else {
                                "upstream stream ended before productive output".to_string()
                            };
                            tracing::warn!(provider = %provider.name, "{msg}");
                            last_failed = Some(provider);
                            last_failed_idx = Some(attempt_idx);
                            last_failure = Some((
                                502,
                                upstream_error_envelope(&msg),
                                "application/json".to_string(),
                                Vec::new(),
                            ));
                            // 连接加固：体未完成即干净断流 → body_read_error
                            //（真实上游状态 = 已收到的 200）
                            last_failure_upstream = Some(status.as_u16());
                            if st.breaker.record_failure(provider.id) {
                                st.metrics.record_breaker_open();
                            }
                            st.metrics.record_upstream_error(provider.id);
                            usage::record_failure_row(
                                st,
                                &meta,
                                "body_read_error",
                                Some(status.as_u16()),
                                Some(attempt_idx),
                                502,
                                started.elapsed().as_millis() as i64,
                            )
                            .await;
                            continue 'candidates;
                        }
                    }
                }
                // 提交：熔断信号结算 + 缓冲块与剩余流拼接（字节顺序不变）
                // 连接加固：SSE 链路（tail_settles = is_sse）由流终止结算——
                // 透传链在 `SseErrorTail`（带终态 EOF → record_success；Err /
                // 缺终态 EOF → record_failure），转换链在 wrap_*_stream 内部。
                // 此处不得再记 success：同一请求会被计 2 次 requests / 1 次
                // failure，连续计数每次归零、错误率恒 0.5 < 0.6，"2xx 后必断流"
                // 的供应商两个维度都不熔断。非 SSE（缓冲路径）提交即 success。
                settle_commit_signal(&st.breaker, provider.id, tail_settles);
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
                let bytes = collect_stream_or_body(
                    &mut stream_opt,
                    &mut resp,
                    body_read_deadline,
                )
                .await;
                // F1：非 2xx + SSE 走到这里时提交点结算已被推迟（tail_settles =
                // is_sse），转换分支又不像透传链那样安装收尾流——不在此显式结算，
                // 半开探测名额永久占位。渠道给出了明确响应（可重试状态早已换家）
                // = 健康，与非 SSE 缓冲路径提交点 record_success 同语义
                if will_stream {
                    st.breaker.record_success(provider.id);
                }
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
                // 连接加固：终局透传上游错误体 → 上游错误指标 + 失败归因
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
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
                let bytes = read_body_capped(
                    resp.take().expect("resp present"),
                    MAX_UPSTREAM_BODY_BYTES,
                    body_read_deadline,
                )
                .await
                .unwrap_or_default();
                let usage = parse_usage(&bytes);
                if let Some(sse) = crate::service::responses::stream::synthesize_sse_from_chat_body(
                    &bytes,
                    &format!("resp_{request_id}"),
                    &model,
                    &tool_ctx,
                ) {
                    // 可合成完整 SSE：按上游真实状态记账（视为成功交付）
                    usage::record(st, &meta, usage.as_ref(), status.as_u16(), latency).await;
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
                // 连接加固：200 + 不可转换体 = 上游故障 → 上游错误指标 +
                // 失败归因（真实上游状态），按 502 记账并整形返回
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
                usage::record(st, &meta, usage.as_ref(), 502, latency).await;
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
                let bytes = collect_stream_or_body(
                    &mut stream_opt,
                    &mut resp,
                    body_read_deadline,
                )
                .await;
                // F1：非 2xx + SSE 的提交点结算已被推迟（tail_settles = is_sse），
                // 转换分支不安装收尾流——显式结算一次（同 anthropic 分支语义）
                if will_stream {
                    st.breaker.record_success(provider.id);
                }
                // 连接加固：终局透传上游错误体 → 上游错误指标 + 失败归因
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
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
            // W1：SSE 透传流的错误在流内转成一条错误帧后正常结束（不再把 Err
            // 交给 hyper 硬断连接）；非 SSE 字节流的流级 Err 同样截断为干净
            // EOF（连接加固：尾部 Err 不再硬断连接，客户端保留已缓冲前缀）
            // 错误帧与收尾副作用（熔断计数 / 指标 / 关联日志）的上下文随流创建一次。
            // 安装判定复用提交点的 `tail_settles`（单一来源；走到这里已排除
            // 转换链路——它们在上方分支返回）。方言跟随客户端端点：透传出口
            // 同时服务原生 Anthropic（/v1/messages + api_type=anthropic 上游）
            // 与原生 Responses 客户端。
            let sse_error = tail_settles.then(|| SseErrorContext {
                dialect: if endpoint.anthropic {
                    SseErrorDialect::Anthropic
                } else if endpoint.responses {
                    SseErrorDialect::Responses
                } else {
                    SseErrorDialect::Chat
                },
                provider_id: provider.id,
                request_id: request_id.to_string(),
                provider: provider.name.clone(),
                model: model.clone(),
                provider_key: provider_key.clone(),
                breaker: st.breaker.clone(),
                metrics: st.metrics.clone(),
            });
            let stream = wrap_stream(
                st.clone(),
                meta,
                upstream,
                latency,
                status.as_u16(),
                capture_usage,
                sse_error,
            );
            // 四-2：SSE 链路静默期向下游发 `: ping` 注释保活（长思考上游不中断连接）
            let stream = if is_sse {
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
            // 连接加固：终局失败行带归因（真实上游状态 + 最后失败候选序号）
            meta.failure_class = Some("upstream_status");
            meta.upstream_status = last_failure_upstream.map(|s| s as i16);
            meta.attempt = last_failed_idx.map(|i| i as i16);
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
        // 连接加固：全候选耗尽且无任何上游响应可透传 → 记请求级失败行
        //（all_candidates_failed；status 与呈现给客户端的终局错误一致）
        let terminal_status = if saw_timeout {
            504u16
        } else if saw_connect_error {
            502u16
        } else {
            503u16
        };
        usage::record_failure_row(
            st,
            &meta,
            "all_candidates_failed",
            None,
            None,
            terminal_status,
            started.elapsed().as_millis() as i64,
        )
        .await;
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
        // 失败归因行的 attempt 列（0 = 主渠道，1.. = 降级链）
        let mut last_failed_idx: Option<usize> = None;
        // 响应体读取的统一截止时间（僵死流兜底）
        let body_read_deadline = Duration::from_secs(st.cfg.stream_idle_timeout_secs.max(1));
        for (attempt_idx, provider) in candidates.iter().enumerate().take(max_attempts) {
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
                    if st.breaker.record_failure(provider.id) {
                        st.metrics.record_breaker_open();
                    }
                    last_failed = Some(provider);
                    continue;
                }
            };
            // F1：出站体构建失败（方言转换/序列化错误）不是渠道健康信号——
            // 此前 `?` 直接上抛，半开探测名额被永久占位。归还探测名额后换下一
            // 候选（与 decrypt 失败分支同形：last_failed 仅作尾段错误上下文归属）
            let (outbound, upstream_path) = match build_outbound(provider) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "provider outbound build failed");
                    st.breaker.release_probe(provider.id);
                    last_failed = Some(provider);
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
                // 终局透传上游错误体：非重试失败（客户端拿到真实状态码）——
                // 上游错误指标 + 失败归因随记账行落库
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
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
                    // 连接加固：候选失败三件套——熔断失败（开闸跃迁计指标）、
                    // 上游错误计数、失败归因行（类别 + 候选序号）
                    if st.breaker.record_failure(provider.id) {
                        st.metrics.record_breaker_open();
                    }
                    let (class, status_out, header_timeout) = classify_send_failure(e.as_ref());
                    st.metrics.record_upstream_error(provider.id);
                    usage::record_failure_row(
                        st,
                        &meta,
                        class,
                        None,
                        Some(attempt_idx),
                        status_out,
                        started.elapsed().as_millis() as i64,
                    )
                    .await;
                    // 连接加固：header 等待超时默认判终局（不再换候选重发——
                    // 请求体已完整写出，上游可能已在生成，重发有双份生成/计费
                    // 风险）；GATEWAY_FAILOVER_ON_HEADER_TIMEOUT=true 保留旧行为
                    if header_timeout && !st.cfg.failover_on_header_timeout {
                        return header_timeout_terminal_response(
                            provider,
                            &model,
                            request_id,
                            endpoint,
                        );
                    }
                    last_failed = Some(provider);
                    last_failed_idx = Some(attempt_idx);
                    continue;
                }
            };
            let status = resp.status();
            let content_type = content_type_of(&resp);
            // P0-7：白名单限流/退避头（供本候选各返回出口附加）
            let rl = rate_limit_headers(&resp);
            if status_retryable(status) {
                // 连接加固：429/503 带明确 Retry-After 时换候选前先等待（钳 2s
                // ——长窗口退避交由客户端 SDK；候选循环久等会拖死整条降级链）
                let wait_secs = retry_after_secs(
                    resp.headers(),
                    matches!(
                        status,
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                    ),
                );
                // 可重试：记录本次响应后尝试降级（M3：计为熔断失败）
                let bytes = read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES, body_read_deadline)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                tracing::warn!(provider = %provider.name, status = %status, "upstream retryable failure");
                last_response = Some((status.as_u16(), bytes, content_type, rl.clone()));
                if st.breaker.record_failure(provider.id) {
                    st.metrics.record_breaker_open();
                }
                // 连接加固：上游错误指标 + 失败归因行（真实上游状态）
                st.metrics.record_upstream_error(provider.id);
                usage::record_failure_row(
                    st,
                    &meta,
                    "upstream_status",
                    Some(status.as_u16()),
                    Some(attempt_idx),
                    status.as_u16(),
                    started.elapsed().as_millis() as i64,
                )
                .await;
                if let Some(secs) = wait_secs {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
                last_failed = Some(provider);
                last_failed_idx = Some(attempt_idx);
                continue;
            }
            // M34：先完整读体再记 success（读体失败=可重试传输错误，熔断器不得
            // 误标健康；cc-switch forwarder.rs:2387-2396 同款）
            let bytes = match read_body_capped(resp, MAX_UPSTREAM_BODY_BYTES, body_read_deadline)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(provider = %provider.name, error = %e, "upstream body read failed");
                    // 连接加固：读体截止 → body_timeout，其余 → body_read_error
                    let (class, status_out) = classify_body_read_failure(e.as_ref());
                    if st.breaker.record_failure(provider.id) {
                        st.metrics.record_breaker_open();
                    }
                    st.metrics.record_upstream_error(provider.id);
                    usage::record_failure_row(
                        st,
                        &meta,
                        class,
                        None,
                        Some(attempt_idx),
                        status_out,
                        started.elapsed().as_millis() as i64,
                    )
                    .await;
                    last_failed = Some(provider);
                    last_failed_idx = Some(attempt_idx);
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
                // 连接加固：终局上游故障（协议违约体）→ 上游错误指标 + 失败归因
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
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
            let mut usage = parse_usage(&bytes);
            // 原生 anthropic 透传：input 不含缓存桶，先归一为 OpenAI 口径再入账
            if endpoint.anthropic && provider.api_type.eq_ignore_ascii_case("anthropic") {
                if let Some(u) = usage.as_mut() {
                    u.fold_anthropic_input();
                }
            }
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
            // 连接加固：非成功终局（转换失败 / 非成功状态透传）→ 上游错误指标
            // + 失败归因（真实上游状态随归因列落库；status 列写呈现给客户端的
            // 终局状态，状态页错误率与客户端所见一致）
            if !response_status.is_success() {
                st.metrics.record_upstream_error(provider.id);
                meta.failure_class = Some("upstream_status");
                meta.upstream_status = Some(status.as_u16() as i16);
                meta.attempt = Some(attempt_idx as i16);
            }
            usage::record(st, &meta, usage.as_ref(), response_status.as_u16(), latency).await;
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
        // 全部候选失败：优先透传最后一次上游响应，否则 503
        if let Some((status, bytes, content_type, rl)) = last_response {
            let latency = started.elapsed().as_millis() as i64;
            // 连接加固：终局失败行带归因（真实上游状态 + 最后失败候选序号）
            meta.failure_class = Some("upstream_status");
            meta.upstream_status = Some(status as i16);
            meta.attempt = last_failed_idx.map(|i| i as i16);
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
        // 连接加固：全候选耗尽且无任何上游响应可透传 → 记请求级失败行
        usage::record_failure_row(
            st,
            &meta,
            "all_candidates_failed",
            None,
            None,
            503,
            started.elapsed().as_millis() as i64,
        )
        .await;
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
    let bytes = read_body_capped(
        resp,
        MAX_UPSTREAM_BODY_BYTES,
        // 连接加固：4xx 错误体预读同样有僵死流兜底
        Duration::from_secs(st.cfg.stream_idle_timeout_secs.max(1)),
    )
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
            // 重试体已被改写：复用客户端 idempotency-key 会命中上游
            // 「同 key 不同体」冲突拒绝（Anthropic 幂等语义），改写腿必须换键
            let mut retry_headers = client_headers.clone();
            retry_headers.remove("idempotency-key");
            let retried = send_upstream(
                st,
                provider,
                provider_key,
                &fixed,
                endpoint,
                request_id,
                streamed,
                &retry_headers,
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
///（转换路径错误整形共用；防 resp 已被流式预读消费后的 expect panic）。
/// deadline 兜底僵死流（连接加固）。
async fn collect_stream_or_body(
    stream_opt: &mut Option<
        Pin<Box<dyn Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>,
    >,
    resp: &mut Option<reqwest::Response>,
    deadline: Duration,
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
        Some(r) => read_body_capped(r, MAX_UPSTREAM_BODY_BYTES, deadline)
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
        // 完整 UTF-8 BOM 剥离：只剥首字节 0xEF 会残留 BB BF，后续 SSE/JSON
        // 判定照旧失败（P1-15 的容忍目标就是 content-type 说谎的上游）
        if s.starts_with(b"\xEF\xBB\xBF") {
            s = &s[3..];
        }
        while let Some((first, rest)) = s.split_first() {
            if matches!(first, b' ' | b'\t' | b'\r' | b'\n') {
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
        // 完整 UTF-8 BOM 剥离（同 looks_like_sse）：残留 BB BF 会让
        // serde_json 解析失败，错误 envelope 逃过换家判定
        if s.starts_with(b"\xEF\xBB\xBF") {
            s = &s[3..];
        }
        while let Some((first, rest)) = s.split_first() {
            if matches!(first, b' ' | b'\t' | b'\r' | b'\n') {
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

/// 连接加固：header 等待超时且未启用 failover（GATEWAY_FAILOVER_ON_HEADER_TIMEOUT
/// = false，默认）→ 该候选直接判终局 504，按客户端方言整形后返回，不再换候选
/// 重发——请求体已完整写出，上游可能已在生成，重发有双份生成/计费风险。
fn header_timeout_terminal_response(
    provider: &Provider,
    model: &str,
    request_id: Uuid,
    endpoint: Endpoint,
) -> Result<Response, AppError> {
    let ctx = error_context(provider, model, request_id);
    let body = if endpoint.anthropic {
        crate::service::anthropic::reshape_upstream_error(504, b"upstream header timeout", &ctx)
    } else if endpoint.responses {
        crate::service::responses::reshape_upstream_error_for_responses(
            504,
            b"upstream header timeout",
            &ctx,
        )
    } else {
        upstream_error_envelope("upstream header timeout")
    };
    Ok(Response::builder()
        .status(StatusCode::GATEWAY_TIMEOUT)
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Request-Id", request_id.to_string())
        .body(Body::from(body))
        .map_err(AppError::internal)?)
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
/// 连接加固：header 等待时限（毫秒；None = 不等响应头）。
/// - `timeout_ms == 0`：显式关闭 header 等待（请求体写出后无限期等头；
///   上游慢由流式首字节 / 非流式总超时兜底）
/// - `>= 1000`：原值
/// - 其余（1..=999 及负值等非法配置）：钳到 1000ms（历史兜底语义）
/// - 非流式且 `non_stream_timeout_secs > 0`（两者都启用）：等待取
///   `max(timeout_ms, non_stream_timeout_secs*1000/2)`——缓冲型上游完整
///   生成后才回响应头，不得被 120s header 等待在 600s 非流式预算内掐断
fn header_wait_ms(timeout_ms: i32, streamed: bool, non_stream_timeout_secs: u64) -> Option<u64> {
    if timeout_ms == 0 {
        return None;
    }
    let mut ms = if timeout_ms >= 1000 {
        timeout_ms as u64
    } else {
        1000
    };
    if !streamed && non_stream_timeout_secs > 0 {
        ms = ms.max(non_stream_timeout_secs.saturating_mul(1000) / 2);
    }
    Some(ms)
}

/// 连接加固：429/503 的 Retry-After 秒数（仅数字秒形式；缺失 / 非法 /
/// HTTP-date 形态 → None）。调用方在换候选前按该值等待（helper 已钳 [0, 2]，
/// 候选循环内绝不长等）。
fn retry_after_secs(headers: &HeaderMap, applicable: bool) -> Option<u64> {
    if !applicable {
        return None;
    }
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .map(|s| s.clamp(0, 2) as u64)
}

/// send 阶段候选失败归因：header 等待超时 → (header_timeout, 504)；其余
/// （连接超时 / 拒绝 / 重置）→ (connect_error, 502)。
/// 返回 (失败类别, 呈现给客户端的状态码, 是否 header 等待超时)。
fn classify_send_failure(
    e: &(dyn Error + Send + Sync + 'static),
) -> (&'static str, u16, bool) {
    if e.to_string().contains("header timeout") {
        ("header_timeout", 504, true)
    } else {
        ("connect_error", 502, false)
    }
}

/// 读体阶段候选失败归因：读体截止（"upstream body read timeout"）→
/// (body_timeout, 504)；其余读体错误（解码 / 连接中断 / 超限）→
/// (body_read_error, 502)。
fn classify_body_read_failure(
    e: &(dyn Error + Send + Sync + 'static),
) -> (&'static str, u16) {
    if e.to_string().contains("body read timeout") {
        ("body_timeout", 504)
    } else {
        ("body_read_error", 502)
    }
}

/// 发送上游请求；timeout_ms 作用于响应头等待（见 [`header_wait_ms`]，0 = 不等头）；
/// 非流式额外施加请求总超时（含响应体）。
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
    // 连接加固：header 等待语义（见 header_wait_ms）：timeout_ms=0 → 不等头；
    // 1..=999 钳 1000ms；非流式等待不早于非流式总预算的一半（缓冲型上游
    // 完整生成后才回响应头）
    let send = req.send();
    let resp = match header_wait_ms(provider.timeout_ms, streamed, st.cfg.non_stream_timeout_secs)
    {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), send)
            .await
            .map_err(|_| "upstream header timeout".to_string())?,
        None => send.await,
    };
    resp.map_err(|e| redact_secrets(format!("upstream request failed: {e}"), provider_key).into())
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
        // 连接加固：半开探测名额归还——流未结算即被丢弃时只释放名额不计数
        // （见 QC 评审 S-2：极端时序下可能清除下一位探针，暂不处理）
        self.st.breaker.release_probe(self.meta.provider_id);
        if !self.recorded.swap(true, Ordering::SeqCst) {
            // 连接加固：客户端中断计数（流式响应中途断开/丢弃）
            self.st
                .metrics
                .client_abort_total
                .fetch_add(1, Ordering::Relaxed);
            let st = self.st.clone();
            let meta = self.meta.clone();
            let usage = self.captured.lock().take();
            let status = self.status;
            let latency = self.latency;
            // 断开时点即锁定 gen_ms，避免 spawn 调度延迟被计入生成长度
            let gen_ms = usage::gen_ms_at(&meta, latency);
            // 尽力而为：客户端中断后无法确认上游实际消耗，按已捕获 usage 记账
            tokio::spawn(async move {
                tracing::warn!(request_id = %meta.request_id, "stream dropped before completion; billing best-effort");
                usage::record_with_gen_ms(&st, &meta, usage.as_ref(), status, latency, gen_ms).await;
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

/// SSE 透传出口的错误帧方言：跟随客户端端点，客户端 SDK 才能把终止事件归为
/// typed error（而不是没有 event 名的裸 data 帧）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SseErrorDialect {
    /// `/v1/chat/completions`（OpenAI 兼容）：仅 data 帧
    Chat,
    /// 原生 Anthropic `/v1/messages`（api_type=anthropic 上游透传）
    Anthropic,
    /// 原生 Responses `/v1/responses`（原生 Responses 上游透传）
    Responses,
}

/// 构造 SSE 错误帧与收尾副作用所需的上下文：随流创建一次，此后复用。
struct SseErrorContext {
    dialect: SseErrorDialect,
    /// 上游渠道 id：提交后失败据此记一次熔断（让"200 + 错误帧"的供应商被隔离）
    provider_id: i64,
    /// 日志关联字段：提交后断流能定位到具体请求 / 渠道 / 模型
    request_id: String,
    provider: String,
    model: String,
    /// 客户端错误文本落帧前经 `redact_secrets` 脱敏（聚合成 base_url 形态时
    /// 错误 Display 可能含密钥）
    provider_key: String,
    breaker: Arc<Breaker>,
    /// 进程内指标（上游错误 / 熔断开闸计数）
    metrics: Arc<crate::metrics::Metrics>,
}

/// SSE 错误帧：透传链路在响应已提交后遇到上游错误时，用一条**客户端方言对应**
/// 的终态错误事件收尾（不追加 `[DONE]`/`message_stop`——流本就未正常结束）。
/// 帧形状与错误码与各自转换链路同词表：Anthropic 见
/// `anthropic/stream.rs::error_event`，Responses 见
/// `responses/stream.rs::record_transport_error`（`stream_error`）。
fn sse_error_frame(ctx: &SseErrorContext, msg: &str) -> Bytes {
    match ctx.dialect {
        // Chat：仅 data 帧、无 event 名（OpenAI Chat SDK 按帧内 error 字段归错）
        SseErrorDialect::Chat => {
            let payload = serde_json::json!({
                "error": {"message": msg, "type": "upstream_error", "code": "upstream_error"}
            });
            Bytes::from(format!("data: {payload}\n\n"))
        }
        // Anthropic：`event: error` + `{"type":"error","error":{…}}`；传输层故障
        // 与转换链路一致归 stream_error
        SseErrorDialect::Anthropic => {
            let payload = serde_json::json!({
                "type": "error",
                "error": {"type": "stream_error", "message": msg}
            });
            Bytes::from(format!("event: error\ndata: {payload}\n\n"))
        }
        // Responses：OpenAI Responses 规范的流内错误事件
        //（`event: error` + `{"type":"error","code":…,"message":…}`）。
        // 不伪造 `response.failed`：透传链没有权威 response id（上游的
        // `response.created` 已把上游 id 发给客户端）也没有累计 output，
        // 伪造会与客户端已收到的流自相矛盾
        SseErrorDialect::Responses => {
            let payload = serde_json::json!({
                "type": "error",
                "code": "stream_error",
                "message": msg
            });
            Bytes::from(format!("event: error\ndata: {payload}\n\n"))
        }
    }
}

/// 提交时结算熔断信号：`tail_settles`（连接加固后 = `is_sse`——所有 SSE 链路
/// 的信号都由流终止结算：透传链在 `SseErrorTail`，转换链在各自 `wrap_*_stream`
/// 内部）为真时**不在此记 success**，成功/失败都由流终态各记一次，保证每个
/// 请求只产生一个熔断信号。非 SSE（缓冲路径）保持原语义：渠道有响应且体完整
/// 读出 = 提交即 success。`tail_settles` 由调用方与错误收尾上下文的安装点共用
/// 同一判定。
fn settle_commit_signal(breaker: &Breaker, provider_id: i64, tail_settles: bool) {
    if !tail_settles {
        breaker.record_success(provider_id);
    }
}

/// 上游流错误收尾：SSE 响应把 `Err` 转成一条方言对应的错误帧后正常结束（不再把
/// `Err` 交给 hyper 导致 chunked body 硬断）；非 SSE（`error_ctx == None`）的
/// `Err` 按干净 EOF 截断（连接加固：chunked JSON 体的尾部 Err 不再 hyper 硬断
/// 连接，客户端拿到已缓冲的完整前缀）。
struct SseErrorTail<S> {
    inner: Pin<Box<S>>,
    error_ctx: Option<SseErrorContext>,
    finished: bool,
    /// 透传字节里是否见过方言终态标记（连接加固）：干净 EOF 但无终态 = 上游
    /// 流未正常结束，按失败结算并给客户端补发错误帧
    saw_terminal: bool,
}

/// 各方言流正常结束的终态标记（字节级扫描；标记跨 chunk 分裂时不识别，
/// 与 first_chunk_error 同一粒度权衡）
fn has_terminal_marker(dialect: SseErrorDialect, chunk: &[u8]) -> bool {
    const ANTHROPIC: &[&str] = &["message_stop"];
    const CHAT: &[&str] = &["data: [DONE]"];
    const RESPONSES: &[&str] = &[
        "response.completed",
        "response.incomplete",
        "response.failed",
    ];
    let markers: &[&str] = match dialect {
        SseErrorDialect::Anthropic => ANTHROPIC,
        SseErrorDialect::Chat => CHAT,
        SseErrorDialect::Responses => RESPONSES,
    };
    markers
        .iter()
        .any(|m| chunk.windows(m.len()).any(|w| w == m.as_bytes()))
}

impl<S> Stream for SseErrorTail<S>
where
    S: Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>>,
{
    type Item = Result<Bytes, Box<dyn Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 全字段 Unpin，get_mut 安全
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(ctx) = &this.error_ctx {
                    if has_terminal_marker(ctx.dialect, &chunk) {
                        this.saw_terminal = true;
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finished = true;
                let Some(ctx) = &this.error_ctx else {
                    // 连接加固：非 SSE chunked 体的流级 Err 不再交给 hyper 硬断
                    // 连接——截断为干净 EOF，客户端拿到已缓冲的完整前缀
                    tracing::warn!(%e, "non-SSE upstream stream error; truncating body cleanly");
                    return Poll::Ready(None);
                };
                // 终止性流内错误 = 上游在提交后失败：记一次熔断失败，让"200 +
                // 错误帧"的供应商在后续请求被降级链跳过（`finished` 保证只记一次）。
                // 客户端断开时流被 drop、不会产生 `Err`，天然不计。
                if ctx.breaker.record_failure(ctx.provider_id) {
                    ctx.metrics.record_breaker_open();
                }
                ctx.metrics.record_upstream_error(ctx.provider_id);
                // 客户端只看到脱敏文本；日志保留可排障细节（密钥同样替换为 ***）
                let msg = redact_secrets(e.to_string(), &ctx.provider_key);
                tracing::warn!(
                    request_id = %ctx.request_id,
                    provider = %ctx.provider,
                    model = %ctx.model,
                    error = %msg,
                    "upstream stream error after commit; emitting SSE error frame"
                );
                Poll::Ready(Some(Ok(sse_error_frame(ctx, &msg))))
            }
            Poll::Ready(None) => {
                this.finished = true;
                let Some(ctx) = &this.error_ctx else {
                    return Poll::Ready(None);
                };
                if this.saw_terminal {
                    // 正常结束（已见终态标记）：提交时未记 success，由这里补记
                    // 一次——与 `Err` 分支的 `record_failure` 互斥，每个请求
                    // 只产生一个熔断信号
                    ctx.breaker.record_success(ctx.provider_id);
                    return Poll::Ready(None);
                }
                // 连接加固：干净 EOF 但未见方言终态标记 = 上游流在终态事件前
                // 结束（"200 后静默断流"）——不得记成功；记熔断失败并给客户端
                // 补发一条方言错误帧后正常结束
                if ctx.breaker.record_failure(ctx.provider_id) {
                    ctx.metrics.record_breaker_open();
                }
                ctx.metrics.record_upstream_error(ctx.provider_id);
                tracing::warn!(
                    request_id = %ctx.request_id,
                    provider = %ctx.provider,
                    model = %ctx.model,
                    "upstream stream ended before terminal event"
                );
                Poll::Ready(Some(Ok(sse_error_frame(
                    ctx,
                    "upstream stream ended before terminal event",
                ))))
            }
            other => other,
        }
    }
}

impl<S> Drop for SseErrorTail<S> {
    fn drop(&mut self) {
        // 客户端断开 / 流被丢弃且从未结算（既无 EOF 也无 Err）：提交时未记 success，
        // 这里也不计数，只归还半开探测名额——否则被中断的探测请求会让
        // `half_open_probe_in_flight` 永久占位，该渠道之后再也无法 `allow` 放行
        if self.finished {
            return;
        }
        if let Some(ctx) = &self.error_ctx {
            ctx.breaker.release_probe(ctx.provider_id);
        }
    }
}

/// 流式透传 + 流内 usage 捕获与记账：
/// - capture_usage = true（上游 2xx）：逐帧合并 usage（后帧非 None 字段覆盖），
///   终态事件（[DONE]/response.*/message_stop）或流尾兜底记账（仅一次），字节流原样透传
/// - capture_usage = false（上游非 2xx）：错误体不解析不计量，按真实状态记一条零 token 明细
/// - 流异常中断时由 chain 兜底记账（含 EOF flush 后的 captured）；客户端断开由 BillingOnDrop 兜底
/// - `sse_error = Some(ctx)`（SSE 响应）：上游 `Err` 转成一条该方言的错误帧后正常结束
///   （记熔断失败）；干净 EOF 未见方言终态标记同样记失败并补发错误帧，见到终态
///   标记才记成功（提交处不再记 success）；流被 drop 时归还半开探测名额。
///   `None`（非 SSE 流）：流级 `Err` 截断为干净 EOF（不再交给 hyper 硬断连接）
fn wrap_stream(
    st: AppState,
    meta: UsageMeta,
    upstream: impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send + 'static,
    latency: i64,
    record_status: u16,
    capture_usage: bool,
    sse_error: Option<SseErrorContext>,
) -> BillingStream {
    let upstream = SseErrorTail {
        inner: Box::pin(upstream),
        error_ctx: sse_error,
        finished: false,
        saw_terminal: false,
    };
    // 逐 chunk 共享上下文：流适配器每帧只能克隆一个 Arc，替代此前每帧克隆
    // AppState（~20 个 Arc 原子操作）+ UsageMeta（String/Vec 堆分配）
    let ctx = Arc::new(UsageCapture {
        recorded: Arc::new(AtomicBool::new(false)),
        captured: Arc::new(parking_lot::Mutex::new(None)),
        buf: Arc::new(parking_lot::Mutex::new(Vec::new())),
        st,
        meta,
    });

    let main = upstream.then({
        let ctx = ctx.clone();
        move |chunk| {
            let ctx = ctx.clone();
            async move {
                let chunk = chunk?;
                let (usage, done) = if capture_usage {
                    feed_sse(&mut ctx.buf.lock(), &chunk)
                } else {
                    (None, false)
                };
                // 合并进 captured（后帧非 None 覆盖），不提前记账——anthropic 流
                // message_start 桩（output≈0）先于 message_delta 终值到达
                if let Some(u) = usage {
                    merge_usage(&mut ctx.captured.lock(), u);
                }
                // 记账只在终态事件（[DONE]/response.*/message_stop）触发一次
                if done && !ctx.recorded.swap(true, Ordering::SeqCst) {
                    let u = ctx.captured.lock().take();
                    usage::record(&ctx.st, &ctx.meta, u.as_ref(), record_status, latency).await;
                }
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(chunk)
            }
        }
    });

    let tail = once({
        let ctx = ctx.clone();
        async move {
            // EOF flush：流可能终止在未以空行收尾的最后事件（宽松上游不发
            // 尾空行），追加事件边界强制解析残余 buffer，终态 usage 才不丢
            if capture_usage {
                let (usage, _) = feed_sse(&mut ctx.buf.lock(), b"\n\n");
                if let Some(u) = usage {
                    merge_usage(&mut ctx.captured.lock(), u);
                }
            }
            // 上游未正常结束（无 [DONE]/usage）时兜底记账
            if !ctx.recorded.swap(true, Ordering::SeqCst) {
                let u = ctx.captured.lock().take();
                usage::record(&ctx.st, &ctx.meta, u.as_ref(), record_status, latency).await;
            }
            Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::new())
        }
    });

    BillingStream {
        inner: Box::pin(main.chain(tail)),
        _billing: BillingOnDrop {
            st: ctx.st.clone(),
            meta: ctx.meta.clone(),
            recorded: ctx.recorded.clone(),
            captured: ctx.captured.clone(),
            status: record_status,
            latency,
        },
    }
}

/// wrap_stream 逐 chunk 共享的记账上下文（含兜底记账与 BillingOnDrop 所需全部句柄）
struct UsageCapture {
    recorded: Arc<AtomicBool>,
    captured: Arc<parking_lot::Mutex<Option<Usage>>>,
    buf: Arc<parking_lot::Mutex<Vec<u8>>>,
    st: AppState,
    meta: UsageMeta,
}

/// 从 OpenAI 兼容响应体解析 usage
pub fn parse_usage(body: &[u8]) -> Option<Usage> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let u = value.get("usage")?;
    serde_json::from_value(u.clone()).ok()
}

/// 从单条 SSE data 帧提取 usage，按事件类型区分方言：
/// - anthropic（type: message_*）：message_start 嵌在 message.usage，message_delta 在顶层
/// - Chat：顶层 usage；Responses：response 内 usage
/// 返回 (usage, 是否 anthropic 口径)——anthropic 的 input_tokens 不含缓存桶，
/// 调用方须 fold_anthropic_input 归一后再入账。
fn extract_usage_frame(v: &Value) -> Option<(Usage, bool)> {
    let anthropic = v
        .get("type")
        .and_then(|t| t.as_str())
        .map(|t| t.starts_with("message_"))
        .unwrap_or(false);
    let u = if anthropic {
        v.get("message")
            .and_then(|m| m.get("usage"))
            .or_else(|| v.get("usage"))
    } else {
        v.get("usage")
            .or_else(|| v.get("response").and_then(|r| r.get("usage")))
    }?;
    serde_json::from_value(u.clone())
        .ok()
        .map(|u| (u, anthropic))
}

/// 流式 usage 帧合并：后帧非 None 字段覆盖前帧，缺省字段保留前帧。
/// anthropic 流 message_start 给 input/缓存桶桩，message_delta 才补终值 output。
fn merge_usage(dst: &mut Option<Usage>, src: Usage) {
    match dst {
        None => *dst = Some(src),
        Some(d) => {
            if src.prompt_tokens.is_some() {
                d.prompt_tokens = src.prompt_tokens;
            }
            if src.completion_tokens.is_some() {
                d.completion_tokens = src.completion_tokens;
            }
            if src.input_tokens.is_some() {
                d.input_tokens = src.input_tokens;
            }
            if src.output_tokens.is_some() {
                d.output_tokens = src.output_tokens;
            }
            if src.cache_read_tokens.is_some() {
                d.cache_read_tokens = src.cache_read_tokens;
            }
            if src.cache_write_tokens.is_some() {
                d.cache_write_tokens = src.cache_write_tokens;
            }
        }
    }
}

/// 增量解析 SSE 字节流：返回 (本次新解析到的合并 usage, 是否遇到终态)
/// 缓冲跨块保留，事件按 "\n\n" 或 "\r\n\r\n" 切分。终态 = [DONE] /
/// response.completed|incomplete|failed|done / message_stop（anthropic 无 [DONE]）。
pub fn feed_sse(buf: &mut Vec<u8>, chunk: &[u8]) -> (Option<Usage>, bool) {
    buf.extend_from_slice(chunk);
    let mut usage: Option<Usage> = None;
    let mut done = false;
    loop {
        let Some(idx) = find_event_end(buf) else {
            break;
        };
        let event: Vec<u8> = buf.drain(..idx).collect();
        let text = String::from_utf8_lossy(&event);
        // 帧级预筛：content delta 帧占流中绝对多数且必无 usage/终态，跳过
        // serde DOM 解析（透传路径每 token 的最大单项开销）
        if !frame_may_carry_usage_or_terminal(&text) {
            continue;
        }
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data == "[DONE]" {
                    done = true;
                } else if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some((mut frame, anthropic)) = extract_usage_frame(&v) {
                        if anthropic {
                            // 原生 anthropic 口径归一为 OpenAI（input 含缓存桶），
                            // 记账/计价/Plan 计数全链路才与 Chat 方言一致
                            frame.fold_anthropic_input();
                        }
                        merge_usage(&mut usage, frame);
                    }
                    if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
                        if matches!(
                            ty,
                            "response.completed"
                                | "response.incomplete"
                                | "response.failed"
                                | "response.done"
                                | "message_stop"
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

/// feed_sse 帧级预筛：这些标记在序列化 JSON/文本中必以字面子串出现（键名与
/// 类型值不做转义），缺失 ⇒ 该帧必不含 usage 也不是终态。字符串值偶含同形
/// 子串只会多解析一次（假阳性无害），漏检不可能——usage 帧必含键 "usage"，
/// 终态帧必含对应 type 字面量，[DONE] 帧即 "[DONE]"。
fn frame_may_carry_usage_or_terminal(text: &str) -> bool {
    const TERMINAL_TYPES: [&str; 5] = [
        "response.completed",
        "response.incomplete",
        "response.failed",
        "response.done",
        "message_stop",
    ];
    text.contains("[DONE]")
        || text.contains("usage")
        || TERMINAL_TYPES.iter().any(|t| text.contains(t))
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

    /// P1-18：Anthropic message_start 的 usage 嵌在 message 对象内；原生
    /// anthropic 口径 input 不含缓存桶 → 捕获侧折算为 OpenAI 口径（input 含桶）
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
        // 折叠：input = 10 + 5(写) + 7(读) = 22；缓存桶原样保留供分桶计价
        assert_eq!(u.input_tokens, Some(22));
        assert_eq!(u.cache_write_tokens, Some(5));
        assert_eq!(u.cache_read_tokens, Some(7));
        assert_eq!(u.total(), 22);
        // 顶层 Chat usage 形态仍优先
        let mut buf2 = Vec::new();
        let (usage2, _) = feed_sse(
            &mut buf2,
            b"data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n",
        );
        assert_eq!(usage2.expect("top-level usage").prompt_tokens, Some(3));
    }

    /// 原生 anthropic 流：message_start 桩（output≈0）+ message_delta 终值
    /// → 帧合并后 output 取终值，input/缓存桶保留首帧
    #[test]
    fn feed_sse_merges_anthropic_message_delta_final_usage() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(
            &mut buf,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\
              \"input_tokens\":25000,\"output_tokens\":1}}}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3500}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert!(done, "message_stop 应判定终态");
        let u = usage.expect("merged usage");
        assert_eq!(u.input_tokens, Some(25000));
        assert_eq!(u.output(), 3500, "终帧 output 覆盖桩值");
    }

    /// EOF flush：流终止在未以空行收尾的末事件，wrap_stream 尾部追加事件
    /// 边界后残余帧必须可解析（此前永不解析 → usage 丢成 0）
    #[test]
    fn feed_sse_eof_flush_parses_unterminated_tail_event() {
        let mut buf = Vec::new();
        let (usage, done) = feed_sse(
            &mut buf,
            b"data: {\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":1}}\n",
        );
        assert!(!done);
        assert!(usage.is_none(), "未收尾事件不在解析窗口内");
        // 尾部兜底：追加事件边界强制解析
        let (usage, _) = feed_sse(&mut buf, b"\n\n");
        let u = usage.expect("residual event flushed at EOF");
        assert_eq!(u.input(), 9);
        assert_eq!(u.output(), 1);
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

    /// W1：超时分类——既有超时文案与链式 source 都命中，连接失败不误判
    #[test]
    fn is_timeout_error_walks_source_chain_and_texts() {
        let header: Box<dyn Error + Send + Sync> = "upstream header timeout".into();
        assert!(is_timeout_error(header.as_ref()));
        let first_byte: Box<dyn Error + Send + Sync> =
            "upstream stream first byte timeout".into();
        assert!(is_timeout_error(first_byte.as_ref()));
        let idle: Box<dyn Error + Send + Sync> = "upstream stream idle timeout".into();
        assert!(is_timeout_error(idle.as_ref()));
        let refused: Box<dyn Error + Send + Sync> =
            "upstream request failed: connection refused".into();
        assert!(!is_timeout_error(refused.as_ref()), "连接失败不是超时");

        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("upstream stream idle timeout")
            }
        }
        impl Error for Inner {}
        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("wrapped transport failure")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }
        let wrapped: Box<dyn Error + Send + Sync> = Box::new(Outer(Inner));
        assert!(is_timeout_error(wrapped.as_ref()), "外层文案无关，source 链命中");
    }

    /// 熔断器（测试用）：只让连续失败计数生效，错误率维度需要 1000 个样本
    fn test_breaker(failure_threshold: u32) -> Arc<Breaker> {
        Arc::new(Breaker::new_with(crate::service::breaker::BreakerConfig {
            failure_threshold,
            min_requests: 1_000,
            ..Default::default()
        }))
    }

    /// 生产调用序列的最小复刻：一次已提交的透明 SSE 透传请求 =
    /// 提交结算（`tail_settles` → 不记 success）+ `SseErrorTail` 驱动到终止
    async fn drive_committed_sse(
        breaker: &Arc<Breaker>,
        events: Vec<Result<Bytes, Box<dyn Error + Send + Sync>>>,
    ) {
        settle_commit_signal(breaker, 7, true);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(futures_util::stream::iter(events)),
            error_ctx: Some(sse_ctx(SseErrorDialect::Chat, breaker.clone())),
            finished: false,
            saw_terminal: false,
        });
        while tail.next().await.is_some() {}
    }

    fn sse_ctx(dialect: SseErrorDialect, breaker: Arc<Breaker>) -> SseErrorContext {
        SseErrorContext {
            dialect,
            provider_id: 7,
            request_id: "req_test".into(),
            provider: "prov".into(),
            model: "gpt-x".into(),
            provider_key: "sk-secret".into(),
            breaker,
            metrics: Arc::new(crate::metrics::Metrics::new()),
        }
    }

    /// W1：SSE 流错误 → 单条错误帧后正常结束（无 Err 泄漏、无 [DONE] 伪造）；
    /// 非 SSE 流级 Err 截断为干净 EOF（连接加固：不再交给 hyper 硬断连接）
    #[tokio::test]
    async fn sse_error_tail_wraps_stream_error_and_ends() {
        let head = Bytes::from_static(b"data: {\"choices\":[]}\n\n");
        let inner = futures_util::stream::iter(vec![
            Ok::<_, Box<dyn Error + Send + Sync>>(head.clone()),
            Err(Box::<dyn Error + Send + Sync>::from(
                "upstream stream idle timeout",
            )),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ]);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(inner),
            error_ctx: Some(sse_ctx(SseErrorDialect::Chat, test_breaker(4))),
            finished: false,
            saw_terminal: false,
        });
        assert_eq!(tail.next().await.unwrap().unwrap(), head);
        let frame = tail.next().await.unwrap().expect("Err 转为错误帧");
        let text = String::from_utf8(frame.to_vec()).unwrap();
        assert!(text.starts_with("data: ") && text.ends_with("\n\n"));
        assert!(!text.contains("[DONE]"), "错误收尾不得伪造 [DONE]");
        let v: serde_json::Value =
            serde_json::from_str(text["data: ".len()..].trim_end()).unwrap();
        assert_eq!(v["error"]["type"], "upstream_error");
        assert_eq!(v["error"]["code"], "upstream_error");
        assert_eq!(v["error"]["message"], "upstream stream idle timeout");
        assert!(tail.next().await.is_none(), "错误后流结束，后续块不透传");

        // 非 SSE：Err 截断为干净 EOF（客户端保留已缓冲前缀，不再 Err 泄漏）
        let inner = futures_util::stream::iter(vec![Err::<Bytes, Box<dyn Error + Send + Sync>>(
            Box::from("boom"),
        )]);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(inner),
            error_ctx: None,
            finished: false,
            saw_terminal: false,
        });
        assert!(tail.next().await.is_none(), "非 SSE Err 截断为干净 EOF");
        assert!(tail.next().await.is_none());
    }

    /// 透传错误帧按客户端方言选形状：三种帧的 event 名与字段各自成立，
    /// 错误类型词表与转换链路一致（`stream_error`）
    #[test]
    fn sse_error_frame_shapes_follow_client_dialect() {
        let ctx = |dialect| sse_ctx(dialect, test_breaker(4));

        // Chat：只有 data 帧，无 event 名
        let chat = sse_error_frame(&ctx(SseErrorDialect::Chat), "boom");
        let chat = String::from_utf8(chat.to_vec()).unwrap();
        assert!(
            chat.starts_with("data: ") && chat.ends_with("\n\n"),
            "{chat}"
        );
        assert!(!chat.contains("event: "), "Chat 帧不得带 event 名: {chat}");
        let v: Value = serde_json::from_str(chat["data: ".len()..].trim_end()).unwrap();
        assert_eq!(v["error"]["type"], "upstream_error");
        assert_eq!(v["error"]["code"], "upstream_error");
        assert_eq!(v["error"]["message"], "boom");
        assert!(v.get("type").is_none(), "Chat 帧不得有 Anthropic 顶层 type");

        // Anthropic：event: error + type/error 外层，错误类型与转换链路一致为 stream_error
        let anthropic = sse_error_frame(&ctx(SseErrorDialect::Anthropic), "boom");
        let anthropic = String::from_utf8(anthropic.to_vec()).unwrap();
        assert!(
            anthropic.starts_with("event: error\ndata: ") && anthropic.ends_with("\n\n"),
            "{anthropic}"
        );
        let v: Value =
            serde_json::from_str(anthropic["event: error\ndata: ".len()..].trim_end()).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "stream_error");
        assert_eq!(v["error"]["message"], "boom");

        // Responses：OpenAI Responses 规范的流内错误事件（event: error + 顶层
        // type/code/message）；不伪造 response.failed（透传链没有权威 response id
        // 与累计 output）
        let responses = sse_error_frame(&ctx(SseErrorDialect::Responses), "boom");
        let responses = String::from_utf8(responses.to_vec()).unwrap();
        assert!(
            responses.starts_with("event: error\ndata: ") && responses.ends_with("\n\n"),
            "{responses}"
        );
        let v: Value =
            serde_json::from_str(responses["event: error\ndata: ".len()..].trim_end()).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "stream_error");
        assert_eq!(v["message"], "boom");
        assert!(
            v.get("response").is_none(),
            "不得伪造 response.failed 响应体: {responses}"
        );
    }

    /// 方言接线：同一上游 Err 经 tail 转成对应 event 名；非 SSE 不转换
    #[tokio::test]
    async fn sse_error_tail_emits_dialect_frames_and_passes_non_sse_errors() {
        for dialect in [SseErrorDialect::Anthropic, SseErrorDialect::Responses] {
            let inner =
                futures_util::stream::iter(vec![Err::<Bytes, _>(
                    Box::<dyn Error + Send + Sync>::from("upstream stream idle timeout"),
                )]);
            let mut tail = Box::pin(SseErrorTail {
                inner: Box::pin(inner),
                error_ctx: Some(sse_ctx(dialect, test_breaker(4))),
                finished: false,
                saw_terminal: false,
            });
            let frame = tail.next().await.unwrap().expect("Err 转为方言错误帧");
            let text = String::from_utf8(frame.to_vec()).unwrap();
            assert!(text.starts_with("event: error\n"), "{dialect:?}: {text}");
            assert!(text.ends_with("\n\n"), "{dialect:?}: {text}");
            assert!(
                tail.next().await.is_none(),
                "{dialect:?}: 错误帧后流必须结束"
            );
        }

        // 非 SSE（error_ctx None）：Err 截断为干净 EOF，不做帧转换
        let inner = futures_util::stream::iter(vec![Err::<Bytes, _>(
            Box::<dyn Error + Send + Sync>::from("boom"),
        )]);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(inner),
            error_ctx: None,
            finished: false,
            saw_terminal: false,
        });
        assert!(tail.next().await.is_none(), "非 SSE Err 截断为干净 EOF");
        assert!(tail.next().await.is_none());
    }

    /// QC2 S1：进客户端错误帧的文本经 `redact_secrets` 脱敏——密钥子串替换为 ***
    #[tokio::test]
    async fn sse_error_tail_redacts_provider_key_from_client_frame() {
        let inner = futures_util::stream::iter(vec![Err::<Bytes, _>(
            Box::<dyn Error + Send + Sync>::from(
                "error decoding response body (https://gw/sk-secret/v1/messages)",
            ),
        )]);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(inner),
            error_ctx: Some(sse_ctx(SseErrorDialect::Chat, test_breaker(4))),
            finished: false,
            saw_terminal: false,
        });
        let frame = tail.next().await.unwrap().expect("Err 转为错误帧");
        let text = String::from_utf8(frame.to_vec()).unwrap();
        assert!(!text.contains("sk-secret"), "密钥不得进客户端帧: {text}");
        let v: Value = serde_json::from_str(text["data: ".len()..].trim_end()).unwrap();
        assert_eq!(
            v["error"]["message"],
            "error decoding response body (https://gw/***/v1/messages)"
        );
    }

    /// QC3 F-001：提交后首个 `Err` 记一次熔断失败（阈值 2：两次这样的流才熔断，
    /// 证明每条流只记一次）；带终态标记的正常结束记成功、不计失败
    /// （连接加固：无终态标记的 EOF 现在按失败结算，见
    /// sse_eof_without_terminal_marker_records_failure_and_error_frame）
    #[tokio::test]
    async fn sse_error_tail_records_one_breaker_failure_per_stream() {
        let breaker = test_breaker(2);
        let provider_id = 7;
        assert!(breaker.allow(provider_id), "初始放行");
        for expected_allow in [true, false] {
            let inner = futures_util::stream::iter(vec![Err::<Bytes, _>(
                Box::<dyn Error + Send + Sync>::from("upstream stream idle timeout"),
            )]);
            let mut tail = Box::pin(SseErrorTail {
                inner: Box::pin(inner),
                error_ctx: Some(sse_ctx(SseErrorDialect::Chat, breaker.clone())),
                finished: false,
                saw_terminal: false,
            });
            tail.next().await.unwrap().expect("错误帧");
            assert!(tail.next().await.is_none(), "错误帧后不再产出（只记一次）");
            assert_eq!(
                breaker.allow(provider_id),
                expected_allow,
                "第一条流记 1 次失败（仍放行），第二条流累计 2 次才熔断"
            );
        }

        // 带终态标记的正常结束：EOF 记成功，不计失败
        let breaker = test_breaker(1);
        let inner = futures_util::stream::iter(vec![Ok::<_, Box<dyn Error + Send + Sync>>(
            Bytes::from_static(b"data: [DONE]\n\n"),
        )]);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(inner),
            error_ctx: Some(sse_ctx(SseErrorDialect::Chat, breaker.clone())),
            finished: false,
            saw_terminal: false,
        });
        while tail.next().await.is_some() {}
        assert!(breaker.allow(7), "带终态标记的正常结束记成功，不熔断");
    }

    /// QC1 W1-new / QC2 F1：按真实调用序列（提交结算 + 尾部首个 `Err`），连续
    /// `failure_threshold` 条「200 后必断流」的透明 SSE 请求必须熔断。旧实现在
    /// 提交处无条件 `record_success`：连续计数每次归零、错误率恒 0.5 < 0.6，
    /// 两个维度都永不打开
    #[tokio::test]
    async fn committed_sse_stream_failures_open_breaker() {
        let breaker = test_breaker(4);
        for attempt in 0..4 {
            assert!(breaker.allow(7), "第 {attempt} 次请求：熔断前应放行");
            drive_committed_sse(
                &breaker,
                vec![Err(Box::<dyn Error + Send + Sync>::from(
                    "upstream stream idle timeout",
                ))],
            )
            .await;
        }
        assert!(
            !breaker.allow(7),
            "连续 4 条提交后断流必须熔断（提交处记 success 的旧实现恒放行）"
        );
    }

    /// 尾部带终态标记的正常 EOF 记一次成功 → 连续失败复位：2 败 + 1 正常结束 +
    /// 2 败不达阈值 4，不得熔断（若 EOF 不记成功，累计 4 次连续失败会误熔断）
    #[tokio::test]
    async fn committed_sse_normal_eof_resets_consecutive_failures() {
        let breaker = test_breaker(4);
        for _ in 0..2 {
            drive_committed_sse(
                &breaker,
                vec![Err(Box::<dyn Error + Send + Sync>::from(
                    "upstream stream idle timeout",
                ))],
            )
            .await;
        }
        drive_committed_sse(&breaker, vec![Ok(Bytes::from_static(b"data: [DONE]\n\n"))]).await;
        for _ in 0..2 {
            drive_committed_sse(
                &breaker,
                vec![Err(Box::<dyn Error + Send + Sync>::from(
                    "upstream stream idle timeout",
                ))],
            )
            .await;
        }
        assert!(
            breaker.allow(7),
            "正常结束已复位连续计数：2+2 不达阈值 4，不得熔断"
        );
    }

    /// 连接加固：SSE 流干净 EOF 但无方言终态标记 → 记熔断失败 + 给客户端补发
    /// 方言错误帧（"200 后静默断流"不得再按成功结算）
    #[tokio::test]
    async fn sse_eof_without_terminal_marker_records_failure_and_error_frame() {
        let breaker = test_breaker(1);
        settle_commit_signal(&breaker, 7, true);
        let inner = futures_util::stream::iter(vec![Ok::<_, Box<dyn Error + Send + Sync>>(
            Bytes::from_static(b"data: {\"choices\":[]}\n\n"),
        )]);
        let mut tail = Box::pin(SseErrorTail {
            inner: Box::pin(inner),
            error_ctx: Some(sse_ctx(SseErrorDialect::Chat, breaker.clone())),
            finished: false,
            saw_terminal: false,
        });
        // 首项 = 原样透传的数据块；次项 = EOF 无终态 → 补发方言错误帧
        let chunk = tail.next().await.unwrap().expect("透传数据块");
        assert_eq!(chunk, Bytes::from_static(b"data: {\"choices\":[]}\n\n"));
        let frame = tail.next().await.unwrap().expect("EOF 补发方言错误帧");
        let text = String::from_utf8(frame.to_vec()).unwrap();
        assert!(
            text.contains("upstream stream ended before terminal event"),
            "{text}"
        );
        assert!(tail.next().await.is_none(), "错误帧后流结束");
        assert!(!breaker.allow(7), "EOF 无终态 → 记失败（阈值 1 即熔断）");
    }

    /// 连接加固：读体截止时间兜底僵死流——上游响应头已回但体永不推进时，
    /// 在 deadline 处报 "upstream body read timeout"（归因 body_timeout）
    #[tokio::test]
    async fn read_body_capped_deadline_breaks_stalled_stream() {
        let stalled = futures_util::stream::pending::<Result<Bytes, std::convert::Infallible>>();
        let resp = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(stalled))
                .unwrap(),
        );
        let started = std::time::Instant::now();
        let err = read_body_capped(resp, 1024, Duration::from_millis(50))
            .await
            .expect_err("僵死流必须在截止时间报错");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "截止时间必须及时返回"
        );
        assert!(
            err.to_string().contains("upstream body read timeout"),
            "超时文案须可归因 body_timeout: {err}"
        );
    }

    /// 连接加固：header 等待时限推导（0 = 关闭；非法值钳 1000ms；
    /// 非流式不早于非流式总预算的一半）
    #[test]
    fn header_wait_ms_semantics() {
        // 0 = 显式关闭（流式 / 非流式一致）
        assert_eq!(header_wait_ms(0, true, 600), None);
        assert_eq!(header_wait_ms(0, false, 600), None);
        // 流式：>= 1000 原值；非法值钳 1000ms
        assert_eq!(header_wait_ms(30_000, true, 600), Some(30_000));
        assert_eq!(header_wait_ms(500, true, 600), Some(1_000));
        assert_eq!(header_wait_ms(-5, true, 600), Some(1_000));
        // 非流式：非流式总超时禁用（0）→ 不放大
        assert_eq!(header_wait_ms(120_000, false, 0), Some(120_000));
        // 非流式：header 等待不早于非流式总预算的一半（600s → 300s 下限）——
        // 缓冲型上游完整生成后才回响应头，不得被 120s header 等待在 600s 预算内掐断
        assert_eq!(header_wait_ms(120_000, false, 600), Some(300_000));
        // 非流式：header 原值更大 → 取原值
        assert_eq!(header_wait_ms(400_000, false, 600), Some(400_000));
        // 非流式 + 非法小值：钳 1000 后再与非流式半预算取 max
        assert_eq!(header_wait_ms(500, false, 600), Some(300_000));
    }

    /// 连接加固：全局并发兜底——cap > 0 合成 global 规则（与 DB 全局规则同键、
    /// apply_concurrency_limits 去重取最严），同一 all-or-nothing acquire 覆盖；
    /// cap 耗尽后新请求必须被拒绝（拒绝复用既有 429 路径）
    #[test]
    fn global_concurrency_cap_rejects_when_exceeded() {
        use crate::store::rules::RateRule;
        // cap = 0（关闭）→ 原样返回
        assert_eq!(with_global_cap(Vec::new(), 0).len(), 0);
        // cap > 0 → 合成一条 global 作用域、不限模型的规则
        let merged = with_global_cap(Vec::new(), 1);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].scope, "global");
        assert_eq!(merged[0].scope_id, None);
        assert_eq!(merged[0].model, None);
        assert_eq!(merged[0].concurrency, 1);
        // 与 DB 全局规则并存：合成规则原样参与（最严上限由 dedup_min_limit 取 min）
        let merged = with_global_cap(
            vec![RateRule {
                scope: "global".to_string(),
                scope_id: None,
                model: None,
                concurrency: 5,
            }],
            3,
        );
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|r| r.concurrency == 3));

        // 超限拒绝：占用唯一名额后，携带同一桶键的 acquire 必须整体拒绝
        //（matched_rules 对 scope=global 的桶键即 "global"）
        let limiter = crate::service::ratelimit::ConcurrencyLimiter::new();
        let bucket = vec![("global".to_string(), 1i32)];
        let guard = limiter.try_acquire(&bucket).expect("首个请求占坑");
        assert!(
            limiter.try_acquire(&bucket).is_err(),
            "全局并发兜底：cap 耗尽后新请求必须被拒绝"
        );
        drop(guard);
        assert!(limiter.try_acquire(&bucket).is_ok(), "归还后可再次占坑");
    }

    /// 半开探测名额：未结算的流被 drop（客户端断开）后 `release_probe` 归还名额。
    /// `open_secs=0` 让熔断窗口立即到期，`allow` 立即可发探测；名额不归还时
    /// 该渠道会被永久拒绝
    #[tokio::test]
    async fn dropped_unsettled_sse_tail_releases_half_open_probe() {
        let breaker = Arc::new(Breaker::new_with(
            crate::service::breaker::BreakerConfig {
                failure_threshold: 1,
                open_secs: 0,
                min_requests: 1_000,
                ..Default::default()
            },
        ));
        breaker.record_failure(7); // 打开熔断（窗口 0s → 立即可半开）
        assert!(breaker.allow(7), "窗口到期后第一个请求获得探测名额");
        assert!(!breaker.allow(7), "探测名额已被占用");
        {
            let mut tail = Box::pin(SseErrorTail {
                inner: Box::pin(futures_util::stream::pending::<
                    Result<Bytes, Box<dyn Error + Send + Sync>>,
                >()),
                error_ctx: Some(sse_ctx(SseErrorDialect::Chat, breaker.clone())),
                finished: false,
                saw_terminal: false,
            });
            // 客户端断开：流未结算（既无 EOF 也无 Err）即被 drop
            assert!(
                tokio::time::timeout(Duration::from_millis(20), tail.next())
                    .await
                    .is_err(),
                "上游未结束，流处于进行中"
            );
        }
        assert!(breaker.allow(7), "未结算的流被 drop 后探测名额归还");
    }

    // ---------- QC1 F1：post-allow() 退出路径的探测名额结算 ----------

    /// 一次性假上游：接受单个连接，排空一次读后回写预置 HTTP 响应并断开
    async fn spawn_one_shot_upstream(response: String) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            // 小请求（头+体通常一个分段）排空一次读即可；失败不影响回写
            let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await;
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        addr
    }

    fn http_response(status_line: &str, content_type: &str, body: &str) -> String {
        format!(
            "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn test_provider(
        id: i64,
        api_type: &str,
        base_url: &str,
        key: &str,
        master_key: &[u8; 32],
    ) -> Provider {
        Provider {
            id,
            name: format!("p{id}"),
            api_type: api_type.into(),
            base_url: base_url.into(),
            api_key_encrypted: crate::crypto::encrypt(key.as_bytes(), master_key).expect("encrypt"),
            timeout_ms: 1000,
            extra_body: serde_json::json!({}),
            auth_scheme: "bearer".into(),
            extra_headers: serde_json::json!({}),
            supports_images: true,
        }
    }

    fn test_route(
        pattern: &str,
        primary: i64,
        fallbacks: Vec<i64>,
    ) -> crate::store::upstream::ModelRoute {
        crate::store::upstream::ModelRoute {
            model_pattern: pattern.into(),
            provider_id: primary,
            fallback_ids: fallbacks,
            upstream_model: None,
            extra_body: serde_json::json!({}),
            strict_system_head: false,
            system_head_merge: false,
            reasoning_effort_mode: None,
            thinking_form: None,
            responses_passthrough_fields: None,
        }
    }

    /// 双候选链 + 立即半开的夹具：open_secs=0 让熔断窗口立刻到期，主候选
    /// allow() 消费探测名额后暴露 post-allow() 泄漏
    fn probe_fixture(st: &mut crate::state::AppState) {
        st.breaker = Arc::new(Breaker::new_with(crate::service::breaker::BreakerConfig {
            failure_threshold: 1,
            open_secs: 0,
            min_requests: 1_000,
            ..Default::default()
        }));
    }

    /// QC1 F1(a)：出站体构建失败必须归还半开探测名额——此前 `?` 直接上抛，
    /// 名额永久占位，该渠道窗口到期后再也无法 allow 放行；降级链第二候选正常
    /// 完成请求
    #[tokio::test]
    async fn outbound_build_failure_releases_half_open_probe() {
        let mut st = crate::state::AppState::test_fixture();
        probe_fixture(&mut st);
        let upstream =
            spawn_one_shot_upstream(http_response(
                "HTTP/1.1 200 OK",
                "text/event-stream",
                "data: {\"type\":\"message_stop\"}\n\n",
            ))
            .await;
        // 主候选 api_type=openai + 缺 messages 的 Anthropic 请求 → build_outward
        // 必失败（Anthropic→Chat 转换报 "missing 'messages' field"）
        let broken = test_provider(1, "openai", "http://127.0.0.1:9", "sk-broken", &st.cfg.master_key);
        let healthy =
            test_provider(2, "anthropic", &format!("http://{upstream}"), "sk-ok", &st.cfg.master_key);
        *st.providers.write() = vec![broken, healthy];
        *st.routes.write() = vec![test_route("t-model", 1, vec![2])];
        // 半开：1 次失败即开闸（open_secs=0 → 窗口立即到期）；探测名额由
        // 候选循环内的 allow() 消费（唯一消费者），不在夹具里预占
        st.breaker.record_failure(1);

        let json = serde_json::json!({"model": "t-model", "stream": true});
        let resp = proxy_test(
            &st,
            axum::http::HeaderMap::new(),
            Bytes::from(serde_json::to_vec(&json).expect("serialize")),
            Endpoint {
                api: "/v1/messages",
                upstream: "/messages",
                responses: false,
                anthropic: true,
            },
            "127.0.0.1".parse().expect("ip"),
        )
        .await
        .expect("build 失败换候选后降级链必须完成请求");
        assert_eq!(resp.status(), StatusCode::OK, "第二候选成功流式响应");
        assert!(
            st.breaker.allow(1),
            "build_outward 失败后探测名额必须归还（可再次放行）"
        );
    }

    /// QC1 F1(b)：converted 非 2xx + SSE 回退分支必须显式结算一次——此前
    /// 提交点推迟结算（tail_settles）后分支直接整形返回，半开探测名额永久占位
    #[tokio::test]
    async fn converted_non2xx_sse_fallback_settles_probe_once() {
        let mut st = crate::state::AppState::test_fixture();
        probe_fixture(&mut st);
        // 406：非可重试（不换家）、不可整流（不进 PreRead）→ 落入 converted
        // 非 2xx 回退分支；SSE content-type → 提交点结算被推迟
        let upstream =
            spawn_one_shot_upstream(http_response(
                "HTTP/1.1 406 Not Acceptable",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"boom\"}}]}\n\n",
            ))
            .await;
        let converted =
            test_provider(1, "openai", &format!("http://{upstream}"), "sk-c", &st.cfg.master_key);
        let unused = test_provider(2, "anthropic", "http://127.0.0.1:9", "sk-u", &st.cfg.master_key);
        *st.providers.write() = vec![converted, unused];
        *st.routes.write() = vec![test_route("t-model", 1, vec![2])];
        // 半开：探测名额由候选循环内的 allow() 消费（唯一消费者）
        st.breaker.record_failure(1);

        let json = serde_json::json!({
            "model": "t-model",
            "stream": true,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let resp = proxy_test(
            &st,
            axum::http::HeaderMap::new(),
            Bytes::from(serde_json::to_vec(&json).expect("serialize")),
            Endpoint {
                api: "/v1/messages",
                upstream: "/messages",
                responses: false,
                anthropic: true,
            },
            "127.0.0.1".parse().expect("ip"),
        )
        .await
        .expect("converted 非 2xx 回退按方言整形返回");
        assert_eq!(resp.status(), StatusCode::NOT_ACCEPTABLE, "上游状态透传");
        assert!(
            st.breaker.allow(1),
            "回退分支结算后探测名额归还（可再次放行）"
        );
        let snap = st
            .breaker
            .snapshot()
            .into_iter()
            .find(|s| s.provider_id == 1)
            .expect("snapshot");
        assert_eq!(
            snap.window_requests, 1,
            "必须恰好结算一次（重复结算会计 2 次窗口请求）"
        );
    }

    /// QC1 W2 / QC3 F-003：持续同步就绪的中性帧洪水 + 墙钟 deadline →
    /// deadline 必须先返回（biased 轮询顺序护住防饥饿），且已到期的 deadline
    /// 不会被复用重置
    #[tokio::test]
    async fn prime_deadline_preempts_synchronously_ready_chunk_flood() {
        let mut flood = futures_util::stream::repeat_with(|| {
            Ok::<_, Box<dyn Error + Send + Sync>>(Bytes::from_static(b": ping\n\n"))
        });
        let mut deadline = PrimeDeadline::new(Some(Duration::from_millis(100)));
        // 未到期：帧洪水照常取帧，deadline 不抢跑
        for _ in 0..3 {
            assert!(
                matches!(
                    deadline.next(&mut flood).await,
                    PrimePoll::Item(Some(_))
                ),
                "deadline 未到期时先交付就绪帧"
            );
        }
        // 睡过窗口：帧仍然同步就绪，但 deadline 必须先赢
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(matches!(
            deadline.next(&mut flood).await,
            PrimePoll::Deadline
        ));
        assert!(
            matches!(deadline.next(&mut flood).await, PrimePoll::Deadline),
            "已到期的 deadline 不得被后续帧重置"
        );
    }

    /// deadline 未配置 / 已撤销时退化为纯流读取
    #[tokio::test]
    async fn prime_deadline_disabled_reads_items_only() {
        let mut items = futures_util::stream::iter(vec![
            Ok::<_, Box<dyn Error + Send + Sync>>(Bytes::from_static(b"a")),
            Ok(Bytes::from_static(b"b")),
        ]);
        let mut deadline = PrimeDeadline::new(None);
        assert!(matches!(
            deadline.next(&mut items).await,
            PrimePoll::Item(Some(_))
        ));
        assert!(matches!(
            deadline.next(&mut items).await,
            PrimePoll::Item(Some(_))
        ));
        assert!(matches!(
            deadline.next(&mut items).await,
            PrimePoll::Item(None)
        ));

        let mut deadline = PrimeDeadline::new(Some(Duration::from_millis(100)));
        deadline.disarm();
        let mut items = futures_util::stream::iter(vec![Ok::<_, Box<dyn Error + Send + Sync>>(
            Bytes::from_static(b"a"),
        )]);
        assert!(matches!(
            deadline.next(&mut items).await,
            PrimePoll::Item(Some(_))
        ));
    }

    /// 按用户×模型并发 guard：同用户同模型累加、同用户异模型独立计数、
    /// 缺 model 落空串键、归零移除条目、不同用户互不影响
    #[test]
    fn user_active_guard_counts_and_cleans_up() {
        let map = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let key = |uid: i64, m: &str| (uid, m.to_string());
        let g1 = UserActiveGuard::enter(&map, 7, Some("m1".into()));
        let _g2 = UserActiveGuard::enter(&map, 7, Some("m1".into()));
        let _g3 = UserActiveGuard::enter(&map, 7, Some("m2".into()));
        let _g4 = UserActiveGuard::enter(&map, 8, Some("m1".into()));
        let _g5 = UserActiveGuard::enter(&map, 7, None);
        assert_eq!(map.lock()[&key(7, "m1")], 2);
        assert_eq!(map.lock()[&key(7, "m2")], 1, "同用户异模型独立计数");
        assert_eq!(map.lock()[&key(8, "m1")], 1, "同模型异用户独立计数");
        assert_eq!(map.lock()[&key(7, "")], 1, "缺 model 落空串键");
        drop(g1);
        assert_eq!(map.lock()[&key(7, "m1")], 1, "仅归还一份并发");
        drop(_g2);
        assert!(!map.lock().contains_key(&key(7, "m1")), "归零条目移除");
        assert_eq!(map.lock()[&key(7, "m2")], 1, "同用户其他模型不受影响");
        drop(_g3);
        drop(_g4);
        drop(_g5);
        assert!(
            map.lock().is_empty(),
            "全部归还后条目应移除,表不得随历史用户×模型增长"
        );
    }

    /// guard 绑定响应体:数据帧流尽才释放(handler 返回 ≠ 响应结束,SSE 长流
    /// 期间计数必须保持);客户端中途断开时 body 连同 unfold 状态一起 drop。
    #[tokio::test]
    async fn guards_release_only_after_body_stream_exhausted() {
        let counter = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let map = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let active = ActiveRequestGuard::enter(&counter);
        let user_active = Some(UserActiveGuard::enter(&map, 42, Some("m".into())));
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("hello"))
            .unwrap();
        let resp = bind_guards_to_body(
            resp,
            ResponseGuards {
                active,
                user_active,
                concurrency: None,
            },
        );
        // body 未消费 → guard 仍持有
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(map.lock()[&(42, "m".to_string())], 1);
        let mut stream = resp.into_body().into_data_stream();
        while let Some(chunk) = stream.next().await {
            chunk.unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0, "流尽后全局计数归零");
        assert!(map.lock().is_empty(), "流尽后用户条目移除");
    }

    /// 入口解析（挂用户并发 guard 前的唯一一次解压+解析）：identity 直通、
    /// gzip 解压、坏 JSON / 不支持编码 → 400；模型提取空/缺失 → None
    #[test]
    fn parse_request_json_at_entry_and_requested_model() {
        use std::io::Write;
        let raw = br#"{"model":"gpt-x","messages":[]}"#;
        let json = parse_request_json(&Bytes::from_static(raw), &HeaderMap::new()).unwrap();
        assert_eq!(requested_model(&json).as_deref(), Some("gpt-x"));

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(raw).unwrap();
        let gz = enc.finish().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let json = parse_request_json(&Bytes::from(gz.clone()), &headers).unwrap();
        assert_eq!(
            requested_model(&json).as_deref(),
            Some("gpt-x"),
            "gzip 请求体解压后解析"
        );

        assert!(matches!(
            parse_request_json(&Bytes::from_static(b"{not json"), &HeaderMap::new()),
            Err(AppError::BadRequest(_))
        ));
        let mut bad = HeaderMap::new();
        bad.insert(header::CONTENT_ENCODING, "lzma".parse().unwrap());
        assert!(matches!(
            parse_request_json(&Bytes::from(gz), &bad),
            Err(AppError::BadRequest(_))
        ));

        assert_eq!(requested_model(&serde_json::json!({ "model": "" })), None);
        assert_eq!(
            requested_model(&serde_json::json!({ "messages": [] })),
            None
        );
        assert_eq!(requested_model(&serde_json::json!({ "model": 1 })), None);
    }
}
