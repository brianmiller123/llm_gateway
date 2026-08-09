use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

/// 管理操作审计（控制台操作留痕）
pub async fn log(
    pool: &PgPool,
    actor_id: Option<i64>,
    action: &str,
    target_type: Option<&str>,
    target_id: Option<i64>,
    detail: Option<Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_logs (actor_id, action, target_type, target_id, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(actor_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub actor_id: Option<i64>,
    /// 操作者用户名（LEFT JOIN users；用户被删时可能为 NULL）
    pub actor_username: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<i64>,
    pub detail: Option<Value>,
    pub created_at: DateTime<Utc>,
}

/// 审计查询过滤条件（全部可选）
#[derive(Debug, Default)]
pub struct AuditFilter {
    /// 动作精确匹配
    pub action: Option<String>,
    /// 操作者用户名模糊匹配
    pub actor: Option<String>,
    /// 全文模糊：动作 / 目标类型 / 详情
    pub q: Option<String>,
}

/// 审计分页查询（管理端点）：返回 (当前页, 满足条件的总数)
pub async fn query(
    pool: &PgPool,
    f: &AuditFilter,
    limit: i64,
    offset: i64,
) -> Result<(Vec<AuditRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, AuditRow>(
        "SELECT a.id, a.actor_id, u.username AS actor_username, a.action, a.target_type, \
                a.target_id, a.detail, a.created_at \
         FROM audit_logs a LEFT JOIN users u ON u.id = a.actor_id \
         WHERE ($1::text IS NULL OR a.action = $1) \
           AND ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%') \
           AND ($3::text IS NULL OR a.action ILIKE '%' || $3 || '%' \
                OR a.target_type ILIKE '%' || $3 || '%' \
                OR a.detail::text ILIKE '%' || $3 || '%') \
         ORDER BY a.id DESC LIMIT $4 OFFSET $5",
    )
    .bind(&f.action)
    .bind(&f.actor)
    .bind(&f.q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_logs a LEFT JOIN users u ON u.id = a.actor_id \
         WHERE ($1::text IS NULL OR a.action = $1) \
           AND ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%') \
           AND ($3::text IS NULL OR a.action ILIKE '%' || $3 || '%' \
                OR a.target_type ILIKE '%' || $3 || '%' \
                OR a.detail::text ILIKE '%' || $3 || '%')",
    )
    .bind(&f.action)
    .bind(&f.actor)
    .bind(&f.q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}
