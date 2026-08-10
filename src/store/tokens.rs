use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

/// Refresh Token 行
#[derive(Debug, FromRow)]
pub struct RefreshTokenRow {
    pub id: i64,
    pub user_id: i64,
}

pub async fn insert(
    pool: &PgPool,
    user_id: i64,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(user_id)
    .bind(token_hash)
    .bind(expires_at)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// 事务内插入（refresh 旋转与用户行锁同事务，防强制下线竞态）
pub async fn insert_tx(
    conn: &mut sqlx::PgConnection,
    user_id: i64,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(user_id)
    .bind(token_hash)
    .bind(expires_at)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// 原子单次使用：仅当未吊销且未过期时吊销并返回行。
/// 并发刷新同一 token 时只有一个能拿到行（防双发）；配合用户行锁防强制下线竞态。
pub async fn revoke_if_active(
    conn: &mut sqlx::PgConnection,
    token_hash: &str,
) -> Result<Option<RefreshTokenRow>, sqlx::Error> {
    sqlx::query_as::<_, RefreshTokenRow>(
        "UPDATE refresh_tokens SET revoked_at = now() \
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now() \
         RETURNING id, user_id",
    )
    .bind(token_hash)
    .fetch_optional(&mut *conn)
    .await
}

/// 查找未吊销且未过期的 token（哈希精确匹配）
pub async fn find_active(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<RefreshTokenRow>, sqlx::Error> {
    sqlx::query_as::<_, RefreshTokenRow>(
        "SELECT id, user_id FROM refresh_tokens \
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await
}

pub async fn revoke(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 吊销某用户全部会话（禁用用户/强制下线时调用）
pub async fn revoke_all_for_user(pool: &PgPool, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}
