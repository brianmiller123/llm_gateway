use sqlx::{FromRow, PgPool};

/// 限流规则（scope: global | user | api_key；model 为 Some 时仅限该模型）
#[derive(Debug, Clone, FromRow)]
pub struct RateRule {
    pub scope: String,
    pub scope_id: Option<i64>,
    pub rpm: i32,
    pub burst: i32,
    /// 模型限定（NULL = 所有模型）；按客户端请求的模型名精确匹配
    pub model: Option<String>,
    /// 并发上限（在途请求数；0 = 不限）
    pub concurrency: i32,
}

/// 用户月度配额
#[derive(Debug, Clone, FromRow)]
pub struct UserQuota {
    pub user_id: i64,
    pub monthly_token_quota: Option<i64>,
    pub monthly_cost_quota: Option<f64>,
}

/// 模型计费单价（每百万 token；缓存桶缺省回退 input 单价）
#[derive(Debug, Clone, FromRow)]
pub struct ModelPrice {
    pub model: String,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    /// 缓存命中读单价（NULL = 回退 input_price_per_m，与历史行为一致）
    pub cache_read_price_per_m: Option<f64>,
    /// 缓存写入单价（NULL = 回退 input_price_per_m）
    pub cache_write_price_per_m: Option<f64>,
}

pub async fn load_rules(pool: &PgPool) -> Result<Vec<RateRule>, sqlx::Error> {
    sqlx::query_as::<_, RateRule>(
        "SELECT scope, scope_id, rpm, burst, model, concurrency FROM rate_limit_rules WHERE enabled = TRUE",
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
    sqlx::query_as::<_, ModelPrice>(
        "SELECT model, input_price_per_m::float8, output_price_per_m::float8, \
         cache_read_price_per_m::float8, cache_write_price_per_m::float8 \
         FROM (SELECT DISTINCT ON (model) model, input_price_per_m, output_price_per_m, \
               cache_read_price_per_m, cache_write_price_per_m \
               FROM model_prices ORDER BY model, effective_from DESC) t",
    )
    .fetch_all(pool)
    .await
}
