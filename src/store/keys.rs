use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

/// API Key 行（仅用于鉴权查询）
#[derive(Debug, FromRow)]
pub struct ApiKeyRow {
    pub id: i64,
    pub user_id: i64,
    pub key_hash: String,
    pub expires_at: Option<DateTime<Utc>>,
    /// 用户状态（1=启用）；禁用用户的所有 Key 立即失效
    pub user_status: i16,
}

/// 按 12 位前缀定位启用的 Key（JOIN 用户状态）
pub async fn find_by_prefix(pool: &PgPool, prefix: &str) -> Result<Option<ApiKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeyRow>(
        "SELECT k.id, k.user_id, k.key_hash, k.expires_at, u.status AS user_status \
         FROM api_keys k JOIN users u ON u.id = k.user_id \
         WHERE k.key_prefix = $1 AND k.status = 1",
    )
    .bind(prefix)
    .fetch_optional(pool)
    .await
}

/// 自助 Key 列表（仅启用的，不含哈希）
#[derive(Debug, FromRow, serde::Serialize)]
pub struct KeyMeta {
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// 是否可再次复制（加密存储上线前的旧 Key 明文已不可恢复）
    pub copyable: bool,
}

pub async fn list_for_user(pool: &PgPool, user_id: i64) -> Result<Vec<KeyMeta>, sqlx::Error> {
    sqlx::query_as::<_, KeyMeta>(
        "SELECT id, name, key_prefix, expires_at, last_used_at, created_at, \
                key_encrypted IS NOT NULL AS copyable \
         FROM api_keys WHERE user_id = $1 AND status = 1 ORDER BY id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// 创建 Key（调用方负责生成明文，仅存前缀 + SHA-256 + 可逆加密副本）
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    user_id: i64,
    name: &str,
    key_prefix: &str,
    key_hash: &str,
    key_encrypted: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<KeyMeta, sqlx::Error> {
    sqlx::query_as::<_, KeyMeta>(
        "INSERT INTO api_keys (user_id, name, key_prefix, key_hash, key_encrypted, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, name, key_prefix, expires_at, last_used_at, \
                                   created_at, key_encrypted IS NOT NULL AS copyable",
    )
    .bind(user_id)
    .bind(name)
    .bind(key_prefix)
    .bind(key_hash)
    .bind(key_encrypted)
    .bind(expires_at)
    .fetch_one(pool)
    .await
}

/// 取加密副本（仅本人、仅启用）。
/// 返回：None = 行不存在（或非本人）；Some(None) = 行存在但为加密上线前的旧 Key；Some(Some(密文)) = 可复制
pub async fn find_encrypted(
    pool: &PgPool,
    user_id: i64,
    key_id: i64,
) -> Result<Option<Option<String>>, sqlx::Error> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT key_encrypted FROM api_keys WHERE id = $1 AND user_id = $2 AND status = 1",
    )
    .bind(key_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// 吊销 Key（软删）
pub async fn revoke(pool: &PgPool, user_id: i64, key_id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE api_keys SET status = 0 WHERE id = $1 AND user_id = $2")
        .bind(key_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}
