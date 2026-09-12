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
    /// 模型级开关：多条 system 收拢时是否合并为单条（TRUE = 合并，
    /// MiniMax 类；FALSE = 保持多条独立前移，qwen3 类）
    pub system_head_merge: bool,
    /// H3：reasoning_effort 值域钳制模式（NULL = passthrough）
    pub reasoning_effort_mode: Option<String>,
    /// H3：thinking 形态（NULL = 剥离；thinking_param / reasoning_split / enable_thinking）
    pub thinking_form: Option<String>,
    /// H2：Responses 方言字段透传白名单（逗号分隔；NULL = 全部剥离）
    pub responses_passthrough_fields: Option<String>,
}
pub async fn list_routes(pool: &PgPool) -> Result<Vec<AdminRoute>, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "SELECT id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, system_head_merge, reasoning_effort_mode, thinking_form, responses_passthrough_fields \
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
    system_head_merge: bool,
    reasoning_effort_mode: Option<&str>,
    thinking_form: Option<&str>,
    responses_passthrough_fields: Option<&str>,
) -> Result<AdminRoute, sqlx::Error> {
    sqlx::query_as::<_, AdminRoute>(
        "INSERT INTO model_routes (model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, system_head_merge, reasoning_effort_mode, thinking_form, responses_passthrough_fields) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         RETURNING id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, system_head_merge, reasoning_effort_mode, thinking_form, responses_passthrough_fields",
    )
    .bind(model_pattern)
    .bind(provider_id)
    .bind(priority)
    .bind(fallback_ids)
    .bind(upstream_model)
    .bind(enabled)
    .bind(extra_body)
    .bind(strict_system_head)
    .bind(system_head_merge)
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
    system_head_merge: Option<bool>,
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
            responses_passthrough_fields = CASE WHEN $16 THEN $15 ELSE responses_passthrough_fields END, \
            system_head_merge = COALESCE($17, system_head_merge) \
         WHERE id = $1 \
         RETURNING id, model_pattern, provider_id, priority, fallback_ids, upstream_model, enabled, extra_body, strict_system_head, system_head_merge, reasoning_effort_mode, thinking_form, responses_passthrough_fields",
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
    .bind(system_head_merge)
    .fetch_optional(pool)
    .await
}

// ---------- 限流规则 ----------

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminRateRule {
    pub id: i64,
    pub scope: String,
    pub scope_id: Option<i64>,
    /// 模型限定（NULL = 所有模型）
    pub model: Option<String>,
    pub rpm: i32,
    pub burst: i32,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
}

pub async fn list_rate_rules(pool: &PgPool) -> Result<Vec<AdminRateRule>, sqlx::Error> {
    sqlx::query_as::<_, AdminRateRule>(
        "SELECT id, scope, scope_id, model, rpm, burst, enabled, updated_at FROM rate_limit_rules ORDER BY id",
    )
    .fetch_all(pool)
    .await
}

pub async fn create_rate_rule(
    pool: &PgPool,
    scope: &str,
    scope_id: Option<i64>,
    model: Option<&str>,
    rpm: i32,
    burst: i32,
    enabled: bool,
) -> Result<AdminRateRule, sqlx::Error> {
    sqlx::query_as::<_, AdminRateRule>(
        "INSERT INTO rate_limit_rules (scope, scope_id, model, rpm, burst, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, scope, scope_id, model, rpm, burst, enabled, updated_at",
    )
    .bind(scope)
    .bind(scope_id)
    .bind(model)
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
    model: Option<Option<&str>>,
    rpm: Option<i32>,
    burst: Option<i32>,
    enabled: Option<bool>,
) -> Result<Option<AdminRateRule>, sqlx::Error> {
    sqlx::query_as::<_, AdminRateRule>(
        "UPDATE rate_limit_rules SET \
            scope = COALESCE($2, scope), \
            scope_id = CASE WHEN $8 THEN $3 ELSE scope_id END, \
            model = CASE WHEN $9 THEN $4 ELSE model END, \
            rpm = COALESCE($5, rpm), \
            burst = COALESCE($6, burst), \
            enabled = COALESCE($7, enabled), \
            updated_at = now() \
         WHERE id = $1 \
         RETURNING id, scope, scope_id, model, rpm, burst, enabled, updated_at",
    )
    .bind(id)
    .bind(scope)
    .bind(scope_id)
    .bind(model)
    .bind(rpm)
    .bind(burst)
    .bind(enabled)
    .bind(scope_id.is_some()) // $8：字段是否显式提供（None=保留原值；Some(inner)=设置/清空）
    .bind(model.is_some()) // $9：同上
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
    /// 启停：false = 该模型在该供应商不作为路由候选、不在 /v1/models 列出
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

const ADMIN_MODEL_SELECT: &str = "SELECT m.id, m.provider_id, p.name AS provider_name, m.model_id, m.enabled, m.created_at \
     FROM models m JOIN providers p ON p.id = m.provider_id";

pub async fn list_models(pool: &PgPool) -> Result<Vec<AdminModel>, sqlx::Error> {
    sqlx::query_as::<_, AdminModel>(&format!("{ADMIN_MODEL_SELECT} ORDER BY p.name, m.model_id"))
        .fetch_all(pool)
        .await
}

/// 模型启停；返回更新后的行（不存在 → None）
pub async fn set_model_enabled(
    pool: &PgPool,
    id: i64,
    enabled: bool,
) -> Result<Option<AdminModel>, sqlx::Error> {
    let res = sqlx::query("UPDATE models SET enabled = $2 WHERE id = $1")
        .bind(id)
        .bind(enabled)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    sqlx::query_as::<_, AdminModel>(&format!("{ADMIN_MODEL_SELECT} WHERE m.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 已禁用模型：provider_id → 该供应商下已禁用的 model_id 集合——运行时缓存在
/// AppState，代理管线按出站模型名过滤候选、/v1/models 过滤目录
pub async fn load_disabled_models(
    pool: &PgPool,
) -> Result<std::collections::HashMap<i64, std::collections::HashSet<String>>, sqlx::Error> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT provider_id, model_id FROM models WHERE NOT enabled")
            .fetch_all(pool)
            .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (pid, model) in rows {
        out.entry(pid).or_default().insert(model);
    }
    Ok(out)
}

/// 与 service::routing::matches_pattern 同语义：尾缀 `*` 前缀匹配或精确相等
fn pattern_hits(pattern: &str, model: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None => pattern == model,
    }
}

/// 同步某供应商的模型列表（事务；测试连接/手动刷新时调用）：上游已不存在的
/// 模型删除，仍存在的保留原行（启停状态、创建时间不变），新模型插入（默认启用）。
/// 同时为模型库中无任何已启用路由可命中的模型自动创建精确路由（兜底：
/// 列表中的模型必须可调用；已有通配/精确路由的模型不受影响，尊重手动配置）。
/// 返回写入的模型数
pub async fn replace_provider_models(
    pool: &PgPool,
    provider_id: i64,
    models: &[String],
) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // 整表删除重插会把管理员的禁用状态一并抹掉——只删上游已不再返回的模型
    sqlx::query("DELETE FROM models WHERE provider_id = $1 AND NOT (model_id = ANY($2))")
        .bind(provider_id)
        .bind(models)
        .execute(&mut *tx)
        .await?;
    let enabled_patterns: Vec<(String,)> =
        sqlx::query_as("SELECT model_pattern FROM model_routes WHERE enabled")
            .fetch_all(&mut *tx)
            .await?;
    for m in models {
        sqlx::query(
            "INSERT INTO models (provider_id, model_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(provider_id)
        .bind(m)
        .execute(&mut *tx)
        .await?;
        if enabled_patterns.iter().any(|(p,)| pattern_hits(p, m)) {
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
    sqlx::query(
        "UPDATE system_settings SET extra_body_enabled = $1, updated_at = now() WHERE id = 1",
    )
    .bind(enabled)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------- 系统设置（自定义 Header） ----------

/// 全局自定义 Header（保存时已验证：合法 HeaderName/HeaderValue、
/// 非网关管理头；运行时只读消费，无需再次校验）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HeaderSettings {
    /// 网关 → 上游提供商请求追加的头（渠道级 extra_headers 优先级更高）
    pub upstream: serde_json::Map<String, serde_json::Value>,
    /// 网关 → 客户端 /v1/* 响应附加的头
    pub response: serde_json::Map<String, serde_json::Value>,
}

/// 读取自定义 Header 配置（迁移保证列存在；非对象值防御性回退空）
pub async fn load_header_settings(pool: &PgPool) -> Result<HeaderSettings, sqlx::Error> {
    let row: Option<(serde_json::Value, serde_json::Value)> = sqlx::query_as(
        "SELECT upstream_headers, response_headers FROM system_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    let to_map = |v: serde_json::Value| v.as_object().cloned().unwrap_or_default();
    Ok(row
        .map(|(u, r)| HeaderSettings {
            upstream: to_map(u),
            response: to_map(r),
        })
        .unwrap_or_default())
}

/// 保存自定义 Header 配置（单行 upsert 语义；UPDATE 命中 id=1 恒存在）
pub async fn save_header_settings(
    pool: &PgPool,
    upstream: &serde_json::Value,
    response: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE system_settings SET upstream_headers = $1, response_headers = $2, \
         updated_at = now() WHERE id = 1",
    )
    .bind(upstream)
    .bind(response)
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

// ---------- 系统设置（SMTP，Coding Plan 告警邮件） ----------

/// SMTP 配置（system_settings 单行扩展；password 为 AES-GCM 密文）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password_enc: String,
    pub from: String,
}

impl SmtpSettings {
    /// host/port/from 齐备即可发信（username/password 仅在服务器要求认证时必需）
    pub fn configured(&self) -> bool {
        !self.host.trim().is_empty() && self.port > 0 && !self.from.trim().is_empty()
    }
}

pub async fn load_smtp_settings(pool: &PgPool) -> Result<SmtpSettings, sqlx::Error> {
    let row: (String, i32, String, String, String) = sqlx::query_as(
        "SELECT smtp_host, smtp_port, smtp_username, smtp_password_enc, smtp_from \
         FROM system_settings WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(SmtpSettings {
        host: row.0,
        port: row.1.clamp(0, u16::MAX as i32) as u16,
        username: row.2,
        password_enc: row.3,
        from: row.4,
    })
}

/// 保存 SMTP；password_enc 传 None = 不修改
pub async fn save_smtp_settings(
    pool: &PgPool,
    host: &str,
    port: u16,
    username: &str,
    password_enc: Option<&str>,
    from: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE system_settings SET \
           smtp_host = $1, smtp_port = $2, smtp_username = $3, \
           smtp_password_enc = COALESCE($4, smtp_password_enc), smtp_from = $5 \
         WHERE id = 1",
    )
    .bind(host)
    .bind(port as i32)
    .bind(username)
    .bind(password_enc)
    .bind(from)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_provider(pool: &PgPool, name: &str) -> AdminProvider {
        create_provider(
            pool,
            name,
            "openai",
            "https://example.invalid/v1",
            "",
            30_000,
            true,
            &serde_json::json!({}),
            "bearer",
            &serde_json::json!({}),
            false,
        )
        .await
        .unwrap()
    }

    fn models_of(rows: &[AdminModel], provider_id: i64) -> Vec<(String, bool)> {
        rows.iter()
            .filter(|m| m.provider_id == provider_id)
            .map(|m| (m.model_id.clone(), m.enabled))
            .collect()
    }

    /// 模型库启停：默认启用；set_model_enabled 落库并回带供应商名、不存在 → None；
    /// 刷新只删上游不再返回的模型——仍在列表的保留原行（禁用状态与 id 不变）、
    /// 新模型默认启用；禁用集合按供应商分组，同名模型在另一供应商不受影响；
    /// 空列表刷新清空该供应商
    #[sqlx::test(migrations = "./migrations")]
    async fn model_enabled_survives_refresh_and_loads_disabled_set(pool: PgPool) {
        let p1 = seed_provider(&pool, "p1").await;
        let p2 = seed_provider(&pool, "p2").await;
        let s = |v: &[&str]| v.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        replace_provider_models(&pool, p1.id, &s(&["a", "b", "c"]))
            .await
            .unwrap();
        replace_provider_models(&pool, p2.id, &s(&["b"]))
            .await
            .unwrap();
        let rows = list_models(&pool).await.unwrap();
        assert!(rows.iter().all(|m| m.enabled), "入库默认启用");
        assert!(load_disabled_models(&pool).await.unwrap().is_empty());

        let b = rows
            .iter()
            .find(|m| m.provider_id == p1.id && m.model_id == "b")
            .unwrap()
            .clone();
        let updated = set_model_enabled(&pool, b.id, false)
            .await
            .unwrap()
            .unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.provider_name, "p1", "返回行带供应商名");
        assert!(
            set_model_enabled(&pool, i64::MAX, false)
                .await
                .unwrap()
                .is_none(),
            "不存在 → None"
        );

        // 刷新：b 仍在（保留禁用）、c 下架（删除）、d 新增（默认启用）
        replace_provider_models(&pool, p1.id, &s(&["a", "b", "d"]))
            .await
            .unwrap();
        let rows = list_models(&pool).await.unwrap();
        assert_eq!(
            models_of(&rows, p1.id),
            vec![
                ("a".to_string(), true),
                ("b".to_string(), false),
                ("d".to_string(), true)
            ],
            "刷新保留禁用状态、删除下架模型、新模型默认启用"
        );
        let b_after = rows
            .iter()
            .find(|m| m.provider_id == p1.id && m.model_id == "b")
            .unwrap();
        assert_eq!(b_after.id, b.id, "仍在列表的模型保留原行, id 不变");

        let disabled = load_disabled_models(&pool).await.unwrap();
        assert_eq!(disabled.len(), 1);
        assert!(disabled[&p1.id].contains("b"));
        assert!(
            !disabled.contains_key(&p2.id),
            "同名模型 b 在 p2 仍启用, 禁用按 (供应商, 模型) 独立"
        );

        replace_provider_models(&pool, p1.id, &[]).await.unwrap();
        assert!(
            models_of(&list_models(&pool).await.unwrap(), p1.id).is_empty(),
            "空列表刷新清空该供应商模型"
        );
        assert!(
            load_disabled_models(&pool).await.unwrap().is_empty(),
            "禁用行随下架一并清理"
        );
    }
}
