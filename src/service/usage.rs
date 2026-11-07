use parking_lot::Mutex;
use std::collections::HashMap;

use crate::state::AppState;
use crate::store::usage::{Usage, UsageMeta, record_usage};

/// Plan 当期用量缓存：key = (user_id, plan_id) → (period_key, tokens)。
/// 记账事务提交后更新；Plan 配额预检查读取；启动/周期从 DB 重载兜底。
/// （月度配额子系统已移除：user_monthly_usage 仅作用量展示，不再缓存/拦截）
#[derive(Default)]
pub struct UsageCache {
    plan_usage: Mutex<HashMap<(i64, i64), (String, i64)>>,
}

impl UsageCache {
    pub fn new() -> Self {
        Self::default()
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

    /// 从 DB 全量重载 Plan 当期计数（只覆盖不清理，避免并发 incr 在快照与
    /// 写入之间丢失）。启动时与周期 reload 中调用。
    /// 快照在锁外取：持锁后同 period_key 现值 ≥ 快照值说明快照之后发生过
    /// 并发 incr，覆盖会把刚入账的 token 抹回旧值（自愈前最长一个 reload 周期
    /// 少计）——只允许「翻周期」或「DB 更新」覆盖现值。
    pub async fn reload_from_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let snapshots = crate::store::plans::load_usage_snapshots(pool).await?;
        let mut pmap = self.plan_usage.lock();
        for s in snapshots {
            match pmap.get(&(s.user_id, s.plan_id)) {
                Some((k, v)) if *k == s.period_key && *v >= s.used => {}
                _ => {
                    pmap.insert((s.user_id, s.plan_id), (s.period_key, s.used));
                }
            }
        }
        Ok(())
    }
}

/// 生成阶段耗时（纯函数，便于单测）：
/// - 有首 token：elapsed − first（流式真实生成长度）；
/// - 首块即末块 / 亚毫秒流（差值 ≤ 0）：回退总耗时下界；
/// - 无首 token（非流式/缓冲路径）：总耗时（含预填充，下界）。
/// 结果最小 1ms：0 会被速度 SQL 的 `gen_ms > 0` 过滤，整行样本丢失。
fn gen_ms_from(first_token_ms: Option<i64>, elapsed_ms: i64, latency_ms: i64) -> i64 {
    match first_token_ms {
        Some(first) => {
            let span = elapsed_ms - first;
            if span > 0 {
                span
            } else {
                latency_ms.max(1)
            }
        }
        None => latency_ms.max(1),
    }
}

/// 记账时点的生成阶段耗时（客户端断开兜底也用它，避免 spawn 调度延迟计入）
pub fn gen_ms_at(meta: &UsageMeta, latency_ms: i64) -> i64 {
    gen_ms_from(
        meta.first_token_ms,
        meta.started.elapsed().as_millis() as i64,
        latency_ms,
    )
}

/// 记账执行器：同步事务直写 + 更新缓存
pub async fn record(
    st: &AppState,
    meta: &UsageMeta,
    usage: Option<&Usage>,
    status: u16,
    latency_ms: i64,
) {
    let gen_ms = gen_ms_at(meta, latency_ms);
    record_with_gen_ms(st, meta, usage, status, latency_ms, gen_ms).await;
}

/// 带显式生成阶段耗时的记账（客户端断开兜底在断开时点先算好 gen_ms）。
pub async fn record_with_gen_ms(
    st: &AppState,
    meta: &UsageMeta,
    usage: Option<&Usage>,
    status: u16,
    latency_ms: i64,
    gen_ms: i64,
) {
    // 管理员测试调用不记账：不写 usage_logs，不污染状态页/错误率
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
    match record_usage(&st.pool, meta, usage, status, latency_ms, gen_ms, cost).await {
        Ok(()) => {
            if let Some(user_id) = meta.user_id {
                let tokens = usage.map(|u| u.total()).unwrap_or(0);
                // Coding Plan 周期计数推进 + 阈值告警检查（80/95/100%）
                if let Some(bill) = meta.plan.as_ref() {
                    st.usage
                        .incr_plan(user_id, bill.plan_id, &bill.period_key, tokens);
                    // 用账单的 period_key 而非现推：跨桶请求（如 11:58 发起
                    // 12:02 记账）阈值在旧桶被跨过时也能触发告警
                    crate::service::plans::after_usage_record_with_key(
                        st,
                        user_id,
                        bill.plan_id,
                        &bill.period_key,
                    );
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

#[cfg(test)]
mod tests {
    use super::gen_ms_from;

    #[test]
    fn gen_ms_stream_uses_post_first_token_duration() {
        assert_eq!(gen_ms_from(Some(800), 5_800, 900), 5_000);
    }

    #[test]
    fn gen_ms_non_stream_uses_total_latency() {
        assert_eq!(gen_ms_from(None, 9_999, 4_200), 4_200);
    }

    #[test]
    fn gen_ms_zero_duration_falls_back_to_latency_floor() {
        // 首块即末块（上游忽略 stream）或亚毫秒流：差值 0/负，回退总耗时
        assert_eq!(gen_ms_from(Some(1), 1, 250), 250);
        assert_eq!(gen_ms_from(Some(10), 5, 120), 120);
        // 非流式 0ms：仍取 1ms 下限，避免 gen_ms > 0 过滤丢样本
        assert_eq!(gen_ms_from(None, 0, 0), 1);
    }
}
