//! 用户分组存储：CRUD、成员管理（手动/批量/全部快照）、LDAP 成员增量对账、
//! 成员分页检索与导出。
//!
//! 海量成员性能：批量添加用单条 INSERT..SELECT（unnest 数组绑定），全部用户
//! 快照为单语句 INSERT..SELECT..ON CONFLICT DO NOTHING；列表检索走
//! (group_id, user_id) 主键 JOIN + LIMIT/OFFSET。

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

/// 分组行（管理视图：含绑定 Plan 名与成员计数）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct GroupRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub plan_id: Option<i64>,
    pub plan_name: Option<String>,
    pub plan_enabled: Option<bool>,
    pub ldap_sync: bool,
    pub member_count: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_sync_result: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn list_groups(pool: &PgPool) -> Result<Vec<GroupRow>, sqlx::Error> {
    sqlx::query_as::<_, GroupRow>(
        "SELECT g.id, g.name, g.description, g.plan_id, p.name AS plan_name, \
                p.enabled AS plan_enabled, g.ldap_sync, \
                (SELECT COUNT(*) FROM user_group_members m WHERE m.group_id = g.id) AS member_count, \
                g.last_sync_at, g.last_sync_result, g.created_at, g.updated_at \
         FROM user_groups g \
         LEFT JOIN coding_plans p ON p.id = g.plan_id \
         ORDER BY g.id",
    )
    .fetch_all(pool)
    .await
}

pub async fn find_group(pool: &PgPool, id: i64) -> Result<Option<GroupRow>, sqlx::Error> {
    sqlx::query_as::<_, GroupRow>(
        "SELECT g.id, g.name, g.description, g.plan_id, p.name AS plan_name, \
                p.enabled AS plan_enabled, g.ldap_sync, \
                (SELECT COUNT(*) FROM user_group_members m WHERE m.group_id = g.id) AS member_count, \
                g.last_sync_at, g.last_sync_result, g.created_at, g.updated_at \
         FROM user_groups g \
         LEFT JOIN coding_plans p ON p.id = g.plan_id \
         WHERE g.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn create_group(
    pool: &PgPool,
    name: &str,
    description: &str,
    plan_id: Option<i64>,
    ldap_sync: bool,
) -> Result<GroupRow, sqlx::Error> {
    let id: (i64,) = sqlx::query_as(
        "INSERT INTO user_groups (name, description, plan_id, ldap_sync) \
         VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(name)
    .bind(description)
    .bind(plan_id)
    .bind(ldap_sync)
    .fetch_one(pool)
    .await?;
    find_group(pool, id.0)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

/// 部分更新；plan_id 三态：None=不修改 / Some(None)=解绑 / Some(Some(id))=绑定
pub async fn update_group(
    pool: &PgPool,
    id: i64,
    name: Option<&str>,
    description: Option<&str>,
    plan_id: Option<Option<i64>>,
    ldap_sync: Option<bool>,
) -> Result<Option<GroupRow>, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE user_groups SET \
           name = COALESCE($2, name), \
           description = COALESCE($3, description), \
           plan_id = COALESCE($4, plan_id), \
           ldap_sync = COALESCE($5, ldap_sync), \
           updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(plan_id)
    .bind(ldap_sync)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(None);
    }
    find_group(pool, id).await
}

pub async fn delete_group(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    // 成员行由 FK ON DELETE CASCADE 级联清理
    Ok(sqlx::query("DELETE FROM user_groups WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

// ---------- 成员 ----------

/// LDAP 同步建档：不存在则建（source='ldap'，非管理员）；存在则仅补邮箱/显示名/DN，
/// 不触碰 is_admin/status/source（同步不做管理员判定，与登录 JIT 语义不同）。
pub async fn upsert_sync_user(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    display_name: Option<&str>,
    dn: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, display_name, ldap_dn, source, is_admin) \
         VALUES ($1, $2, $3, $4, 'ldap', FALSE) \
         ON CONFLICT (username) DO UPDATE SET \
           email = COALESCE(EXCLUDED.email, users.email), \
           display_name = COALESCE(EXCLUDED.display_name, users.display_name), \
           ldap_dn = EXCLUDED.ldap_dn \
         RETURNING id",
    )
    .bind(username)
    .bind(email)
    .bind(display_name)
    .bind(dn)
    .fetch_one(pool)
    .await
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct MemberRow {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub source: String,
    pub status: i16,
    pub added_at: DateTime<Utc>,
}

/// 成员分页列表（q 模糊匹配用户名/显示名/邮箱）
pub async fn list_members(
    pool: &PgPool,
    group_id: i64,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<MemberRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, MemberRow>(
        "SELECT u.id AS user_id, u.username, u.display_name, u.email, m.source, u.status, m.added_at \
         FROM user_group_members m JOIN users u ON u.id = m.user_id \
         WHERE m.group_id = $1 \
           AND ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%' \
                OR u.display_name ILIKE '%' || $2 || '%' \
                OR u.email ILIKE '%' || $2 || '%') \
         ORDER BY u.id LIMIT $3 OFFSET $4",
    )
    .bind(group_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM user_group_members m JOIN users u ON u.id = m.user_id \
         WHERE m.group_id = $1 \
           AND ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%' \
                OR u.display_name ILIKE '%' || $2 || '%' \
                OR u.email ILIKE '%' || $2 || '%')",
    )
    .bind(group_id)
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

/// 批量添加成员（手动）。返回实际新增数（已在组内者跳过）。
pub async fn add_members(
    pool: &PgPool,
    group_id: i64,
    user_ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if user_ids.is_empty() {
        return Ok(0);
    }
    Ok(sqlx::query(
        "INSERT INTO user_group_members (group_id, user_id, source) \
         SELECT $1, x, 'manual' FROM unnest($2::bigint[]) AS x \
         ON CONFLICT (group_id, user_id) DO NOTHING",
    )
    .bind(group_id)
    .bind(user_ids)
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn remove_members(
    pool: &PgPool,
    group_id: i64,
    user_ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if user_ids.is_empty() {
        return Ok(0);
    }
    Ok(sqlx::query(
        "DELETE FROM user_group_members \
         WHERE group_id = $1 AND user_id = ANY($2::bigint[])",
    )
    .bind(group_id)
    .bind(user_ids)
    .execute(pool)
    .await?
    .rows_affected())
}

/// 一键添加全部启用用户（快照语义；单语句完成，ON CONFLICT 跳过已在组内者）
pub async fn add_all_users(pool: &PgPool, group_id: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "INSERT INTO user_group_members (group_id, user_id, source) \
         SELECT $1, id, 'all' FROM users WHERE status = 1 \
         ON CONFLICT (group_id, user_id) DO NOTHING",
    )
    .bind(group_id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// LDAP 成员增量对账（单事务）：
/// - 目录侧新增用户 → 补插成员（source='ldap'；与既有 manual 成员冲突时保留 manual）
/// - 目录侧移除用户 → 仅删 source='ldap' 的成员，手动添加/移除不受同步影响
pub async fn reconcile_ldap_members(
    pool: &PgPool,
    group_id: i64,
    ldap_user_ids: &[i64],
) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let added = sqlx::query(
        "INSERT INTO user_group_members (group_id, user_id, source) \
         SELECT $1, x, 'ldap' FROM unnest($2::bigint[]) AS x \
         ON CONFLICT (group_id, user_id) DO NOTHING",
    )
    .bind(group_id)
    .bind(ldap_user_ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let removed = sqlx::query(
        "DELETE FROM user_group_members \
         WHERE group_id = $1 AND source = 'ldap' AND NOT user_id = ANY($2::bigint[])",
    )
    .bind(group_id)
    .bind(ldap_user_ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE user_groups SET last_sync_at = now(), \
           last_sync_result = $2, updated_at = now() WHERE id = $1",
    )
    .bind(group_id)
    .bind(format!("+{added} / -{removed}"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((added, removed))
}

pub async fn record_sync_failure(
    pool: &PgPool,
    group_id: i64,
    result: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE user_groups SET last_sync_at = now(), last_sync_result = $2 WHERE id = $1")
        .bind(group_id)
        .bind(result)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct UserPickRow {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub source: String,
    pub status: i16,
    pub is_member: bool,
}

/// 成员选择器：全用户分页检索 + 组内标记（单查询渲染勾选态）
pub async fn search_users_for_group(
    pool: &PgPool,
    group_id: i64,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<UserPickRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, UserPickRow>(
        "SELECT u.id, u.username, u.display_name, u.email, u.source, u.status, \
                (m.user_id IS NOT NULL) AS is_member \
         FROM users u \
         LEFT JOIN user_group_members m ON m.group_id = $1 AND m.user_id = u.id \
         WHERE ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%' \
                OR u.display_name ILIKE '%' || $2 || '%' \
                OR u.email ILIKE '%' || $2 || '%') \
         ORDER BY u.id LIMIT $3 OFFSET $4",
    )
    .bind(group_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM users u \
         WHERE ($1::text IS NULL OR u.username ILIKE '%' || $1 || '%' \
                OR u.display_name ILIKE '%' || $1 || '%' \
                OR u.email ILIKE '%' || $1 || '%')",
    )
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

#[derive(Debug, FromRow)]
pub struct MemberExportRow {
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub source: String,
    pub added_at: DateTime<Utc>,
}

/// 全量成员（CSV 导出）
pub async fn export_members(
    pool: &PgPool,
    group_id: i64,
) -> Result<Vec<MemberExportRow>, sqlx::Error> {
    sqlx::query_as::<_, MemberExportRow>(
        "SELECT u.username, u.display_name, u.email, m.source, m.added_at \
         FROM user_group_members m JOIN users u ON u.id = m.user_id \
         WHERE m.group_id = $1 ORDER BY u.id",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await
}

/// 全部开启 LDAP 同步的分组 id（周期同步任务用）
pub async fn ldap_sync_group_ids(pool: &PgPool) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT id, name FROM user_groups WHERE ldap_sync = TRUE ORDER BY id",
    )
    .fetch_all(pool)
    .await
}
