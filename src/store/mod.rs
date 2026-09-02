pub mod access;
pub mod audit;
pub mod config;
pub mod groups;
pub mod keys;
pub mod plans;
pub mod rules;
pub mod tokens;
pub mod upstream;
pub mod usage;
pub mod users;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

use crate::config::AppConfig;
use crate::crypto::encrypt;

/// 初始化连接池 + 执行迁移 + 种子上游（providers 为空时）
pub async fn init(cfg: &AppConfig) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        // 池满时快速失败而非挂约 30s（sqlx 默认 acquire_timeout）；取连接前校验健康
        .acquire_timeout(Duration::from_secs(5))
        .test_before_acquire(true)
        .connect(&cfg.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    seed(&pool, cfg).await?;
    seed_admin(&pool, cfg).await?;
    Ok(pool)
}

/// break-glass 本地管理员：users 中无本地管理员且配置了 SEED_ADMIN_* 时创建
async fn seed_admin(pool: &PgPool, cfg: &AppConfig) -> Result<(), sqlx::Error> {
    let (Some(username), Some(password)) = (&cfg.seed_admin_username, &cfg.seed_admin_password)
    else {
        return Ok(());
    };
    if users::has_local_admin(pool).await? {
        return Ok(());
    }
    let password = password.to_string();
    let hash =
        tokio::task::spawn_blocking(move || crate::service::session::hash_password(&password))
            .await
            .map_err(|e| sqlx::Error::Protocol(format!("argon2 task failed: {e}")))?;
    users::create_local_user(pool, username, username, &hash, true).await?;
    tracing::info!("seeded local admin '{username}'");
    Ok(())
}

async fn seed(pool: &PgPool, cfg: &AppConfig) -> Result<(), sqlx::Error> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM providers")
        .fetch_one(pool)
        .await?;
    if count.0 > 0 {
        return Ok(());
    }
    let (Some(name), Some(base_url), Some(api_key)) = (
        &cfg.seed_provider_name,
        &cfg.seed_provider_base_url,
        &cfg.seed_provider_api_key,
    ) else {
        tracing::info!("providers empty and no SEED_* vars set; skipping seed");
        return Ok(());
    };
    let encrypted = encrypt(api_key.as_bytes(), &cfg.master_key)
        .map_err(|e| sqlx::Error::Protocol(format!("seed encrypt failed: {e}")))?;
    let provider_id: (i64,) = sqlx::query_as(
        "INSERT INTO providers (name, base_url, api_key_encrypted) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(&name)
    .bind(&base_url)
    .bind(encrypted)
    .fetch_one(pool)
    .await?;
    let pattern = cfg
        .seed_model_pattern
        .clone()
        .unwrap_or_else(|| "*".to_string());
    sqlx::query(
        "INSERT INTO model_routes (model_pattern, provider_id, priority) VALUES ($1,$2,100)",
    )
    .bind(&pattern)
    .bind(provider_id.0)
    .execute(pool)
    .await?;
    tracing::info!("seeded provider '{name}' at {base_url} with route '{pattern}'");
    Ok(())
}

/// 强制下线：token_version + 1 与全量吊销 refresh token 在同一事务。
/// 与并发 refresh 的用户行锁（find_by_id_for_update）串行化，
/// 防止新签发的 refresh token 在吊销之后插入而逃过强制下线。
pub async fn force_logout(pool: &PgPool, user_id: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET token_version = token_version + 1 WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}
