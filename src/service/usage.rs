use std::collections::HashMap;
use parking_lot::Mutex;

use crate::error::AppError;
use crate::state::AppState;
use crate::store::usage::{record_usage, Usage, UsageMeta};

/// 月度用量计数缓存：key = (user_id, "YYYY-MM") → (tokens, cost)
/// 记账事务提交后更新；配额预检查读取；30s 周期从 DB 重载兜底
#[derive(Default)]
pub struct UsageCache {
    monthly: Mutex<HashMap<(i64, String), (i64, f64)>>,
}

impl UsageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, user_id: i64, month: &str) -> (i64, f64) {
        self.monthly
            .lock()
            .get(&(user_id, month.to_string()))
            .copied()
            .unwrap_or((0, 0.0))
    }

    pub fn incr(&self, user_id: i64, month: &str, tokens: i64, cost: f64) {
        let mut map = self.monthly.lock();
        let e = map.entry((user_id, month.to_string())).or_insert((0, 0.0));
        e.0 += tokens;
        e.1 += cost;
    }
}

/// 配额预检查：超限返回 429；首次超限写 audit 告警（按 用户×月 去重）
pub fn check_quota(st: &AppState, user_id: i64) -> Result<(), AppError> {
    let month = chrono::Utc::now().format("%Y-%m").to_string();
    let (tokens, cost) = st.usage.get(user_id, &month);

    let quotas = st.quotas.read();
    let Some(q) = quotas.get(&user_id) else {
        return Ok(());
    };
    let exceeded: Option<(String, f64, f64)> = if let Some(limit) = q.monthly_token_quota {
        if tokens >= limit {
            Some(("token".into(), tokens as f64, limit as f64))
        } else {
            None
        }
    } else if let Some(limit) = q.monthly_cost_quota {
        if cost >= limit {
            Some(("cost".into(), cost, limit))
        } else {
            None
        }
    } else {
        None
    };
    let Some((kind, used, limit)) = exceeded else {
        return Ok(());
    };

    tracing::warn!(user_id, kind, used, limit, "quota exceeded");
    // 首次超限（本进程内存）→ 审计告警一条，避免每次请求刷库
    let mut seen = st.quota_alerts.lock();
    if seen.insert((user_id, month.clone())) {
        drop(seen);
        let st = st.clone();
        let month2 = month.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::store::audit::log(
                &st.pool,
                None,
                "quota.exceeded",
                Some("user"),
                Some(user_id),
                Some(serde_json::json!({"kind": kind, "used": used, "limit": limit, "month": month2})),
            )
            .await
            {
                tracing::warn!(error = %e, "quota alert audit failed");
            }
        });
    }
    Err(AppError::QuotaExceeded)
}

/// 记账执行器：同步事务直写 + 更新缓存
pub async fn record(
    st: &AppState,
    meta: &UsageMeta,
    usage: Option<&Usage>,
    status: u16,
    latency_ms: i64,
) {
    let cost = crate::service::routing::compute_cost(st, &meta.model, {
        usage.and_then(|u| u.prompt_tokens)
    }, usage.and_then(|u| u.completion_tokens));
    match record_usage(&st.pool, meta, usage, status, latency_ms, cost).await {
        Ok(()) => {
            if let Some(user_id) = meta.user_id {
                let month = chrono::Utc::now().format("%Y-%m").to_string();
                let tokens = usage
                    .map(|u| u.prompt_tokens.unwrap_or(0) + u.completion_tokens.unwrap_or(0))
                    .unwrap_or(0);
                st.usage.incr(user_id, &month, tokens, cost);
            }
            tracing::info!(
                request_id = %meta.request_id,
                model = %meta.model,
                status,
                latency_ms,
                input_tokens = usage.and_then(|u| u.prompt_tokens),
                output_tokens = usage.and_then(|u| u.completion_tokens),
                cost,
                "usage recorded"
            );
        }
        Err(e) => {
            // 记账失败不回滚客户端响应，仅告警（request_id 保证可对账）
            tracing::error!(request_id = %meta.request_id, error = %e, "usage record failed");
        }
    }
}
