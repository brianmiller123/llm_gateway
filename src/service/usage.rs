use std::collections::HashMap;
use parking_lot::Mutex;

use crate::error::AppError;
use crate::state::AppState;
use crate::store::usage::{record_usage, Usage, UsageMeta};

/// 月度用量计数缓存：key = (user_id, "YYYY-MM") → (tokens, cost)
/// 记账事务提交后更新；配额预检查读取；启动/周期从 DB 重载兜底（防重启后配额清零）
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
        // 饱和加法防畸形 usage 把配额缓存拉成负数
        e.0 = e.0.saturating_add(tokens);
        e.1 += cost;
    }

    /// 从 DB 全量重载当月计数（只覆盖不清理，避免并发 incr 在快照与写入之间丢失；
    /// 条目数按 用户×月 增长，有界）。启动时与周期 reload 中调用。
    pub async fn reload_from_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        let rows: Vec<(i64, i64, f64)> = sqlx::query_as(
            "SELECT user_id, tokens, cost::float8 FROM user_monthly_usage WHERE month = $1",
        )
        .bind(&month)
        .fetch_all(pool)
        .await?;
        let mut map = self.monthly.lock();
        for (user_id, tokens, cost) in rows {
            map.insert((user_id, month.clone()), (tokens, cost));
        }
        Ok(())
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
    // 管理员测试调用不记账：不写 usage_logs，不污染状态页/用量统计
    if meta.test_call {
        return;
    }
    // M33：计价双查（cc-switch pricing_model_source 语义）——客户端模型名优先，
    // 回退路由映射后的上游模型名；均无价格 → 记 0 并告警（此前静默记 0，
    // 无对账线索）
    let mut cost = crate::service::routing::compute_cost(
        st,
        &meta.model,
        usage.map(|u| u.input()),
        usage.map(|u| u.output()),
        usage.and_then(|u| u.cache_read_tokens),
        usage.and_then(|u| u.cache_write_tokens),
    );
    if cost == 0.0 && usage.is_some() {
        for fallback in &meta.pricing_models {
            if *fallback == meta.model {
                continue;
            }
            cost = crate::service::routing::compute_cost(
                st,
                fallback,
                usage.map(|u| u.input()),
                usage.map(|u| u.output()),
                usage.and_then(|u| u.cache_read_tokens),
                usage.and_then(|u| u.cache_write_tokens),
            );
            if cost > 0.0 {
                break;
            }
        }
    }
    if cost == 0.0 && usage.is_some() {
        // M33：缺价告警（cc-switch USG-002 同款）——启用模型名映射而价格表
        // 只配上游名（或反之）时，计费全部记 0 且此前无任何告警
        tracing::warn!(
            request_id = %meta.request_id,
            model = %meta.model,
            pricing_models = ?meta.pricing_models,
            "no price configured for model; cost recorded as 0"
        );
    }
    match record_usage(&st.pool, meta, usage, status, latency_ms, cost).await {
        Ok(()) => {
            if let Some(user_id) = meta.user_id {
                let month = chrono::Utc::now().format("%Y-%m").to_string();
                let tokens = usage.map(|u| u.total()).unwrap_or(0);
                st.usage.incr(user_id, &month, tokens, cost);
            }
            tracing::info!(
                request_id = %meta.request_id,
                model = %meta.model,
                status,
                latency_ms,
                input_tokens = usage.map(|u| u.input()),
                output_tokens = usage.map(|u| u.output()),
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
