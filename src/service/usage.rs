use std::collections::HashMap;
use parking_lot::Mutex;

use crate::error::AppError;
use crate::state::AppState;
use crate::store::usage::{record_usage, Usage, UsageMeta};

/// 月度用量计数缓存：key = (user_id, "YYYY-MM") → (tokens, cost)
/// 记账事务提交后更新；配额预检查读取；启动/周期从 DB 重载兜底（防重启后配额清零）
/// plan_usage：Coding Plan 当期计数 (user_id, plan_id) → (period_key, tokens)，
/// 与月度缓存同语义（DB 真值覆盖 + 提交后增量）
#[derive(Default)]
pub struct UsageCache {
    monthly: Mutex<HashMap<(i64, String), (i64, f64)>>,
    plan_usage: Mutex<HashMap<(i64, i64), (String, i64)>>,
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

    /// Plan 当期已用 tokens（period_key 不匹配视为新周期 → 0）
    pub fn get_plan(&self, user_id: i64, plan_id: i64, period_key: &str) -> i64 {
        match self.plan_usage.lock().get(&(user_id, plan_id)) {
            Some((k, v)) if k == period_key => *v,
            _ => 0,
        }
    }

    pub fn incr_plan(&self, user_id: i64, plan_id: i64, period_key: &str, tokens: i64) {
        let mut map = self.plan_usage.lock();
        let e = map
            .entry((user_id, plan_id))
            .or_insert((period_key.to_string(), 0));
        if e.0 == period_key {
            e.1 = e.1.saturating_add(tokens);
        } else {
            // 周期翻转：以本笔增量重置
            *e = (period_key.to_string(), tokens.max(0));
        }
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
        {
            let mut map = self.monthly.lock();
            for (user_id, tokens, cost) in rows {
                map.insert((user_id, month.clone()), (tokens, cost));
            }
        }
        // Coding Plan 当期快照：先取数后持锁（MutexGuard 不得跨 await，非 Send）
        let snapshots = crate::store::plans::load_usage_snapshots(pool).await?;
        let mut pmap = self.plan_usage.lock();
        for s in snapshots {
            pmap.insert((s.user_id, s.plan_id), (s.period_key, s.used));
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
                // Coding Plan 周期计数推进 + 阈值告警检查（80/95/100%）
                if let Some(bill) = meta.plan.as_ref() {
                    st.usage
                        .incr_plan(user_id, bill.plan_id, &bill.period_key, tokens);
                    crate::service::plans::after_usage_record(st, user_id);
                }
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
