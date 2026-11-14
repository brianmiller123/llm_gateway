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
        // F2（连接加固）：池上限接配置（默认 32），代理主链路与状态页/聚合等
        // 后台任务共享池时不再挤在硬编码的 10 上
        .max_connections(cfg.db_max_conns)
        // 池满时快速失败而非挂约 30s（sqlx 默认 acquire_timeout）；不做
        // test_before_acquire（每次取连接多一个往返，故障风暴下放大池压力）
        .acquire_timeout(Duration::from_secs(5))
        .connect(&cfg.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await.map_err(|e| {
        // 连接加固：数据库 schema 比二进制新（降级部署）→ 显式拒绝而非让
        // 后续 SQL 在缺列上半途失败，留下难以回溯的半迁移状态
        if let sqlx::migrate::MigrateError::VersionMissing(v) = &e {
            return sqlx::Error::Protocol(format!(
                "DB schema 已在迁移 {v}，当前二进制更旧：不支持回滚部署；请部署匹配镜像或恢复升级前 pg_dump 快照"
            ));
        }
        e.into()
    })?;
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

#[cfg(test)]
mod tests {
    use super::init;
    use crate::config::AppConfig;

    /// F2 契约：池上限接 cfg.db_max_conns——max=2 时前两个并发获取恰好建满
    /// 配置上限，第三个获取在窗口内拿不到连接（acquire 阻塞等待而非新建），
    /// 证明上限不再硬编码 10、也不受未配置默认影响。断连验证同时覆盖
    /// test_before_acquire 移除：获取路径不再附加每连接健康校验往返。
    #[tokio::test]
    async fn pool_caps_at_configured_max_conns() {
        // 同 state::test_fixture 约定：测试环境无 GATEWAY_MASTER_KEY 时注入
        // （from_env 强依赖；一次性写入，其余测试只读）
        if std::env::var("GATEWAY_MASTER_KEY").is_err() {
            // SAFETY: 单测进程内一次性 env 写入、无并发读者依赖旧值
            unsafe {
                std::env::set_var("GATEWAY_MASTER_KEY", "ab".repeat(32));
            }
        }
        let mut cfg = AppConfig::from_env().expect("测试环境配置可用");
        cfg.database_url =
            std::env::var("DATABASE_URL").expect("套件运行须设 DATABASE_URL（house 约定）");
        cfg.db_max_conns = 2;
        let pool = init(&cfg).await.expect("池构建成功（迁移幂等）");

        let c1 = pool.acquire().await.expect("第一个获取成功");
        let c2 = pool.acquire().await.expect("第二个获取成功");
        assert_eq!(pool.size(), 2, "两个并发获取恰好建满配置上限 db_max_conns");

        let third =
            tokio::time::timeout(std::time::Duration::from_millis(300), pool.acquire()).await;
        assert!(
            third.is_err(),
            "db_max_conns=2 时第三个获取必须等待而非拿到连接（上限未硬编码）"
        );
        drop((c1, c2));
    }
}
