//! 用户 × 供应商/模型 访问授权（白名单）。
//! 加载进 AppState 供代理路径热生效；写入后由调用方触发 `AppState::reload()`。

use sqlx::{FromRow, PgPool};

/// 缓存中的授权规则（无需 id/created_at）
#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct UserAccessRule {
    pub user_id: i64,
    pub provider_id: Option<i64>,
    pub model_pattern: Option<String>,
}

/// 管理视图行（含供应商名，供 UI 展示）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AdminAccessRule {
    pub id: i64,
    pub user_id: i64,
    pub provider_id: Option<i64>,
    pub provider_name: Option<String>,
    pub model_pattern: Option<String>,
}

/// 全量加载（缓存 + 管理端点共用）
pub async fn load_user_access(pool: &PgPool) -> Result<Vec<UserAccessRule>, sqlx::Error> {
    sqlx::query_as::<_, UserAccessRule>(
        "SELECT user_id, provider_id, model_pattern FROM user_access_rules",
    )
    .fetch_all(pool)
    .await
}

/// 某用户的规则（含供应商名，按 id 排序）
pub async fn list_user_access(pool: &PgPool, user_id: i64) -> Result<Vec<AdminAccessRule>, sqlx::Error> {
    sqlx::query_as::<_, AdminAccessRule>(
        "SELECT r.id, r.user_id, r.provider_id, p.name AS provider_name, r.model_pattern \
         FROM user_access_rules r \
         LEFT JOIN providers p ON p.id = r.provider_id \
         WHERE r.user_id = $1 ORDER BY r.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// 全量替换某用户的规则（事务；空列表 = 清空授权 → 恢复默认放行）
pub async fn replace_user_access(
    pool: &PgPool,
    user_id: i64,
    rules: &[(Option<i64>, Option<String>)],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_access_rules WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for (provider_id, model_pattern) in rules {
        sqlx::query(
            "INSERT INTO user_access_rules (user_id, provider_id, model_pattern) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(user_id)
        .bind(provider_id)
        .bind(model_pattern)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}
