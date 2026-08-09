//! 管理配置 CRUD：供应商 / 路由规则 / 限流 / 配额 / 单价。
//! 写入后由调用方触发 `AppState::reload()` 即时生效（周期 reload 兜底）。

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

// ---------- 供应商 ----------

/// 管理视图的供应商行（含 enabled/创建时间；key 已脱敏或密文）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminProvider {
    pub id: i64,
    pub name: String,
    pub api_type: String,
    pub base_url: String,
    pub api_key_encrypted: String,
    pub timeout_ms: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// 按 id 查供应商（存在性校验用）
pub async fn find_provider(pool: &PgPool, id: i64) -> Result<Option<AdminProvider>, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "SELECT id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, created_at \
         FROM providers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list_providers(pool: &PgPool) -> Result<Vec<AdminProvider>, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "SELECT id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, created_at \
         FROM providers ORDER BY id",
    )
    .fetch_all(pool)
    .await
}

pub async fn create_provider(
    pool: &PgPool,
    name: &str,
    api_type: &str,
    base_url: &str,
    api_key_encrypted: &str,
    timeout_ms: i32,
    enabled: bool,
) -> Result<AdminProvider, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "INSERT INTO providers (name, api_type, base_url, api_key_encrypted, timeout_ms, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, created_at",
    )
    .bind(name)
    .bind(api_type)
    .bind(base_url)
    .bind(api_key_encrypted)
    .bind(timeout_ms)
    .bind(enabled)
    .fetch_one(pool)
    .await
}

pub async fn update_provider(
    pool: &PgPool,
    id: i64,
    name: Option<&str>,
    api_type: Option<&str>,
    base_url: Option<&str>,
    api_key_encrypted: Option<&str>,
    timeout_ms: Option<i32>,
    enabled: Option<bool>,
) -> Result<Option<AdminProvider>, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "UPDATE providers SET \
            name = COALESCE($2, name), \
            api_type = COALESCE($3, api_type), \
            base_url = COALESCE($4, base_url), \
            api_key_encrypted = COALESCE($5, api_key_encrypted), \
            timeout_ms = COALESCE($6, timeout_ms), \
            enabled = COALESCE($7, enabled) \
         WHERE id = $1 \
         RETURNING id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, created_at",
    )
    .bind(id)
    .bind(name)
    .bind(api_type)
    .bind(base_url)
    .bind(api_key_encrypted)
    .bind(timeout_ms)
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

// ---------- 路由规则 ----------

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminRoute {
    pub id: i64,
    pub model_pattern: String,
    pub provider_id: i64,
    pub priority: i32,
    pub fallback_ids: Vec<i64>,
    /// 上游实际模型名（映射）；NULL = 透传客户端模型名
    pub upstream_model: Option<String>,
    pub enabled: bool,
}

pub async fn list_routes(pool: &PgPool) -> Result<Vec<AdminRoute>, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "SELECT id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled \
         FROM model_routes ORDER BY priority, id",
    )
    .fetch_all(pool)
    .await
}

pub async fn create_route(
    pool: &PgPool,
    model_pattern: &str,
    provider_id: i64,
    priority: i32,
    fallback_ids: &[i64],
    upstream_model: Option<&str>,
    enabled: bool,
) -> Result<AdminRoute, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "INSERT INTO model_routes (model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled",
    )
    .bind(model_pattern)
    .bind(provider_id)
    .bind(priority)
    .bind(fallback_ids)
    .bind(upstream_model)
    .bind(enabled)
    .fetch_one(pool)
    .await
}

pub async fn update_route(
    pool: &PgPool,
    id: i64,
    model_pattern: Option<&str>,
    provider_id: Option<i64>,
    priority: Option<i32>,
    fallback_ids: Option<Vec<i64>>,
    upstream_model: Option<Option<String>>,
    enabled: Option<bool>,
) -> Result<Option<AdminRoute>, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "UPDATE model_routes SET \
            model_pattern = COALESCE($2, model_pattern), \
            provider_id = COALESCE($3, provider_id), \
            priority = COALESCE($4, priority), \
            fallback_ids = COALESCE($5, fallback_ids), \
            upstream_model = COALESCE($6, upstream_model), \
            enabled = COALESCE($7, enabled) \
         WHERE id = $1 \
         RETURNING id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled",
    )
    .bind(id)
    .bind(model_pattern)
    .bind(provider_id)
    .bind(priority)
    .bind(fallback_ids)
    .bind(upstream_model)
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

// ---------- 限流规则 ----------

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminRateRule {
    pub id: i64,
    pub scope: String,
    pub scope_id: Option<i64>,
    pub rpm: i32,
    pub burst: i32,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
}

pub async fn list_rate_rules(pool: &PgPool) -> Result<Vec<AdminRateRule>, sqlx::Error> {
    sqlx::query_as::<_, AdminRateRule>(
        "SELECT id, scope, scope_id, rpm, burst, enabled, updated_at FROM rate_limit_rules ORDER BY id",
    )
    .fetch_all(pool)
    .await
}

pub async fn create_rate_rule(
    pool: &PgPool,
    scope: &str,
    scope_id: Option<i64>,
    rpm: i32,
    burst: i32,
    enabled: bool,
) -> Result<AdminRateRule, sqlx::Error> {
    sqlx::query_as::<_, AdminRateRule>(
        "INSERT INTO rate_limit_rules (scope, scope_id, rpm, burst, enabled) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, scope, scope_id, rpm, burst, enabled, updated_at",
    )
    .bind(scope)
    .bind(scope_id)
    .bind(rpm)
    .bind(burst)
    .bind(enabled)
    .fetch_one(pool)
    .await
}

pub async fn update_rate_rule(
    pool: &PgPool,
    id: i64,
    scope: Option<&str>,
    scope_id: Option<Option<i64>>,
    rpm: Option<i32>,
    burst: Option<i32>,
    enabled: Option<bool>,
) -> Result<Option<AdminRateRule>, sqlx::Error> {
    sqlx::query_as::<_, AdminRateRule>(
        "UPDATE rate_limit_rules SET \
            scope = COALESCE($2, scope), \
            scope_id = $3, \
            rpm = COALESCE($4, rpm), \
            burst = COALESCE($5, burst), \
            enabled = COALESCE($6, enabled), \
            updated_at = now() \
         WHERE id = $1 \
         RETURNING id, scope, scope_id, rpm, burst, enabled, updated_at",
    )
    .bind(id)
    .bind(scope)
    .bind(scope_id)
    .bind(rpm)
    .bind(burst)
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

pub async fn delete_rate_rule(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM rate_limit_rules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ---------- 用户配额 ----------

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminQuota {
    pub user_id: i64,
    pub username: String,
    pub monthly_token_quota: Option<i64>,
    pub monthly_cost_quota: Option<f64>,
    pub billing_day: i32,
    pub notify_percent: i32,
    pub enabled: bool,
}

pub async fn list_quotas(pool: &PgPool) -> Result<Vec<AdminQuota>, sqlx::Error> {
    // 列出全部用户（LEFT JOIN：未配置配额的用户也展示，配额列为空表示不限制）
    sqlx::query_as::<_, AdminQuota>(
        "SELECT u.id AS user_id, u.username, q.monthly_token_quota, q.monthly_cost_quota::float8, \
                COALESCE(q.billing_day, 1) AS billing_day, \
                COALESCE(q.notify_percent, 80) AS notify_percent, \
                COALESCE(q.enabled, TRUE) AS enabled \
         FROM users u LEFT JOIN user_quotas q ON q.user_id = u.id ORDER BY u.id",
    )
    .fetch_all(pool)
    .await
}

pub async fn upsert_quota(
    pool: &PgPool,
    user_id: i64,
    monthly_token_quota: Option<Option<i64>>,
    monthly_cost_quota: Option<Option<f64>>,
    billing_day: Option<i32>,
    notify_percent: Option<i32>,
    enabled: Option<bool>,
) -> Result<Option<AdminQuota>, sqlx::Error> {
    sqlx::query(
        "INSERT INTO user_quotas (user_id, monthly_token_quota, monthly_cost_quota, billing_day, notify_percent, enabled) \
         VALUES ($1, COALESCE($2, NULL), COALESCE($3, NULL), COALESCE($4, 1), COALESCE($5, 80), COALESCE($6, TRUE)) \
         ON CONFLICT (user_id) DO UPDATE SET \
            monthly_token_quota = $7, \
            monthly_cost_quota = $8, \
            billing_day = COALESCE($4, user_quotas.billing_day), \
            notify_percent = COALESCE($5, user_quotas.notify_percent), \
            enabled = COALESCE($6, user_quotas.enabled), \
            updated_at = now()",
    )
    .bind(user_id)
    .bind(monthly_token_quota)
    .bind(monthly_cost_quota)
    .bind(billing_day)
    .bind(notify_percent)
    .bind(enabled)
    .bind(monthly_token_quota) // $7
    .bind(monthly_cost_quota) // $8
    .execute(pool)
    .await?;

    // 再查完整行（带 username）
    sqlx::query_as::<_, AdminQuota>(
        "SELECT q.user_id, u.username, q.monthly_token_quota, q.monthly_cost_quota::float8, \
                q.billing_day, q.notify_percent, q.enabled \
         FROM user_quotas q JOIN users u ON u.id = q.user_id WHERE q.user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

// ---------- 模型价格 ----------

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminPrice {
    pub id: i64,
    pub model: String,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    pub currency: String,
    pub effective_from: chrono::NaiveDate,
}

pub async fn list_prices(pool: &PgPool) -> Result<Vec<AdminPrice>, sqlx::Error> {
    sqlx::query_as::<_, AdminPrice>(
        "SELECT id, model, input_price_per_m::float8, output_price_per_m::float8, currency, effective_from \
         FROM model_prices ORDER BY model, effective_from DESC",
    )
    .fetch_all(pool)
    .await
}

pub async fn create_price(
    pool: &PgPool,
    model: &str,
    input_price_per_m: Option<f64>,
    output_price_per_m: Option<f64>,
    currency: &str,
    effective_from: chrono::NaiveDate,
) -> Result<AdminPrice, sqlx::Error> {
    // INSERT 只 RETURNING id（Postgres 对 RETURNING 的 cast 列 describe 有兼容性问题），再查完整行
    let id: (i64,) = sqlx::query_as(
        "INSERT INTO model_prices (model, input_price_per_m, output_price_per_m, currency, effective_from) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(model)
    .bind(input_price_per_m)
    .bind(output_price_per_m)
    .bind(currency)
    .bind(effective_from)
    .fetch_one(pool)
    .await?;
    sqlx::query_as::<_, AdminPrice>(
        "SELECT id, model, input_price_per_m::float8, output_price_per_m::float8, currency, effective_from \
         FROM model_prices WHERE id = $1",
    )
    .bind(id.0)
    .fetch_one(pool)
    .await
}

pub async fn delete_price(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM model_prices WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ---------- 模型库 ----------

/// 模型库行（JOIN 供应商名，供下拉展示）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminModel {
    pub id: i64,
    pub provider_id: i64,
    pub provider_name: String,
    pub model_id: String,
    pub created_at: DateTime<Utc>,
}

pub async fn list_models(pool: &PgPool) -> Result<Vec<AdminModel>, sqlx::Error> {
    sqlx::query_as::<_, AdminModel>(
        "SELECT m.id, m.provider_id, p.name AS provider_name, m.model_id, m.created_at \
         FROM models m JOIN providers p ON p.id = m.provider_id \
         ORDER BY p.name, m.model_id",
    )
    .fetch_all(pool)
    .await
}

/// 与 service::routing::matches_pattern 同语义：尾缀 `*` 前缀匹配或精确相等
fn pattern_hits(pattern: &str, model: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None => pattern == model,
    }
}

/// 整表替换某供应商的模型列表（事务；测试连接/手动刷新时调用）
/// 同时为模型库中无任何已启用路由可命中的模型自动创建精确路由（兜底：
/// 列表中的模型必须可调用；已有通配/精确路由的模型不受影响，尊重手动配置）。
/// 返回写入的模型数
pub async fn replace_provider_models(
    pool: &PgPool,
    provider_id: i64,
    models: &[String],
) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM models WHERE provider_id = $1")
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;
    let enabled_patterns: Vec<(String,)> =
        sqlx::query_as("SELECT model_pattern FROM model_routes WHERE enabled")
            .fetch_all(&mut *tx)
            .await?;
    for m in models {
        sqlx::query("INSERT INTO models (provider_id, model_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(provider_id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
        if enabled_patterns
            .iter()
            .any(|(p,)| pattern_hits(p, m))
        {
            continue;
        }
        sqlx::query("INSERT INTO model_routes (model_pattern, provider_id) VALUES ($1, $2)")
            .bind(m)
            .bind(provider_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(models.len())
}

pub async fn delete_model(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM models WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}
