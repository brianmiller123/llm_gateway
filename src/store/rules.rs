use sqlx::{FromRow, PgPool};

/// 限流规则（scope: global | user | api_key）
#[derive(Debug, Clone, FromRow)]
pub struct RateRule {
    pub scope: String,
    pub scope_id: Option<i64>,
    pub rpm: i32,
    pub burst: i32,
}

/// 用户月度配额
#[derive(Debug, Clone, FromRow)]
pub struct UserQuota {
    pub user_id: i64,
    pub monthly_token_quota: Option<i64>,
    pub monthly_cost_quota: Option<f64>,
}

/// 模型计费单价（每百万 token）
#[derive(Debug, Clone, FromRow)]
pub struct ModelPrice {
    pub model: String,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
}

pub async fn load_rules(pool: &PgPool) -> Result<Vec<RateRule>, sqlx::Error> {
    sqlx::query_as::<_, RateRule>(
        "SELECT scope, scope_id, rpm, burst FROM rate_limit_rules WHERE enabled = TRUE",
    )
    .fetch_all(pool)
    .await
}

pub async fn load_quotas(pool: &PgPool) -> Result<Vec<UserQuota>, sqlx::Error> {
    sqlx::query_as::<_, UserQuota>(
        "SELECT user_id, monthly_token_quota, monthly_cost_quota
         FROM user_quotas WHERE enabled = TRUE",
    )
    .fetch_all(pool)
    .await
}

pub async fn load_prices(pool: &PgPool) -> Result<Vec<ModelPrice>, sqlx::Error> {
    // 子查询包一层：DISTINCT ON 内层不 cast，外层投影 cast——
    // 避免 DISTINCT ON 与 ::float8 同层时 describe 返回 NUMERIC 的兼容问题
    sqlx::query_as::<_, ModelPrice>(
        "SELECT model, input_price_per_m::float8, output_price_per_m::float8 \
         FROM (SELECT DISTINCT ON (model) model, input_price_per_m, output_price_per_m \
               FROM model_prices ORDER BY model, effective_from DESC) t",
    )
    .fetch_all(pool)
    .await
}
