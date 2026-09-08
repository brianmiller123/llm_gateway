//! Coding Plan 运行时：请求期配额检查（拦截/降级/记录）、记账后告警触发、
//! 分组 LDAP 成员增量同步（手动 + 周期）。
//!
//! 配额语义（check-then-act）：预检查通过后单请求收尾可能使计数溢出
//! （流式请求 usage 在流结束才落账，无法中途可靠截断），超出部分计入本周期，
//! 下一个请求按新余量处置——与既有月度配额行为一致。

use crate::error::AppError;
use crate::state::AppState;
use crate::store::{groups, plans as plan_store};

/// 请求期 Plan 检查结果：
/// - `plan`：模型感知解析出的生效 Plan。代理记账载荷直接复用它，保证「检查」与
///   「记账」同源——模型作用域下二次解析可能选中另一个 Plan（或因不匹配而漏记）。
/// - `downgrade_to`：超额降级目标模型（`overage_action = downgrade` 且目标 ≠ 当前模型）。
#[derive(Debug, Clone)]
pub struct PlanDecision {
    pub plan: Option<plan_store::PlanRuntime>,
    pub downgrade_to: Option<String>,
}

/// 请求期配额检查（按当前请求模型解析生效 Plan，模型作用域随请求即时生效）：
/// - `downgrade_to = Some(m)` — 超额降级：请求模型改写为 m（路由/计价/记账随之，
///   且用量记入本 Plan 的计数器——降级是本 Plan 的处置动作）
/// - `downgrade_to = None` — 放行（含 `log` 策略超额：仅告警不拦截）
/// - `Err` — 超额拦截（429 insufficient_quota）
pub fn check_plan(st: &AppState, user_id: i64, model: &str) -> Result<PlanDecision, AppError> {
    // 生效时段按服务器本地墙钟判定；配额统计周期仍一律 UTC
    let Some(plan) = plan_store::resolve_plan(
        &st.plans.read(),
        user_id,
        chrono::Local::now().time(),
        Some(model),
    ) else {
        return Ok(PlanDecision {
            plan: None,
            downgrade_to: None,
        });
    };
    let now = chrono::Utc::now();
    let used = st
        .usage
        .get_plan(user_id, plan.plan_id, &plan.period_key(now));

    // 80/95/100% 阈值告警（进程内 + DB 双层去重，异步派发不阻塞热路径）
    crate::service::notify::check_and_dispatch(st, user_id, &plan, used);

    if used < plan.token_limit {
        return Ok(PlanDecision {
            plan: Some(plan),
            downgrade_to: None,
        });
    }
    match plan.overage_action.as_str() {
        plan_store::OVERAGE_DOWNGRADE => match plan.downgrade_model.as_deref() {
            // 目标即当前请求模型时降级无意义 → 拦截（否则死循环在降级模型上继续超用）
            Some(target) if target != model => {
                let downgrade_to = Some(target.to_string());
                Ok(PlanDecision {
                    plan: Some(plan),
                    downgrade_to,
                })
            }
            _ => Err(blocked(&plan, used, now)),
        },
        // log：仅记录并告警（上方 check_and_dispatch 已覆盖 100% 级）
        plan_store::OVERAGE_LOG => Ok(PlanDecision {
            plan: Some(plan),
            downgrade_to: None,
        }),
        _ => Err(blocked(&plan, used, now)),
    }
}

fn blocked(
    plan: &plan_store::PlanRuntime,
    used: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> AppError {
    // Retry-After = 距当前周期边界的秒数（total 周期无边界 → None）。
    // 窗口内重试必然失败：携带明确等待可让遵循报头的客户端睡到周期重置，
    // 而非按自身默认退避把重试预算烧在注定失败的努力上。
    let retry_after = plan
        .period_end(now)
        .map(|end| ((end - now).num_seconds().max(1) as u64).min(86400));
    AppError::PlanQuotaExceeded(
        format!(
            "Coding Plan「{}」{}配额已用尽（{}/{} tokens）",
            plan.plan_name,
            crate::service::notify::period_label(&plan.period_type),
            used,
            plan.token_limit
        ),
        retry_after,
    )
}

/// 记账事务提交后调用：用量推进可能跨过 80/95/100% 阈值 → 触发告警检查。
/// `plan_id` 传代理路径解析出的记账 Plan（[`PlanDecision::plan`]）——模型作用域下
/// 无模型二次解析可能选中另一个 Plan，把告警记到错误的配额上。
pub fn after_usage_record(st: &AppState, user_id: i64, plan_id: i64) {
    let Some(plan) = st
        .plans
        .read()
        .get(&user_id)
        .and_then(|c| c.iter().find(|p| p.plan_id == plan_id).cloned())
    else {
        // Plan 已删除/停用（运行时已刷新）→ 无从告警，静默返回
        return;
    };
    let used = st
        .usage
        .get_plan(user_id, plan.plan_id, &plan.period_key(chrono::Utc::now()));
    if crate::service::notify::crossed_level(used, plan.token_limit).is_some() {
        crate::service::notify::check_and_dispatch(st, user_id, &plan, used);
    }
}

/// 分组同步互斥：手动触发与周期同步串行（目录全量拉取 + 对账不并发重入）
static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 分组 LDAP 增量同步：目录全量用户 → JIT 建档（不改既有管理员/状态）→ 成员对账。
/// 返回 (新增成员数, 移除成员数)。
pub async fn sync_group_ldap(st: &AppState, group_id: i64) -> Result<(u64, u64), String> {
    let _guard = SYNC_LOCK.lock().await;
    let group = groups::find_group(&st.pool, group_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "分组不存在".to_string())?;
    if !group.ldap_sync {
        return Err("该分组未开启 LDAP 同步".into());
    }
    do_sync(st, group_id, &group.name).await
}

async fn do_sync(st: &AppState, group_id: i64, group_name: &str) -> Result<(u64, u64), String> {
    let settings = st.ldap.read().clone();
    let entries = crate::service::ldap::list_users(&settings)
        .await
        .map_err(|e| e.to_string())?;
    // JIT 建档：目录新增用户先入 users 表（is_admin/status 不受同步影响）
    let mut ids = Vec::with_capacity(entries.len());
    for e in &entries {
        match groups::upsert_sync_user(
            &st.pool,
            &e.username,
            e.email.as_deref(),
            e.display_name.as_deref(),
            &e.dn,
        )
        .await
        {
            Ok(id) => ids.push(id),
            Err(err) => {
                tracing::warn!(username = %e.username, error = %err, "ldap sync user upsert failed")
            }
        }
    }
    let (added, removed) = groups::reconcile_ldap_members(&st.pool, group_id, &ids)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(group = %group_name, added, removed, "ldap group synced");
    Ok((added, removed))
}

/// 周期同步全部 ldap_sync 分组（worker 调用；单组失败不影响其余，结果落库可查）
pub async fn sync_all_ldap_groups(st: &AppState) {
    let Ok(group_list) = groups::ldap_sync_group_ids(&st.pool).await else {
        return;
    };
    if group_list.is_empty() {
        return;
    }
    let _guard = SYNC_LOCK.lock().await;
    for (id, name) in group_list {
        let settings = st.ldap.read().clone();
        let res = crate::service::ldap::list_users(&settings).await;
        let result_str = match res {
            Ok(entries) => {
                let mut ids = Vec::with_capacity(entries.len());
                let mut failed = 0usize;
                for e in &entries {
                    match groups::upsert_sync_user(
                        &st.pool,
                        &e.username,
                        e.email.as_deref(),
                        e.display_name.as_deref(),
                        &e.dn,
                    )
                    .await
                    {
                        Ok(uid) => ids.push(uid),
                        Err(_) => failed += 1,
                    }
                }
                match groups::reconcile_ldap_members(&st.pool, id, &ids).await {
                    Ok((added, removed)) => {
                        let s = format!("+{added} / -{removed}");
                        if failed > 0 {
                            format!("{s}（{failed} 个用户建档失败）")
                        } else {
                            s
                        }
                    }
                    Err(e) => format!("对账失败: {e}"),
                }
            }
            Err(e) => format!("LDAP 拉取失败: {e}"),
        };
        if let Err(e) = groups::record_sync_failure(&st.pool, id, &result_str).await {
            tracing::warn!(group = %name, error = %e, "sync result persist failed");
        }
        if result_str.starts_with("LDAP") || result_str.starts_with("对账") {
            tracing::warn!(group = %name, result = %result_str, "ldap group sync issue");
        } else {
            tracing::info!(group = %name, result = %result_str, "ldap group synced");
        }
    }
}
