//! Coding Plan 分组 LDAP 周期同步：每 10 分钟对全部 ldap_sync 分组做增量对账
//! （目录新增用户 JIT 建档进组、目录移除用户出组；手动成员不受影响）。

use std::time::Duration;

use crate::service::plans;
use crate::state::AppState;

/// 同步周期：LDAP 变更最大传播延迟 ≈ 10 分钟（管理员可随时手动触发立即同步）
const SYNC_INTERVAL: Duration = Duration::from_secs(600);

pub async fn run(st: AppState) {
    let mut tick = tokio::time::interval(SYNC_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!("plan group ldap sync worker started");
    loop {
        tick.tick().await;
        plans::sync_all_ldap_groups(&st).await;
    }
}
