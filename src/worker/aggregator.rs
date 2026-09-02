use std::time::Duration;

use sqlx::PgPool;

use crate::store::usage::aggregate_daily;

/// 每 60s 将增量 usage_logs 聚合进 usage_daily（水位线幂等）；
/// 每 10 分钟顺带清理过期/已吊销超过 30 天的 refresh token（表只增不减的收敛）。
pub async fn run(pool: PgPool) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut ticks: u64 = 0;
    tracing::info!("usage aggregator started");
    loop {
        tick.tick().await;
        ticks += 1;
        match aggregate_daily(&pool).await {
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "daily aggregation failed"),
        }
        if ticks % 10 == 0 {
            match sqlx::query(
                "DELETE FROM refresh_tokens WHERE expires_at < now() - interval '30 days'",
            )
            .execute(&pool)
            .await
            {
                Ok(res) => {
                    if res.rows_affected() > 0 {
                        tracing::info!(
                            removed = res.rows_affected(),
                            "expired refresh tokens cleaned"
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "refresh token cleanup failed"),
            }
        }
    }
}
