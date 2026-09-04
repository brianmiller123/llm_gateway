//! 用户分组存储：CRUD、成员管理（手动/批量/全部快照）、LDAP 成员增量对账、
//! 成员分页检索与导出。分组与 Plan 的关联在 plan_groups（见 store/plans.rs），
//! 此处只读聚合展示已加入的 Plan 列表。
//!
//! 海量成员性能：批量添加用单条 INSERT..SELECT（unnest 数组绑定），全部用户
//! 快照为单语句 INSERT..SELECT..ON CONFLICT DO NOTHING；列表检索走
//! (group_id, user_id) 主键 JOIN + LIMIT/OFFSET。

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

/// 分组行（管理视图：plans 为已加入 Plan 的 jsonb 数组 [{id,name,enabled}]，含成员计数）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct GroupRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// 已加入的 Plan（plan_groups 关联，按 plan id 序）
    pub plans: Value,
    pub ldap_sync: bool,
    pub member_count: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_sync_result: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// plans 聚合列：jsonb 数组 → GroupRow 直接携带（前端按数组渲染）
const GROUP_SELECT: &str = "SELECT g.id, g.name, g.description, \
            COALESCE(jsonb_agg(jsonb_build_object('id', p.id, 'name', p.name, 'enabled', p.enabled) \
                               ORDER BY p.id) FILTER (WHERE p.id IS NOT NULL), '[]'::jsonb) AS plans, \
            g.ldap_sync, \
            (SELECT COUNT(*) FROM user_group_members m WHERE m.group_id = g.id) AS member_count, \
            g.last_sync_at, g.last_sync_result, g.created_at, g.updated_at \
     FROM user_groups g \
     LEFT JOIN plan_groups pg ON pg.group_id = g.id \
     LEFT JOIN coding_plans p ON p.id = pg.plan_id";

pub async fn list_groups(pool: &PgPool) -> Result<Vec<GroupRow>, sqlx::Error> {
    sqlx::query_as::<_, GroupRow>(&format!("{GROUP_SELECT} GROUP BY g.id ORDER BY g.id"))
        .fetch_all(pool)
        .await
}

pub async fn find_group(pool: &PgPool, id: i64) -> Result<Option<GroupRow>, sqlx::Error> {
    sqlx::query_as::<_, GroupRow>(&format!("{GROUP_SELECT} WHERE g.id = $1 GROUP BY g.id"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn create_group(
    pool: &PgPool,
    name: &str,
    description: &str,
    ldap_sync: bool,
) -> Result<GroupRow, sqlx::Error> {
    let id: (i64,) = sqlx::query_as(
        "INSERT INTO user_groups (name, description, ldap_sync) \
         VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(name)
    .bind(description)
    .bind(ldap_sync)
    .fetch_one(pool)
    .await?;
    find_group(pool, id.0)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

/// 部分更新（None = 保留）。加入/退出 Plan 走 plan_members API，不在分组编辑内。
pub async fn update_group(
    pool: &PgPool,
    id: i64,
    name: Option<&str>,
    description: Option<&str>,
    ldap_sync: Option<bool>,
) -> Result<Option<GroupRow>, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE user_groups SET \
           name = COALESCE($2, name), \
           description = COALESCE($3, description), \
           ldap_sync = COALESCE($4, ldap_sync), \
           updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(description)
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
