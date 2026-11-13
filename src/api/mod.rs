pub mod admin_config;
pub mod console;
pub mod plans;
pub mod status;
pub mod v1;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let web_dir = state.cfg.web_dir.clone();
    // /v1 中间件栈（外→内）：请求体上限提前检查 → 请求 tracing（DEBUG span）→
    // v1::routes 内部的自定义响应头
    let v1 = v1::routes(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            reject_oversized_body,
        ))
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request| {
            // DEBUG 级：默认 INFO 日志保持干净；请求级排查时调高 tower_http 过滤
            tracing::span!(
                tracing::Level::DEBUG,
                "http_request",
                method = %req.method(),
                path = %req.uri().path(),
            )
        }));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/", get(root))
        .merge(v1)
        .merge(status::routes())
        .merge(console::routes(state.clone()))
        .merge(admin_config::routes(state.clone()))
        .merge(plans::routes(state.clone()))
        // 控制台前端静态资源（Vue dist）；目录不存在时 ServeDir 自然 404
        .nest_service("/assets", ServeDir::new(web_dir.join("assets")))
        // SPA 深层路由（浏览器导航）→ index.html；API 未匹配路径 → JSON 404
        .fallback(spa_fallback)
        // LLM 请求体（长上下文 agent 会话、多轮 tool_result）可远超 axum 默认
        // 2MB 限制；上限可配（GATEWAY_MAX_BODY_BYTES，默认 200MiB：32MB 曾拒绝
        // 真实长上下文客户端请求）。/v1 的 Content-Length 提前检查见
        // reject_oversized_body；此处为全路由读取侧兜底（分块传输等无长度头的请求）
        .layer(DefaultBodyLimit::max(state.cfg.max_body_bytes))
        .with_state(state)
}

async fn healthz() -> Response {
    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
}

/// Prometheus 文本格式指标导出（进程启动以来累计；内存态，重启清零是有意为之）。
/// 实例即 AppState.metrics 单例，与代理/熔断等记录方共享同一份计数。
async fn metrics(State(st): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        st.metrics.render(),
    )
        .into_response()
}

/// 就绪探针：DB 可达（2s 预算）才算就绪。LB/编排依此摘除依赖抖动的实例；
/// 区别于 /healthz（进程存活即 200，不查依赖，供 liveness 使用）。
async fn readyz(State(st): State<AppState>) -> Response {
    let probe = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(&st.pool),
    )
    .await;
    match probe {
        Ok(Ok(_)) => (StatusCode::OK, Json(json!({"status": "ok"}))).into_response(),
        // 超时或查询失败：依赖不可用 → degraded（503），触发 LB 摘除/重启策略
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "degraded"})),
        )
            .into_response(),
    }
}

/// /v1 入口请求体上限提前检查（审计 #6a）：Content-Length 超限时立即回
/// 413 + Connection: close，且不读请求体——先读体再断流，客户端只会看到
/// 连接被 RST、收不到 413，大体量 agent 客户端会无脑重试，反复制造大请求。
/// 无 Content-Length（分块/流式传输）放行，由 DefaultBodyLimit 读取侧兜底。
async fn reject_oversized_body(State(st): State<AppState>, req: Request, next: Next) -> Response {
    match oversized_content_length_response(req.headers(), st.cfg.max_body_bytes) {
        Some(resp) => resp,
        None => next.run(req).await,
    }
}

/// Content-Length 提前判定：超限返回 413 响应（Connection: close + JSON 错误体）；
/// 头缺失/非法/未超限返回 None。独立纯函数以便离线测试。
pub(crate) fn oversized_content_length_response(
    headers: &axum::http::HeaderMap,
    max_bytes: usize,
) -> Option<Response> {
    let content_length = headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse::<usize>()
        .ok()?;
    if content_length <= max_bytes {
        return None;
    }
    Some((
        StatusCode::PAYLOAD_TOO_LARGE,
        [(header::CONNECTION, "close")],
        Json(json!({
            "error": {
                "type": "request_too_large",
                "message": "request body exceeds limit",
            }
        })),
    )
        .into_response())
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
                "openai_compat": ["/v1/chat/completions", "/v1/responses", "/v1/messages", "/v1/completions", "/v1/embeddings", "/v1/models"]
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
    // SPA 壳禁缓存：assets 均为内容哈希名（index-XXXX.js 永久可缓存），
    // 但 index.html 本体必须每次回源重验——否则部署后旧壳一直引用已被
    // 新构建删除的旧 chunk，页面停留在旧版本
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

/// 客户端来源解析的信任边界（审计 #15）：对端地址命中 `trust_cidrs`
/// （CSV IPv4 CIDR，如 "10.0.0.0/8,192.168.1.0/24"，env GATEWAY_TRUST_PROXY_CIDRS）
/// 时采信 XFF 首值——可信代理链上首值是客户端真实来源；否则（含配置为空 =
/// 直连拓扑）一律用对端 socket 地址，XFF 可伪造、采信会绕过按 IP 限流。
/// 仅用于用量统计/限流桶，非安全边界承诺。
/// IPv4-mapped IPv6 对端（::ffff:a.b.c.d，双栈 socket 常见）先归一为 IPv4 再匹配。
pub fn resolve_trusted_ip(
    peer: SocketAddr,
    xff: Option<&axum::http::HeaderValue>,
    trust_cidrs: &str,
) -> IpAddr {
    let peer_ip = normalize_ipv4_mapped(peer.ip());
    if !peer_in_trust_cidrs(peer_ip, trust_cidrs) {
        return peer_ip;
    }
    // 受信代理链路：只认 XFF 首值（后值是代理自行追加，不可作为客户端来源）；
    // 首值非法则拒绝（宁可退回对端地址，不可让伪造头决定限流桶键）
    xff.and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(parse_xff_ip)
        .unwrap_or(peer_ip)
}

/// 对端是否命中受信网段；逐条匹配不落集合（请求期热路径，零分配）。
/// 畸形条目跳过，单条 warn 汇总（配置错误应修配置，不逐条刷日志）。
fn peer_in_trust_cidrs(peer: IpAddr, csv: &str) -> bool {
    let mut matched = false;
    let mut malformed = 0usize;
    let mut first_malformed: Option<&str> = None;
    for entry in csv.split(',').map(str::trim) {
        if entry.is_empty() {
            continue;
        }
        match entry.parse::<V4Cidr>() {
            Ok(cidr) => matched |= cidr.contains(peer),
            Err(()) => {
                malformed += 1;
                if first_malformed.is_none() {
                    first_malformed = Some(entry);
                }
            }
        }
    }
    if malformed > 0 {
        tracing::warn!(
            count = malformed,
            first = first_malformed.unwrap_or(""),
            "GATEWAY_TRUST_PROXY_CIDRS 含非法 IPv4 CIDR 条目，已跳过"
        );
    }
    matched
}

/// IPv4 CIDR（a.b.c.d/prefix）；仅支持 IPv4——网关业务里代理网段均为 v4 内网
struct V4Cidr {
    addr: Ipv4Addr,
    prefix: u8,
}

impl std::str::FromStr for V4Cidr {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = s.split_once('/').ok_or(())?;
        let addr: Ipv4Addr = addr.trim().parse().map_err(|_| ())?;
        let prefix: u8 = prefix.trim().parse().map_err(|_| ())?;
        if prefix > 32 {
            return Err(());
        }
        Ok(Self { addr, prefix })
    }
}

impl V4Cidr {
    /// 前缀掩码比对；prefix 0（0.0.0.0/0）匹配全部 IPv4
    fn contains(&self, ip: IpAddr) -> bool {
        let IpAddr::V4(v4) = ip else {
            return false;
        };
        let mask = if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(self.prefix))
        };
        u32::from(self.addr) & mask == u32::from(v4) & mask
    }
}

/// XFF 单条目 → IP：剥端口（"1.2.3.4:5678" 与 "[v6]:port" 两种带端口写法）后
/// 解析；非法条目返回 None。裸 IPv6 不能按冒号剥端口（否则 "::1" 被截断）。
fn parse_xff_ip(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, _) = rest.split_once(']')?;
        return host.parse::<IpAddr>().ok().map(normalize_ipv4_mapped);
    }
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(normalize_ipv4_mapped(ip));
    }
    // 剩余唯一合法形态是 IPv4:port；端口段非法即整体拒绝
    let (host, port) = entry.rsplit_once(':')?;
    if port.parse::<u16>().is_ok() {
        return host.parse::<Ipv4Addr>().ok().map(IpAddr::V4);
    }
    None
}

/// ::ffff:a.b.c.d（IPv4-mapped IPv6）归一为 IPv4，避免同一客户端产生两种
/// 统计/限流桶键；非 mapped 范围的 v6 原样保留
fn normalize_ipv4_mapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer(addr: &str) -> SocketAddr {
        // 含冒号即 IPv6：SocketAddr 解析要求 [v6]:port 带方括号
        let s = if addr.contains(':') {
            format!("[{addr}]:12345")
        } else {
            format!("{addr}:12345")
        };
        s.parse().expect("valid peer")
    }

    fn ip(addr: &str) -> IpAddr {
        addr.parse().expect("valid ip")
    }

    fn xff(value: &'static str) -> Option<HeaderValue> {
        Some(HeaderValue::from_static(value))
    }

    /// resolve_trusted_ip 的测试包装：持有 XFF 所有权并在调用处转引用
    fn trusted(peer_addr: &str, forwarded: Option<HeaderValue>, cidrs: &str) -> IpAddr {
        resolve_trusted_ip(peer(peer_addr), forwarded.as_ref(), cidrs)
    }

    #[test]
    fn trusted_peer_takes_first_xff_entry() {
        let resolved = trusted("10.1.2.3", xff("203.0.113.7, 10.1.2.3"), "10.0.0.0/8");
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    #[test]
    fn untrusted_peer_ignores_xff() {
        let resolved = trusted("8.8.8.8", xff("6.6.6.6"), "10.0.0.0/8");
        assert_eq!(resolved, ip("8.8.8.8"));
    }

    #[test]
    fn empty_trust_list_means_direct_topology() {
        let resolved = trusted("10.1.2.3", xff("6.6.6.6"), "");
        assert_eq!(resolved, ip("10.1.2.3"));
        let resolved = trusted("10.1.2.3", xff("6.6.6.6"), " , ");
        assert_eq!(resolved, ip("10.1.2.3"));
    }

    #[test]
    fn malformed_cidr_entries_skipped_valid_ones_still_match() {
        let resolved = resolve_trusted_ip(
            peer("192.168.1.5"),
            None,
            "banana, 192.168.0.0/16, 10.0.0.0/999, 10.0.0.0",
        );
        assert_eq!(resolved, ip("192.168.1.5"));
    }

    #[test]
    fn only_malformed_cidrs_means_direct() {
        let resolved = trusted("192.168.1.5", xff("6.6.6.6"), "banana/8, 10.0.0.0/999");
        assert_eq!(resolved, ip("192.168.1.5"));
    }

    #[test]
    fn cidr_prefix_is_exact() {
        let resolved = trusted("192.168.1.5", None, "192.168.1.5/32");
        assert_eq!(resolved, ip("192.168.1.5"));
        // /32 只匹配自身：相邻地址不受信，不采信其 XFF
        let resolved = trusted("192.168.1.6", xff("6.6.6.6"), "192.168.1.5/32");
        assert_eq!(resolved, ip("192.168.1.6"));
    }

    #[test]
    fn zero_prefix_matches_every_ipv4() {
        let resolved = trusted("203.0.113.9", xff("6.6.6.6"), "0.0.0.0/0");
        assert_eq!(resolved, ip("6.6.6.6"));
    }

    #[test]
    fn malformed_xff_entries_fall_back_to_peer() {
        for bad in ["not-an-ip", "999.1.2.3", "1.2.3.4:abc", ""] {
            let resolved = trusted("10.1.2.3", xff(bad), "10.0.0.0/8");
            assert_eq!(resolved, ip("10.1.2.3"), "entry {bad:?} must be rejected");
        }
    }

    #[test]
    fn xff_entry_with_port_is_stripped() {
        let resolved = trusted("10.1.2.3", xff("203.0.113.7:5678"), "10.0.0.0/8");
        assert_eq!(resolved, ip("203.0.113.7"));
        let resolved = trusted("10.1.2.3", xff("[2001:db8::1]:443"), "10.0.0.0/8");
        assert_eq!(resolved, ip("2001:db8::1"));
    }

    #[test]
    fn bare_ipv6_xff_entry_survives_port_strip() {
        // "::1" 若按冒号剥端口会被截断成 "::"，此断言锁定不剥
        let resolved = trusted("10.1.2.3", xff("2001:db8::1"), "10.0.0.0/8");
        assert_eq!(resolved, ip("2001:db8::1"));
    }

    #[test]
    fn v4_mapped_v6_peer_normalized_before_matching() {
        // 双栈 socket 的 v4-mapped 对端按 v4 网段判定受信
        let resolved = trusted("::ffff:10.1.2.3", xff("203.0.113.7"), "10.0.0.0/8");
        assert_eq!(resolved, ip("203.0.113.7"));
        // 网段外的 mapped 对端不受信：忽略 XFF，返回归一后的 v4 地址
        let resolved = trusted("::ffff:8.8.8.8", xff("6.6.6.6"), "10.0.0.0/8");
        assert_eq!(resolved, ip("8.8.8.8"));
    }

    #[test]
    fn v4_mapped_v6_xff_entry_normalized() {
        let resolved = trusted("10.1.2.3", xff("::ffff:203.0.113.7"), "10.0.0.0/8");
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    // ---------- Content-Length 提前检查（审计 #6a） ----------

    fn req_with_length(value: &'static str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn oversized_content_length_rejected() {
        let resp = oversized_content_length_response(&req_with_length("999999"), 1024)
            .expect("over-limit must be rejected");
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            resp.headers().get(header::CONNECTION),
            Some(&HeaderValue::from_static("close")),
            "413 必须带 Connection: close"
        );
    }

    #[tokio::test]
    async fn oversized_rejection_body_is_json_error_shape() {
        // 恰好超限一字节：200MiB + 1
        let resp = oversized_content_length_response(&req_with_length("209715201"), 200 * 1024 * 1024)
            .expect("over-limit must be rejected");
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["error"]["type"], "request_too_large");
        assert_eq!(parsed["error"]["message"], "request body exceeds limit");
    }

    #[test]
    fn at_limit_missing_or_invalid_length_pass_through() {
        // 恰好等于上限：放行
        assert!(oversized_content_length_response(&req_with_length("1024"), 1024).is_none());
        assert!(oversized_content_length_response(&req_with_length("1024"), 1025).is_none());
        // 无 Content-Length：放行（读取侧 DefaultBodyLimit 兜底）
        assert!(oversized_content_length_response(&axum::http::HeaderMap::new(), 1024).is_none());
        // 非法头值：不误判，放行
        assert!(oversized_content_length_response(&req_with_length("garbage"), 1024).is_none());
    }

    // ---------- /metrics 渲染契约 ----------

    #[test]
    fn metrics_render_contains_help_type_lines_for_every_family() {
        let m = crate::metrics::Metrics::new();
        m.conn_accepted
            .store(7, std::sync::atomic::Ordering::Relaxed);
        m.record_upstream_error(3);
        m.record_upstream_error(3);
        m.record_upstream_error(9);
        m.record_breaker_open();
        let out = m.render();
        for family in [
            "gateway_conn_accepted_total",
            "gateway_conn_error_total",
            "gateway_tls_handshake_fail_total",
            "gateway_client_abort_total",
            "gateway_upstream_error_total",
            "gateway_breaker_open_total",
        ] {
            assert!(
                out.contains(&format!("# HELP {family} ")),
                "missing HELP line for {family}"
            );
            assert!(
                out.contains(&format!("# TYPE {family} counter")),
                "missing TYPE line for {family}"
            );
        }
        assert!(out.contains("gateway_conn_accepted_total 7"));
        // 带 provider_id 标签的样本紧跟总数之后
        assert!(out.contains("gateway_upstream_error_total{provider_id=\"3\"} 2"));
        assert!(out.contains("gateway_upstream_error_total{provider_id=\"9\"} 1"));
        assert!(out.contains("gateway_breaker_open_total 1"));
    }
}
