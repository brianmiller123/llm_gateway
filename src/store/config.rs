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
    /// extra_body 透传配置（JSON 对象；{} = 未配置）
    pub extra_body: serde_json::Value,
    /// 认证形态：bearer（默认）/ x-api-key
    pub auth_scheme: String,
    /// 渠道级静态附加请求头（{} = 未配置）
    pub extra_headers: serde_json::Value,
    /// H6：渠道是否支持图像输入（FALSE → 发前主动降级图片 part）
    pub supports_images: bool,
    pub created_at: DateTime<Utc>,
}

/// 按 id 查供应商（存在性校验用）
pub async fn find_provider(pool: &PgPool, id: i64) -> Result<Option<AdminProvider>, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "SELECT id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, extra_body, auth_scheme, extra_headers, supports_images, created_at \
         FROM providers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list_providers(pool: &PgPool) -> Result<Vec<AdminProvider>, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "SELECT id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, extra_body, auth_scheme, extra_headers, supports_images, created_at \
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
    extra_body: &serde_json::Value,
    auth_scheme: &str,
    extra_headers: &serde_json::Value,
    supports_images: bool,
) -> Result<AdminProvider, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "INSERT INTO providers (name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, extra_body, auth_scheme, extra_headers, supports_images) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         RETURNING id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, extra_body, auth_scheme, extra_headers, supports_images, created_at",
    )
    .bind(name)
    .bind(api_type)
    .bind(base_url)
    .bind(api_key_encrypted)
    .bind(timeout_ms)
    .bind(enabled)
    .bind(extra_body)
    .bind(auth_scheme)
    .bind(extra_headers)
    .bind(supports_images)
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
    // None = 不修改；Some(空对象) = 清空
    extra_body: Option<&serde_json::Value>,
    auth_scheme: Option<&str>,
    extra_headers: Option<&serde_json::Value>,
    supports_images: Option<bool>,
) -> Result<Option<AdminProvider>, sqlx::Error> {
    sqlx::query_as::<_, AdminProvider>(
        "UPDATE providers SET \
            name = COALESCE($2, name), \
            api_type = COALESCE($3, api_type), \
            base_url = COALESCE($4, base_url), \
            api_key_encrypted = COALESCE($5, api_key_encrypted), \
            timeout_ms = COALESCE($6, timeout_ms), \
            enabled = COALESCE($7, enabled), \
            extra_body = COALESCE($8, extra_body), \
            auth_scheme = COALESCE($9, auth_scheme), \
            extra_headers = COALESCE($10, extra_headers), \
            supports_images = COALESCE($11, supports_images) \
         WHERE id = $1 \
         RETURNING id, name, api_type, base_url, api_key_encrypted, timeout_ms, enabled, extra_body, auth_scheme, extra_headers, supports_images, created_at",
    )
    .bind(id)
    .bind(name)
    .bind(api_type)
    .bind(base_url)
    .bind(api_key_encrypted)
    .bind(timeout_ms)
    .bind(enabled)
    .bind(extra_body)
    .bind(auth_scheme)
    .bind(extra_headers)
    .bind(supports_images)
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
    /// 模型级 extra_body（覆盖渠道级同名叶键；{} = 未配置）
    pub extra_body: serde_json::Value,
    /// 模型级开关：system 消息收拢到头部（管理员按模型启用）
    pub strict_system_head: bool,
    /// H3：reasoning_effort 值域钳制模式（NULL = passthrough）
    pub reasoning_effort_mode: Option<String>,
    /// H3：thinking 形态（NULL = 剥离；thinking_param / reasoning_split / enable_thinking）
    pub thinking_form: Option<String>,
    /// H2：Responses 方言字段透传白名单（逗号分隔；NULL = 全部剥离）
    pub responses_passthrough_fields: Option<String>,
}

pub async fn list_routes(pool: &PgPool) -> Result<Vec<AdminRoute>, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "SELECT id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, reasoning_effort_mode, thinking_form, responses_passthrough_fields \
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
    extra_body: &serde_json::Value,
    strict_system_head: bool,
    reasoning_effort_mode: Option<&str>,
    thinking_form: Option<&str>,
    responses_passthrough_fields: Option<&str>,
) -> Result<AdminRoute, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "INSERT INTO model_routes (model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, reasoning_effort_mode, thinking_form, responses_passthrough_fields) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         RETURNING id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, reasoning_effort_mode, thinking_form, responses_passthrough_fields",
    )
    .bind(model_pattern)
    .bind(provider_id)
    .bind(priority)
    .bind(fallback_ids)
    .bind(upstream_model)
    .bind(enabled)
    .bind(extra_body)
    .bind(strict_system_head)
    .bind(reasoning_effort_mode)
    .bind(thinking_form)
    .bind(responses_passthrough_fields)
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
    // None = 不修改；Some(空对象) = 清空
    extra_body: Option<&serde_json::Value>,
    strict_system_head: Option<bool>,
    // H3：None = 不修改；Some(None) = 清空回 passthrough；Some(Some(v)) = 设置
    reasoning_effort_mode: Option<Option<String>>,
    thinking_form: Option<Option<String>>,
    responses_passthrough_fields: Option<Option<String>>,
) -> Result<Option<AdminRoute>, sqlx::Error> {
    // upstream_model 语义：None = 不修改；Some(None) = 清空映射；Some(Some(v)) = 设置
    // SQL 无法区分 NULL 与缺失，故用 $8 布尔标记（$6 为 NULL 时 CASE 决定保留/清空）
    let (upstream_val, upstream_provided) = match upstream_model {
        Some(inner) => (inner, true),
        None => (None, false),
    };
    let (mode_val, mode_provided) = match reasoning_effort_mode {
        Some(inner) => (inner, true),
        None => (None, false),
    };
    let (thinking_val, thinking_provided) = match thinking_form {
        Some(inner) => (inner, true),
        None => (None, false),
    };
    let (passthrough_val, passthrough_provided) = match responses_passthrough_fields {
        Some(inner) => (inner, true),
        None => (None, false),
    };
    sqlx::query_as::<_, AdminRoute>(
        "UPDATE model_routes SET \
            model_pattern = COALESCE($2, model_pattern), \
            provider_id = COALESCE($3, provider_id), \
            priority = COALESCE($4, priority), \
            fallback_ids = COALESCE($5, fallback_ids), \
            upstream_model = CASE WHEN $8 THEN $6 ELSE upstream_model END, \
            enabled = COALESCE($7, enabled), \
            extra_body = COALESCE($9, extra_body), \
            strict_system_head = COALESCE($10, strict_system_head), \
            reasoning_effort_mode = CASE WHEN $12 THEN $11 ELSE reasoning_effort_mode END, \
            thinking_form = CASE WHEN $14 THEN $13 ELSE thinking_form END, \
            responses_passthrough_fields = CASE WHEN $16 THEN $15 ELSE responses_passthrough_fields END \
         WHERE id = $1 \
         RETURNING id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, reasoning_effort_mode, thinking_form, responses_passthrough_fields",
    )
    .bind(id)
    .bind(model_pattern)
    .bind(provider_id)
    .bind(priority)
    .bind(fallback_ids)
    .bind(upstream_val)
    .bind(enabled)
    .bind(upstream_provided)
    .bind(extra_body)
    .bind(strict_system_head)
    .bind(mode_val)
    .bind(mode_provided)
    .bind(thinking_val)
    .bind(thinking_provided)
    .bind(passthrough_val)
    .bind(passthrough_provided)
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
            scope_id = CASE WHEN $7 THEN $3 ELSE scope_id END, \
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
    .bind(scope_id.is_some()) // $7：字段是否显式提供（None=保留原值；Some(inner)=设置/清空）
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
            monthly_token_quota = CASE WHEN $9 THEN $7 ELSE user_quotas.monthly_token_quota END, \
            monthly_cost_quota  = CASE WHEN $10 THEN $8 ELSE user_quotas.monthly_cost_quota END, \
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
    .bind(monthly_token_quota.is_some()) // $9：字段是否显式提供（None=保留原值；Some(inner)=设置/清空）
    .bind(monthly_cost_quota.is_some()) // $10
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
        // 复用同 provider 已禁用的精确路由（避免重复行），否则新建
        let res = sqlx::query(
            "UPDATE model_routes SET enabled = TRUE \
             WHERE model_pattern = $1 AND provider_id = $2 AND enabled = FALSE",
        )
        .bind(m)
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            sqlx::query("INSERT INTO model_routes (model_pattern, provider_id) VALUES ($1, $2)")
                .bind(m)
                .bind(provider_id)
                .execute(&mut *tx)
                .await?;
        }
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

// ---------- 系统设置（LDAP） ----------

/// system_settings 行的 DB 表示；bind_password 为密文
#[derive(Debug, Clone, FromRow)]
pub struct LdapDbSettings {
    pub ldap_url: String,
    pub ldap_starttls: bool,
    pub ldap_bind_dn: String,
    pub ldap_bind_password_enc: String,
    pub ldap_base_dn: String,
    pub ldap_user_filter: String,
    pub ldap_admin_groups: String,
}

/// 读取系统设置（单行表，迁移保证存在）
pub async fn load_ldap_settings(pool: &PgPool) -> Result<LdapDbSettings, sqlx::Error> {
    sqlx::query_as::<_, LdapDbSettings>(
        "SELECT ldap_url, ldap_starttls, ldap_bind_dn, ldap_bind_password_enc, \
         ldap_base_dn, ldap_user_filter, ldap_admin_groups \
         FROM system_settings WHERE id = 1",
    )
    .fetch_one(pool)
    .await
}

/// 保存系统设置（单行 upsert）；bind_password_enc 传 None = 不修改。
/// admin_groups 以 JSON 数组存 TEXT（DN 含逗号，不能用逗号分隔）
pub async fn save_ldap_settings(
    pool: &PgPool,
    url: &str,
    starttls: bool,
    bind_dn: &str,
    bind_password_enc: Option<&str>,
    base_dn: &str,
    user_filter: &str,
    admin_groups: &[String],
) -> Result<(), sqlx::Error> {
    let groups_json = serde_json::to_string(admin_groups).unwrap_or_else(|_| "[]".into());
    sqlx::query(
        "INSERT INTO system_settings \
           (id, ldap_url, ldap_starttls, ldap_bind_dn, ldap_bind_password_enc, \
            ldap_base_dn, ldap_user_filter, ldap_admin_groups, updated_at) \
         VALUES (1, $1, $2, $3, CASE WHEN $4 IS NULL THEN '' ELSE $4 END, $5, $6, $7, now()) \
         ON CONFLICT (id) DO UPDATE SET \
           ldap_url = EXCLUDED.ldap_url, \
           ldap_starttls = EXCLUDED.ldap_starttls, \
           ldap_bind_dn = EXCLUDED.ldap_bind_dn, \
           ldap_bind_password_enc = CASE WHEN $4 IS NULL THEN system_settings.ldap_bind_password_enc ELSE $4 END, \
           ldap_base_dn = EXCLUDED.ldap_base_dn, \
           ldap_user_filter = EXCLUDED.ldap_user_filter, \
           ldap_admin_groups = EXCLUDED.ldap_admin_groups, \
           updated_at = now()",
    )
    .bind(url)
    .bind(starttls)
    .bind(bind_dn)
    .bind(bind_password_enc)
    .bind(base_dn)
    .bind(user_filter)
    .bind(groups_json)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------- 系统设置（extra_body 全局开关） ----------

/// extra_body 合并全局开关（false = 保留配置但不合并，方便临时停用）
pub async fn load_extra_body_enabled(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let row: Option<(bool,)> =
        sqlx::query_as("SELECT extra_body_enabled FROM system_settings WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0).unwrap_or(true))
}

pub async fn save_extra_body_enabled(pool: &PgPool, enabled: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE system_settings SET extra_body_enabled = $1, updated_at = now() WHERE id = 1")
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------- 系统设置（API 端点管理） ----------

/// API 端点运行时开关：启用/停用与用户页地址可见性（Response API × Messages API 独立）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiEndpointSettings {
    pub responses_enabled: bool,
    pub responses_visible: bool,
    pub messages_enabled: bool,
    pub messages_visible: bool,
}

#[derive(FromRow)]
struct ApiEndpointRow {
    responses_enabled: bool,
    responses_visible: bool,
    messages_enabled: bool,
    messages_visible: bool,
}

/// 读取 API 端点开关（单行表，迁移保证列存在；缺省全部启用/可见）
pub async fn load_api_endpoint_settings(pool: &PgPool) -> Result<ApiEndpointSettings, sqlx::Error> {
    let row: Option<ApiEndpointRow> = sqlx::query_as(
        "SELECT responses_enabled, responses_visible, messages_enabled, messages_visible \
         FROM system_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|r| ApiEndpointSettings {
            responses_enabled: r.responses_enabled,
            responses_visible: r.responses_visible,
            messages_enabled: r.messages_enabled,
            messages_visible: r.messages_visible,
        })
        .unwrap_or(ApiEndpointSettings {
            responses_enabled: true,
            responses_visible: true,
            messages_enabled: true,
            messages_visible: true,
        }))
}

/// 保存 API 端点开关（单行 upsert 语义；UPDATE 命中 id=1 恒存在）
pub async fn save_api_endpoint_settings(
    pool: &PgPool,
    s: &ApiEndpointSettings,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE system_settings SET responses_enabled = $1, responses_visible = $2, \
         messages_enabled = $3, messages_visible = $4, updated_at = now() WHERE id = 1",
    )
    .bind(s.responses_enabled)
    .bind(s.responses_visible)
    .bind(s.messages_enabled)
    .bind(s.messages_visible)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------- API 测试结果 ----------

/// 管理员 API 测试历史行（按时间倒序返回给前端）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct ApiTestResultRow {
    pub id: i64,
    pub api: String,
    pub admin_id: Option<i64>,
    pub model: String,
    pub stream: bool,
    pub status_code: i32,
    pub ok: bool,
    pub latency_ms: i64,
    pub error: String,
    pub body_preview: String,
    pub created_at: DateTime<Utc>,
}

pub async fn insert_api_test_result(
    pool: &PgPool,
    api: &str,
    admin_id: Option<i64>,
    model: &str,
    stream: bool,
    status_code: i32,
    ok: bool,
    latency_ms: i64,
    error: &str,
    body_preview: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO api_test_results \
         (api, admin_id, model, stream, status_code, ok, latency_ms, error, body_preview) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(api)
    .bind(admin_id)
    .bind(model)
    .bind(stream)
    .bind(status_code)
    .bind(ok)
    .bind(latency_ms)
    .bind(error)
    .bind(body_preview)
    .execute(pool)
    .await?;
    Ok(())
}

/// 最近 N 条测试结果（时间倒序；limit 由调用方钳制）
pub async fn list_api_test_results(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ApiTestResultRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, api, admin_id, model, stream, status_code, ok, latency_ms, error, \
         body_preview, created_at FROM api_test_results ORDER BY created_at DESC, id DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}
