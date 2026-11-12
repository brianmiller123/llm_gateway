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
    /// 有效管理员权限：恒等于 COALESCE(admin_override, admin_ldap)
    pub is_admin: bool,
    /// 目录授予的管理员真值（LDAP JIT 登录按 memberOf 组写入；本地账号恒 false）
    pub admin_ldap: bool,
    /// 手动覆盖：非空时优先于目录真值（本地账号的管理员身份也存于此列）；None = 跟随目录
    pub admin_override: Option<bool>,
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
        "INSERT INTO users (username, display_name, source, password_hash, is_admin, admin_override) \
         VALUES ($1, $2, 'local', $3, $4, $4) RETURNING *",
    )
    .bind(username)
    .bind(display_name)
    .bind(password_hash)
    .bind(is_admin)
    .fetch_one(pool)
    .await
}

/// LDAP 用户 JIT 同步：不存在则创建，存在则更新邮箱/显示名并保持本地状态字段。
///
/// 管理员权限双来源：`admin_ldap` 每次登录按目录组重写真值；`is_admin` 只在没有
/// 手动覆盖时跟随目录真值——控制台的授予/撤销是显式覆盖，登录不得覆盖它。
pub async fn upsert_ldap_user(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    display_name: Option<&str>,
    ldap_dn: &str,
    is_admin: bool,
) -> Result<UserRow, sqlx::Error> {
    sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, email, display_name, ldap_dn, source, is_admin, admin_ldap) \
         VALUES ($1, $2, $3, $4, 'ldap', $5, $5) \
         ON CONFLICT (username) DO UPDATE SET \
           email = COALESCE(EXCLUDED.email, users.email), \
           display_name = COALESCE(EXCLUDED.display_name, users.display_name), \
           ldap_dn = EXCLUDED.ldap_dn, \
           source = 'ldap', \
           admin_ldap = EXCLUDED.admin_ldap, \
           is_admin = COALESCE(users.admin_override, EXCLUDED.admin_ldap) \
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

/// 权限来源：`manual` = 控制台手动覆盖（本地账号的管理员身份亦属手动），
/// `directory` = 跟随目录真值（仅 LDAP 用户可处于此态）
pub fn admin_source(admin_override: Option<bool>) -> &'static str {
    if admin_override.is_some() {
        "manual"
    } else {
        "directory"
    }
}

/// 手动授予/撤销管理员：写 admin_override 并同步有效权限。覆盖一旦写入即优先于
/// 目录真值，LDAP 登录不再改写它；清除覆盖走 [`reset_admin_source`]。
/// 控制台中间件每请求查库天然即时；代理路径靠调用方刷新 admin_ids 缓存。
pub async fn set_is_admin(pool: &PgPool, id: i64, is_admin: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET admin_override = $1, is_admin = $1 WHERE id = $2")
        .bind(is_admin)
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 恢复目录同步：清除手动覆盖，有效权限回到目录真值 admin_ldap。
/// 只有 LDAP 用户有目录真值；`source <> 'ldap'` 的行不做任何写入（调用方据此
/// 返回 400），避免本地账号落入 `admin_override = NULL` 的「directory」来源态。
pub async fn reset_admin_source(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET admin_override = NULL, is_admin = admin_ldap \
         WHERE id = $1 AND source = 'ldap'",
    )
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
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
    pub admin_override: Option<bool>,
    pub status: i16,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub month_tokens: Option<i64>,
    pub month_cost: Option<f64>,
}

pub async fn list_users(pool: &PgPool, month: &str) -> Result<Vec<UserWithUsage>, sqlx::Error> {
    sqlx::query_as::<_, UserWithUsage>(
        "SELECT u.id, u.username, u.email, u.display_name, u.source, u.is_admin, \
                u.admin_override, u.status, \
                u.last_login_at, u.created_at, um.tokens AS month_tokens, um.cost::float8 AS month_cost \
         FROM users u \
         LEFT JOIN user_monthly_usage um ON um.user_id = u.id AND um.month = $1 \
         ORDER BY u.id",
    )
    .bind(month)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const DN: &str = "uid=bob,ou=people,dc=example,dc=org";

    /// 目录真值随组走：授予组后登录 true，移出组后重登 false（无覆盖时不保留旧值）
    #[sqlx::test(migrations = "./migrations")]
    async fn ldap_admin_follows_directory_group(pool: PgPool) {
        let u = upsert_ldap_user(&pool, "bob", None, None, DN, true)
            .await
            .unwrap();
        assert!(u.is_admin, "目录组成员登录即管理员");
        assert!(u.admin_ldap, "目录真值落 admin_ldap");
        assert_eq!(u.admin_override, None, "无手动覆盖");
        assert_eq!(admin_source(u.admin_override), "directory");

        let u = upsert_ldap_user(&pool, "bob", None, None, DN, false)
            .await
            .unwrap();
        assert!(!u.is_admin, "移出目录组后重登撤销管理员");
        assert!(!u.admin_ldap);
        assert_eq!(u.admin_override, None);
    }

    /// 手动覆盖优先于目录真值：连续 JIT 登录不覆盖手动授予
    #[sqlx::test(migrations = "./migrations")]
    async fn manual_override_survives_ldap_login(pool: PgPool) {
        let alice = upsert_ldap_user(&pool, "alice", None, None, "uid=alice,ou=people,dc=example,dc=org", false)
            .await
            .unwrap();
        assert!(!alice.is_admin);

        set_is_admin(&pool, alice.id, true).await.unwrap();
        for round in 0..3 {
            let u = upsert_ldap_user(&pool, "alice", None, None, "uid=alice,ou=people,dc=example,dc=org", false)
                .await
                .unwrap();
            assert!(u.is_admin, "第 {} 次登录后手动授予仍生效", round + 1);
            assert_eq!(u.admin_override, Some(true));
            assert!(!u.admin_ldap, "目录真值仍为 false");
            assert_eq!(admin_source(u.admin_override), "manual");
        }
    }

    /// reset 清除覆盖，有效权限回到目录真值（授予与撤销两个方向）
    #[sqlx::test(migrations = "./migrations")]
    async fn reset_clears_override_to_directory_truth(pool: PgPool) {
        // 目录内管理员被手动撤销 → reset 回 true
        let bob = upsert_ldap_user(&pool, "bob", None, None, DN, true)
            .await
            .unwrap();
        set_is_admin(&pool, bob.id, false).await.unwrap();
        let u = upsert_ldap_user(&pool, "bob", None, None, DN, true)
            .await
            .unwrap();
        assert!(!u.is_admin, "手动撤销不被目录登录覆盖");
        reset_admin_source(&pool, bob.id).await.unwrap();
        let u = find_by_id(&pool, bob.id).await.unwrap().unwrap();
        assert!(u.is_admin, "reset 回到目录真值 true");
        assert_eq!(u.admin_override, None);
        assert_eq!(admin_source(u.admin_override), "directory");

        // 目录外用户被手动授予 → reset 回 false
        let carol = upsert_ldap_user(&pool, "carol", None, None, "uid=carol,ou=people,dc=example,dc=org", false)
            .await
            .unwrap();
        set_is_admin(&pool, carol.id, true).await.unwrap();
        reset_admin_source(&pool, carol.id).await.unwrap();
        let u = find_by_id(&pool, carol.id).await.unwrap().unwrap();
        assert!(!u.is_admin, "reset 回到目录真值 false");
        assert_eq!(u.admin_override, None);
    }

    /// 不变式：任一时刻 is_admin = COALESCE(admin_override, admin_ldap)。
    /// 每条写路径后各断言一次，避免后续写入把中间态「自愈」掉而掩盖回归。
    async fn assert_invariant(pool: &PgPool) {
        let (violations,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM users WHERE is_admin <> COALESCE(admin_override, admin_ldap)",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(violations, 0, "有效权限必须等于来源列的 COALESCE");
    }

    /// 四条写路径（本地创建 / LDAP JIT / 手动覆盖 / reset）都维持不变式
    #[sqlx::test(migrations = "./migrations")]
    async fn is_admin_equals_coalesce_of_source_columns(pool: PgPool) {
        let local_admin = create_local_user(&pool, "root", "Root", "hash", true)
            .await
            .unwrap();
        assert!(local_admin.is_admin);
        assert_eq!(local_admin.admin_override, Some(true), "本地权限存于覆盖列");
        assert!(!local_admin.admin_ldap);
        assert_invariant(&pool).await;

        let local_user = create_local_user(&pool, "plain", "Plain", "hash", false)
            .await
            .unwrap();
        assert_eq!(local_user.admin_override, Some(false));
        assert_invariant(&pool).await;

        let ldap_admin = upsert_ldap_user(&pool, "bob", None, None, DN, true)
            .await
            .unwrap();
        assert_invariant(&pool).await;

        let ldap_plain = upsert_ldap_user(
            &pool,
            "carol",
            None,
            None,
            "uid=carol,ou=people,dc=example,dc=org",
            false,
        )
        .await
        .unwrap();
        assert_invariant(&pool).await;

        set_is_admin(&pool, ldap_plain.id, true).await.unwrap();
        assert_invariant(&pool).await;
        set_is_admin(&pool, ldap_admin.id, false).await.unwrap();
        assert_invariant(&pool).await;

        reset_admin_source(&pool, ldap_admin.id).await.unwrap();
        assert_invariant(&pool).await;
        reset_admin_source(&pool, ldap_plain.id).await.unwrap();
        assert_invariant(&pool).await;

        upsert_ldap_user(&pool, "bob", None, None, DN, true)
            .await
            .unwrap();
        assert_invariant(&pool).await;
    }

    /// reset 只对 LDAP 用户生效：本地账号的手动权限不被清除，也不会出现
    /// `admin_source = 'directory'` 的本地行（API 侧据此返回 400）。
    #[sqlx::test(migrations = "./migrations")]
    async fn reset_admin_source_skips_non_ldap_users(pool: PgPool) {
        let root = create_local_user(&pool, "root", "Root", "hash", true)
            .await
            .unwrap();
        reset_admin_source(&pool, root.id).await.unwrap();

        let u = find_by_id(&pool, root.id).await.unwrap().unwrap();
        assert!(u.is_admin, "reset 不得清除本地管理员权限");
        assert_eq!(u.admin_override, Some(true), "本地权限仍存于覆盖列");
        assert_eq!(
            admin_source(u.admin_override),
            "manual",
            "不得出现 directory 来源的本地行"
        );
        assert_invariant(&pool).await;
    }
}
