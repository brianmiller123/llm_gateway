use std::time::Duration;

use sqlx::PgPool;

use crate::store::usage::aggregate_daily;

/// 每 60s 将增量 usage_logs 聚合进 usage_daily（水位线幂等）
pub async fn run(pool: PgPool) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tracing::info!("usage aggregator started");
    loop {
        tick.tick().await;
        match aggregate_daily(&pool).await {
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "daily aggregation failed"),
        }
    }
}
