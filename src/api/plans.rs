//! Coding Plan 与用户分组管理 API：
//! - 管理员（require_admin）：Plan CRUD + 用量看板/回溯、分组 CRUD + 成员管理
//!   （手动/批量/一键全部/LDAP 同步/CSV 导出）、成员选择器分页检索、阈值告警查询、
//!   SMTP 告警邮箱配置
//! - 普通用户（ConsoleUser）：本人生效 Plan 额度与消耗明细、站内通知
//!
//! 全部配置变更写审计日志（变更前后快照）；写入后 `AppState::reload()` 即时生效。

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::service::plans as plan_svc;
use crate::state::AppState;
use crate::store::{audit, groups, plans as plan_store, users};

pub fn routes(state: AppState) -> Router<AppState> {
    let admin = axum::middleware::from_fn_with_state(state.clone(), super::console::require_admin);
    Router::new()
        // ---------- Coding Plan ----------
        .route(
            "/api/admin/plans",
            get(list_plans).post(create_plan).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}",
            patch(update_plan).delete(delete_plan).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/usage",
            get(plan_usage).layer(admin.clone()),
        )
        .route(
            "/api/admin/plan-alerts",
            get(list_alerts).layer(admin.clone()),
        )
        // ---------- 用户分组 ----------
        .route(
            "/api/admin/groups",
            get(list_groups).post(create_group).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}",
            patch(update_group).delete(delete_group).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/members",
            get(list_members).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/members/add",
            post(add_members).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/members/remove",
            post(remove_members).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/members/add-all",
            post(add_all_members).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/members/export",
            get(export_members).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/sync",
            post(sync_group).layer(admin.clone()),
        )
        .route(
            "/api/admin/groups/{id}/users",
            get(search_users).layer(admin.clone()),
        )
        // ---------- SMTP（告警邮件） ----------
        .route(
            "/api/admin/smtp",
            get(get_smtp)
                .put(put_smtp)
                .layer(admin.clone()),
        )
        .route("/api/admin/smtp/test", post(test_smtp).layer(admin))
        // ---------- 普通用户 ----------
        .route("/api/me/plan", get(my_plan))
        .route("/api/me/notifications", get(my_notifications))
}

// ---------- 共用 ----------

/// token_limit 入参：数字（token）或 "1.5G" 单位串
fn parse_limit_value(v: &Value) -> Result<i64, AppError> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .filter(|t| *t > 0)
            .ok_or_else(|| AppError::BadRequest("invalid token_limit".into())),
        Value::String(s) => {
            plan_store::parse_token_limit(s).map_err(AppError::BadRequest)
        }
        _ => Err(AppError::BadRequest("invalid token_limit".into())),
    }
}

fn validate_plan_fields(
    period_type: Option<&str>,
    overage_action: Option<&str>,
    channels: Option<&[String]>,
    overage_action_changed: bool,
    downgrade_model: Option<&Option<String>>,
    st: &AppState,
) -> Result<(), AppError> {
    if let Some(p) = period_type {
        if !plan_store::VALID_PERIODS.contains(&p) {
            return Err(AppError::BadRequest(format!("invalid period_type: {p}")));
        }
    }
    let mut downgraded = false;
    if let Some(o) = overage_action {
        if !plan_store::VALID_OVERAGES.contains(&o) {
            return Err(AppError::BadRequest(format!(
                "invalid overage_action: {o}"
            )));
        }
        downgraded = o == plan_store::OVERAGE_DOWNGRADE;
    }
    if let Some(ch) = channels {
        if ch.iter().any(|c| !plan_store::VALID_ALERT_CHANNELS.contains(&c.as_str())) {
            return Err(AppError::BadRequest(
                "invalid alert_channels (use in_site/email/webhook)".into(),
            ));
        }
    }
    // downgrade 需要目标模型且可路由（改写后无路由 = 必然 400，存期拦截）
    let model_required = downgraded
        || (overage_action_changed && overage_action.is_none());
    let _ = model_required;
    if let Some(Some(target)) = downgrade_model {
        let routed = {
            let routes = st.routes.read();
            crate::service::routing::resolve_route(&routes, target).is_some()
        };
        if !routed {
            return Err(AppError::BadRequest(format!(
                "downgrade model '{target}' is not routed"
            )));
        }
    }
    Ok(())
}

fn plan_row_to_value(p: &plan_store::CodingPlan) -> Value {
    let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "token_limit_display".into(),
            json!(plan_store::format_token_limit(p.token_limit)),
        );
    }
    v
}

// ---------- Coding Plan CRUD ----------

async fn list_plans(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let rows = plan_store::list_plan_summaries(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let plans: Vec<Value> = rows
        .iter()
        .map(|p| {
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "token_limit_display".into(),
                    json!(plan_store::format_token_limit(p.token_limit)),
                );
            }
            v
        })
        .collect();
    Ok(Json(json!({ "plans": plans })))
}

#[derive(Deserialize)]
struct PlanCreateReq {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    priority: i32,
    token_limit: Value,
    #[serde(default = "default_period")]
    period_type: String,
    #[serde(default = "default_overage")]
    overage_action: String,
    #[serde(default)]
    downgrade_model: Option<String>,
    #[serde(default)]
    alert_channels: Option<Vec<String>>,
    #[serde(default)]
    webhook_url: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_period() -> String {
    plan_store::PERIOD_MONTHLY.into()
}
fn default_overage() -> String {
    plan_store::OVERAGE_BLOCK.into()
}
fn default_true() -> bool {
    true
}

async fn create_plan(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<PlanCreateReq>,
) -> Result<impl IntoResponse, AppError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name is required".into()));
    }
    let limit = parse_limit_value(&req.token_limit)?;
    let channels: Vec<String> = req
        .alert_channels
        .unwrap_or_else(|| vec!["in_site".into()]);
    validate_plan_fields(
        Some(&req.period_type),
        Some(&req.overage_action),
        Some(&channels),
        true,
        Some(&req.downgrade_model),
        &st,
    )?;
    if req.overage_action == plan_store::OVERAGE_DOWNGRADE && req.downgrade_model.is_none() {
        return Err(AppError::BadRequest(
            "downgrade_action requires downgrade_model".into(),
        ));
    }
    let created = plan_store::create_plan(
        &st.pool,
        name,
        req.description.trim(),
        req.priority,
        limit,
        &req.period_type,
        &req.overage_action,
        req.downgrade_model.as_deref(),
        &json!(channels),
        req.webhook_url.trim(),
        req.enabled,
    )
    .await
    .map_err(|e| map_plan_conflict(e))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "plan.create",
        Some("coding_plan"),
        Some(created.id),
        Some(json!({"after": plan_row_to_value(&created)})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok((StatusCode::CREATED, Json(json!({"plan": plan_row_to_value(&created)}))))
}

#[derive(Deserialize)]
struct PlanUpdateReq {
    name: Option<String>,
    description: Option<String>,
    priority: Option<i32>,
    token_limit: Option<Value>,
    period_type: Option<String>,
    overage_action: Option<String>,
    /// Some(None) = 清空；Some(Some(m)) = 设置
    downgrade_model: Option<Option<String>>,
    alert_channels: Option<Vec<String>>,
    webhook_url: Option<String>,
    enabled: Option<bool>,
}

async fn update_plan(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<PlanUpdateReq>,
) -> Result<impl IntoResponse, AppError> {
    let before = plan_store::find_plan(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("plan not found".into()))?;
    let limit = match &req.token_limit {
        Some(v) => Some(parse_limit_value(v)?),
        None => None,
    };
    let channels_json = req.alert_channels.as_ref().map(|c| json!(c));
    validate_plan_fields(
        req.period_type.as_deref(),
        req.overage_action.as_deref(),
        req.alert_channels.as_deref(),
        req.overage_action.is_some(),
        req.downgrade_model.as_ref(),
        &st,
    )?;
    if req.overage_action.as_deref() == Some(plan_store::OVERAGE_DOWNGRADE)
        && req.downgrade_model.is_none()
        && before.downgrade_model.is_none()
    {
        return Err(AppError::BadRequest(
            "downgrade_action requires downgrade_model".into(),
        ));
    }
    let updated = plan_store::update_plan(
        &st.pool,
        id,
        req.name.as_deref().map(str::trim),
        req.description.as_deref().map(str::trim),
        req.priority,
        limit,
        req.period_type.as_deref(),
        req.overage_action.as_deref(),
        req.downgrade_model
            .as_ref()
            .map(|o| o.as_deref().map(str::trim)),
        channels_json.as_ref(),
        req.webhook_url.as_deref().map(str::trim),
        req.enabled,
    )
    .await
    .map_err(|e| map_plan_conflict(e))?
    .ok_or_else(|| AppError::BadRequest("plan not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "plan.update",
        Some("coding_plan"),
        Some(id),
        Some(json!({
            "before": plan_row_to_value(&before),
            "after": plan_row_to_value(&updated),
        })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"plan": plan_row_to_value(&updated)})))
}

async fn delete_plan(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let before = plan_store::find_plan(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("plan not found".into()))?;
    let affected = plan_store::delete_plan(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .unwrap_or(0);
    st.reload().await.map_err(AppError::internal)?;
    // 兜底通知：绑定的分组失去 Plan，成员回退次优先级分组或暂不限额
    crate::service::notify::system_notice(
        &st,
        &format!(
            "Coding Plan「{}」已被管理员 {} 删除，{} 个绑定分组失去配额（成员回退其他分组 Plan 或暂不限额）",
            before.name, admin.0.username, affected
        ),
    )
    .await;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "plan.delete",
        Some("coding_plan"),
        Some(id),
        Some(json!({"before": plan_row_to_value(&before), "affected_groups": affected})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true, "affected_groups": affected})))
}

/// 名称唯一冲突 → 400 可读提示（其余 DB 错误照常 internal）
fn map_plan_conflict(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        if db.constraint().is_some_and(|c| c.contains("coding_plans_name_key")) {
            return AppError::BadRequest("同名 Coding Plan 已存在".into());
        }
    }
    AppError::internal(e)
}

// ---------- Plan 用量看板/回溯 ----------

#[derive(Deserialize)]
struct PlanUsageParams {
    /// 趋势天数（默认 30，上限 365）
    days: Option<i32>,
    /// 用户维度回溯区间（缺省近 30 天）
    from: Option<String>,
    to: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn plan_usage(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<PlanUsageParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let plan = plan_store::find_plan(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("plan not found".into()))?;
    let days = p.days.unwrap_or(30).clamp(1, 365);
    let trend = plan_store::plan_daily_trend(&st.pool, id, days)
        .await
        .map_err(AppError::internal)?;
    let periods = plan_store::plan_period_history(&st.pool, id, 90)
        .await
        .map_err(AppError::internal)?;
    let to: NaiveDate = p
        .to
        .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
        .unwrap_or_else(|| Utc::now().date_naive());
    let from: NaiveDate = p
        .from
        .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
        .unwrap_or_else(|| to - chrono::Duration::days(29));
    let limit = p.limit.unwrap_or(20).clamp(1, 200);
    let offset = p.offset.unwrap_or(0).max(0);
    let (user_rows, total) = plan_store::plan_user_usage(&st.pool, id, from, to, limit, offset)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "plan": plan_row_to_value(&plan),
        "trend": trend,
        "periods": periods,
        "range": {"from": from, "to": to},
        "users": {"rows": user_rows, "total": total, "limit": limit, "offset": offset},
    })))
}

#[derive(Deserialize)]
struct AlertListParams {
    limit: Option<i64>,
    level: Option<i16>,
    plan_id: Option<i64>,
}

async fn list_alerts(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<AlertListParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let limit = p.limit.unwrap_or(100).clamp(1, 500);
    let rows: Vec<(i64, Option<i64>, String, Option<i64>, String, i16, String, i64, i64, String, Value, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT id, plan_id, plan_name, user_id, username, level, period_key, \
                    used, limit_tokens, message, delivered, created_at \
             FROM plan_alerts \
             WHERE ($1::smallint IS NULL OR level = $1) \
               AND ($2::bigint IS NULL OR plan_id = $2) \
             ORDER BY id DESC LIMIT $3",
        )
        .bind(p.level)
        .bind(p.plan_id)
        .bind(limit)
        .fetch_all(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let alerts: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0, "plan_id": r.1, "plan_name": r.2, "user_id": r.3,
                "username": r.4, "level": r.5, "period_key": r.6, "used": r.7,
                "limit_tokens": r.8, "message": r.9, "delivered": r.10, "created_at": r.11,
            })
        })
        .collect();
    Ok(Json(json!({ "alerts": alerts })))
}

// ---------- 用户分组 ----------

async fn list_groups(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let rows = groups::list_groups(&st.pool)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({ "groups": rows })))
}

#[derive(Deserialize)]
struct GroupCreateReq {
    name: String,
    #[serde(default)]
    description: String,
    plan_id: Option<i64>,
    #[serde(default)]
    ldap_sync: bool,
}

async fn create_group(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<GroupCreateReq>,
) -> Result<impl IntoResponse, AppError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name is required".into()));
    }
    if let Some(pid) = req.plan_id {
        ensure_plan_exists(&st, pid).await?;
    }
    let group = groups::create_group(&st.pool, name, req.description.trim(), req.plan_id, req.ldap_sync)
        .await
        .map_err(map_group_conflict)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "group.create",
        Some("user_group"),
        Some(group.id),
        Some(json!({"after": group})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok((StatusCode::CREATED, Json(json!({"group": group}))))
}

#[derive(Deserialize)]
struct GroupUpdateReq {
    name: Option<String>,
    description: Option<String>,
    /// 绑定到指定 Plan
    plan_id: Option<i64>,
    /// 解绑 Plan（plan_id 与 clear_plan 同时出现时以 clear_plan 优先）
    clear_plan: Option<bool>,
    ldap_sync: Option<bool>,
}

async fn update_group(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<GroupUpdateReq>,
) -> Result<impl IntoResponse, AppError> {
    let before = groups::find_group(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("group not found".into()))?;
    if let Some(pid) = req.plan_id {
        ensure_plan_exists(&st, pid).await?;
    }
    // 三态：clear_plan=true → 解绑；否则 plan_id Some → 换绑；都缺省 → 不修改
    let plan_tri: Option<Option<i64>> = if req.clear_plan == Some(true) {
        Some(None)
    } else {
        req.plan_id.map(Some)
    };
    let updated = groups::update_group(
        &st.pool,
        id,
        req.name.as_deref().map(str::trim),
        req.description.as_deref().map(str::trim),
        plan_tri,
        req.ldap_sync,
    )
    .await
    .map_err(|e| map_group_conflict(e))?
    .ok_or_else(|| AppError::BadRequest("group not found".into()))?;
    // 绑定调整即时生效（运行时 Plan 表重载）
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "group.update",
        Some("user_group"),
        Some(id),
        Some(json!({"before": before, "after": updated})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"group": updated})))
}

async fn delete_group(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let before = groups::find_group(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("group not found".into()))?;
    groups::delete_group(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "group.delete",
        Some("user_group"),
        Some(id),
        Some(json!({"before": before})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})))
}

async fn ensure_plan_exists(st: &AppState, plan_id: i64) -> Result<(), AppError> {
    plan_store::find_plan(&st.pool, plan_id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("plan not found".into()))?;
    Ok(())
}

fn map_group_conflict(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        if db.constraint().is_some_and(|c| c.contains("user_groups_name_key")) {
            return AppError::BadRequest("同名分组已存在".into());
        }
    }
    AppError::internal(e)
}

// ---------- 成员管理 ----------

#[derive(Deserialize)]
struct MemberListParams {
    q: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

async fn list_members(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<MemberListParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let page_size = p.page_size.unwrap_or(20).clamp(1, 200);
    let page = p.page.unwrap_or(1).max(1);
    let (rows, total) = groups::list_members(
        &st.pool,
        id,
        non_empty(&p.q),
        page_size,
        (page - 1) * page_size,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({
        "members": rows, "total": total,
        "page": page, "page_size": page_size,
    })))
}

#[derive(Deserialize)]
struct MembersAddReq {
    user_ids: Vec<i64>,
}

async fn add_members(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<MembersAddReq>,
) -> Result<impl IntoResponse, AppError> {
    let added = groups::add_members(&st.pool, id, &req.user_ids)
        .await
        .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "group.members.add",
        Some("user_group"),
        Some(id),
        Some(json!({"user_ids": req.user_ids, "added": added})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"added": added})))
}

async fn remove_members(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<MembersAddReq>,
) -> Result<impl IntoResponse, AppError> {
    let removed = groups::remove_members(&st.pool, id, &req.user_ids)
        .await
        .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "group.members.remove",
        Some("user_group"),
        Some(id),
        Some(json!({"user_ids": req.user_ids, "removed": removed})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"removed": removed})))
}

async fn add_all_members(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    // 单语句 INSERT..SELECT（海量用户一次成库，ON CONFLICT 幂等）
    let added = groups::add_all_users(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "group.members.add_all",
        Some("user_group"),
        Some(id),
        Some(json!({"added": added})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"added": added})))
}
fn opt_str(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("")
}

async fn export_members(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<Response, AppError> {
    let _ = admin;
    let group = require_group(&st, id).await?;
    let rows = groups::export_members(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    let esc = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut body = String::from("\u{feff}username,display_name,email,source,added_at\n");
    for r in rows {
        body.push_str(&format!(
            "{},{},{},{},{}\n",
            esc(&r.username),
            esc(opt_str(&r.display_name)),
            esc(opt_str(&r.email)),
            r.source,
            r.added_at.to_rfc3339()
        ));
    }
    let disposition = format!("attachment; filename=\"group_{}.csv\"", group.name);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(Body::from(body))
        .map_err(|e| AppError::internal(e.to_string()))?)
}

async fn sync_group(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    match plan_svc::sync_group_ldap(&st, id).await {
        Ok((added, removed)) => {
            audit::log(
                &st.pool,
                Some(admin.0.id),
                "group.ldap.sync",
                Some("user_group"),
                Some(id),
                Some(json!({"added": added, "removed": removed})),
            )
            .await
            .map_err(AppError::internal)?;
            st.reload().await.map_err(AppError::internal)?;
            Ok(Json(json!({"ok": true, "added": added, "removed": removed})))
        }
        Err(e) => {
            let _ = groups::record_sync_failure(&st.pool, id, &format!("手动同步失败: {e}")).await;
            Err(AppError::BadRequest(format!("LDAP 同步失败: {e}")))
        }
    }
}

#[derive(Deserialize)]
struct SearchUsersParams {
    q: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

/// 成员选择器：全用户分页检索 + 指定分组内标记（单查询渲染勾选态）
async fn search_users(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<SearchUsersParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let page_size = p.page_size.unwrap_or(20).clamp(1, 200);
    let page = p.page.unwrap_or(1).max(1);
    let (rows, total) = groups::search_users_for_group(
        &st.pool,
        id,
        non_empty(&p.q),
        page_size,
        (page - 1) * page_size,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({
        "users": rows, "total": total, "page": page, "page_size": page_size,
    })))
}

async fn require_group(st: &AppState, id: i64) -> Result<groups::GroupRow, AppError> {
    groups::find_group(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("group not found".into()))
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

// ---------- SMTP ----------

async fn get_smtp(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let s = crate::store::config::load_smtp_settings(&st.pool)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "host": s.host, "port": s.port, "username": s.username, "from": s.from,
        "has_password": !s.password_enc.is_empty(),
    })))
}

#[derive(Deserialize)]
struct SmtpReq {
    host: String,
    port: u16,
    #[serde(default)]
    username: String,
    /// None = 不修改；Some("") = 清空
    password: Option<String>,
    from: String,
}

async fn put_smtp(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<SmtpReq>,
) -> Result<impl IntoResponse, AppError> {
    let password_enc: Option<String> = match &req.password {
        None => None,
        Some(p) if p.is_empty() => Some(String::new()),
        Some(p) => Some(
            crate::crypto::encrypt(p.as_bytes(), &st.cfg.master_key)
                .map_err(AppError::BadRequest)?,
        ),
    };
    crate::store::config::save_smtp_settings(
        &st.pool,
        req.host.trim(),
        req.port,
        req.username.trim(),
        password_enc.as_deref(),
        req.from.trim(),
    )
    .await
    .map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "settings.smtp.update",
        Some("system"),
        None,
        Some(json!({
            "after": {"host": req.host.trim(), "port": req.port,
                      "username": req.username.trim(), "from": req.from.trim(),
                      "password_changed": req.password.as_ref().is_some_and(|p| !p.is_empty())},
        })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct SmtpTestReq {
    to: String,
}

async fn test_smtp(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<SmtpTestReq>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let smtp = crate::store::config::load_smtp_settings(&st.pool)
        .await
        .map_err(AppError::internal)?;
    if !smtp.configured() {
        return Err(AppError::BadRequest("SMTP 未配置".into()));
    }
    crate::service::notify::send_test_email(&st, &smtp, req.to.trim())
        .await
        .map_err(AppError::BadRequest)?;
    Ok(Json(json!({"ok": true})))
}

// ---------- 普通用户：本人 Plan 与通知 ----------

/// 用户分页查询的原始行
#[derive(sqlx::FromRow, serde::Serialize)]
struct DailyRow {
    stat_date: NaiveDate,
    call_count: i64,
    tokens: i64,
}

async fn my_plan(
    State(st): State<AppState>,
    user: super::console::ConsoleUser,
) -> Result<impl IntoResponse, AppError> {
    let uid = user.user.id;
    let rt = st.plans.read().get(&uid).cloned();
    let now = chrono::Utc::now();
    let period = match &rt {
        Some(p) => {
            let key = p.period_key(now);
            let used = st.usage.get_plan(uid, p.plan_id, &key);
            let percent = if p.token_limit > 0 {
                used as f64 * 100.0 / p.token_limit as f64
            } else {
                0.0
            };
            Some(json!({
                "key": key,
                "used": used,
                "remaining": (p.token_limit - used).max(0),
                "percent": (percent * 10.0).round() / 10.0,
            }))
        }
        None => None,
    };
    let plan_json = rt.as_ref().map(|p| {
        json!({
            "id": p.plan_id,
            "name": p.plan_name,
            "group": p.group_name,
            "period_type": p.period_type,
            "token_limit": p.token_limit,
            "token_limit_display": plan_store::format_token_limit(p.token_limit),
            "overage_action": p.overage_action,
            "downgrade_model": p.downgrade_model,
        })
    });
    // 本人近 30 日逐日消耗（明细）
    let daily: Vec<DailyRow> = sqlx::query_as(
        "SELECT stat_date, SUM(call_count)::bigint AS call_count, \
                SUM(input_tokens + output_tokens)::bigint AS tokens \
         FROM usage_daily WHERE user_id = $1 AND stat_date >= CURRENT_DATE - 30 \
         GROUP BY stat_date ORDER BY stat_date",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({
        "plan": plan_json,
        "period": period,
        "daily": daily,
    })))
}

async fn my_notifications(
    State(st): State<AppState>,
    user: super::console::ConsoleUser,
) -> Result<impl IntoResponse, AppError> {
    let uid = user.user.id;
    let rows: Vec<(i64, Option<i64>, String, i16, String, i64, i64, Value, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT id, plan_id, plan_name, level, period_key, used, limit_tokens, \
                    delivered, created_at \
             FROM plan_alerts WHERE user_id = $1 \
             ORDER BY id DESC LIMIT 50",
        )
        .bind(uid)
        .fetch_all(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let notifications: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0, "plan_id": r.1, "plan_name": r.2, "level": r.3,
                "period_key": r.4, "used": r.5, "limit_tokens": r.6,
                "delivered": r.7, "created_at": r.8,
            })
        })
        .collect();
    Ok(Json(json!({ "notifications": notifications })))
}

