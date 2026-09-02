//! 公开服务状态页 API（仿 status.openai.com）：组件实时状态（网关 / PostgreSQL /
//! 各上游供应商）+ 近 30 天按日可用率 + 事故历史。
//!
//! 无鉴权公开访问——状态页设计为对外分享；仅暴露供应商名称与错误率，
//! 不含 Key、请求内容等敏感信息。
//!
//! 供应商状态 = 主动健康探测 × 调用统计 双证据融合（service::health）：
//! - 主动探测：GET `{base_url}/1/status`，按 HTTP 状态码 + 响应体（status 字段 /
//!   服务标识）判定；网络失败/超时/5xx/self-report down → down，格式异常 →
//!   degraded，404/401/429 等非结论性响应 → 回退调用统计。结果缓存 30s。
//! - 调用统计（被动证据，保留原机制）：usage_logs 窗口错误率（status >= 400 视为
//!   失败；鉴权/限流等客户端错误不写 usage_logs 不污染错误率）；窗口样本不足
//!   （< 5 次）按 10 分钟 → 60 分钟 → 24 小时回退；24 小时无调用 → unknown。
//! - 融合：探测存活时状态至多 degraded（调用统计的 down 常为 Key 失效/配额等
//!   客户端侧原因，健康检查无法也不应据此判死上游）；探测 down → down；
//!   探测 inconclusive → 完全采用调用统计。
//! - 事故 = 单小时桶调用 >= 5 且错误率 >= 50%，相邻事故小时合并为一次事故
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::FromRow;
use std::collections::HashMap;

use crate::error::AppError;
use crate::service::health::{self, ProbeResult, Verdict};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/endpoints", get(public_endpoints))
}

/// 公开：用户页面展示的 API 调用地址（按管理员可见性开关过滤）。
/// 仅暴露路径与地址，无任何敏感信息
async fn public_endpoints(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let eps = st.api_endpoints.read();
    let base = crate::service::endpoints::public_base(&st, &headers);
    let out = json!({
        "responses": if eps.responses_visible {
            json!({ "path": "/v1/responses", "address": format!("{base}/v1/responses") })
        } else {
            serde_json::Value::Null
        },
        "messages": if eps.messages_visible {
            json!({ "path": "/v1/messages", "address": format!("{base}/v1/messages") })
        } else {
            serde_json::Value::Null
        },
    });
    Ok(Json(out).into_response())
}

/// 状态判定阈值
const MIN_CALLS: i64 = 5;
const DOWN_RATE: f64 = 0.5;
const DEGRADED_RATE: f64 = 0.1;
const INCIDENT_MIN_CALLS: i64 = 5;
const INCIDENT_RATE: f64 = 0.5;
const UPTIME_DAYS: i64 = 30;
const MAX_INCIDENTS: usize = 20;

#[derive(Serialize)]
struct StatusResp {
    generated_at: DateTime<Utc>,
    overall: &'static str,
    components: Vec<ComponentOut>,
    uptime: Vec<UptimeSeries>,
    incidents: Vec<IncidentOut>,
    /// L3：当前进行中的代理请求数（活跃连接观测；cc-switch ActiveConnectionGuard 同款动机）
    active_requests: i64,
}

#[derive(Serialize)]
struct ComponentOut {
    key: String,
    name: String,
    kind: &'static str,
    status: &'static str,
    latency_ms: Option<i64>,
    detail: Option<String>,
    uptime_30d: Option<f64>,
    calls_30d: i64,
    /// 供应商组件：/1/status 主动探测元数据（系统组件为 None）
    probe: Option<ProbeMeta>,
}

/// 主动健康探测元数据（前端据此展示「探测正常/异常」与依据）
#[derive(Serialize)]
struct ProbeMeta {
    endpoint: String,
    http_status: Option<u16>,
    latency_ms: Option<i64>,
    checked_at: DateTime<Utc>,
    verdict: &'static str,
}

#[derive(Serialize)]
struct UptimeSeries {
    key: String,
    name: String,
    days: Vec<DayPoint>,
}

#[derive(Serialize)]
struct DayPoint {
    date: NaiveDate,
    success_rate: Option<f64>,
    calls: i64,
    errors: i64,
}

#[derive(Serialize)]
struct IncidentOut {
    provider_id: i64,
    provider_name: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    ongoing: bool,
    severity: &'static str,
    error_rate: f64,
    calls: i64,
    errors: i64,
    title: String,
}

#[derive(FromRow)]
struct WindowRow {
    provider_id: i64,
    calls_10m: i64,
    errors_10m: i64,
    calls_60m: i64,
    errors_60m: i64,
    calls_24h: i64,
    errors_24h: i64,
}

#[derive(FromRow)]
struct DailyRow {
    provider_id: i64,
    d: NaiveDate,
    calls: i64,
    errors: i64,
}

#[derive(FromRow)]
struct HourRow {
    provider_id: i64,
    hour: DateTime<Utc>,
    calls: i64,
    errors: i64,
}

async fn status(State(st): State<AppState>) -> Result<Response, AppError> {
    let started = std::time::Instant::now();
    let db_ok = sqlx::query("SELECT 1").execute(&st.pool).await.is_ok();
    let db_latency = started.elapsed().as_millis() as i64;

    let providers = st.providers.read().clone();
    let provider_names: HashMap<i64, String> =
        providers.iter().map(|p| (p.id, p.name.clone())).collect();

    // 用量数据（DB 不可用时置空，供应商组件统一标记 unknown）
    let (windows, daily, hourly) = if db_ok {
        let windows = sqlx::query_as::<_, WindowRow>(
            "SELECT provider_id, \
                    COUNT(*) FILTER (WHERE created_at >= now() - interval '10 minutes')::bigint AS calls_10m, \
                    COUNT(*) FILTER (WHERE created_at >= now() - interval '10 minutes' AND status >= 400)::bigint AS errors_10m, \
                    COUNT(*) FILTER (WHERE created_at >= now() - interval '60 minutes')::bigint AS calls_60m, \
                    COUNT(*) FILTER (WHERE created_at >= now() - interval '60 minutes' AND status >= 400)::bigint AS errors_60m, \
                    COUNT(*) FILTER (WHERE created_at >= now() - interval '24 hours')::bigint AS calls_24h, \
                    COUNT(*) FILTER (WHERE created_at >= now() - interval '24 hours' AND status >= 400)::bigint AS errors_24h \
             FROM usage_logs WHERE created_at >= now() - interval '24 hours' GROUP BY provider_id",
        )
        .fetch_all(&st.pool)
        .await;
        let daily = sqlx::query_as::<_, DailyRow>(
            "SELECT provider_id, (created_at AT TIME ZONE 'UTC')::date AS d, \
                    COUNT(*)::bigint AS calls, \
                    COUNT(*) FILTER (WHERE status >= 400)::bigint AS errors \
             FROM usage_logs WHERE created_at >= now() - interval '30 days' \
             GROUP BY provider_id, d ORDER BY d",
        )
        .fetch_all(&st.pool)
        .await;
        let hourly = sqlx::query_as::<_, HourRow>(
            "SELECT provider_id, date_trunc('hour', created_at) AS hour, \
                    COUNT(*)::bigint AS calls, \
                    COUNT(*) FILTER (WHERE status >= 400)::bigint AS errors \
             FROM usage_logs WHERE created_at >= now() - interval '30 days' \
             GROUP BY provider_id, hour ORDER BY hour",
        )
        .fetch_all(&st.pool)
        .await;
        match (windows, daily, hourly) {
            (Ok(w), Ok(d), Ok(h)) => (w, d, h),
            (e1, e2, e3) => {
                tracing::warn!(
                    windows = ?e1.as_ref().err(),
                    daily = ?e2.as_ref().err(),
                    hourly = ?e3.as_ref().err(),
                    "status: usage data queries failed, falling back to unknown components"
                );
                (vec![], vec![], vec![])
            }
        }
    } else {
        (vec![], vec![], vec![])
    };

    // 1. 系统组件：网关（能响应本请求即正常）+ PostgreSQL（SELECT 1）
    let mut components = vec![
        ComponentOut {
            key: "gateway".into(),
            name: "网关 API".into(),
            kind: "system",
            status: "operational",
            latency_ms: Some(db_latency),
            detail: None,
            uptime_30d: None,
            calls_30d: 0,
            probe: None,
        },
        ComponentOut {
            key: "postgres".into(),
            name: "PostgreSQL".into(),
            kind: "system",
            status: if db_ok { "operational" } else { "down" },
            latency_ms: Some(db_latency),
            detail: if db_ok {
                None
            } else {
                Some("数据库连接失败，用量统计暂不可用".into())
            },
            uptime_30d: None,
            calls_30d: 0,
            probe: None,
        },
    ];

    // 2. 供应商组件：/1/status 主动探测 × 调用统计窗口错误率 融合
    // 探测结果带 TTL 缓存（service::health），公开端点多观察者轮询不会打满上游
    let probes = health::snapshot(&st).await;
    let windows: HashMap<i64, WindowRow> =
        windows.into_iter().map(|w| (w.provider_id, w)).collect();
    for p in &providers {
        let (status, detail, latency_ms, probe) =
            fuse_provider(probes.get(&p.id), window_status(windows.get(&p.id)));
        components.push(ComponentOut {
            key: format!("provider:{}", p.id),
            name: p.name.clone(),
            kind: "provider",
            status,
            latency_ms,
            detail,
            uptime_30d: None,
            calls_30d: 0,
            probe,
        });
    }

    // 3. 按日可用率（仅供应商；系统组件无历史合成探测数据，不展示）
    let mut by_provider: HashMap<i64, Vec<DailyRow>> = HashMap::new();
    for r in daily {
        by_provider.entry(r.provider_id).or_default().push(r);
    }
    let mut uptime = Vec::new();
    let today = Utc::now().date_naive();
    for p in &providers {
        let key = format!("provider:{}", p.id);
        let rows = by_provider.remove(&p.id).unwrap_or_default();
        let mut it = rows.into_iter().peekable();
        let mut total_calls = 0i64;
        let mut total_errors = 0i64;
        let mut days = Vec::with_capacity(UPTIME_DAYS as usize);
        for offset in (0..UPTIME_DAYS).rev() {
            let date = today - Duration::days(offset);
            let mut day = DayPoint {
                date,
                success_rate: None,
                calls: 0,
                errors: 0,
            };
            while let Some(r) = it.peek() {
                if r.d == date {
                    let r = it.next().unwrap();
                    day.calls = r.calls;
                    day.errors = r.errors;
                    // 有调用才有行，calls 恒 > 0
                    day.success_rate = Some((r.calls - r.errors) as f64 / r.calls as f64);
                    total_calls += r.calls;
                    total_errors += r.errors;
                    break;
                }
                if r.d < date {
                    it.next();
                    continue;
                }
                break;
            }
            days.push(day);
        }
        let uptime_30d = if total_calls > 0 {
            Some((total_calls - total_errors) as f64 / total_calls as f64)
        } else {
            None
        };
        if let Some(c) = components.iter_mut().find(|c| c.key == key) {
            c.uptime_30d = uptime_30d;
            c.calls_30d = total_calls;
        }
        uptime.push(UptimeSeries {
            key,
            name: p.name.clone(),
            days,
        });
    }

    // 4. 事故：小时桶合并（含已停用供应商的历史数据）
    let incidents = build_incidents(hourly, &provider_names);

    // 5. 整体状态：任一组件 down → down；任一 degraded → degraded
    let overall = if components.iter().any(|c| c.status == "down") {
        "down"
    } else if components.iter().any(|c| c.status == "degraded") {
        "degraded"
    } else {
        "operational"
    };

    Ok(Json(StatusResp {
        generated_at: Utc::now(),
        overall,
        components,
        uptime,
        incidents,
        active_requests: st
            .active_requests
            .load(std::sync::atomic::Ordering::Relaxed),
    })
    .into_response())
}

/// 供应商实时状态：按 10 分钟 → 60 分钟 → 24 小时顺序取首个样本充足的窗口
fn window_status(w: Option<&WindowRow>) -> (&'static str, Option<String>) {
    let Some(w) = w else {
        return ("unknown", Some("近 24 小时无调用流量".into()));
    };
    let windows: [(&str, i64, i64); 3] = [
        ("10 分钟", w.calls_10m, w.errors_10m),
        ("60 分钟", w.calls_60m, w.errors_60m),
        ("24 小时", w.calls_24h, w.errors_24h),
    ];
    for (label, calls, errors) in windows {
        if calls < MIN_CALLS {
            continue;
        }
        let rate = errors as f64 / calls as f64;
        let status = if rate >= DOWN_RATE {
            "down"
        } else if rate >= DEGRADED_RATE {
            "degraded"
        } else {
            "operational"
        };
        let detail = if errors == 0 {
            format!("近 {label} {calls} 次请求全部成功")
        } else {
            format!(
                "近 {label} 错误率 {:.1}%（{errors}/{calls} 请求失败）",
                rate * 100.0
            )
        };
        return (status, Some(detail));
    }
    ("unknown", Some("近期调用样本不足（< 5 次）".into()))
}

/// 探测证据 × 调用统计证据融合（状态判定核心）：
/// - 探测 down（连接失败/超时/5xx/自报故障）→ down：直连不可达是结论性证据
/// - 探测健康/降级 → 取 probe 与 min(traffic, degraded) 中更差者：服务存活时，
///   调用统计的 down（常见为 Key 失效/配额/模型 4xx 等调用侧原因）至多升到
///   degraded；调用统计 unknown（无流量）不拉低健康探测
/// - 探测 inconclusive（端点 404/401/403/429 等非结论性响应）→ 完全回退调用统计
fn fuse_provider(
    probe: Option<&ProbeResult>,
    (traffic_status, traffic_detail): (&'static str, Option<String>),
) -> (&'static str, Option<String>, Option<i64>, Option<ProbeMeta>) {
    let Some(pr) = probe else {
        return (traffic_status, traffic_detail, None, None);
    };
    let meta = ProbeMeta {
        endpoint: pr.endpoint.clone(),
        http_status: pr.http_status,
        latency_ms: pr.latency_ms,
        checked_at: pr.checked_at,
        verdict: pr.verdict.as_str(),
    };
    match pr.verdict {
        Verdict::Inconclusive => (
            traffic_status,
            join_details(pr.detail.clone(), traffic_detail),
            None,
            Some(meta),
        ),
        Verdict::Down => ("down", pr.detail.clone(), None, Some(meta)),
        Verdict::Healthy | Verdict::Degraded => {
            let probe_status = if pr.verdict == Verdict::Healthy {
                "operational"
            } else {
                "degraded"
            };
            let traffic_eff = match traffic_status {
                "unknown" => "operational",
                s if sev(s) > sev("degraded") => "degraded",
                s => s,
            };
            let status = if sev(probe_status) >= sev(traffic_eff) {
                probe_status
            } else {
                traffic_eff
            };
            // 最终状态比探测结论差 → 拼入调用统计明细解释冲突；否则以探测明细为准
            let detail = if sev(status) > sev(probe_status) {
                join_details(pr.detail.clone(), traffic_detail)
            } else {
                pr.detail.clone().or(traffic_detail)
            };
            (status, detail, pr.latency_ms, Some(meta))
        }
    }
}

/// 状态严重度排序：operational < unknown < degraded < down
fn sev(s: &str) -> u8 {
    match s {
        "down" => 3,
        "degraded" => 2,
        "unknown" => 1,
        _ => 0,
    }
}

fn join_details(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(x), Some(y)) => Some(format!("{x}；{y}")),
        (x, y) => x.or(y),
    }
}

/// 事故检测：单小时桶 调用 >= 5 且错误率 >= 50% → 事故小时；相邻小时合并为一次事故。
/// 严重度：范围内峰值错误率 >= 80% → major，否则 minor。
fn build_incidents(rows: Vec<HourRow>, names: &HashMap<i64, String>) -> Vec<IncidentOut> {
    let mut by_provider: HashMap<i64, Vec<HourRow>> = HashMap::new();
    for r in rows {
        by_provider.entry(r.provider_id).or_default().push(r);
    }
    let now = Utc::now();
    let mut out = Vec::new();
    for (pid, mut buckets) in by_provider {
        buckets.sort_by_key(|b| b.hour);
        let name = names
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| format!("#{pid}"));
        // (start_hour, last_hour, calls, errors, peak_rate)
        let mut run: Option<(DateTime<Utc>, DateTime<Utc>, i64, i64, f64)> = None;
        let mut flush = |run: &mut Option<(DateTime<Utc>, DateTime<Utc>, i64, i64, f64)>| {
            if let Some((start, end, calls, errors, peak)) = run.take() {
                let error_rate = if calls > 0 {
                    errors as f64 / calls as f64
                } else {
                    0.0
                };
                out.push(IncidentOut {
                    provider_id: pid,
                    provider_name: name.clone(),
                    start,
                    end: end + Duration::hours(1),
                    ongoing: end >= now - Duration::hours(1),
                    severity: if peak >= 0.8 { "major" } else { "minor" },
                    error_rate,
                    calls,
                    errors,
                    title: format!("{name} 上游服务异常"),
                });
            }
        };
        for b in buckets {
            let rate = b.errors as f64 / b.calls as f64;
            let bad = b.calls >= INCIDENT_MIN_CALLS && rate >= INCIDENT_RATE;
            if bad {
                match &mut run {
                    Some((_, end, calls, errors, peak))
                        if *end == b.hour || *end == b.hour - Duration::hours(1) =>
                    {
                        *end = b.hour;
                        *calls += b.calls;
                        *errors += b.errors;
                        *peak = peak.max(rate);
                    }
                    _ => {
                        flush(&mut run);
                        run = Some((b.hour, b.hour, b.calls, b.errors, rate));
                    }
                }
            } else {
                flush(&mut run);
            }
        }
        flush(&mut run);
    }
    out.sort_by(|a, b| b.start.cmp(&a.start));
    out.truncate(MAX_INCIDENTS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::health::Verdict;

    fn probe(v: Verdict) -> ProbeResult {
        ProbeResult {
            verdict: v,
            detail: Some(format!("探测明细：{:?}", v)),
            latency_ms: Some(42),
            http_status: Some(200),
            endpoint: "https://up.example.com/1/status".into(),
            checked_at: Utc::now(),
        }
    }

    fn fuse(v: Option<Verdict>, traffic: &'static str) -> (&'static str, Option<String>) {
        let p = v.map(|v| probe(v));
        let (s, d, _, m) = fuse_provider(
            p.as_ref(),
            (traffic, Some(format!("调用统计明细：{traffic}"))),
        );
        assert_eq!(m.is_some(), v.is_some());
        (s, d)
    }

    #[test]
    fn no_probe_falls_back_to_traffic() {
        assert_eq!(fuse(None, "degraded").0, "degraded");
        assert_eq!(fuse(None, "unknown").0, "unknown");
    }

    #[test]
    fn healthy_probe_beats_stale_or_absent_traffic() {
        assert_eq!(fuse(Some(Verdict::Healthy), "operational").0, "operational");
        // 无流量（unknown）不拉低健康探测——这是本次重构要解决的核心场景
        assert_eq!(fuse(Some(Verdict::Healthy), "unknown").0, "operational");
    }

    #[test]
    fn alive_probe_caps_traffic_down_at_degraded() {
        let (s, d) = fuse(Some(Verdict::Healthy), "down");
        assert_eq!(s, "degraded");
        // 冲突时明细合并解释
        assert!(d.unwrap().contains("调用统计明细"));
        let (s, _) = fuse(Some(Verdict::Degraded), "down");
        assert_eq!(s, "degraded");
    }

    #[test]
    fn traffic_degraded_worsens_healthy_probe() {
        let (s, d) = fuse(Some(Verdict::Healthy), "degraded");
        assert_eq!(s, "degraded");
        assert!(d.unwrap().contains("探测明细"));
    }

    #[test]
    fn down_probe_is_conclusive() {
        assert_eq!(fuse(Some(Verdict::Down), "operational").0, "down");
        assert_eq!(fuse(Some(Verdict::Down), "unknown").0, "down");
    }

    #[test]
    fn inconclusive_probe_defers_to_traffic() {
        assert_eq!(fuse(Some(Verdict::Inconclusive), "down").0, "down");
        assert_eq!(fuse(Some(Verdict::Inconclusive), "unknown").0, "unknown");
        assert_eq!(
            fuse(Some(Verdict::Inconclusive), "operational").0,
            "operational"
        );
    }

    #[test]
    fn sev_orders_status_strings() {
        assert!(sev("operational") < sev("unknown"));
        assert!(sev("unknown") < sev("degraded"));
        assert!(sev("degraded") < sev("down"));
    }
}
