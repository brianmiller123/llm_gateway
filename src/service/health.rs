//! 上游供应商主动健康探测：先 GET `{base_url}/1/status`，非结论性时回退
//! GET `{base_url}/models`（OpenAI 兼容事实标准探针）。
//!
//! 状态页此前仅依赖 usage_logs 窗口错误率（被动证据）：无流量或流量陈旧时
//! 无法判断服务是否真正可用。本模块按「HTTP 状态码 + 响应体内容」综合判定：
//!
//! 第一跳 `/1/status`（Algolia Status API 式公开惯例；主流 LLM 上游普遍未部署，
//! 实测 OpenAI/Anthropic/Moonshot/SiliconFlow 等均为 404）：
//! - 2xx + `status` 字段识别词汇表（ok/degraded/down 族）→ 结论性判定
//! - 2xx + 无 status 字段但含服务标识（service/server/version/success=true 等）
//!   → 按连通性判 operational（正向证据）
//! - 2xx + 非 JSON / 无法识别的格式 → degraded（可达但无法确认服务身份，
//!   防劫持/错误页误报为正常）
//! - 5xx → down；网络层失败（DNS/TCP/TLS/超时，重试 1 次后仍失败）→ down
//!
//! 第一跳返回 404/401/403/429/其他 4xx（非结论性）时回退第二跳
//! `{base_url}/models`（免费不计费，拼接方式与代理转发一致）：
//! - 2xx + OpenAI 兼容模型列表（object="list" / data 数组）→ operational
//! - 401/403 → degraded（端点存活但渠道 Key 无效/无权限，调用预期失败）
//! - 404/429/其他 4xx / 网络失败（第一跳刚拿到过 HTTP 应答，多为抖动）→
//!   仍非结论性，回退调用统计判定
//!
//! 两跳均非结论性时，组件状态完全采用调用统计（原机制）。
//!
//! 探测结果缓存于 AppState（TTL 30s）：状态页是公开端点，多观察者 × 60s 轮询
//! 不能每次都打满上游；TTL 小于页面轮询周期，保证每次轮询都拿到新鲜探测。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use reqwest::header;
use serde_json::Value;

use crate::state::AppState;
use crate::store::upstream::Provider;

/// 健康检查端点路径（拼接在 base_url 之后）
pub const HEALTH_PATH: &str = "/1/status";
/// 单次探测超时：状态页探测须快速返回（公开端点，多个供应商并发探测取最慢者）
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 网络层失败重试间隔（CN 网络偶发 TCP 抖动；与 admin_config::fetch_upstream_models 同理）
const PROBE_RETRY_DELAY: Duration = Duration::from_millis(400);
/// 响应体读取上限：健康响应应为小 JSON，超限截断（防异常上游拖爆内存）
const PROBE_BODY_LIMIT: usize = 64 * 1024;
/// 探测缓存 TTL：小于前端 60s 轮询周期 → 每次轮询触发新探测
const PROBE_TTL: chrono::Duration = chrono::Duration::seconds(30);

/// 探测判定结论
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 2xx + 可识别的健康信号
    Healthy,
    /// 可达但信号异常：自报降级 / 响应格式不符合预期
    Degraded,
    /// 明确不可用：网络失败 / 超时 / 5xx / status 字段自报故障
    Down,
    /// 无法作为判定依据（404/401/403/429 等非结论性响应）：回退调用统计
    Inconclusive,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::Degraded => "degraded",
            Verdict::Down => "down",
            Verdict::Inconclusive => "inconclusive",
        }
    }
}

/// 单次探测结果（缓存与融合的最小单元）
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub verdict: Verdict,
    /// 面向状态页的中文说明（错误原因 / 判定依据）
    pub detail: Option<String>,
    /// 探测往返耗时（网络失败时为整轮尝试总耗时）
    pub latency_ms: Option<i64>,
    /// HTTP 状态码（网络失败时 None）
    pub http_status: Option<u16>,
    /// 实际探测的完整 URL（诊断用；URL 不含 Key）
    pub endpoint: String,
    pub checked_at: DateTime<Utc>,
}

/// base_url 清洗（与 proxy::send_upstream 同款）：去首尾空白、去尾 `/`、剥 query/fragment。
fn clean_base(base_url: &str) -> &str {
    base_url
        .trim()
        .trim_end_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or("")
}

/// 第一跳探测 URL：`{base}/1/status`。base_url 约定以 /v1 结尾的渠道得到
/// `{base}/v1/1/status`；主流上游未部署时 404 → 触发 models 回退探测。
pub fn health_url(base_url: &str) -> String {
    format!("{}{}", clean_base(base_url), HEALTH_PATH)
}

/// 第二跳回退探测 URL：`{base}/models`——拼接方式与代理转发一致，
/// base_url 含 /v1 时即 OpenAI 兼容标准端点 `/v1/models`。
fn models_url(base_url: &str) -> String {
    format!("{}/models", clean_base(base_url))
}

/// 探测单个供应商：第一跳 `/1/status`；非结论性时回退第二跳 `/models`。
/// 网络层失败重试 1 次；HTTP 层响应不重试（服务已应答，结果即证据）。
pub async fn probe_provider(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: Option<&str>,
) -> ProbeResult {
    let first = request_probe(
        client,
        provider,
        api_key,
        &health_url(&provider.base_url),
        classify_success_body,
        Verdict::Down,
    )
    .await;
    if first.verdict != Verdict::Inconclusive {
        return first;
    }
    // 回退第二跳：models 列表是 OpenAI 兼容生态的事实标准连通性探针（免费不计费）。
    // 401/403 说明端点存活但渠道 Key 无效/无 models 权限 → 结论性 degraded；
    // 网络失败视为抖动（第一跳刚拿到过 HTTP 应答）→ 非结论性交还调用统计。
    let mut second = request_probe(
        client,
        provider,
        api_key,
        &models_url(&provider.base_url),
        classify_models_body,
        Verdict::Inconclusive,
    )
    .await;
    if second.verdict == Verdict::Inconclusive
        && matches!(second.http_status, Some(401) | Some(403))
    {
        second.verdict = Verdict::Degraded;
        second.detail = Some(format!(
            "models 端点存活但鉴权被拒（HTTP {}）：渠道 Key 可能失效或无 models 权限，调用预期失败",
            second.http_status.unwrap()
        ));
    }
    // 状态页详情拼接两跳叙事：保留第一跳 404/401/429 的原始原因
    second.detail = Some(match second.detail.take() {
        Some(d) => format!(
            "{}；回退 models 探测：{d}",
            first.detail.unwrap_or_default()
        ),
        None => first.detail.unwrap_or_default(),
    });
    second
}

/// 单次探测请求（两跳共用）：重试 1 次后仍网络失败 → 按 `network_verdict` 收场。
async fn request_probe(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: Option<&str>,
    url: &str,
    success: fn(&str) -> (Verdict, String),
    network_verdict: Verdict,
) -> ProbeResult {
    let started = std::time::Instant::now();
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(PROBE_RETRY_DELAY).await;
        }
        let mut rb = client.get(url).timeout(PROBE_TIMEOUT);
        // 认证形态与代理转发一致（send_upstream 同款）：x-api-key 或 Bearer。
        // Key 只发往该渠道自身的 base_url，与真实调用暴露面一致，无额外泄露。
        if let Some(k) = api_key {
            rb = if provider
                .auth_scheme
                .trim()
                .eq_ignore_ascii_case("x-api-key")
            {
                rb.header("x-api-key", k)
            } else {
                rb.header(header::AUTHORIZATION, format!("Bearer {k}"))
            };
        }
        match rb.send().await {
            Ok(resp) => {
                let latency = started.elapsed();
                return classify_response(resp, url.to_string(), latency, success).await;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let elapsed = started.elapsed().as_millis() as i64;
    let e = last_err.expect("at least one attempt recorded");
    let kind = if e.is_timeout() {
        "超时"
    } else if e.is_connect() {
        "连接失败"
    } else {
        "请求失败"
    };
    ProbeResult {
        verdict: network_verdict,
        detail: Some(format!(
            "健康检查{kind}（{url}，重试后仍失败）：{}",
            error_chain(&e)
        )),
        latency_ms: Some(elapsed),
        http_status: None,
        endpoint: url.to_string(),
        checked_at: Utc::now(),
    }
}

/// HTTP 响应 → 判定：状态码分诊 + 2xx 响应体解析（`success` 判定器由探测跳别提供）
async fn classify_response(
    resp: reqwest::Response,
    url: String,
    latency: Duration,
    success: fn(&str) -> (Verdict, String),
) -> ProbeResult {
    let code = resp.status();
    let body = read_body_limited(resp, PROBE_BODY_LIMIT).await;
    let checked_at = Utc::now();
    let latency_ms = Some(latency.as_millis() as i64);
    let (verdict, detail) = if code.is_success() {
        success(&String::from_utf8_lossy(&body))
    } else if code.as_u16() == 404 {
        (
            Verdict::Inconclusive,
            format!("健康检查端点不存在（HTTP 404）：{url} 未部署，回退调用统计判定"),
        )
    } else if matches!(code.as_u16(), 401 | 403) {
        (
            Verdict::Inconclusive,
            format!(
                "健康检查被拒绝（HTTP {}，鉴权不通过）；服务可达，回退调用统计判定",
                code.as_u16()
            ),
        )
    } else if code.as_u16() == 429 {
        (
            Verdict::Inconclusive,
            "健康检查被限流（HTTP 429），回退调用统计判定".to_string(),
        )
    } else if code.is_server_error() {
        (
            Verdict::Down,
            format!(
                "健康检查 HTTP {code}：{}",
                snippet(&String::from_utf8_lossy(&body), 200)
            ),
        )
    } else {
        (
            Verdict::Inconclusive,
            format!("健康检查返回异常状态 HTTP {code}，回退调用统计判定"),
        )
    };
    ProbeResult {
        verdict,
        detail: Some(detail),
        latency_ms,
        http_status: Some(code.as_u16()),
        endpoint: url,
        checked_at,
    }
}

/// 2xx 响应体判定（纯函数，便于单测）：
/// status 字段词汇表 → 结论；服务标识 / success=true → 连通性正向证据；
/// 其余（非 JSON、无任何可识别字段）→ degraded（降级展示，不误报正常）。
fn classify_success_body(body: &str) -> (Verdict, String) {
    let trimmed = body.trim();
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return (
            Verdict::Degraded,
            format!(
                "HTTP 200 但响应体非 JSON（{}），无法确认服务状态",
                snippet(trimmed, 80)
            ),
        );
    };
    if let Some(sv) = v.get("status") {
        let Some(s) = (match sv {
            Value::String(s) => Some(s.trim().to_ascii_lowercase()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }) else {
            return (
                Verdict::Degraded,
                "HTTP 200 但 status 字段类型异常（非字符串/布尔），按降级展示".to_string(),
            );
        };
        return match s.as_str() {
            "ok" | "healthy" | "up" | "normal" | "serving" | "green" | "true" | "运行正常"
            | "正常" => (Verdict::Healthy, format!("健康检查 HTTP 200（status={s}）")),
            "degraded" | "warning" | "warn" | "yellow" | "性能下降" | "降级" => (
                Verdict::Degraded,
                format!("健康检查 HTTP 200，服务自报降级（status={s}）"),
            ),
            "down" | "error" | "fail" | "failed" | "critical" | "outage" | "red" | "false"
            | "故障" | "不可用" | "维护" => (
                Verdict::Down,
                format!("健康检查 HTTP 200，服务自报故障（status={s}）"),
            ),
            other => (
                Verdict::Degraded,
                format!("HTTP 200 但 status 字段值无法识别（\"{other}\"），按降级展示"),
            ),
        };
    }
    // 无 status 字段：找服务标识（证明对端确为目标服务而非劫持页/登录页）
    let ident = ["service", "server", "name", "app", "version", "build"]
        .into_iter()
        .find_map(|k| {
            v.get(k)
                .and_then(|x| match x {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .map(|val| (k, val))
        });
    if let Some((k, val)) = ident {
        return (
            Verdict::Healthy,
            format!("健康检查 HTTP 200，服务标识 {k}=\"{val}\"（无 status 字段，按连通性判定）"),
        );
    }
    // new-api 风格 {"success": bool, "data": {...}}
    match v.get("success") {
        Some(Value::Bool(true)) => {
            return (
                Verdict::Healthy,
                "健康检查 HTTP 200（success=true）".to_string(),
            );
        }
        Some(Value::Bool(false)) => {
            return (
                Verdict::Down,
                "健康检查 HTTP 200，success=false".to_string(),
            );
        }
        _ => {}
    }
    if v.get("data").map(Value::is_object).unwrap_or(false) {
        return (
            Verdict::Healthy,
            "健康检查 HTTP 200，含 data 状态对象（按连通性判定）".to_string(),
        );
    }
    (
        Verdict::Degraded,
        "HTTP 200 但响应格式不符合预期（未找到 status 字段或服务标识），按降级展示".to_string(),
    )
}

/// 第二跳 models 回退探测的 2xx 响应体判定：
/// OpenAI 兼容模型列表（object="list" 或含 data 数组，空列表也算——列出能力
/// 本身就是连通性证明）→ 正常；其余沿用通用判定（status 词汇表 / 服务标识 / 降级）。
fn classify_models_body(body: &str) -> (Verdict, String) {
    let trimmed = body.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        let is_list = v.get("object").and_then(Value::as_str) == Some("list")
            || v.get("data").map(Value::is_array).unwrap_or(false);
        if is_list {
            let count = v
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.len().to_string())
                .unwrap_or_else(|| "?".into());
            return (
                Verdict::Healthy,
                format!("models 端点 HTTP 200，返回模型列表（{count} 个模型）"),
            );
        }
    }
    classify_success_body(body)
}

/// 限量读取响应体；读一半失败时保留已读部分（够做判定/截断展示）
async fn read_body_limited(mut resp: reqwest::Response, max: usize) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < max {
        let Ok(Some(chunk)) = resp.chunk().await else {
            break;
        };
        let take = (max - buf.len()).min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    buf
}

fn snippet(body: &str, max_chars: usize) -> String {
    body.chars().take(max_chars).collect()
}

/// 展开错误链（hyper 底层原因：dns / tcp / tls）；与 fetch_upstream_models 同款
fn error_chain(e: &reqwest::Error) -> String {
    let mut detail = e.to_string();
    let mut src = std::error::Error::source(e);
    let mut hops = 0;
    while let Some(s) = src {
        if hops >= 4 {
            break;
        }
        detail.push_str(&format!(" -> {s}"));
        src = s.source();
        hops += 1;
    }
    detail
}

/// 供状态页取用：TTL 内复用缓存，过期条目（含新上线供应商）并发探测后落缓存。
/// 持有 tokio Mutex 跨 await——并发请求合并为一次探测批次，防公开端点被刷爆上游。
pub async fn snapshot(st: &AppState) -> HashMap<i64, ProbeResult> {
    let providers = st.providers.read().clone();
    let mut cache = st.provider_health.lock().await;
    let now = Utc::now();
    // 已删除/停用的供应商不再保留陈旧探测结果
    let ids: HashSet<i64> = providers.iter().map(|p| p.id).collect();
    cache.retain(|id, _| ids.contains(id));
    let stale: Vec<Provider> = providers
        .into_iter()
        .filter(|p| {
            cache
                .get(&p.id)
                .map_or(true, |r| now - r.checked_at > PROBE_TTL)
        })
        .collect();
    if !stale.is_empty() {
        let probes = stale.iter().map(|p| {
            let client = st.client.clone();
            let key = crate::crypto::decrypt(&p.api_key_encrypted, &st.cfg.master_key).ok();
            let p = p.clone();
            async move { (p.id, probe_provider(&client, &p, key.as_deref()).await) }
        });
        for (id, result) in join_all(probes).await {
            cache.insert(id, result);
        }
    }
    cache.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(body: &str) -> (Verdict, String) {
        classify_success_body(body)
    }

    #[test]
    fn health_url_sanitizes_base() {
        assert_eq!(health_url("https://a.com"), "https://a.com/1/status");
        assert_eq!(health_url("https://a.com/"), "https://a.com/1/status");
        assert_eq!(health_url("https://a.com/v1"), "https://a.com/v1/1/status");
        assert_eq!(
            health_url(" https://a.com/v1?x=1#f "),
            "https://a.com/v1/1/status"
        );
    }
    #[test]
    fn models_url_appends_models_path() {
        assert_eq!(models_url("https://a.com/v1"), "https://a.com/v1/models");
        assert_eq!(models_url("https://a.com"), "https://a.com/models");
        assert_eq!(
            models_url(" https://a.com/v1?x=1#f "),
            "https://a.com/v1/models"
        );
    }

    #[test]
    fn status_field_vocabulary() {
        for b in [
            "{\"status\":\"ok\"}",
            "{\"status\":\"Healthy\"}",
            "{\"status\":true}",
            "{\"status\":\" 正常 \"}",
        ] {
            assert_eq!(classify(b).0, Verdict::Healthy, "{b}");
        }
        for b in [
            "{\"status\":\"degraded\"}",
            "{\"status\":\"warning\"}",
            "{\"status\":\"降级\"}",
        ] {
            assert_eq!(classify(b).0, Verdict::Degraded, "{b}");
        }
        for b in [
            "{\"status\":\"down\"}",
            "{\"status\":\"error\"}",
            "{\"status\":false}",
            "{\"status\":\"不可用\"}",
        ] {
            assert_eq!(classify(b).0, Verdict::Down, "{b}");
        }
    }

    #[test]
    fn status_field_unknown_value_is_degraded() {
        let (v, d) = classify("{\"status\":\"kinda-fine\"}");
        assert_eq!(v, Verdict::Degraded);
        assert!(d.contains("kinda-fine"));
    }

    #[test]
    fn status_field_wrong_type_is_degraded() {
        assert_eq!(classify("{\"status\":{\"a\":1}}").0, Verdict::Degraded);
    }

    #[test]
    fn service_identifier_confirms_liveness() {
        for b in [
            "{\"service\":\"oxyapi\",\"version\":\"2\"}",
            "{\"name\":\"gw\",\"uptime\":1}",
            "{\"version\":3}",
        ] {
            assert_eq!(classify(b).0, Verdict::Healthy, "{b}");
        }
        let (v, d) = classify("{\"service\":\"oxyapi\"}");
        assert!(d.contains("service=\"oxyapi\""));
        assert_eq!(v, Verdict::Healthy);
    }

    #[test]
    fn new_api_style_success_field() {
        assert_eq!(
            classify("{\"success\":true,\"data\":{\"version\":\"x\"}}").0,
            Verdict::Healthy
        );
        assert_eq!(classify("{\"success\":false}").0, Verdict::Down);
    }

    #[test]
    fn non_json_and_shapeless_bodies_degrade() {
        let (v, d) = classify("<html>login</html>");
        assert_eq!(v, Verdict::Degraded);
        assert!(d.contains("非 JSON"));
        let (v, _) = classify("{\"foo\":1}");
        assert_eq!(v, Verdict::Degraded);
        let (v, _) = classify("");
        assert_eq!(v, Verdict::Degraded);
    }

    #[test]
    fn detail_messages_are_chinese_and_specific() {
        assert!(classify("{\"status\":\"degraded\"}").1.contains("自报降级"));
        assert!(classify("<html>").1.contains("非 JSON"));
    }
    #[test]
    fn models_list_bodies_are_healthy() {
        assert_eq!(
            classify_models_body("{\"object\":\"list\",\"data\":[{\"id\":\"m1\"}]}").0,
            Verdict::Healthy
        );
        let (v, d) = classify_models_body("{\"data\":[]}");
        assert_eq!(v, Verdict::Healthy);
        assert!(d.contains("0 个模型"));
        assert_eq!(
            classify_models_body("{\"object\":\"list\"}").0,
            Verdict::Healthy
        );
        // 带 status 字段的模型列表同样按列表判定（先于通用词汇表）
        assert_eq!(
            classify_models_body("{\"status\":\"weird\",\"data\":[]}").0,
            Verdict::Healthy
        );
    }

    #[test]
    fn non_list_models_bodies_fall_through_to_generic() {
        // 200 + 错误对象 / 形状不符 / 非 JSON → 沿用通用规则降级
        assert_eq!(
            classify_models_body("{\"error\":{\"message\":\"nope\"}}").0,
            Verdict::Degraded
        );
        assert_eq!(
            classify_models_body("<html>login</html>").0,
            Verdict::Degraded
        );
        assert_eq!(classify_models_body("").0, Verdict::Degraded);
    }

    // ---- 全链路：本地假上游验证两跳回退（/1/status → /models） ----

    fn test_provider(base: String) -> Provider {
        Provider {
            id: 1,
            name: "t".into(),
            api_type: "openai".into(),
            base_url: base,
            api_key_encrypted: String::new(),
            timeout_ms: 5000,
            extra_body: serde_json::json!({}),
            auth_scheme: "bearer".into(),
            extra_headers: serde_json::json!({}),
            supports_images: false,
        }
    }

    /// 假上游：按预置 (命中路径子串, 状态码, 响应体) 顺序应答，答完即退出。
    fn serve_fake_upstream(responses: Vec<(&'static str, u16, &'static str)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut queue = std::collections::VecDeque::from(responses);
            while let Some((path, code, body)) = queue.pop_front() {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    // GET 无请求体：读到头部终止符即可
                    let n = match std::io::Read::read(&mut stream, &mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf);
                let hit = head
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|p| p.contains(path));
                let (code, body) = if hit { (code, body) } else { (404, "{}") };
                let resp = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut stream, resp.as_bytes());
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn status_ok_short_circuits_without_models_fallback() {
        let base = serve_fake_upstream(vec![("/1/status", 200, "{\"status\":\"ok\"}")]);
        let client = reqwest::Client::new();
        let r = probe_provider(&client, &test_provider(base), Some("sk-test")).await;
        assert_eq!(r.verdict, Verdict::Healthy);
        assert!(r.endpoint.ends_with("/v1/1/status"), "{}", r.endpoint);
    }

    #[tokio::test]
    async fn status_404_falls_back_to_models_list() {
        let base = serve_fake_upstream(vec![
            ("/1/status", 404, "{\"error\":\"not found\"}"),
            (
                "/models",
                200,
                "{\"object\":\"list\",\"data\":[{\"id\":\"m1\"}]}",
            ),
        ]);
        let client = reqwest::Client::new();
        let r = probe_provider(&client, &test_provider(base), Some("sk-test")).await;
        assert_eq!(r.verdict, Verdict::Healthy);
        assert!(r.endpoint.ends_with("/v1/models"), "{}", r.endpoint);
        let d = r.detail.unwrap_or_default();
        assert!(d.contains("404"), "{d}");
        assert!(d.contains("回退 models 探测"), "{d}");
        assert!(d.contains("模型列表"), "{d}");
    }

    #[tokio::test]
    async fn models_auth_rejected_is_degraded() {
        let base = serve_fake_upstream(vec![
            ("/1/status", 404, "nf"),
            ("/models", 401, "{\"error\":{\"message\":\"bad key\"}}"),
        ]);
        let client = reqwest::Client::new();
        let r = probe_provider(&client, &test_provider(base), Some("sk-bad")).await;
        assert_eq!(r.verdict, Verdict::Degraded);
        let d = r.detail.unwrap_or_default();
        assert!(d.contains("鉴权被拒"), "{d}");
        assert!(r.endpoint.ends_with("/v1/models"), "{}", r.endpoint);
    }

    #[tokio::test]
    async fn both_hops_missing_is_inconclusive() {
        let base = serve_fake_upstream(vec![("/1/status", 404, "nf"), ("/models", 404, "nf")]);
        let client = reqwest::Client::new();
        let r = probe_provider(&client, &test_provider(base), Some("sk-test")).await;
        assert_eq!(r.verdict, Verdict::Inconclusive);
        let d = r.detail.unwrap_or_default();
        assert!(d.contains("回退 models 探测"), "{d}");
    }
}
