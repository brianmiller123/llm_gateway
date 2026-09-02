//! 上游供应商主动健康探测：GET `{base_url}/1/status`。
//!
//! 状态页此前仅依赖 usage_logs 窗口错误率（被动证据）：无流量或流量陈旧时
//! 无法判断服务是否真正可用。本模块向各渠道 base_url 下的 `/1/status` 发起
//! 探测，按「HTTP 状态码 + 响应体内容（status 字段 / 服务标识）」综合判定：
//!
//! - 2xx + `status` 字段识别词汇表（ok/degraded/down 族）→ 结论性判定
//! - 2xx + 无 status 字段但含服务标识（service/server/version/success=true 等）
//!   → 按连通性判 operational（正向证据）
//! - 2xx + 非 JSON / 无法识别的格式 → degraded（可达但无法确认服务身份，
//!   防劫持/错误页误报为正常）
//! - 404 / 401 / 403 / 429 / 其他 4xx → inconclusive（端点不可用作判定依据，
//!   回退调用统计；服务本身可达，不武断判死）
//! - 5xx → down；网络层失败（DNS/TCP/TLS/超时，重试 1 次后仍失败）→ down
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

/// 探测 URL：与 proxy::send_upstream 同款 base_url 清洗（去尾 `/`、剥 query/fragment）。
/// base_url 约定以 /v1 结尾的渠道会得到 `{base}/v1/1/status`——若端点未部署将
/// 404 → inconclusive 回退调用统计，不会误报。
pub fn health_url(base_url: &str) -> String {
    let base = base_url
        .trim()
        .trim_end_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    format!("{base}{HEALTH_PATH}")
}

/// 探测单个供应商：网络层失败重试 1 次；HTTP 层响应不重试（服务已应答，结果即证据）
pub async fn probe_provider(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: Option<&str>,
) -> ProbeResult {
    let url = health_url(&provider.base_url);
    let started = std::time::Instant::now();
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(PROBE_RETRY_DELAY).await;
        }
        let mut rb = client.get(&url).timeout(PROBE_TIMEOUT);
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
                return classify_response(resp, url, latency).await;
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
        verdict: Verdict::Down,
        detail: Some(format!(
            "健康检查{kind}（{url}，重试后仍失败）：{}",
            error_chain(&e)
        )),
        latency_ms: Some(elapsed),
        http_status: None,
        endpoint: url,
        checked_at: Utc::now(),
    }
}

/// HTTP 响应 → 判定：状态码分诊 + 2xx 响应体解析
async fn classify_response(resp: reqwest::Response, url: String, latency: Duration) -> ProbeResult {
    let code = resp.status();
    let body = read_body_limited(resp, PROBE_BODY_LIMIT).await;
    let checked_at = Utc::now();
    let latency_ms = Some(latency.as_millis() as i64);
    let (verdict, detail) = if code.is_success() {
        classify_success_body(&String::from_utf8_lossy(&body))
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
}
