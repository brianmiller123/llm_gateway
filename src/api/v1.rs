use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use serde_json::json;
use std::net::SocketAddr;

use crate::error::AppError;
use crate::service::proxy::{self, Endpoint};
use crate::state::{AppState, BufferGuard};

/// 客户端 IP：对端地址命中受信代理网段（GATEWAY_TRUST_PROXY_CIDRS）时采信
/// XFF 最右非空条目（W-1：proxy_add_x_forwarded_for 追加语义下最左是攻击者
/// 可伪造首值；多行头并入拼接后取最右）；否则（含配置为空 = 直连拓扑）一律
/// 用对端地址——直连场景 XFF 可伪造，采信会绕过按 IP 限流。仅用于用量统计/
/// 限流，非安全边界承诺。
pub(crate) fn resolve_client_ip(
    peer: SocketAddr,
    headers: &axum::http::HeaderMap,
    trust_cidrs: &str,
) -> std::net::IpAddr {
    crate::api::resolve_trusted_ip(peer, crate::api::last_xff_entry(headers), trust_cidrs)
}

/// /v1 代理端点的请求体提取器（F1）：进程级字节预算受限的整包缓冲。提取前
/// 按预估大小 try-acquire 缓冲预算（Content-Length 可估时按实际值；chunked
/// 无长度头初始预留 [`CHUNKED_INITIAL_RESERVE_BYTES`]，见 collect_chunked）；
/// 预算耗尽 → 429（try_acquire 语义，永不排队，大请求并发不再可 OOM）。许可
/// 以 [`BufferGuard`] 随 body 传递给 proxy：流式请求 handler 结束即释放；
/// 非流式由 proxy 绑定响应体，流尽/客户端断开释放（缓冲内存活多久预算占
/// 多久）。实际占用差额（估算偏差/解压膨胀）在解码期经 guard 逐块原子
/// 入账（try_grow 拒收 → 413 中止解码），成功后 proxy 入口
/// resize_to(解压后长度) 收缩收敛。读取侧超限 413（Content-Length 体由路由
/// DefaultBodyLimit，chunked 由 collect_chunked 本地判定），失败路径 guard
/// 随局部变量 Drop 即刻归还预算。
struct BufLimitedBody(Bytes, BufferGuard);

/// chunked/无有效 Content-Length 请求的初始预留（M-2）：不再按 max_body_bytes
/// 最坏情形整段预留——慢速 trickle 不再以 200MiB 预算/连接长期占坑（2 个
/// chunked 慢连接即可冻结整个预算）；随字节到达经 [`BufferGuard::resize_to`]
/// 增长，收集完成后缩至实际长度。
const CHUNKED_INITIAL_RESERVE_BYTES: u64 = 1024 * 1024;

impl axum::extract::FromRequest<AppState> for BufLimitedBody {
    type Rejection = axum::response::Response;

    async fn from_request(
        req: axum::extract::Request,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let content_length = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let Some(estimate) = content_length else {
            // M-2：无有效 Content-Length（chunked）→ 初始 ≤1MiB 增量预留收集
            return collect_chunked(req, state).await;
        };
        let guard = state.buffer_budget.try_acquire(estimate).ok_or_else(|| {
            tracing::warn!(estimate, "buffer byte budget exhausted; rejecting with 429");
            AppError::RateLimited(1.0).into_response()
        })?;
        match Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(Self(bytes, guard)),
            Err(rej) => {
                // 413（DefaultBodyLimit）等收集失败：预算随 guard Drop 即刻归还
                drop(guard);
                Err(rej.into_response())
            }
        }
    }
}

/// M-2：chunked 请求体的增量预留收集。初始预留
/// [`CHUNKED_INITIAL_RESERVE_BYTES`]，字节到达按块经 guard.resize_to 增长：
/// 预算余量耗尽 → 413 request_too_large 中止收集（体超出预算可容纳量，非
/// 429——429 仅保留给初始 try-acquire 耗尽），完成后缩至实际长度（富余即刻
/// 归还预算）。读取侧上限（cfg.max_body_bytes）本地判定：poll_frame 直读
/// 不经过 with_limited_body，路由 DefaultBodyLimit 不代为限量。传输错误 →
/// 400，预算随 guard Drop 即刻归还。
async fn collect_chunked(
    req: axum::extract::Request,
    state: &AppState,
) -> Result<BufLimitedBody, axum::response::Response> {
    let mut guard = state
        .buffer_budget
        .try_acquire(CHUNKED_INITIAL_RESERVE_BYTES)
        .ok_or_else(|| {
            tracing::warn!(
                estimate = CHUNKED_INITIAL_RESERVE_BYTES,
                "buffer byte budget exhausted; rejecting with 429"
            );
            AppError::RateLimited(1.0).into_response()
        })?;
    let max_bytes = state.cfg.max_body_bytes as u64;
    let mut buf = BytesMut::new();
    let mut reserved = CHUNKED_INITIAL_RESERVE_BYTES;
    let mut stream = req.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "chunked request body read failed");
                drop(guard);
                return Err(
                    AppError::BadRequest("failed to read request body".into()).into_response(),
                );
            }
        };
        let new_len = buf.len() as u64 + chunk.len() as u64;
        if new_len > max_bytes {
            drop(guard);
            return Err(AppError::PayloadTooLarge(format!(
                "request body exceeds the {max_bytes}-byte limit"
            ))
            .into_response());
        }
        // 只增长不收缩（逐块收缩会把已释放的预算反复重占，白耗记账）
        if new_len > reserved {
            if guard.resize_to(new_len).is_err() {
                drop(guard);
                return Err(AppError::PayloadTooLarge(
                    "request body exceeds the gateway buffer budget".into(),
                )
                .into_response());
            }
            reserved = new_len;
        }
        buf.extend_from_slice(&chunk);
    }
    // 缩至实际长度（收缩路径不失败；预留富余即刻归还预算）
    guard
        .resize_to(buf.len() as u64)
        .expect("shrink-to-actual cannot fail");
    Ok(BufLimitedBody(buf.freeze(), guard))
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1/systemone", post(systemone))
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
    body: BufLimitedBody,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers, &st.cfg.trust_proxy_cidrs);
    proxy::proxy(
        &st,
        headers,
        body.0,
        body.1,
        Endpoint {
            api: "/v1/responses",
            upstream: "/responses",
            responses: true,
            anthropic: false,
            systemone: false,
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
    body: BufLimitedBody,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers, &st.cfg.trust_proxy_cidrs);
    match proxy::proxy(
        &st,
        headers,
        body.0,
        body.1,
        Endpoint {
            api: "/v1/messages",
            upstream: "/messages",
            responses: false,
            anthropic: true,
            systemone: false,
        },
        ip,
    )
    .await
    {
        Ok(resp) => Ok(resp),
        Err(e) => Ok(crate::service::anthropic::error_response(&e)),
    }
}

/// POST /v1/systemone：TypeSafe System One 决策模型（单次请求返回全部问题的
/// 概率分布）。仅 `api_type=systemone` 上游原生透传，其余配对方言不匹配 400；
/// 无流式。网关内部错误整形为 `{"detail": ...}`（客户端方言）。
async fn systemone(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: BufLimitedBody,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers, &st.cfg.trust_proxy_cidrs);
    match proxy::proxy(
        &st,
        headers,
        body.0,
        body.1,
        Endpoint {
            api: "/v1/systemone",
            upstream: "/systemone",
            responses: false,
            anthropic: false,
            systemone: true,
        },
        ip,
    )
    .await
    {
        Ok(resp) => Ok(resp),
        Err(e) => Ok(crate::service::systemone::error_response(&e)),
    }
}

async fn chat_completions(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: BufLimitedBody,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers, &st.cfg.trust_proxy_cidrs);
    proxy::proxy(
        &st,
        headers,
        body.0,
        body.1,
        Endpoint {
            api: "/v1/chat/completions",
            upstream: "/chat/completions",
            responses: false,
            anthropic: false,
            systemone: false,
        },
        ip,
    )
    .await
}

async fn completions(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: BufLimitedBody,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers, &st.cfg.trust_proxy_cidrs);
    proxy::proxy(
        &st,
        headers,
        body.0,
        body.1,
        Endpoint {
            api: "/v1/completions",
            upstream: "/completions",
            responses: false,
            anthropic: false,
            systemone: false,
        },
        ip,
    )
    .await
}

async fn embeddings(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: BufLimitedBody,
) -> Result<Response, AppError> {
    let ip = resolve_client_ip(addr, &headers, &st.cfg.trust_proxy_cidrs);
    proxy::proxy(
        &st,
        headers,
        body.0,
        body.1,
        Endpoint {
            api: "/v1/embeddings",
            upstream: "/embeddings",
            responses: false,
            anthropic: false,
            systemone: false,
        },
        ip,
    )
    .await
}

/// GET /v1/models：列出全部模型名（模型库 models 表已启用行 + 出站模型在库内
/// 启用的路由别名 pattern，去重）；通配符 pattern（不可作为模型名提交）不列出。
///
/// 鉴权：`auth_mode = api_key`（默认）必须携带有效 API Key——`Authorization:
/// Bearer <key>` 或 `x-api-key: <key>`，缺失/无效一律 401 `invalid_api_key`；
/// `auth_mode = none`（本地开发）直接放行。普通用户按 provider 走用户授权过滤，
/// 管理员与无 uid（`auth_mode=none`）看全量。
///
/// 模型库中设置了 context_length 的模型同值下发三个别名字段
/// （context_length / context_window / max_model_len——分别覆盖 one-api /
/// Anthropic / vLLM 生态客户端的读取约定），未设置则不下发，客户端回落自身默认。

/// 同名模型出现在多个供应商时的数值合并：取已设置值的最小值——请求可能路由到
/// 任一候选渠道，最小值才是对客户端保证可用的值（用于上下文长度、最大输出）。
fn merge_min_opt(acc: Option<i64>, next: Option<i64>) -> Option<i64> {
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

/// 模型库目录行：`(model_id, provider_id, context_length, capability, max_output_tokens)`
type CatalogRow = (String, i64, Option<i64>, Option<String>, Option<i64>);

/// 路由别名的方言标记判定：候选链（主供应商 + 降级链）**全部**是原生
/// systemone 才标记——proxy 的方言配对只过滤不可行候选、保留可行候选
/// （`pairing_unsupported`），链上有任一 chat 供应商时该别名实际可走
/// `/v1/chat/completions`。快照缺 provider（已停用/过期）按非 systemone 处理。
fn chain_systemone_only(
    primary: i64,
    fallbacks: &[i64],
    provider_systemone: &std::collections::HashMap<i64, bool>,
) -> bool {
    std::iter::once(&primary)
        .chain(fallbacks)
        .all(|pid| provider_systemone.get(pid).copied().unwrap_or(false))
}

async fn models(State(st): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    // R2-SEC-01：`api_key` 模式的鉴权错误不再被 `.ok()` 吞掉——匿名/无效 Key
    // 请求曾可列出全量模型（与 README 的 Bearer 鉴权契约冲突）；现在缺失/无效
    // → 401 `invalid_api_key`。`auth_mode=none` 由 `authenticate` 直接放行
    // （`uid=None` → 全量），本地开发兼容不变。
    let uid = match crate::service::auth::authenticate(&st, &headers).await {
        Ok((uid, _)) => uid,
        Err(e) => return e.into_response(),
    };
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
    let mut push = |name: String,
                    ctx: Option<i64>,
                    max_out: Option<i64>,
                    capability: Option<&str>,
                    systemone: bool| {
        if seen.contains(&name) {
            return;
        }
        seen.push(name.clone());
        let mut obj = json!({
            "id": name,
            "object": "model",
            "owned_by": "llm-gateway",
        });
        // 决策模型与 chat 模型混列且无方言标记时，客户端按列表打
        // /v1/chat/completions 必得 400；仅 systemone 上游提供的模型显式打标
        if systemone {
            obj["api_type"] = json!("systemone");
        }
        if let Some(n) = ctx {
            obj["context_length"] = json!(n);
            obj["context_window"] = json!(n);
            obj["max_model_len"] = json!(n);
        }
        if let Some(n) = max_out {
            obj["max_output_tokens"] = json!(n);
            obj["max_completion_tokens"] = json!(n);
        }
        if let Some(cap) = capability {
            obj["capability"] = json!(cap);
        }
        data.push(obj);
    };

    // 1) 模型库（测试连接/手动刷新入库的模型）；仅列启用供应商下已启用的模型。
    //    聚合与授权过滤同集合：只对该用户可路由的供应商取最小值——更精确
    //    （不因无权渠道的更小设置而低估可用上下文），且仍不高估。
    //    方言标记口径与 /v1/systemone 透传判定同一规则（api_type 大小写不敏感），
    //    直接复用 provider_native_systemone，不另写一份字符串比较。
    let provider_systemone: std::collections::HashMap<i64, bool> = st
        .providers
        .read()
        .iter()
        .map(|p| (p.id, crate::service::systemone::provider_native_systemone(p)))
        .collect();
    let catalog: Vec<CatalogRow> = sqlx::query_as::<_, CatalogRow>(
        "SELECT m.model_id, m.provider_id, m.context_length, m.capability, m.max_output_tokens FROM models m \
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
    let mut catalog_max_out: std::collections::HashMap<&str, Option<i64>> =
        std::collections::HashMap::new();
    // 标记口径与聚合同集合（授权可见行集）：任一行不是原生 systemone →
    // 该用户能走 chat，不标记。逐行 AND 入表，避免每条 catalog 行扫全表
    let mut catalog_systemone: std::collections::HashMap<&str, bool> =
        std::collections::HashMap::new();
    for (model, provider_id, ctx, cap, max_out) in &catalog {
        if can_use(*provider_id, model) {
            let entry = catalog_ctx.entry(model.as_str()).or_insert(None);
            *entry = merge_min_opt(*entry, *ctx);
            // 种子取首个候选的原始值而非 None：None 在 merge_capability 里是
            // “未标注=最弱”，作单位元会让首个已标注候选被吞掉（永不下发）
            let cap_entry = catalog_cap.entry(model.as_str()).or_insert(cap.as_deref());
            *cap_entry = merge_capability(*cap_entry, cap.as_deref());
            let out_entry = catalog_max_out.entry(model.as_str()).or_insert(None);
            *out_entry = merge_min_opt(*out_entry, *max_out);
            let sys_entry = catalog_systemone.entry(model.as_str()).or_insert(true);
            *sys_entry &= provider_systemone
                .get(provider_id)
                .copied()
                .unwrap_or(false);
        }
    }
    for (model, provider_id, _, _, _) in &catalog {
        if can_use(*provider_id, model) {
            let cap = catalog_cap.get(model.as_str()).copied().flatten();
            let max_out = catalog_max_out.get(model.as_str()).copied().flatten();
            push(
                model.clone(),
                catalog_ctx.get(model.as_str()).copied().flatten(),
                max_out,
                cap,
                catalog_systemone
                    .get(model.as_str())
                    .copied()
                    .unwrap_or(false),
            );
        }
    }

    // 2) 已启用路由的具体 pattern（通配符仅用于匹配，不对外列出）。
    //    仅当出站模型（upstream_model，缺省 = pattern 本身）是主供应商模型库
    //    中已启用的模型时才列出——模型库启停是网关的模型白名单，库里没有或
    //    已禁用的模型不因挂了路由就对外暴露；命中时别名继承该模型的
    //    上下文/输出/能力标注（同一上游模型，能力一致）。
    //    与模型库同名的 pattern 已在前一步 push，此处 push 因 seen 去重跳过。
    let routes = st.routes.read();
    for r in routes.iter() {
        if r.model_pattern.ends_with('*') {
            continue;
        }
        let upstream_model = r.upstream_model.as_deref().unwrap_or(&r.model_pattern);
        let Some((_, _, ctx, cap, max_out)) = catalog
            .iter()
            .find(|(m, pid, ..)| *pid == r.provider_id && m == upstream_model)
        else {
            continue;
        };
        if can_use(r.provider_id, &r.model_pattern) {
            // 标记按候选链判定（主 + 降级链）：链上有 chat 供应商时该别名实际
            // 可走 /v1/chat/completions，不能标成 decision-only（只列具体
            // pattern，通配符已在上面跳过）
            push(
                r.model_pattern.clone(),
                *ctx,
                *max_out,
                cap.as_deref(),
                chain_systemone_only(r.provider_id, &r.fallback_ids, &provider_systemone),
            );
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
    use super::{
        BufLimitedBody, Bytes, chain_systemone_only, merge_capability, merge_min_opt, models,
    };
    use crate::state::{AppState, BufferBudget};
    use crate::store::keys::ApiKeyRow;
    use axum::body::{Body, to_bytes};
    use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use sha2::Digest;

    /// R2-SEC-01 测试用合法 Key（≥16 字节：短于此 `authenticate` 直接 401）
    const VALID_KEY: &str = "sk-test-valid-key-000001";

    /// `auth_mode=api_key` 的测试状态：DB 断连，合法 Key 行直接种进前缀缓存
    /// （TTL 内命中 → 不查库），另有短 Key 走"格式非法"分支
    fn api_key_state(mode: crate::config::AuthMode) -> AppState {
        let mut st = AppState::test_fixture();
        // handler 的目录查询（models 表）在断连池上必须快速失败，否则 sqlx
        // 默认 30s acquire 超时会让每个 200 用例白等半分钟
        st.pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect_lazy("postgres://127.0.0.1:1/none")
            .expect("lazy pool");
        let mut cfg = (*st.cfg).clone();
        cfg.auth_mode = mode;
        cfg.auth_cache_secs = 60;
        st.cfg = std::sync::Arc::new(cfg);
        let prefix = VALID_KEY[..12].to_string();
        st.auth_cache.lock().insert(
            prefix,
            (
                std::time::Instant::now(),
                vec![ApiKeyRow {
                    id: 1,
                    user_id: 7,
                    key_hash: hex::encode(sha2::Sha256::digest(VALID_KEY.as_bytes())),
                    expires_at: None,
                    user_status: 1,
                }],
            ),
        );
        st
    }

    fn bearer(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {key}").parse().unwrap());
        headers
    }

    async fn error_code(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// R2-SEC-01：`api_key` 模式下 `/v1/models` 必须鉴权——匿名请求曾可列出全量
    /// 模型（`.ok()` 吞掉 `authenticate` 错误），与 README 的 Bearer 契约冲突
    #[tokio::test]
    async fn models_rejects_missing_key_in_api_key_mode() {
        let st = api_key_state(crate::config::AuthMode::ApiKey);
        let (status, body) = error_code(models(State(st), HeaderMap::new()).await).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "缺失 Key 必须 401");
        assert_eq!(body["error"]["code"], "invalid_api_key");
    }

    /// R2-SEC-01：`api_key` 模式下无效 Key 同样 401（短于 16 字节在校验前即拒绝）
    #[tokio::test]
    async fn models_rejects_invalid_key_in_api_key_mode() {
        let st = api_key_state(crate::config::AuthMode::ApiKey);
        let (status, body) = error_code(models(State(st), bearer("sk-invalid")).await).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "无效 Key 必须 401");
        assert_eq!(body["error"]["code"], "invalid_api_key");
    }

    /// R2-SEC-01：缓存中的合法 Key 通过鉴权 → 200（DB 断连 → 目录为空，
    /// 但 200 证明请求已越过鉴权门）
    #[tokio::test]
    async fn models_accepts_cached_valid_key() {
        let st = api_key_state(crate::config::AuthMode::ApiKey);
        let (status, body) = error_code(models(State(st), bearer(VALID_KEY)).await).await;
        assert_eq!(status, StatusCode::OK, "合法 Key 必须放行: {body}");
        assert_eq!(body["object"], "list");
        assert!(
            body["data"].as_array().is_some_and(|d| d.is_empty()),
            "DB 断连 → 空目录"
        );
    }

    /// R2-SEC-01：`auth_mode=none`（本地开发）继续匿名放行，不因新鉴权回归
    #[tokio::test]
    async fn models_allows_anonymous_when_auth_disabled() {
        let st = api_key_state(crate::config::AuthMode::None);
        let (status, body) = error_code(models(State(st), HeaderMap::new()).await).await;
        assert_eq!(status, StatusCode::OK, "auth_mode=none 必须匿名放行: {body}");
        assert_eq!(body["object"], "list");
    }

    /// F1 字节预算不变量（预算实例为测试局部，无跨测共享状态）。1) 正常收集
    /// → 预算按 Content-Length 预估占用，guard 随 body 持有；2) 读取侧超限
    /// → 413 行为不变且失败路径预算即刻归还；3) 剩余预算不足 → try_acquire
    /// 失败直接 429（rate_limit_exceeded 形状 + Retry-After），不排队；
    /// 4) guard Drop → 预算归零，后续请求恢复正常。
    #[tokio::test]
    async fn buf_limited_body_budget_invariant() {
        let mut st = AppState::test_fixture();
        st.buffer_budget = BufferBudget::new(64);

        // 1) 正常路径：collect 完成，预算按 CL 预估占用（guard 随 body 持有）
        let body = BufLimitedBody::from_request(
            Request::builder()
                .header(header::CONTENT_LENGTH, "5")
                .body(Body::from("hello"))
                .unwrap(),
            &st,
        )
        .await
        .expect("预算充足时应正常收集");
        assert_eq!(body.0, "hello");
        assert_eq!(st.buffer_budget.in_use(), 5, "按 Content-Length 预估占用");

        // 2) 超限 413：读取侧 DefaultBodyLimit 行为不变，失败路径归还本请求预算
        let mut req = Request::builder()
            .header(header::CONTENT_LENGTH, "9")
            .body(Body::from(vec![b'x'; 9]))
            .unwrap();
        DefaultBodyLimit::max(8).apply(&mut req);
        let Err(resp) = BufLimitedBody::from_request(req, &st).await else {
            panic!("超限请求必须被 413 拒绝");
        };
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(st.buffer_budget.in_use(), 5, "413 失败路径即刻归还，不跨请求持有");

        // 3) 剩余预算不足：预算 64 已占 5 → CL=60 的请求 try_acquire 失败 429，不排队
        let Err(resp) = BufLimitedBody::from_request(
            Request::builder()
                .header(header::CONTENT_LENGTH, "60")
                .body(Body::from("y"))
                .unwrap(),
            &st,
        )
        .await
        else {
            panic!("预算耗尽时提取必须被 429 拒绝");
        };
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1");
        let body_bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(parsed["error"]["type"], "rate_limit_exceeded");
        assert_eq!(parsed["error"]["code"], "rate_limit_exceeded");
        assert_eq!(st.buffer_budget.in_use(), 5, "429 拒绝不占预算");

        // 4) guard Drop 归还：释放后预算归零，后续请求恢复正常
        drop(body);
        assert_eq!(st.buffer_budget.in_use(), 0, "guard Drop 归还全部预留");
        let body = BufLimitedBody::from_request(
            Request::builder()
                .header(header::CONTENT_LENGTH, "2")
                .body(Body::from("ok"))
                .unwrap(),
            &st,
        )
        .await
        .expect("预算归还后必须可复用");
        assert_eq!(body.0, "ok");
        drop(body);
        assert_eq!(st.buffer_budget.in_use(), 0);
    }

    #[test]
    fn merge_min_opt_takes_min_of_set_values() {
        assert_eq!(merge_min_opt(None, None), None, "双方未设置 → 不下发");
        assert_eq!(merge_min_opt(None, Some(384_000)), Some(384_000));
        assert_eq!(merge_min_opt(Some(1_000_000), None), Some(1_000_000));
        assert_eq!(
            merge_min_opt(Some(1_000_000), Some(384_000)),
            Some(384_000),
            "取最小：请求可能落到任一候选渠道"
        );
        assert_eq!(merge_min_opt(Some(128_000), Some(384_000)), Some(128_000));
    }

    /// M-2：chunked（无 Content-Length）增量预留——初始 ≤1MiB 随字节到达增长，
    /// 完成后缩至实际长度；增长被预算拒绝 → 413 request_too_large（非 429）；
    /// 超读取上限 → 413；初始预留耗尽仍 429（Retry-After）。预算实例为测试
    /// 局部，无跨测共享状态。
    #[tokio::test]
    async fn chunked_body_reserves_incrementally_and_enforces_budget() {
        // 1) 小 chunked 体：初始 1MiB 预留，完成缩至实际长度
        let mut st = AppState::test_fixture();
        st.buffer_budget = BufferBudget::new(2 * 1024 * 1024);
        let body = BufLimitedBody::from_request(
            Request::builder().body(Body::from("hello")).unwrap(),
            &st,
        )
        .await
        .expect("预算充足时应正常收集");
        assert_eq!(body.0, "hello");
        assert_eq!(st.buffer_budget.in_use(), 5, "收集完成缩至实际长度");
        drop(body);
        assert_eq!(st.buffer_budget.in_use(), 0);

        // 2) 多块 chunked：预留随字节到达从 1MiB 增长到实际长度
        let chunk1 = Bytes::from(vec![b'a'; 1024 * 1024]);
        let chunk2 = Bytes::from(vec![b'b'; 512]);
        let body = BufLimitedBody::from_request(
            Request::builder()
                .body(Body::from_stream(futures_util::stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(chunk1),
                    Ok(chunk2),
                ])))
                .unwrap(),
            &st,
        )
        .await
        .expect("预算（2MiB）足以承载增量增长");
        assert_eq!(body.0.len(), 1024 * 1024 + 512);
        assert_eq!(
            st.buffer_budget.in_use(),
            (1024 * 1024 + 512) as u64,
            "增长后缩至实际长度"
        );
        drop(body);
        assert_eq!(st.buffer_budget.in_use(), 0);

        // 3) 增长拒绝 → 413：预算 1MiB+1024，体 1MiB+2048（初始预留可得，
        //    跨块增长超剩余预算被拒——体超出预算可容纳量，非 429）
        st.buffer_budget = BufferBudget::new(1024 * 1024 + 1024);
        let chunk1 = Bytes::from(vec![b'a'; 1024 * 1024]);
        let chunk2 = Bytes::from(vec![b'b'; 2048]);
        let Err(resp) = BufLimitedBody::from_request(
            Request::builder()
                .body(Body::from_stream(futures_util::stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(chunk1),
                    Ok(chunk2),
                ])))
                .unwrap(),
            &st,
        )
        .await
        else {
            panic!("增长被预算拒绝必须 413");
        };
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body_bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(parsed["error"]["code"], "request_too_large");
        assert_eq!(st.buffer_budget.in_use(), 0, "413 失败路径即刻归还");

        // 4) 超读取上限 → 413：poll_frame 直读不经过 DefaultBodyLimit，本地判定
        st.buffer_budget = BufferBudget::new(2 * 1024 * 1024);
        let mut cfg = (*st.cfg).clone();
        cfg.max_body_bytes = 8;
        st.cfg = std::sync::Arc::new(cfg);
        let Err(resp) = BufLimitedBody::from_request(
            Request::builder().body(Body::from(vec![b'x'; 9])).unwrap(),
            &st,
        )
        .await
        else {
            panic!("超读取上限必须 413");
        };
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(st.buffer_budget.in_use(), 0, "超上限失败路径即刻归还");

        // 5) 初始预留耗尽 → 429（预算 < 初始 1MiB，try_acquire 失败不排队）
        st.buffer_budget = BufferBudget::new(64);
        let Err(resp) = BufLimitedBody::from_request(
            Request::builder().body(Body::from("hello")).unwrap(),
            &st,
        )
        .await
        else {
            panic!("初始预留耗尽必须 429");
        };
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1");
        assert_eq!(st.buffer_budget.in_use(), 0);
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

    /// W1：别名标记按候选链判定——链上全部原生 systemone 才标记；降级链含
    /// chat 供应商（proxy 保留可行候选，实际可走 chat）或快照缺 provider 均
    /// 不标记
    #[test]
    fn chain_systemone_only_requires_whole_chain_native() {
        // provider 1 = systemone，2 = chat，3 = 快照缺失
        let dialect = std::collections::HashMap::from([(1i64, true), (2i64, false)]);

        assert!(
            chain_systemone_only(1, &[], &dialect),
            "纯 systemone 主链 → 标记"
        );
        assert!(
            chain_systemone_only(1, &[1], &dialect),
            "降级链同为 systemone → 标记"
        );
        assert!(
            !chain_systemone_only(1, &[2], &dialect),
            "chat fallback → 不标记"
        );
        assert!(
            !chain_systemone_only(2, &[1], &dialect),
            "主供应商是 chat → 不标记"
        );
        assert!(
            !chain_systemone_only(3, &[], &dialect),
            "快照缺 provider（已停用/过期）→ 按非 systemone"
        );
    }

    /// S3：`/v1/models` 响应级断言——catalog 与别名按方言标记；既有键与列序
    /// 不变；别名按候选链判定（主 systemone + chat fallback → 不标记）
    #[sqlx::test(migrations = "./migrations")]
    async fn models_tags_decision_entries_by_dialect_and_chain(pool: sqlx::PgPool) {
        use crate::store::config::{create_provider, create_route, replace_provider_models};

        async fn provider(pool: &sqlx::PgPool, name: &str, api_type: &str) {
            create_provider(
                pool,
                name,
                api_type,
                "https://example.invalid/v1",
                "",
                30_000,
                true,
                &serde_json::json!({}),
                "bearer",
                &serde_json::json!({}),
                false,
            )
            .await
            .unwrap();
        }
        async fn alias(
            pool: &sqlx::PgPool,
            pattern: &str,
            primary: i64,
            fallbacks: &[i64],
            upstream: &str,
        ) {
            create_route(
                pool,
                pattern,
                primary,
                100,
                fallbacks,
                Some(upstream),
                true,
                &serde_json::json!({}),
                false,
                false,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }

        provider(&pool, "p-chat", "openai").await;
        provider(&pool, "p-sys", "SystemOne").await;
        let providers = crate::store::upstream::load_providers(&pool).await.unwrap();
        let p_chat = providers.iter().find(|p| p.name == "p-chat").unwrap().id;
        let p_sys = providers.iter().find(|p| p.name == "p-sys").unwrap().id;
        // 模型库同步（同时为本供应商各建一条直指精确路由）
        replace_provider_models(&pool, p_chat, &["c-model".to_string()])
            .await
            .unwrap();
        replace_provider_models(&pool, p_sys, &["d-model".to_string()])
            .await
            .unwrap();
        // 别名：精确 pattern + upstream_model（出站模型须是主供应商库内模型）
        alias(&pool, "d-alias", p_sys, &[], "d-model").await;
        alias(&pool, "c-alias", p_chat, &[], "c-model").await;
        // W1：主供应商 systemone + chat fallback → 实际可走 chat，不标记
        alias(&pool, "mixed-alias", p_sys, &[p_chat], "d-model").await;

        let mut st = AppState::test_fixture();
        st.pool = pool.clone();
        let mut cfg = (*st.cfg).clone();
        cfg.auth_mode = crate::config::AuthMode::None;
        st.cfg = std::sync::Arc::new(cfg);
        *st.providers.write() = crate::store::upstream::load_providers(&pool).await.unwrap();
        *st.routes.write() = crate::store::upstream::load_routes(&pool).await.unwrap();

        let resp = models(State(st), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let data = v["data"].as_array().expect("data 数组");
        let ids: Vec<&str> = data
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(
            ids,
            vec!["c-model", "d-model", "d-alias", "c-alias", "mixed-alias"],
            "列序：catalog（入库序）在前、别名按路由序在后"
        );
        let entry = |id: &str| {
            data.iter()
                .find(|m| m["id"] == id)
                .unwrap_or_else(|| panic!("缺少模型 {id}"))
        };
        assert_eq!(
            entry("d-model")["api_type"],
            "systemone",
            "systemone 供应商的模型带标记"
        );
        assert_eq!(
            entry("d-alias")["api_type"],
            "systemone",
            "纯 systemone 别名带标记"
        );
        for id in ["c-model", "c-alias", "mixed-alias"] {
            assert!(
                entry(id).get("api_type").is_none(),
                "{id} 不应带方言标记"
            );
        }
        // 既有键不变
        for id in ["c-model", "d-model", "d-alias"] {
            let m = entry(id);
            assert_eq!(m["object"], "model", "{id} object 键不变");
            assert_eq!(m["owned_by"], "llm-gateway", "{id} owned_by 键不变");
        }
    }

    /// W2：标记口径与授权可见行集一致——受限用户只有 systemone 渠道权限、而
    /// chat 渠道也列同名模型时，条目对该用户仍是 decision-only（可走
    /// /v1/systemone），必须打标；按全量目录判定会误判成混合
    #[sqlx::test(migrations = "./migrations")]
    async fn models_tag_uses_authorized_subset(pool: sqlx::PgPool) {
        use crate::store::config::{create_provider, replace_provider_models};

        let p_sys = create_provider(
            &pool,
            "p-sys",
            "systemone",
            "https://example.invalid/v1",
            "",
            30_000,
            true,
            &serde_json::json!({}),
            "bearer",
            &serde_json::json!({}),
            false,
        )
        .await
        .unwrap();
        let p_chat = create_provider(
            &pool,
            "p-chat",
            "openai",
            "https://example.invalid/v1",
            "",
            30_000,
            true,
            &serde_json::json!({}),
            "bearer",
            &serde_json::json!({}),
            false,
        )
        .await
        .unwrap();
        // 同名模型在两个渠道各一条
        replace_provider_models(&pool, p_sys.id, &["shared".to_string()])
            .await
            .unwrap();
        replace_provider_models(&pool, p_chat.id, &["shared".to_string()])
            .await
            .unwrap();

        let mut st = api_key_state(crate::config::AuthMode::ApiKey);
        st.pool = pool.clone();
        *st.providers.write() = crate::store::upstream::load_providers(&pool).await.unwrap();
        *st.routes.write() = crate::store::upstream::load_routes(&pool).await.unwrap();
        // 用户 7 只授权 systemone 供应商（非 admin）
        st.user_access.write().insert(
            7,
            vec![crate::store::access::UserAccessRule {
                user_id: 7,
                provider_id: Some(p_sys.id),
                model_pattern: None,
            }],
        );

        let resp = models(State(st), bearer(VALID_KEY)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let data = v["data"].as_array().expect("data 数组");
        assert_eq!(data.len(), 1, "只列授权渠道可见的模型: {data:?}");
        assert_eq!(data[0]["id"], "shared");
        assert_eq!(
            data[0]["api_type"], "systemone",
            "可见行集内全是 systemone → 必须标记"
        );
    }
}
