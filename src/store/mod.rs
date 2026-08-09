pub mod audit;
pub mod access;
pub mod config;
pub mod keys;
pub mod rules;
pub mod tokens;
pub mod upstream;
pub mod usage;
pub mod users;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::config::AppConfig;
use crate::crypto::encrypt;

/// 初始化连接池 + 执行迁移 + 种子上游（providers 为空时）
pub async fn init(cfg: &AppConfig) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    seed(&pool, cfg).await?;
    seed_admin(&pool, cfg).await?;
    Ok(pool)
}

/// break-glass 本地管理员：users 中无本地管理员且配置了 SEED_ADMIN_* 时创建
async fn seed_admin(pool: &PgPool, cfg: &AppConfig) -> Result<(), sqlx::Error> {
    let (Some(username), Some(password)) = (&cfg.seed_admin_username, &cfg.seed_admin_password) else {
        return Ok(());
    };
    if users::has_local_admin(pool).await? {
        return Ok(());
    }
    let hash = crate::service::session::hash_password(password);
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
    let provider_id: (i64,) =
        sqlx::query_as("INSERT INTO providers (name, base_url, api_key_encrypted) VALUES ($1,$2,$3) RETURNING id")
            .bind(&name)
            .bind(&base_url)
            .bind(encrypted)
            .fetch_one(pool)
            .await?;
    let pattern = cfg
        .seed_model_pattern
        .clone()
        .unwrap_or_else(|| "*".to_string());
    sqlx::query("INSERT INTO model_routes (model_pattern, provider_id, priority) VALUES ($1,$2,100)")
        .bind(&pattern)
        .bind(provider_id.0)
        .execute(pool)
        .await?;
    tracing::info!("seeded provider '{name}' at {base_url} with route '{pattern}'");
    Ok(())
}
