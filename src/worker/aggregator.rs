use std::time::Duration;
use std::time::Instant;

use sqlx::PgPool;

use crate::store::usage::{aggregate_daily, purge_usage_logs};

/// 保留期清理节流：每 24h 一轮（Instant 闸门）
const PURGE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// 每 60s 将增量 usage_logs 聚合进 usage_daily（水位线幂等）；
/// 每 10 分钟顺带清理过期/已吊销超过 30 天的 refresh token（表只增不减的收敛）。
/// F7：`retention_days > 0` 时按保留期批量清理 usage_logs（表只增不减，
/// 查询随表龄退化）；首次清理在启动后首个常规 tick（interval 首拍立即触发，
/// 跳过以免与启动迁移/预热抢池），此后每 24h 一轮。
pub async fn run(pool: PgPool, retention_days: i64) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut ticks: u64 = 0;
    let mut last_purge: Option<Instant> = None;
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
        if retention_days > 0 {
            let due = match last_purge {
                Some(t) => t.elapsed() >= PURGE_INTERVAL,
                None => ticks > 1,
            };
            if due {
                last_purge = Some(Instant::now());
                match purge_usage_logs(&pool, retention_days).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(removed = n, retention_days, "usage_logs purged"),
                    Err(e) => tracing::warn!(error = %e, "usage_logs purge failed"),
                }
            }
        }
    }
}
