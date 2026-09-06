//! Coding Plan 与用户分组管理 API：
//! - 管理员（require_admin）：Plan CRUD + 用量看板/回溯 + 成员管理（直连用户 /
//!   加入分组：列表、候选分页检索、批量添加/移除）；分组 CRUD + 成员管理
//!   （手动/批量/一键全部/LDAP 同步/CSV 导出）；阈值告警查询；SMTP 告警邮箱配置
//! - 普通用户（ConsoleUser）：本人生效 Plan 额度与消耗明细、站内通知
//!
//! Plan 成员添加为全有或全无：重复加入返回 409（带成员名列表），参数错误返回 400。
//! 全部配置变更写审计日志（变更前后快照）；写入后 `AppState::reload()` 即时生效。

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

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
            "/api/admin/plans/{id}/users",
            get(plan_users_list).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/users/add",
            post(plan_users_add).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/users/remove",
            post(plan_users_remove).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/user-candidates",
            get(plan_user_candidates).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/groups",
            get(plan_groups_list).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/groups/add",
            post(plan_groups_add).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/groups/remove",
            post(plan_groups_remove).layer(admin.clone()),
        )
        .route(
            "/api/admin/plans/{id}/group-candidates",
            get(plan_group_candidates).layer(admin.clone()),
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
            patch(update_group)
                .delete(delete_group)
                .layer(admin.clone()),
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
            get(get_smtp).put(put_smtp).layer(admin.clone()),
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
        Value::String(s) => plan_store::parse_token_limit(s).map_err(AppError::BadRequest),
        _ => Err(AppError::BadRequest("invalid token_limit".into())),
    }
}

fn validate_plan_fields(
    overage_action: Option<&str>,
    channels: Option<&[String]>,
    downgrade_model: Option<Option<&str>>,
    st: &AppState,
) -> Result<(), AppError> {
    if let Some(o) = overage_action {
        if !plan_store::VALID_OVERAGES.contains(&o) {
            return Err(AppError::BadRequest(format!("invalid overage_action: {o}")));
        }
    }
    if let Some(ch) = channels {
        if ch
            .iter()
            .any(|c| !plan_store::VALID_ALERT_CHANNELS.contains(&c.as_str()))
        {
            return Err(AppError::BadRequest(
                "invalid alert_channels (use in_site/email/webhook)".into(),
            ));
        }
    }
    // downgrade 需要目标模型且可路由（改写后无路由 = 必然 400，存期拦截）
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

/// model_scope 精确条目（非通配）必须命中当前任一路由 pattern（大小写不敏感）：
/// 拼写错误的模型名在这里 400 明确报错，而不是静默配出一个永不生效的作用域。
/// 通配条目无法证伪（家族可能尚未建路由），仅做格式校验；当前无任何路由规则时跳过
///（空环境/测试不误伤），warn 留痕。
fn validate_model_scope_routing(
    st: &AppState,
    scope: Option<&plan_store::ModelScope>,
) -> Result<(), AppError> {
    let Some(scope) = scope else {
        return Ok(());
    };
    let routes = st.routes.read();
    if routes.is_empty() {
        tracing::warn!(
            "model_scope configured but no model routes exist; exact-name check skipped"
        );
        return Ok(());
    }
    let check = |patterns: &[String], field: &str| -> Result<(), AppError> {
        for p in patterns {
            if p.ends_with('*') {
                continue;
            }
            if !routes
                .iter()
                .any(|r| plan_store::model_matches_pattern(&r.model_pattern, p))
            {
                let family = p.split('-').next().unwrap_or(p);
                return Err(AppError::BadRequest(format!(
                    "unrecognized model '{p}' in model_scope.{field}: no routed model matches it \
                     (typo?); add a route first or use a wildcard pattern like '{family}-*'"
                )));
            }
        }
        Ok(())
    };
    check(&scope.allow, "allow")?;
    check(&scope.deny, "deny")
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

/// 生效时段入参：start/end 双空 = 全天；成对出现 = 设置（支持跨零点）
#[derive(Deserialize)]
struct PlanActiveWindow {
    start: Option<String>,
    end: Option<String>,
}

/// active_window 入参 → 存储三态：None=保留（仅 PATCH）/ Some(None)=全天 /
/// Some(Some((s, e)))=设置。缺一或相等均 400。
fn parse_active_window(
    w: Option<PlanActiveWindow>,
) -> Result<Option<Option<(chrono::NaiveTime, chrono::NaiveTime)>>, AppError> {
    let Some(w) = w else { return Ok(None) };
    let parse = |v: &Option<String>| -> Result<Option<chrono::NaiveTime>, AppError> {
        v.as_deref()
            .map(plan_store::parse_active_time)
            .transpose()
            .map_err(AppError::BadRequest)
    };
    let s = parse(&w.start)?;
    let e = parse(&w.end)?;
    plan_store::validate_active_window(s, e).map_err(AppError::BadRequest)?;
    Ok(Some(match (s, e) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    }))
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
    /// hourly 窗口长度（小时，1..=168）；非 hourly 类型忽略
    #[serde(default = "default_period_hours")]
    period_hours: i32,
    /// hourly 锚点：fixed=UTC 整点 / join=成员开通时间偏移
    #[serde(default = "default_anchor_mode")]
    period_anchor_mode: String,
    #[serde(default = "default_overage")]
    overage_action: String,
    #[serde(default)]
    downgrade_model: Option<String>,
    #[serde(default)]
    alert_channels: Option<Vec<String>>,
    #[serde(default)]
    webhook_url: String,
    /// 生效时段（None = 全天）
    #[serde(default)]
    active_window: Option<PlanActiveWindow>,
    /// 模型作用域（None = 对所有模型生效）
    #[serde(default)]
    model_scope: Option<Value>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_period() -> String {
    plan_store::PERIOD_MONTHLY.into()
}
fn default_overage() -> String {
    plan_store::OVERAGE_BLOCK.into()
}
fn default_period_hours() -> i32 {
    1
}
fn default_anchor_mode() -> String {
    plan_store::ANCHOR_FIXED.into()
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
    let channels: Vec<String> = req.alert_channels.unwrap_or_else(|| vec!["in_site".into()]);
    plan_store::validate_period_config(&req.period_type, req.period_hours, &req.period_anchor_mode)
        .map_err(AppError::BadRequest)?;
    validate_plan_fields(
        Some(&req.overage_action),
        Some(&channels),
        req.downgrade_model.as_deref().map(Some),
        &st,
    )?;
    if req.overage_action == plan_store::OVERAGE_DOWNGRADE && req.downgrade_model.is_none() {
        return Err(AppError::BadRequest(
            "downgrade_action requires downgrade_model".into(),
        ));
    }
    let active_window = parse_active_window(req.active_window)?;
    let (active_start, active_end) = match active_window.flatten() {
        Some((s, e)) => (Some(s), Some(e)),
        None => (None, None),
    };
    // 模型作用域：写入期解析（格式/未知字段/黑白名单同条目冲突 400）+ 路由拼写校验
    let model_scope =
        plan_store::parse_model_scope(req.model_scope.as_ref()).map_err(AppError::BadRequest)?;
    validate_model_scope_routing(&st, model_scope.as_ref())?;
    let created = plan_store::create_plan(
        &st.pool,
        name,
        req.description.trim(),
        req.priority,
        limit,
        &req.period_type,
        req.period_hours,
        &req.period_anchor_mode,
        &req.overage_action,
        req.downgrade_model.as_deref(),
        &json!(channels),
        req.webhook_url.trim(),
        active_start,
        active_end,
        model_scope.as_ref(),
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
    Ok((
        StatusCode::CREATED,
        Json(json!({"plan": plan_row_to_value(&created)})),
    ))
}

#[derive(Deserialize)]
struct PlanUpdateReq {
    name: Option<String>,
    description: Option<String>,
    priority: Option<i32>,
    token_limit: Option<Value>,
    period_type: Option<String>,
    period_hours: Option<i32>,
    period_anchor_mode: Option<String>,
    overage_action: Option<String>,
    /// 模型作用域之外的同款三态：absent=保留 / null=清空 / 字符串=设置。
    /// 须与 model_scope 一样走 double_option，否则 null 被解成缺省、清空不可达
    #[serde(default, deserialize_with = "double_option")]
    downgrade_model: Option<Option<String>>,
    alert_channels: Option<Vec<String>>,
    #[serde(default)]
    webhook_url: Option<String>,
    /// 生效时段（成对原子更新：absent=保留 / 双 null=全天 / "HH:MM" 对=设置）
    active_window: Option<PlanActiveWindow>,
    /// 模型作用域三态：absent=保留 / null=清空（恢复全模型生效）/ 对象=设置。
    /// JSON null 默认会把 Option<Option<T>> 解成 None（与缺省不可区分），须用
    /// double_option 包一层：null → Some(None)，缺省 → None
    #[serde(default, deserialize_with = "double_option")]
    model_scope: Option<Option<Value>>,
    enabled: Option<bool>,
}

/// 双 Option 三态反序列化（serde_with::double_option 同款，免额外依赖）：
/// 字段缺省走 `default` → None；显式 null → Some(None)；其余值 → Some(Some(v))
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
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
    // 周期配置按「请求字段 ∪ 现值」合并后整体校验（部分更新语义）
    let eff_type = req.period_type.as_deref().unwrap_or(&before.period_type);
    let eff_hours = req.period_hours.unwrap_or(before.period_hours);
    let eff_anchor = req
        .period_anchor_mode
        .as_deref()
        .unwrap_or(&before.period_anchor_mode);
    plan_store::validate_period_config(eff_type, eff_hours, eff_anchor)
        .map_err(AppError::BadRequest)?;
    // 降级目标三态归一化：absent=保留 / null(或空串)=清空 / 字符串=设置
    let downgrade_model: Option<Option<&str>> = req
        .downgrade_model
        .as_ref()
        .map(|o| o.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    validate_plan_fields(
        req.overage_action.as_deref(),
        req.alert_channels.as_deref(),
        downgrade_model,
        &st,
    )?;
    // 超额策略按「请求字段 ∪ 现值」合并后校验：downgrade 必须携带有效目标
    //（请求设置或存量），且不允许把 downgrade 计划的目标清空——null/空串仅用于
    // 非 downgrade 策略的残留目标清理
    let eff_action = req
        .overage_action
        .as_deref()
        .unwrap_or(before.overage_action.as_str());
    let eff_target = downgrade_model
        .as_ref()
        .map_or(before.downgrade_model.as_deref(), |o| o.as_deref());
    if eff_action == plan_store::OVERAGE_DOWNGRADE && eff_target.is_none() {
        return Err(AppError::BadRequest(
            "downgrade_action requires downgrade_model".into(),
        ));
    }
    let active_window = parse_active_window(req.active_window)?;
    // model_scope 三态解析（写入期格式/拼写校验，错误 400 不静默）
    // 三态：absent=保留 / null=清空 / 对象=设置；`{}` 与双空数组归一化为清空
    //（parse_model_scope 返回 Option：None = 无限制，与清空同义）
    let model_scope = match &req.model_scope {
        None => None,
        Some(None) => Some(None),
        Some(Some(v)) => {
            Some(plan_store::parse_model_scope(Some(v)).map_err(AppError::BadRequest)?)
        }
    };
    let set_scope: Option<&plan_store::ModelScope> = match &model_scope {
        Some(Some(s)) => Some(s),
        _ => None,
    };
    validate_model_scope_routing(&st, set_scope)?;
    let updated = plan_store::update_plan(
        &st.pool,
        id,
        req.name.as_deref().map(str::trim),
        req.description.as_deref().map(str::trim),
        req.priority,
        limit,
        req.period_type.as_deref(),
        req.period_hours,
        req.period_anchor_mode.as_deref(),
        req.overage_action.as_deref(),
        downgrade_model,
        channels_json.as_ref(),
        req.webhook_url.as_deref().map(str::trim),
        active_window,
        model_scope,
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
    let impact = plan_store::delete_plan(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("plan not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    // 兜底通知：加入的分组与直连用户失去 Plan，成员回退次优先级分组或暂不限额
    crate::service::notify::system_notice(
        &st,
        &format!(
            "Coding Plan「{}」已被管理员 {} 删除，{} 个加入分组、{} 个直连用户失去配额（成员回退其他分组 Plan 或暂不限额）",
            before.name, admin.0.username, impact.groups, impact.direct_users
        ),
    )
    .await;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "plan.delete",
        Some("coding_plan"),
        Some(id),
        Some(json!({"before": plan_row_to_value(&before), "affected_groups": impact.groups, "affected_direct_users": impact.direct_users})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(
        json!({"ok": true, "affected_groups": impact.groups, "affected_direct_users": impact.direct_users}),
    ))
}

/// 名称唯一冲突 → 400 可读提示（其余 DB 错误照常 internal）
fn map_plan_conflict(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        if db
            .constraint()
            .is_some_and(|c| c.contains("coding_plans_name_key"))
        {
            return AppError::BadRequest("同名 Coding Plan 已存在".into());
        }
    }
    AppError::internal(e)
}

// ---------- Plan 成员管理（直连用户 / 加入分组） ----------

#[derive(Deserialize)]
struct PlanMemberListParams {
    q: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

/// (page, page_size, offset)
fn member_page(p: &PlanMemberListParams) -> (i64, i64, i64) {
    let page_size = p.page_size.unwrap_or(20).clamp(1, 200);
    let page = p.page.unwrap_or(1).max(1);
    (page, page_size, (page - 1) * page_size)
}

/// 去重并排序：重复 id 会让 rows_affected < 请求数，误报并发冲突
fn dedup_ids(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn ids_csv(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn names_cn(pairs: &[(i64, String)]) -> String {
    pairs
        .iter()
        .map(|(id, n)| format!("{n}(#{id})"))
        .collect::<Vec<_>>()
        .join("、")
}

#[derive(Clone, Copy)]
enum MemberKind {
    User,
    Group,
}

impl MemberKind {
    /// 审计动作 / 空参报错 / 冲突报错里的中文标签
    fn label(self) -> &'static str {
        match self {
            MemberKind::User => "用户",
            MemberKind::Group => "分组",
        }
    }
    fn empty_msg(self) -> String {
        match self {
            MemberKind::User => "user_ids 不能为空".into(),
            MemberKind::Group => "group_ids 不能为空".into(),
        }
    }
}

/// 重复加入 → 409（带可读的成员名列表）
async fn plan_member_conflict(st: &AppState, kind: MemberKind, dup: &[i64]) -> AppError {
    let names = match kind {
        MemberKind::User => plan_store::user_names(&st.pool, dup).await,
        MemberKind::Group => plan_store::group_names(&st.pool, dup).await,
    };
    match names {
        Ok(names) => AppError::Conflict(format!(
            "以下{}已加入该 Plan: {}",
            kind.label(),
            names_cn(&names)
        )),
        Err(e) => AppError::internal(e),
    }
}

/// 添加成员公共实现：参数校验(400) → 事务内重复预检(409，全量回滚) → reload + 审计
async fn add_plan_members(
    st: &AppState,
    plan_id: i64,
    admin_id: i64,
    kind: MemberKind,
    raw_ids: Vec<i64>,
) -> Result<axum::Json<Value>, AppError> {
    let ids = dedup_ids(raw_ids);
    if ids.is_empty() {
        return Err(AppError::BadRequest(kind.empty_msg()));
    }
    ensure_plan_exists(st, plan_id).await?;
    let missing = match kind {
        MemberKind::User => plan_store::missing_user_ids(&st.pool, &ids)
            .await
            .map_err(AppError::internal)?,
        MemberKind::Group => plan_store::missing_group_ids(&st.pool, &ids)
            .await
            .map_err(AppError::internal)?,
    };
    if !missing.is_empty() {
        return Err(AppError::BadRequest(format!(
            "以下{}不存在: {}",
            kind.label(),
            ids_csv(&missing)
        )));
    }
    // 重复预检与插入同事务；PK 唯一约束兜底并发窗口（后到者新增 0 行 → 回滚报 409）
    let mut tx = st.pool.begin().await.map_err(AppError::internal)?;
    let dup = match kind {
        MemberKind::User => plan_store::existing_plan_user_ids(&mut *tx, plan_id, &ids)
            .await
            .map_err(AppError::internal)?,
        MemberKind::Group => plan_store::existing_plan_group_ids(&mut *tx, plan_id, &ids)
            .await
            .map_err(AppError::internal)?,
    };
    if !dup.is_empty() {
        return Err(plan_member_conflict(st, kind, &dup).await);
    }
    let added = match kind {
        MemberKind::User => plan_store::add_plan_users(&mut *tx, plan_id, &ids)
            .await
            .map_err(AppError::internal)?,
        MemberKind::Group => plan_store::add_plan_groups(&mut *tx, plan_id, &ids)
            .await
            .map_err(AppError::internal)?,
    };
    if added != ids.len() as u64 {
        // 预检后有并发写入抢注：回滚本次全量，按当前状态报冲突
        let dup = match kind {
            MemberKind::User => plan_store::existing_plan_user_ids(&mut *tx, plan_id, &ids)
                .await
                .map_err(AppError::internal)?,
            MemberKind::Group => plan_store::existing_plan_group_ids(&mut *tx, plan_id, &ids)
                .await
                .map_err(AppError::internal)?,
        };
        return Err(plan_member_conflict(st, kind, &dup).await);
    }
    tx.commit().await.map_err(AppError::internal)?;
    // 成员关系变更影响生效 Plan → 运行时热重载
    st.reload().await.map_err(AppError::internal)?;
    let action = match kind {
        MemberKind::User => "plan.members.add_users",
        MemberKind::Group => "plan.members.add_groups",
    };
    audit::log(
        &st.pool,
        Some(admin_id),
        action,
        Some("coding_plan"),
        Some(plan_id),
        Some(json!({"ids": ids, "added": added})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(axum::Json(json!({"added": added})))
}

/// 移除成员公共实现（移除不存在的 id 静默跳过，与分组成员语义一致）
async fn remove_plan_members(
    st: &AppState,
    plan_id: i64,
    admin_id: i64,
    kind: MemberKind,
    raw_ids: Vec<i64>,
) -> Result<axum::Json<Value>, AppError> {
    let ids = dedup_ids(raw_ids);
    if ids.is_empty() {
        return Err(AppError::BadRequest(kind.empty_msg()));
    }
    ensure_plan_exists(st, plan_id).await?;
    let removed = match kind {
        MemberKind::User => plan_store::remove_plan_users(&st.pool, plan_id, &ids)
            .await
            .map_err(AppError::internal)?,
        MemberKind::Group => plan_store::remove_plan_groups(&st.pool, plan_id, &ids)
            .await
            .map_err(AppError::internal)?,
    };
    st.reload().await.map_err(AppError::internal)?;
    let action = match kind {
        MemberKind::User => "plan.members.remove_users",
        MemberKind::Group => "plan.members.remove_groups",
    };
    audit::log(
        &st.pool,
        Some(admin_id),
        action,
        Some("coding_plan"),
        Some(plan_id),
        Some(json!({"ids": ids, "removed": removed})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(axum::Json(json!({"removed": removed})))
}

#[derive(Deserialize)]
struct PlanUsersAddReq {
    user_ids: Vec<i64>,
}

async fn plan_users_list(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<PlanMemberListParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    ensure_plan_exists(&st, id).await?;
    let (page, page_size, offset) = member_page(&p);
    let (rows, total) =
        plan_store::list_plan_users(&st.pool, id, non_empty(&p.q), page_size, offset)
            .await
            .map_err(AppError::internal)?;
    Ok(Json(
        json!({"members": rows, "total": total, "page": page, "page_size": page_size}),
    ))
}

async fn plan_users_add(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<PlanUsersAddReq>,
) -> Result<impl IntoResponse, AppError> {
    add_plan_members(&st, id, admin.0.id, MemberKind::User, req.user_ids).await
}

async fn plan_users_remove(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<PlanUsersAddReq>,
) -> Result<impl IntoResponse, AppError> {
    remove_plan_members(&st, id, admin.0.id, MemberKind::User, req.user_ids).await
}

async fn plan_user_candidates(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<PlanMemberListParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    ensure_plan_exists(&st, id).await?;
    let (page, page_size, offset) = member_page(&p);
    let (rows, total) =
        plan_store::search_users_for_plan(&st.pool, id, non_empty(&p.q), page_size, offset)
            .await
            .map_err(AppError::internal)?;
    Ok(Json(
        json!({"users": rows, "total": total, "page": page, "page_size": page_size}),
    ))
}

#[derive(Deserialize)]
struct PlanGroupsAddReq {
    group_ids: Vec<i64>,
}

async fn plan_groups_list(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<PlanMemberListParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    ensure_plan_exists(&st, id).await?;
    let (page, page_size, offset) = member_page(&p);
    let (rows, total) =
        plan_store::list_plan_groups(&st.pool, id, non_empty(&p.q), page_size, offset)
            .await
            .map_err(AppError::internal)?;
    Ok(Json(
        json!({"groups": rows, "total": total, "page": page, "page_size": page_size}),
    ))
}

async fn plan_groups_add(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<PlanGroupsAddReq>,
) -> Result<impl IntoResponse, AppError> {
    add_plan_members(&st, id, admin.0.id, MemberKind::Group, req.group_ids).await
}

async fn plan_groups_remove(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<PlanGroupsAddReq>,
) -> Result<impl IntoResponse, AppError> {
    remove_plan_members(&st, id, admin.0.id, MemberKind::Group, req.group_ids).await
}

async fn plan_group_candidates(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(p): Query<PlanMemberListParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    ensure_plan_exists(&st, id).await?;
    let (page, page_size, offset) = member_page(&p);
    let (rows, total) =
        plan_store::search_groups_for_plan(&st.pool, id, non_empty(&p.q), page_size, offset)
            .await
            .map_err(AppError::internal)?;
    Ok(Json(
        json!({"groups": rows, "total": total, "page": page, "page_size": page_size}),
    ))
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
    let to: NaiveDate =
        p.to.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
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
    let rows: Vec<(
        i64,
        Option<i64>,
        String,
        Option<i64>,
        String,
        i16,
        String,
        i64,
        i64,
        String,
        Value,
        chrono::DateTime<Utc>,
    )> = sqlx::query_as(
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
    let group = groups::create_group(&st.pool, name, req.description.trim(), req.ldap_sync)
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
    let updated = groups::update_group(
        &st.pool,
        id,
        req.name.as_deref().map(str::trim),
        req.description.as_deref().map(str::trim),
        req.ldap_sync,
    )
    .await
    .map_err(|e| map_group_conflict(e))?
    .ok_or_else(|| AppError::BadRequest("group not found".into()))?;
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
        if db
            .constraint()
            .is_some_and(|c| c.contains("user_groups_name_key"))
        {
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
            Ok(Json(
                json!({"ok": true, "added": added, "removed": removed}),
            ))
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

/// my_plan 查询参数：`?model=` 按指定客户端模型做作用域感知解析
///（缺省 = 忽略作用域，与存量行为兼容；响应含 model_scope 供客户端自行判断）
#[derive(Deserialize)]
struct MyPlanParams {
    model: Option<String>,
}

async fn my_plan(
    State(st): State<AppState>,
    user: super::console::ConsoleUser,
    Query(p): Query<MyPlanParams>,
) -> Result<impl IntoResponse, AppError> {
    let uid = user.user.id;
    // 生效时段按服务器本地墙钟；配额周期口径仍 UTC。
    // 带 model 参数 = 模型作用域感知（与请求期 check_plan 同一解析语义）
    let queried_model = p.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let rt = plan_store::resolve_plan(
        &st.plans.read(),
        uid,
        chrono::Local::now().time(),
        queried_model,
    );
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
            "period_hours": p.period_hours,
            "period_anchor_mode": p.period_anchor_mode,
            "token_limit": p.token_limit,
            "token_limit_display": plan_store::format_token_limit(p.token_limit),
            "overage_action": p.overage_action,
            "downgrade_model": p.downgrade_model,
            "active_start": p.active_start.map(|t| t.format("%H:%M").to_string()),
            "active_end": p.active_end.map(|t| t.format("%H:%M").to_string()),
            "model_scope": p.model_scope.as_ref()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null)),
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
    let rows: Vec<(
        i64,
        Option<i64>,
        String,
        i16,
        String,
        i64,
        i64,
        Value,
        chrono::DateTime<Utc>,
    )> = sqlx::query_as(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// PATCH 三态反序列化契约：absent=保留 / null=清空 / 值=设置。
    /// Option<Option<T>> 不经 double_option 时 JSON null 与缺省都解成 None，
    /// 「清空」分支不可达（回归覆盖 downgrade_model 与 model_scope 两处）
    #[test]
    fn plan_update_req_three_state_deserialization() {
        let absent: PlanUpdateReq = serde_json::from_str("{}").unwrap();
        assert!(absent.downgrade_model.is_none());
        assert!(absent.model_scope.is_none());

        let cleared: PlanUpdateReq =
            serde_json::from_str(r#"{"downgrade_model":null,"model_scope":null}"#).unwrap();
        assert!(matches!(cleared.downgrade_model, Some(None)));
        assert!(matches!(cleared.model_scope, Some(None)));

        let set: PlanUpdateReq = serde_json::from_str(
            r#"{"downgrade_model":"gpt-4o-mini","model_scope":{"allow":["claude-*"]}}"#,
        )
        .unwrap();
        assert_eq!(set.downgrade_model, Some(Some("gpt-4o-mini".into())));
        assert!(matches!(set.model_scope, Some(Some(_))));
    }
}
