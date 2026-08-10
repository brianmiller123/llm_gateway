use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use sqlx::PgPool;

use crate::config::AppConfig;
use crate::service::ratelimit::RateLimiter;
use crate::service::usage::UsageCache;
use crate::store::access::{load_user_access, UserAccessRule};
use crate::store::rules::{load_prices, load_quotas, load_rules, ModelPrice, RateRule, UserQuota};
use crate::store::upstream::{load_providers, load_routes, ModelRoute, Provider};

/// 全局应用状态（全部 Clone 廉价）
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: Arc<AppConfig>,
    pub client: Client,
    pub providers: Arc<RwLock<Vec<Provider>>>,
    pub routes: Arc<RwLock<Vec<ModelRoute>>>,
    pub rules: Arc<RwLock<Vec<RateRule>>>,
    pub quotas: Arc<RwLock<HashMap<i64, UserQuota>>>,
    pub prices: Arc<RwLock<HashMap<String, ModelPrice>>>,
    /// 用户访问授权白名单：user_id → 规则列表（空/缺省 = 默认放行）
    pub user_access: Arc<RwLock<HashMap<i64, Vec<UserAccessRule>>>>,
    /// 管理员用户 id 集合（授权检查时跳过）
    pub admin_ids: Arc<RwLock<HashSet<i64>>>,
    pub limiter: Arc<RateLimiter>,
    pub usage: Arc<UsageCache>,
    /// 运行时 LDAP 设置（DB 优先、env 兜底；保存后立即生效）
    pub ldap: Arc<RwLock<crate::service::ldap::LdapSettings>>,
    /// 配额超限告警去重（user_id, month）——首次超限才写 audit
    pub quota_alerts: Arc<Mutex<HashSet<(i64, String)>>>,
}

impl AppState {
    pub async fn init(cfg: Arc<AppConfig>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pool = crate::store::init(&cfg).await?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        let ldap = crate::service::ldap::LdapSettings::from(&*cfg);

        let state = Self {
            pool,
            cfg,
            client,
            providers: Arc::new(RwLock::new(Vec::new())),
            routes: Arc::new(RwLock::new(Vec::new())),
            rules: Arc::new(RwLock::new(Vec::new())),
            quotas: Arc::new(RwLock::new(HashMap::new())),
            prices: Arc::new(RwLock::new(HashMap::new())),
            user_access: Arc::new(RwLock::new(HashMap::new())),
            admin_ids: Arc::new(RwLock::new(HashSet::new())),
            limiter: Arc::new(RateLimiter::new()),
            usage: Arc::new(UsageCache::new()),
            ldap: Arc::new(RwLock::new(ldap)),
            quota_alerts: Arc::new(Mutex::new(HashSet::new())),
        };
        state.reload().await?;
        Ok(state)
    }

    /// 重载 LDAP 设置：DB 配置启用（url 非空）时字段级 DB 优先、空字段回退 env；
    /// DB 未启用（url 为空）则整体回退 env（兼容纯环境变量部署）
    pub(crate) async fn reload_ldap(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let db = crate::store::config::load_ldap_settings(&self.pool).await?;
        let env = crate::service::ldap::LdapSettings::from(&*self.cfg);
        let merged = if db.ldap_url.trim().is_empty() {
            env
        } else {
            let bind_password = if db.ldap_bind_password_enc.trim().is_empty() {
                env.bind_password
            } else {
                crate::crypto::decrypt(&db.ldap_bind_password_enc, &self.cfg.master_key).ok()
            };
            crate::service::ldap::LdapSettings {
                url: db.ldap_url,
                starttls: db.ldap_starttls,
                bind_dn: non_empty(db.ldap_bind_dn).or(env.bind_dn),
                bind_password,
                base_dn: non_empty(db.ldap_base_dn).unwrap_or(env.base_dn),
                user_filter: non_empty(db.ldap_user_filter).unwrap_or(env.user_filter),
                admin_groups: serde_json::from_str::<Vec<String>>(&db.ldap_admin_groups)
                    .unwrap_or_else(|_| env.admin_groups),
            }
        };
        *self.ldap.write() = merged;
        Ok(())
    }

    /// 全量热加载：供应商 / 路由 / 限流规则 / 配额 / 单价 / 访问授权 / LDAP
    pub async fn reload(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let providers = load_providers(&self.pool).await?;
        let routes = load_routes(&self.pool).await?;
        let rules = load_rules(&self.pool).await?;
        let quotas: HashMap<_, _> = load_quotas(&self.pool)
            .await?
            .into_iter()
            .map(|q| (q.user_id, q))
            .collect();
        let prices: HashMap<_, _> = load_prices(&self.pool)
            .await?
            .into_iter()
            .map(|p| (p.model.clone(), p))
            .collect();
        let mut user_access: HashMap<i64, Vec<UserAccessRule>> = HashMap::new();
        for r in load_user_access(&self.pool).await? {
            user_access.entry(r.user_id).or_default().push(r);
        }
        let admin_ids: HashSet<i64> = crate::store::users::load_admin_ids(&self.pool).await?;

        *self.providers.write() = providers;
        *self.routes.write() = routes;
        *self.rules.write() = rules;
        *self.quotas.write() = quotas;
        *self.prices.write() = prices;
        *self.user_access.write() = user_access;
        *self.admin_ids.write() = admin_ids;
        self.reload_ldap().await?;
        tracing::info!("config reloaded");
        Ok(())
    }

    /// 周期热加载 + 月度用量缓存兜底重载
    pub fn spawn_reload_tasks(&self) {
        let st = self.clone();
        tokio::spawn(async move {
            // 启动即引导配额缓存（防重启后配额检查读到 0）
            if let Err(e) = st.usage.reload_from_db(&st.pool).await {
                tracing::warn!(error = %e, "usage cache bootstrap failed");
            }
            let interval = st.cfg.reload_interval_secs;
            let mut tick = tokio::time::interval(Duration::from_secs(interval));
            loop {
                tick.tick().await;
                if let Err(e) = st.reload().await {
                    tracing::warn!(error = %e, "config reload failed");
                }
                if let Err(e) = st.usage.reload_from_db(&st.pool).await {
                    tracing::warn!(error = %e, "usage cache reload failed");
                }
            }
        });
    }
}

/// 空串 → None（DB 空字段回退 env 用）
fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}
