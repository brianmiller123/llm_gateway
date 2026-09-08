//! 控制台 API：会话（登录/刷新/登出）、用户自助（me/keys/usage）、管理员（users/audit）。
//! 所有端点均需 Bearer JWT；管理员端点额外校验 is_admin。

use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, Request, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::service::console;
use crate::service::session::Claims;
use crate::state::AppState;
use crate::store::{access, audit, keys, users};

pub fn routes(state: AppState) -> Router<AppState> {
    let admin = middleware::from_fn_with_state(state.clone(), require_admin);
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{id}", delete(revoke_key))
        .route("/api/keys/{id}/reveal", get(reveal_key))
        .route("/api/usage", get(usage))
        .route("/api/usage/trend", get(usage_trend))
        .route(
            "/api/admin/users",
            get(list_users).post(create_user).layer(admin.clone()),
        )
        .route(
            "/api/admin/users/{id}",
            patch(update_user).layer(admin.clone()),
        )
        .route(
            "/api/admin/users/{id}/force-logout",
            post(force_logout).layer(admin.clone()),
        )
        .route(
            "/api/admin/users/{id}/reset-password",
            post(reset_password).layer(admin.clone()),
        )
        .route(
            "/api/admin/users/{id}/access",
            get(get_user_access)
                .put(put_user_access)
                .layer(admin.clone()),
        )
        .route("/api/admin/usage", get(admin_usage).layer(admin.clone()))
        .route(
            "/api/admin/usage/trend",
            get(admin_usage_trend).layer(admin.clone()),
        )
        .route(
            "/api/admin/usage/realtime",
            get(admin_usage_realtime).layer(admin.clone()),
        )
        .route("/api/admin/audit", get(get_audit).layer(admin))
}

// ---------- 鉴权 extractor ----------

/// 从 Authorization: Bearer 解析并校验 JWT（签名 + 用户存在 + 状态 + token_version）
pub struct ConsoleUser {
    pub user: users::UserRow,
}

impl FromRequestParts<AppState> for ConsoleUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::Unauthorized("missing bearer token".into()))?;

        let claims: Claims = crate::service::session::verify_access(&state.cfg.jwt_secret, token)?;
        let user_id: i64 = claims
            .sub
            .parse()
            .map_err(|_| AppError::Unauthorized("invalid token subject".into()))?;
        let user = users::find_by_id(&state.pool, user_id)
            .await
            .map_err(AppError::internal)?
            .ok_or_else(|| AppError::Unauthorized("user not found".into()))?;
        if user.status != 1 {
            return Err(AppError::Forbidden("account disabled".into()));
        }
        if user.token_version != claims.tv {
            return Err(AppError::Unauthorized(
                "session revoked, please login again".into(),
            ));
        }
        Ok(Self { user })
    }
}

/// 管理员中间件：必须携带有效 JWT 且 is_admin（供控制台与配置管理 API 共用）
pub(crate) async fn require_admin(
    state: axum::extract::State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| AppError::Unauthorized("missing bearer token".into()))?;
    let claims: Claims = crate::service::session::verify_access(&state.cfg.jwt_secret, token)?;
    let user_id: i64 = claims
        .sub
        .parse()
        .map_err(|_| AppError::Unauthorized("invalid token subject".into()))?;
    let user = users::find_by_id(&state.pool, user_id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::Unauthorized("user not found".into()))?;
    if user.status != 1 {
        return Err(AppError::Forbidden("account disabled".into()));
    }
    if user.token_version != claims.tv {
        return Err(AppError::Unauthorized(
            "session revoked, please login again".into(),
        ));
    }
    if !user.is_admin {
        return Err(AppError::Forbidden("admin privileges required".into()));
    }
    request.extensions_mut().insert(user);
    Ok(next.run(request).await)
}
// ---------- 会话端点 ----------

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<LoginReq>,
) -> Result<impl IntoResponse, AppError> {
    // 暴力破解防护：每用户名 10 次/分 + 每来源 IP 30 次/分（成功后重置计数）
    // 反代部署下对端地址是 Nginx，IP 桶用 X-Forwarded-For 首值（假定可信反代）
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next().map(str::trim))
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .unwrap_or(addr.ip());
    let username = req.username.trim().to_string();
    if let Err(secs) = st
        .limiter
        .check(&format!("login:user:{username}"), 10.0, 10.0)
    {
        return Err(AppError::RateLimited(secs));
    }
    if let Err(secs) = st
        .limiter
        .check(&format!("login:ip:{client_ip}"), 30.0, 30.0)
    {
        return Err(AppError::RateLimited(secs));
    }
    let session = console::login(&st, &req.username, &req.password).await?;
    // 登录成功：重置失败计数（令牌桶直接清空）
    st.limiter.reset(&format!("login:user:{username}"));
    st.limiter.reset(&format!("login:ip:{client_ip}"));
    Ok(Json(json!({
        "access_token": session.access_token,
        "refresh_token": session.refresh_token,
        "token_type": "Bearer",
        "expires_in": st.cfg.access_token_ttl,
        "user": user_json(&session.user),
    })))
}

#[derive(Deserialize)]
struct RefreshReq {
    refresh_token: String,
}

async fn refresh(
    State(st): State<AppState>,
    Json(req): Json<RefreshReq>,
) -> Result<impl IntoResponse, AppError> {
    let session = console::refresh(&st, &req.refresh_token).await?;
    Ok(Json(json!({
        "access_token": session.access_token,
        "refresh_token": session.refresh_token,
        "token_type": "Bearer",
        "expires_in": st.cfg.access_token_ttl,
        "user": user_json(&session.user),
    })))
}

#[derive(Deserialize)]
struct LogoutReq {
    refresh_token: String,
}

async fn logout(
    State(st): State<AppState>,
    Json(req): Json<LogoutReq>,
) -> Result<impl IntoResponse, AppError> {
    console::logout(&st, &req.refresh_token).await?;
    Ok(Json(json!({"ok": true})))
}

// ---------- 用户自助 ----------

async fn me(State(st): State<AppState>, user: ConsoleUser) -> Result<impl IntoResponse, AppError> {
    let month = Utc::now().format("%Y-%m").to_string();
    let monthly: Option<(i64, f64)> = sqlx::query_as(
        "SELECT tokens, cost::float8 FROM user_monthly_usage WHERE user_id = $1 AND month = $2",
    )
    .bind(user.user.id)
    .bind(&month)
    .fetch_optional(&st.pool)
    .await
    .map_err(AppError::internal)?;
    let key_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE user_id = $1 AND status = 1")
            .bind(user.user.id)
            .fetch_one(&st.pool)
            .await
            .map_err(AppError::internal)?;

    Ok(Json(json!({
        "user": user_json(&user.user),
        "usage": {
            "month": month,
            "tokens": monthly.map(|m| m.0).unwrap_or(0),
            "cost": monthly.map(|m| m.1).unwrap_or(0.0),
        },
        "active_keys": key_count.0,
    })))
}

#[derive(Deserialize)]
struct CreateKeyReq {
    name: String,
    /// 可选过期时间（RFC3339）
    expires_at: Option<chrono::DateTime<Utc>>,
}

/// 创建 API Key：明文返回一次，同时加密落库供再次复制
async fn create_key(
    State(st): State<AppState>,
    user: ConsoleUser,
    Json(req): Json<CreateKeyReq>,
) -> Result<impl IntoResponse, AppError> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::BadRequest("key name must be 1-64 chars".into()));
    }
    let active: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE user_id = $1 AND status = 1")
            .bind(user.user.id)
            .fetch_one(&st.pool)
            .await
            .map_err(AppError::internal)?;
    if active.0 >= 10 {
        return Err(AppError::BadRequest("max 10 active keys per user".into()));
    }

    let mut buf = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    let plain = format!("sk-{}", hex::encode(buf));
    let encrypted = crate::service::keys_crypto::encrypt(&st.cfg.master_key, &plain)
        .map_err(AppError::internal)?;
    let meta = keys::create(
        &st.pool,
        user.user.id,
        name,
        &plain[..12],
        &hex::encode(Sha256::digest(plain.as_bytes())),
        &encrypted,
        req.expires_at,
    )
    .await
    .map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(user.user.id),
        "api_key.create",
        Some("api_key"),
        Some(meta.id),
        Some(json!({"name": name})),
    )
    .await
    .map_err(AppError::internal)?;

    Ok(Json(json!({
        "key": plain,           // 仅此一次可见
        "key_prefix": meta.key_prefix,
        "id": meta.id,
        "name": meta.name,
        "expires_at": meta.expires_at,
    })))
}

async fn list_keys(
    State(st): State<AppState>,
    user: ConsoleUser,
) -> Result<impl IntoResponse, AppError> {
    let list = keys::list_for_user(&st.pool, user.user.id)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({"keys": list})))
}

async fn revoke_key(
    State(st): State<AppState>,
    user: ConsoleUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    if !keys::revoke(&st.pool, user.user.id, id)
        .await
        .map_err(AppError::internal)?
    {
        return Err(AppError::BadRequest(
            "key not found or not owned by you".into(),
        ));
    }
    audit::log(
        &st.pool,
        Some(user.user.id),
        "api_key.revoke",
        Some("api_key"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})))
}

/// 再次查看 API Key 明文（仅本人、仅启用的 Key；解密自加密副本）
async fn reveal_key(
    State(st): State<AppState>,
    user: ConsoleUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let encrypted = match keys::find_encrypted(&st.pool, user.user.id, id)
        .await
        .map_err(AppError::internal)?
    {
        // 行不存在或非本人：与越权同文案，不泄露存在性
        None => {
            return Err(AppError::BadRequest(
                "key not found or not owned by you".into(),
            ))
        }
        // 加密上线前的旧 Key：明文已不可恢复，明确提示重建
        Some(None) => {
            return Err(AppError::BadRequest(
                "key was created before encrypted storage; delete and recreate the key to copy it again"
                    .into(),
            ))
        }
        Some(Some(encrypted)) => encrypted,
    };
    let plain = crate::service::keys_crypto::decrypt(&st.cfg.master_key, &encrypted)
        .map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(user.user.id),
        "api_key.reveal",
        Some("api_key"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"key": plain})))
}

/// 月度按模型汇总行（用户自助与管理视图共用）
#[derive(sqlx::FromRow, serde::Serialize)]
struct ModelStat {
    model: String,
    call_count: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cost: Option<f64>,
}

/// 按日汇总行（用户自助与管理视图共用）
#[derive(sqlx::FromRow, serde::Serialize)]
struct DailyStat {
    stat_date: chrono::NaiveDate,
    call_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
}

/// 我的用量：当月按模型汇总 + 近 7 日每日汇总
async fn usage(
    State(st): State<AppState>,
    user: ConsoleUser,
) -> Result<impl IntoResponse, AppError> {
    let month = Utc::now().format("%Y-%m").to_string();
    let by_model: Vec<ModelStat> = sqlx::query_as(
        "SELECT model, COUNT(*)::bigint AS call_count, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost) AS FLOAT8) AS cost \
         FROM usage_logs WHERE user_id = $1 AND created_at >= date_trunc('month', now()) \
         GROUP BY model ORDER BY call_count DESC",
    )
    .bind(user.user.id)
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct DailyStat {
        stat_date: chrono::NaiveDate,
        call_count: i64,
        input_tokens: i64,
        output_tokens: i64,
        cost: f64,
    }
    let daily: Vec<DailyStat> = sqlx::query_as(
        "SELECT stat_date, SUM(call_count)::bigint AS call_count, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost) AS FLOAT8) AS cost \
         FROM usage_daily WHERE user_id = $1 AND stat_date >= CURRENT_DATE - 7 \
         GROUP BY stat_date ORDER BY stat_date",
    )
    .bind(user.user.id)
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    Ok(Json(json!({
        "month": month,
        "by_model": by_model,
        "last_7_days": daily,
    })))
}

/// 管理员用量分组视图（by_user/by_key/by_ip）的时间范围参数
#[derive(Deserialize)]
struct UsageRangeParams {
    /// 30m | 1d | 7d | 30d | 90d；缺省 = 本月（与汇总视图行为一致）
    range: Option<String>,
}

/// 范围 → usage_logs.created_at 时间下界 SQL 片段（固定白名单，非用户拼插）
fn range_since_sql(range: Option<&str>) -> Result<String, AppError> {
    Ok(match range.map(str::trim).filter(|r| !r.is_empty()) {
        None | Some("month") => "date_trunc('month', now())",
        Some("30m") => "now() - interval '30 minutes'",
        Some("1d") => "now() - interval '1 day'",
        Some("7d") => "now() - interval '7 days'",
        Some("30d") => "now() - interval '30 days'",
        Some("90d") => "now() - interval '90 days'",
        Some(other) => return Err(AppError::BadRequest(format!("invalid range: {other}"))),
    }
    .to_string())
}

/// 管理员用量：全用户汇总（本月按模型 + 近 7 日明细，汇总视图用）
/// + 分组视图（按用户 / API Key / 来源 IP，?range= 切换时间窗口，缺省本月）
async fn admin_usage(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Query(params): Query<UsageRangeParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    // 仅作用于分组视图；by_model / last_7_days 恒为本月/近7日（汇总视图语义不变）
    let since = range_since_sql(params.range.as_deref())?;
    let month = Utc::now().format("%Y-%m").to_string();

    let by_model: Vec<ModelStat> = sqlx::query_as(
        "SELECT model, COUNT(*)::bigint AS call_count, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost) AS FLOAT8) AS cost \
         FROM usage_logs WHERE created_at >= date_trunc('month', now()) \
         GROUP BY model ORDER BY call_count DESC",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    let daily: Vec<DailyStat> = sqlx::query_as(
        "SELECT stat_date, SUM(call_count)::bigint AS call_count, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost) AS FLOAT8) AS cost \
         FROM usage_daily WHERE stat_date >= CURRENT_DATE - 7 \
         GROUP BY stat_date ORDER BY stat_date",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct UserStat {
        user_id: Option<i64>,
        username: Option<String>,
        display_name: Option<String>,
        call_count: i64,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cost: Option<f64>,
    }
    let by_user: Vec<UserStat> = sqlx::query_as(&format!(
        "SELECT u.id AS user_id, u.username, u.display_name, COUNT(*)::bigint AS call_count, \
                CAST(SUM(l.input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(l.output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(l.cost) AS FLOAT8) AS cost \
         FROM usage_logs l LEFT JOIN users u ON u.id = l.user_id \
         WHERE l.created_at >= {since} \
         GROUP BY u.id, u.username, u.display_name ORDER BY call_count DESC",
    ))
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct KeyStat {
        key_id: i64,
        name: String,
        key_prefix: String,
        user_id: Option<i64>,
        username: Option<String>,
        call_count: i64,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cost: Option<f64>,
    }
    let by_key: Vec<KeyStat> = sqlx::query_as(&format!(
        "SELECT k.id AS key_id, k.name, k.key_prefix, u.id AS user_id, u.username, \
                COUNT(*)::bigint AS call_count, \
                CAST(SUM(l.input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(l.output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(l.cost) AS FLOAT8) AS cost \
         FROM usage_logs l \
         JOIN api_keys k ON k.id = l.api_key_id \
         LEFT JOIN users u ON u.id = k.user_id \
         WHERE l.created_at >= {since} \
         GROUP BY k.id, k.name, k.key_prefix, u.id, u.username ORDER BY call_count DESC",
    ))
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct IpStat {
        client_ip: String,
        call_count: i64,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cost: Option<f64>,
        last_seen: chrono::DateTime<Utc>,
    }
    let by_ip: Vec<IpStat> = sqlx::query_as(&format!(
        "SELECT host(client_ip) AS client_ip, COUNT(*)::bigint AS call_count, \
                CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost) AS FLOAT8) AS cost, \
                MAX(created_at) AS last_seen \
         FROM usage_logs \
         WHERE created_at >= {since} AND client_ip IS NOT NULL \
         GROUP BY client_ip ORDER BY call_count DESC",
    ))
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    Ok(Json(json!({
        "month": month,
        "by_model": by_model,
        "last_7_days": daily,
        "by_user": by_user,
        "by_key": by_key,
        "by_ip": by_ip,
    })))
}

/// 实时监控：近 5 分钟全站汇总
#[derive(sqlx::FromRow, serde::Serialize)]
struct RealtimeSummary {
    calls: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    avg_latency_ms: i64,
    cost: f64,
}

/// 实时监控：单个用户窗口统计（近 60 分钟内有过调用的用户）
#[derive(sqlx::FromRow, serde::Serialize)]
struct RealtimeUserStat {
    user_id: Option<i64>,
    username: Option<String>,
    display_name: Option<String>,
    calls_5m: i64,
    errors_5m: i64,
    calls_60m: i64,
    errors_60m: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
    avg_latency_ms: i64,
    last_call_at: chrono::DateTime<Utc>,
}

/// 实时监控：最近请求明细行
#[derive(sqlx::FromRow, serde::Serialize)]
struct RealtimeCallRow {
    id: i64,
    request_id: String,
    username: Option<String>,
    model: String,
    endpoint: Option<String>,
    streamed: bool,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    latency_ms: Option<i32>,
    status: Option<i16>,
    cost: Option<f64>,
    created_at: chrono::DateTime<Utc>,
}

/// 实时监控（管理员）：近 5 分钟全站汇总 + 近 60 分钟按用户统计 + 最近 50 条调用明细。
/// 供前端轮询刷新，不做推送；全部命中 usage_logs 时间/主键索引。
async fn admin_usage_realtime(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;

    let summary: RealtimeSummary = sqlx::query_as(
        "SELECT COUNT(*)::bigint AS calls, \
                COUNT(*) FILTER (WHERE status >= 400)::bigint AS errors, \
                COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(AVG(latency_ms), 0)::bigint AS avg_latency_ms, \
                COALESCE(SUM(cost), 0)::float8 AS cost \
         FROM usage_logs WHERE created_at >= now() - interval '5 minutes'",
    )
    .fetch_one(&st.pool)
    .await
    .map_err(AppError::internal)?;

    let users: Vec<RealtimeUserStat> = sqlx::query_as(
        "SELECT u.id AS user_id, u.username, u.display_name, \
                COUNT(*) FILTER (WHERE l.created_at >= now() - interval '5 minutes')::bigint AS calls_5m, \
                COUNT(*) FILTER (WHERE l.created_at >= now() - interval '5 minutes' AND l.status >= 400)::bigint AS errors_5m, \
                COUNT(*)::bigint AS calls_60m, \
                COUNT(*) FILTER (WHERE l.status >= 400)::bigint AS errors_60m, \
                COALESCE(SUM(l.input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(l.output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(SUM(l.cost), 0)::float8 AS cost, \
                COALESCE(AVG(l.latency_ms), 0)::bigint AS avg_latency_ms, \
                MAX(l.created_at) AS last_call_at \
         FROM usage_logs l LEFT JOIN users u ON u.id = l.user_id \
         WHERE l.created_at >= now() - interval '60 minutes' \
         GROUP BY u.id, u.username, u.display_name \
         ORDER BY calls_5m DESC, calls_60m DESC",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;

    let recent: Vec<RealtimeCallRow> = sqlx::query_as(
        "SELECT l.id, CAST(l.request_id AS TEXT) AS request_id, u.username, l.model, l.endpoint, l.streamed, \
                l.input_tokens, l.output_tokens, l.latency_ms, l.status, CAST(l.cost AS FLOAT8) AS cost, l.created_at \
         FROM usage_logs l LEFT JOIN users u ON u.id = l.user_id \
         ORDER BY l.id DESC LIMIT 50",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(AppError::internal)?;
    // 按用户实时并发（进程内在途计数；与限流器同单实例语义）+ 全站在途总数
    let active_by_user: Vec<serde_json::Value> = {
        let m = st.active_by_user.lock();
        let mut rows: Vec<(i64, i64)> = m.iter().map(|(k, c)| (*k, *c)).collect();
        rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows.into_iter()
            .map(|(user_id, active)| json!({ "user_id": user_id, "active": active }))
            .collect()
    };
    let active_total = st
        .active_requests
        .load(std::sync::atomic::Ordering::Relaxed);

    Ok(Json(json!({
        "now": Utc::now(),
        "summary": summary,
        "active_total": active_total,
        "active_by_user": active_by_user,
        "users": users,
        "recent": recent,
    })))
}

/// 按日趋势点（图表用，日期缺失补零到完整范围）
#[derive(sqlx::FromRow, serde::Serialize)]
struct TrendPoint {
    stat_date: chrono::NaiveDate,
    call_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
}

/// 单个用户的趋势序列（管理员视图）
#[derive(serde::Serialize)]
struct UserTrend {
    user_id: i64,
    username: String,
    display_name: Option<String>,
    daily: Vec<TrendPoint>,
}

#[derive(serde::Deserialize)]
struct TrendParams {
    days: Option<i64>,
    /// day（按天，默认）或 half_hour（近 24 小时每 30 分钟）
    granularity: Option<String>,
    /// 按模型过滤（空/缺省 = 全部模型）
    model: Option<String>,
}

/// 每 30 分钟趋势点（近 24h 用；stat_date 为对齐后的桶起点 UTC）
#[derive(sqlx::FromRow, serde::Serialize)]
struct HalfHourPoint {
    stat_date: chrono::DateTime<Utc>,
    call_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
}

/// 近 24h 每 30 分钟桶补零到完整 48 点（与当前桶对齐）
fn fill_half_hour_gaps(rows: Vec<HalfHourPoint>) -> Vec<HalfHourPoint> {
    let now_ts = Utc::now().timestamp();
    let bucket_now = now_ts - now_ts.rem_euclid(1800);
    let start = bucket_now - 47 * 1800;
    let mut out = Vec::with_capacity(48);
    let mut it = rows.into_iter();
    let mut next = it.next();
    for i in 0..48 {
        let ts = start + i * 1800;
        let date = chrono::DateTime::from_timestamp(ts, 0).expect("valid ts");
        if let Some(p) = next.as_ref() {
            if p.stat_date.timestamp() == ts {
                out.push(HalfHourPoint {
                    stat_date: p.stat_date,
                    call_count: p.call_count,
                    input_tokens: p.input_tokens,
                    output_tokens: p.output_tokens,
                    cost: p.cost,
                });
                next = it.next();
                continue;
            }
        }
        out.push(HalfHourPoint {
            stat_date: date,
            call_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
        });
    }
    out
}

/// 近 24h 每 30 分钟趋势（user_id 为 Some 时限定单用户；model 为 Some 时限定模型）
async fn fetch_trend_half_hour(
    pool: &sqlx::PgPool,
    user_id: Option<i64>,
    model: Option<&str>,
) -> Result<Vec<HalfHourPoint>, AppError> {
    let rows: Vec<HalfHourPoint> = sqlx::query_as(
        "SELECT to_timestamp(floor(extract(epoch FROM created_at) / 1800) * 1800) AS stat_date, \
                COUNT(*)::bigint AS call_count, \
                COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(SUM(cost), 0)::float8 AS cost \
         FROM usage_logs \
         WHERE created_at >= now() - interval '24 hours' AND ($1::bigint IS NULL OR user_id = $1) \
           AND ($2::text IS NULL OR model = $2) \
         GROUP BY 1 ORDER BY 1",
    )
    .bind(user_id)
    .bind(model)
    .fetch_all(pool)
    .await
    .map_err(AppError::internal)?;
    Ok(fill_half_hour_gaps(rows))
}

/// 拉取近 N 天按日趋势（补零；user_id 为 Some 时限定单用户；model 为 Some 时限定模型）
async fn fetch_trend_daily(
    pool: &sqlx::PgPool,
    days: i64,
    user_id: Option<i64>,
    model: Option<&str>,
) -> Result<Vec<TrendPoint>, AppError> {
    sqlx::query_as::<_, TrendPoint>(
        "SELECT d::date AS stat_date, \
                COALESCE(SUM(u.call_count), 0)::bigint AS call_count, \
                COALESCE(SUM(u.input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(u.output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(SUM(u.cost), 0)::float8 AS cost \
         FROM generate_series(CURRENT_DATE - ($1::int - 1), CURRENT_DATE, '1 day') AS d \
         LEFT JOIN usage_daily u ON u.stat_date = d::date AND ($2::bigint IS NULL OR u.user_id = $2) \
                              AND ($3::text IS NULL OR u.model = $3) \
         GROUP BY d::date ORDER BY d::date",
    )
    .bind(days)
    .bind(user_id)
    .bind(model)
    .fetch_all(pool)
    .await
    .map_err(AppError::internal)
}

/// 拉取范围内有调用的用户及其按日序列（平铺行，调用方补零）
#[derive(sqlx::FromRow)]
struct UserTrendRow {
    user_id: i64,
    username: String,
    display_name: Option<String>,
    stat_date: chrono::NaiveDate,
    call_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
}

async fn fetch_trend_by_user(
    pool: &sqlx::PgPool,
    days: i64,
    model: Option<&str>,
) -> Result<Vec<UserTrend>, AppError> {
    let rows: Vec<UserTrendRow> = sqlx::query_as(
        "SELECT u.user_id, usr.username, usr.display_name, u.stat_date, \
                SUM(u.call_count)::bigint AS call_count, \
                CAST(SUM(u.input_tokens) AS BIGINT) AS input_tokens, \
                CAST(SUM(u.output_tokens) AS BIGINT) AS output_tokens, \
                CAST(SUM(u.cost) AS FLOAT8) AS cost \
         FROM usage_daily u JOIN users usr ON usr.id = u.user_id \
         WHERE u.stat_date >= CURRENT_DATE - ($1::int - 1) \
           AND ($2::text IS NULL OR u.model = $2) \
         GROUP BY u.user_id, usr.username, usr.display_name, u.stat_date \
         ORDER BY u.user_id, u.stat_date",
    )
    .bind(days)
    .bind(model)
    .fetch_all(pool)
    .await
    .map_err(AppError::internal)?;

    // 按用户分组并为每个用户补齐完整日期序列
    let mut users: Vec<UserTrend> = Vec::new();
    for row in rows {
        if users.last().map(|u| u.user_id) != Some(row.user_id) {
            users.push(UserTrend {
                user_id: row.user_id,
                username: row.username,
                display_name: row.display_name,
                daily: Vec::new(),
            });
        }
        let cur = users.last_mut().expect("just pushed");
        cur.daily.push(TrendPoint {
            stat_date: row.stat_date,
            call_count: row.call_count,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cost: row.cost,
        });
    }
    // 每个用户补零到完整范围（保持与全站序列对齐）
    let today = chrono::Utc::now().date_naive();
    let start = today
        .checked_sub_days(chrono::Days::new((days - 1) as u64))
        .unwrap_or(today);
    for u in &mut users {
        let mut filled = Vec::with_capacity(days as usize);
        let mut it = u.daily.iter();
        let mut next = it.next();
        let mut date = start;
        while date <= today {
            if let Some(p) = next {
                if p.stat_date == date {
                    filled.push(TrendPoint {
                        stat_date: p.stat_date,
                        call_count: p.call_count,
                        input_tokens: p.input_tokens,
                        output_tokens: p.output_tokens,
                        cost: p.cost,
                    });
                    next = it.next();
                } else {
                    filled.push(TrendPoint {
                        stat_date: date,
                        call_count: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cost: 0.0,
                    });
                }
            } else {
                filled.push(TrendPoint {
                    stat_date: date,
                    call_count: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost: 0.0,
                });
            }
            date = date.succ_opt().unwrap_or(date);
        }
        u.daily = filled;
    }
    Ok(users)
}

/// 单个用户的半小时趋势序列（管理员视图）
#[derive(serde::Serialize)]
struct UserTrendHalfHour {
    user_id: i64,
    username: String,
    display_name: Option<String>,
    daily: Vec<HalfHourPoint>,
}

/// 近 24h 按用户每 30 分钟序列（平铺行 + 分组补零到 48 点）
#[derive(sqlx::FromRow)]
struct UserHalfHourRow {
    user_id: i64,
    username: String,
    display_name: Option<String>,
    stat_date: chrono::DateTime<Utc>,
    call_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
}

async fn fetch_trend_by_user_half_hour(
    pool: &sqlx::PgPool,
    model: Option<&str>,
) -> Result<Vec<UserTrendHalfHour>, AppError> {
    let rows: Vec<UserHalfHourRow> = sqlx::query_as(
        "SELECT l.user_id, usr.username, usr.display_name, \
                to_timestamp(floor(extract(epoch FROM l.created_at) / 1800) * 1800) AS stat_date, \
                COUNT(*)::bigint AS call_count, \
                COALESCE(SUM(l.input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(l.output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(SUM(l.cost), 0)::float8 AS cost \
         FROM usage_logs l JOIN users usr ON usr.id = l.user_id \
         WHERE l.created_at >= now() - interval '24 hours' \
           AND ($1::text IS NULL OR l.model = $1) \
         GROUP BY l.user_id, usr.username, usr.display_name, 4 ORDER BY l.user_id, 4",
    )
    .bind(model)
    .fetch_all(pool)
    .await
    .map_err(AppError::internal)?;

    let now_ts = Utc::now().timestamp();
    let bucket_now = now_ts - now_ts.rem_euclid(1800);
    let start = bucket_now - 47 * 1800;

    let mut users: Vec<UserTrendHalfHour> = Vec::new();
    for row in rows {
        if users.last().map(|u| u.user_id) != Some(row.user_id) {
            users.push(UserTrendHalfHour {
                user_id: row.user_id,
                username: row.username,
                display_name: row.display_name,
                daily: Vec::new(),
            });
        }
        let cur = users.last_mut().expect("just pushed");
        cur.daily.push(HalfHourPoint {
            stat_date: row.stat_date,
            call_count: row.call_count,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cost: row.cost,
        });
    }
    for u in &mut users {
        let mut filled = Vec::with_capacity(48);
        let mut it = u.daily.iter();
        let mut next = it.next();
        for i in 0..48 {
            let ts = start + i * 1800;
            let date = chrono::DateTime::from_timestamp(ts, 0).expect("valid ts");
            if let Some(p) = next {
                if p.stat_date.timestamp() == ts {
                    filled.push(HalfHourPoint {
                        stat_date: p.stat_date,
                        call_count: p.call_count,
                        input_tokens: p.input_tokens,
                        output_tokens: p.output_tokens,
                        cost: p.cost,
                    });
                    next = it.next();
                } else {
                    filled.push(HalfHourPoint {
                        stat_date: date,
                        call_count: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cost: 0.0,
                    });
                }
            } else {
                filled.push(HalfHourPoint {
                    stat_date: date,
                    call_count: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost: 0.0,
                });
            }
        }
        u.daily = filled;
    }
    Ok(users)
}

/// 本月有调用的模型列表（下拉过滤用；user_id 为 Some 时限定单用户）
async fn fetch_models(pool: &sqlx::PgPool, user_id: Option<i64>) -> Result<Vec<String>, AppError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT model FROM usage_logs \
         WHERE created_at >= date_trunc('month', now()) AND ($1::bigint IS NULL OR user_id = $1) \
         ORDER BY model",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(AppError::internal)?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// 我的用量趋势：近 N 天按日（图表）
async fn usage_trend(
    State(st): State<AppState>,
    Query(params): Query<TrendParams>,
    user: ConsoleUser,
) -> Result<impl IntoResponse, AppError> {
    let days = params.days.unwrap_or(30).clamp(1, 90);
    let model = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let models = fetch_models(&st.pool, Some(user.user.id)).await?;
    if params.granularity.as_deref() == Some("half_hour") {
        let daily = fetch_trend_half_hour(&st.pool, Some(user.user.id), model).await?;
        return Ok(Json(json!({
            "days": 1,
            "granularity": "half_hour",
            "daily": daily,
            "by_user": [],
            "models": models,
        })));
    }
    let daily = fetch_trend_daily(&st.pool, days, Some(user.user.id), model).await?;
    Ok(Json(json!({
        "days": days,
        "granularity": "day",
        "daily": daily,
        "by_user": [],
        "models": models,
    })))
}

/// 管理员用量趋势：全站 + 按用户（近 N 天按日 / 近 24h 每 30 分钟）
async fn admin_usage_trend(
    State(st): State<AppState>,
    Query(params): Query<TrendParams>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let days = params.days.unwrap_or(30).clamp(1, 90);
    let model = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let models = fetch_models(&st.pool, None).await?;
    if params.granularity.as_deref() == Some("half_hour") {
        let daily = fetch_trend_half_hour(&st.pool, None, model).await?;
        let by_user = fetch_trend_by_user_half_hour(&st.pool, model).await?;
        return Ok(Json(json!({
            "days": 1,
            "granularity": "half_hour",
            "daily": daily,
            "by_user": by_user,
            "models": models,
        })));
    }
    let daily = fetch_trend_daily(&st.pool, days, None, model).await?;
    let by_user = fetch_trend_by_user(&st.pool, days, model).await?;
    Ok(Json(json!({
        "days": days,
        "granularity": "day",
        "daily": daily,
        "by_user": by_user,
        "models": models,
    })))
}

// ---------- 管理员 ----------

async fn list_users(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
) -> Result<impl IntoResponse, AppError> {
    let _ = admin;
    let month = Utc::now().format("%Y-%m").to_string();
    let list = users::list_users(&st.pool, &month)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({"users": list})))
}

#[derive(Deserialize)]
struct CreateUserReq {
    username: String,
    display_name: Option<String>,
    password: String,
    is_admin: Option<bool>,
}

/// 创建本地用户（管理员）
async fn create_user(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Json(req): Json<CreateUserReq>,
) -> Result<impl IntoResponse, AppError> {
    let actor = admin.id;
    let username = req.username.trim().to_string();
    if username.is_empty() || username.len() > 64 {
        return Err(AppError::BadRequest("username must be 1-64 chars".into()));
    }
    if req.password.len() < 8 {
        return Err(AppError::BadRequest(
            "password must be at least 8 chars".into(),
        ));
    }
    let display_name = req
        .display_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let password = req.password.clone();
    // argon2 为纯 CPU 同步计算，移出 tokio worker 线程
    let hash =
        tokio::task::spawn_blocking(move || crate::service::session::hash_password(&password))
            .await
            .map_err(AppError::internal)?;
    let created = match users::create_local_user(
        &st.pool,
        &username,
        display_name.as_deref().unwrap_or(&username),
        &hash,
        req.is_admin.unwrap_or(false),
    )
    .await
    {
        Ok(u) => u,
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            return Err(AppError::BadRequest("username already exists".into()));
        }
        Err(e) => return Err(AppError::internal(e)),
    };
    audit::log(
        &st.pool,
        Some(actor),
        "user.create",
        Some("user"),
        Some(created.id),
        Some(json!({"username": username, "is_admin": created.is_admin})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"user": user_json(&created)})))
}

#[derive(Deserialize)]
struct UpdateUserReq {
    /// 1 = 启用, 0 = 禁用
    status: Option<i16>,
}

async fn update_user(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUserReq>,
) -> Result<impl IntoResponse, AppError> {
    let actor = admin.id;
    if users::find_by_id(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .is_none()
    {
        return Err(AppError::BadRequest("user not found".into()));
    }
    if let Some(status) = req.status {
        if !(0..=1).contains(&status) {
            return Err(AppError::BadRequest("status must be 0 or 1".into()));
        }
        users::set_status(&st.pool, id, status)
            .await
            .map_err(AppError::internal)?;
        // 禁用时吊销全部会话
        if status == 0 {
            crate::store::tokens::revoke_all_for_user(&st.pool, id)
                .await
                .map_err(AppError::internal)?;
        }
        audit::log(
            &st.pool,
            Some(actor),
            "user.status",
            Some("user"),
            Some(id),
            Some(json!({"status": status})),
        )
        .await
        .map_err(AppError::internal)?;
    }
    let updated = users::find_by_id(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::Internal("user vanished".into()))?;
    Ok(Json(json!({"user": user_json(&updated)})))
}

async fn force_logout(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let actor = admin.id;
    if users::find_by_id(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .is_none()
    {
        return Err(AppError::BadRequest("user not found".into()));
    }
    // 事务内 token_version+1 + 全量吊销 refresh token（与并发 refresh 的用户行锁串行化）
    crate::store::force_logout(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(actor),
        "user.force_logout",
        Some("user"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ResetPasswordReq {
    password: String,
}

async fn reset_password(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Path(id): Path<i64>,
    Json(req): Json<ResetPasswordReq>,
) -> Result<impl IntoResponse, AppError> {
    let actor = admin.id;
    let Some(target) = users::find_by_id(&st.pool, id)
        .await
        .map_err(AppError::internal)?
    else {
        return Err(AppError::BadRequest("user not found".into()));
    };
    if target.source != "local" {
        return Err(AppError::BadRequest(
            "LDAP users are managed in the directory; reset password there".into(),
        ));
    }
    if req.password.len() < 8 {
        return Err(AppError::BadRequest(
            "password must be at least 8 chars".into(),
        ));
    }
    let password = req.password.clone();
    // argon2 为纯 CPU 同步计算，移出 tokio worker 线程
    let hash =
        tokio::task::spawn_blocking(move || crate::service::session::hash_password(&password))
            .await
            .map_err(AppError::internal)?;
    users::set_password_hash(&st.pool, id, &hash)
        .await
        .map_err(AppError::internal)?;
    // 重置后强制重新登录（事务内 bump + 全量吊销）
    crate::store::force_logout(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(actor),
        "user.reset_password",
        Some("user"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})))
}

/// 用户访问授权（白名单）：GET 返回规则列表 + 是否已配置（未配置 = 默认全部放行）
async fn get_user_access(
    State(st): State<AppState>,
    _admin: axum::extract::Extension<users::UserRow>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    if users::find_by_id(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .is_none()
    {
        return Err(AppError::BadRequest("user not found".into()));
    }
    let rules = access::list_user_access(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    let cache = st.user_access.read();
    let configured = cache.get(&id).is_some_and(|r| !r.is_empty());
    Ok(Json(json!({ "configured": configured, "rules": rules })))
}

/// 全量替换用户授权规则（空数组 = 清空 → 恢复默认放行）
#[derive(Deserialize)]
struct AccessRuleReq {
    provider_id: Option<i64>,
    model_pattern: Option<String>,
}

#[derive(Deserialize)]
struct PutAccessReq {
    rules: Vec<AccessRuleReq>,
}

async fn put_user_access(
    State(st): State<AppState>,
    admin: axum::extract::Extension<users::UserRow>,
    Path(id): Path<i64>,
    Json(req): Json<PutAccessReq>,
) -> Result<impl IntoResponse, AppError> {
    if users::find_by_id(&st.pool, id)
        .await
        .map_err(AppError::internal)?
        .is_none()
    {
        return Err(AppError::BadRequest("user not found".into()));
    }
    // 归一化：空 pattern 视为 NULL；trim；长度限制
    let mut pairs = Vec::with_capacity(req.rules.len());
    for r in &req.rules {
        if let Some(pid) = r.provider_id {
            if crate::store::config::find_provider(&st.pool, pid)
                .await
                .map_err(AppError::internal)?
                .is_none()
            {
                return Err(AppError::BadRequest(format!("provider {pid} not found")));
            }
        }
        let pat = r
            .model_pattern
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(p) = &pat {
            if p.len() > 128 {
                return Err(AppError::BadRequest(
                    "model_pattern must be <= 128 chars".into(),
                ));
            }
        }
        pairs.push((r.provider_id, pat));
    }
    access::replace_user_access(&st.pool, id, &pairs)
        .await
        .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "user.access.update",
        Some("user"),
        Some(id),
        Some(json!({ "rule_count": pairs.len() })),
    )
    .await
    .map_err(AppError::internal)?;
    let rules = access::list_user_access(&st.pool, id)
        .await
        .map_err(AppError::internal)?;
    let cache = st.user_access.read();
    let configured = cache.get(&id).is_some_and(|r| !r.is_empty());
    Ok(Json(json!({ "configured": configured, "rules": rules })))
}

/// 审计查询参数（均可选）
#[derive(Deserialize)]
struct AuditParams {
    /// 每页条数（默认 50，最大 200）
    limit: Option<i64>,
    /// 偏移量（默认 0）
    offset: Option<i64>,
    /// 动作精确过滤
    action: Option<String>,
    /// 操作者用户名模糊搜索
    actor: Option<String>,
    /// 全文模糊搜索（动作 / 目标类型 / 详情）
    q: Option<String>,
}

async fn get_audit(
    State(st): State<AppState>,
    _admin: axum::extract::Extension<users::UserRow>,
    Query(params): Query<AuditParams>,
) -> Result<impl IntoResponse, AppError> {
    let _ = _admin;
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let f = audit::AuditFilter {
        action: params.action.filter(|s| !s.trim().is_empty()),
        actor: params.actor.filter(|s| !s.trim().is_empty()),
        q: params.q.filter(|s| !s.trim().is_empty()),
    };
    let (list, total) = audit::query(&st.pool, &f, limit, offset)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({"audit_logs": list, "total": total})))
}

// ---------- 工具 ----------

fn user_json(u: &users::UserRow) -> serde_json::Value {
    json!({
        "id": u.id,
        "username": u.username,
        "display_name": u.display_name,
        "email": u.email,
        "source": u.source,
        "ldap_dn": u.ldap_dn,
        "is_admin": u.is_admin,
        "status": u.status,
        "last_login_at": u.last_login_at,
        "created_at": u.created_at,
    })
}
