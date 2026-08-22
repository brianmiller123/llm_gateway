//! 公开服务状态页 API（仿 status.openai.com）：组件实时状态（网关 / PostgreSQL /
//! 各上游供应商）+ 近 30 天按日可用率 + 事故历史。
//!
//! 无鉴权公开访问——状态页设计为对外分享；仅暴露供应商名称与错误率，
//! 不含 Key、请求内容等敏感信息。
//!
//! 状态判定基于真实调用记录（usage_logs，status >= 400 视为失败；
//! 鉴权/限流等客户端错误在进入代理管线前即返回，不写 usage_logs，不会污染错误率）：
//! - 窗口错误率 >= 50% → down；>= 10% → degraded；否则 operational
//! - 窗口样本不足（< 5 次调用）时按 10 分钟 → 60 分钟 → 24 小时顺序回退
//! - 24 小时无调用 → unknown（无流量数据）
//! - 事故 = 单小时桶调用 >= 5 且错误率 >= 50%，相邻事故小时合并为一次事故

use std::collections::HashMap;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::FromRow;

use crate::error::AppError;
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
    let provider_names: HashMap<i64, String> = providers
        .iter()
        .map(|p| (p.id, p.name.clone()))
        .collect();

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
        },
    ];

    // 2. 供应商组件：窗口错误率 → 实时状态
    let windows: HashMap<i64, WindowRow> =
        windows.into_iter().map(|w| (w.provider_id, w)).collect();
    for p in &providers {
        let (status, detail) = window_status(windows.get(&p.id));
        components.push(ComponentOut {
            key: format!("provider:{}", p.id),
            name: p.name.clone(),
            kind: "provider",
            status,
            latency_ms: None,
            detail,
            uptime_30d: None,
            calls_30d: 0,
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
            format!("近 {label} 错误率 {:.1}%（{errors}/{calls} 请求失败）", rate * 100.0)
        };
        return (status, Some(detail));
    }
    ("unknown", Some("近期调用样本不足（< 5 次）".into()))
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
                let error_rate = if calls > 0 { errors as f64 / calls as f64 } else { 0.0 };
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
