use sqlx::{FromRow, PgPool};

/// 上游供应商（api_key_encrypted 为 AES-256-GCM 密文）
#[derive(Debug, Clone, FromRow)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    /// 上游方言：openai（默认，Chat Completions 兼容）/ openai-responses（原生 Responses API）等
    pub api_type: String,
    pub base_url: String,
    pub api_key_encrypted: String,
    pub timeout_ms: i32,
}

/// 模型路由规则
#[derive(Debug, Clone, FromRow)]
pub struct ModelRoute {
    pub model_pattern: String,
    pub provider_id: i64,
    pub fallback_ids: Vec<i64>,
    /// 上游实际模型名（映射）；NULL = 透传客户端模型名
    pub upstream_model: Option<String>,
}

pub async fn load_providers(pool: &PgPool) -> Result<Vec<Provider>, sqlx::Error> {
    sqlx::query_as::<_, Provider>(
        "SELECT id, name, api_type, base_url, api_key_encrypted, timeout_ms FROM providers WHERE enabled = TRUE",
    )
    .fetch_all(pool)
    .await
}

pub async fn load_routes(pool: &PgPool) -> Result<Vec<ModelRoute>, sqlx::Error> {
    sqlx::query_as::<_, ModelRoute>(
        "SELECT model_pattern, provider_id, fallback_ids, upstream_model
         FROM model_routes WHERE enabled = TRUE ORDER BY priority ASC, id ASC",
    )
    .fetch_all(pool)
    .await
}
