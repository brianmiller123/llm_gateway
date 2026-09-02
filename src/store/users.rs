use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use std::collections::HashSet;

/// 用户行（控制台会话 + 管理）
#[derive(Debug, Clone, FromRow)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub ldap_dn: Option<String>,
    pub source: String,
    pub password_hash: Option<String>,
    pub is_admin: bool,
    /// 1 = 启用, 0 = 禁用
    pub status: i16,
    pub token_version: i32,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub async fn find_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE username = $1")
        .bind(username)
        .fetch_optional(pool)
        .await
}

pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 锁定用户行（与强制下线/重置密码的 token_version 更新串行化）
pub async fn find_by_id_for_update(
    conn: &mut sqlx::PgConnection,
    id: i64,
) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
}

/// 创建本地用户（控制台/管理员创建、break-glass 种子共用）
pub async fn create_local_user(
    pool: &PgPool,
    username: &str,
    display_name: &str,
    password_hash: &str,
    is_admin: bool,
) -> Result<UserRow, sqlx::Error> {
    sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, display_name, source, password_hash, is_admin) \
         VALUES ($1, $2, 'local', $3, $4) RETURNING *",
    )
    .bind(username)
    .bind(display_name)
    .bind(password_hash)
    .bind(is_admin)
    .fetch_one(pool)
    .await
}

/// LDAP 用户 JIT 同步：不存在则创建，存在则更新邮箱/显示名并保持本地状态字段
pub async fn upsert_ldap_user(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    display_name: Option<&str>,
    ldap_dn: &str,
    is_admin: bool,
) -> Result<UserRow, sqlx::Error> {
    sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, email, display_name, ldap_dn, source, is_admin) \
         VALUES ($1, $2, $3, $4, 'ldap', $5) \
         ON CONFLICT (username) DO UPDATE SET \
           email = COALESCE(EXCLUDED.email, users.email), \
           display_name = COALESCE(EXCLUDED.display_name, users.display_name), \
           ldap_dn = EXCLUDED.ldap_dn, \
           source = 'ldap', \
           is_admin = EXCLUDED.is_admin \
         RETURNING *",
    )
    .bind(username)
    .bind(email)
    .bind(display_name)
    .bind(ldap_dn)
    .bind(is_admin)
    .fetch_one(pool)
    .await
}

pub async fn touch_last_login(pool: &PgPool, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_status(pool: &PgPool, user_id: i64, status: i16) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 重置密码：仅本地用户
pub async fn set_password_hash(
    pool: &PgPool,
    user_id: i64,
    password_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(password_hash)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 全部启用管理员 id（授权检查缓存用）
pub async fn load_admin_ids(pool: &PgPool) -> Result<HashSet<i64>, sqlx::Error> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM users WHERE is_admin AND status = 1")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// 是否存在本地管理员（break-glass 种子判断）
pub async fn has_local_admin(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM users WHERE source = 'local' AND is_admin AND status = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

/// 用户列表（管理端点，含当月用量）
#[derive(Debug, FromRow, serde::Serialize)]
pub struct UserWithUsage {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub source: String,
    pub is_admin: bool,
    pub status: i16,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub month_tokens: Option<i64>,
    pub month_cost: Option<f64>,
}

pub async fn list_users(pool: &PgPool, month: &str) -> Result<Vec<UserWithUsage>, sqlx::Error> {
    sqlx::query_as::<_, UserWithUsage>(
        "SELECT u.id, u.username, u.email, u.display_name, u.source, u.is_admin, u.status, \
                u.last_login_at, u.created_at, um.tokens AS month_tokens, um.cost::float8 AS month_cost \
         FROM users u \
         LEFT JOIN user_monthly_usage um ON um.user_id = u.id AND um.month = $1 \
         ORDER BY u.id",
    )
    .bind(month)
    .fetch_all(pool)
    .await
}
