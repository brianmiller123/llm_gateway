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
    /// M3：进程内每渠道熔断器（连续可重试失败 → 短窗跳过；仅内存态）
    pub breaker: Arc<crate::service::breaker::Breaker>,
    pub limiter: Arc<RateLimiter>,
    pub usage: Arc<UsageCache>,
    /// 运行时 LDAP 设置（DB 优先、env 兜底；保存后立即生效）
    pub ldap: Arc<RwLock<crate::service::ldap::LdapSettings>>,
    /// extra_body 合并全局开关（false = 保留配置但不合并进上游请求体）
    pub extra_body_enabled: Arc<RwLock<bool>>,
    /// API 端点运行时开关（Response API × Anthropic Messages API 独立启停/可见性）
    pub api_endpoints: Arc<RwLock<crate::store::config::ApiEndpointSettings>>,
    /// L3：当前进行中的代理请求数（入口 +1 / 结束 -1；状态页暴露）
    pub active_requests: Arc<std::sync::atomic::AtomicI64>,
    /// P0-1：Responses previous_response_id 桥接历史（进程内 LRU；仅转换路径
    /// 记录，原生 openai-responses 透传不记录——上游自身有状态）
    pub responses_history: Arc<crate::service::responses::history::ResponseHistoryStore>,
    pub quota_alerts: Arc<Mutex<HashSet<(i64, String)>>>,
}

impl AppState {
    pub async fn init(cfg: Arc<AppConfig>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pool = crate::store::init(&cfg).await?;
        // L22：连接池调优（cc-switch http_client.rs:216-260 同款思想）——
        // 空闲连接 60s 回收、同 host 最多 10 个空闲连接、TCP keepalive 60s。
        // L29：出站代理（GATEWAY_UPSTREAM_PROXY，http/https/socks5）
        let mut client_builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60));
        if let Some(proxy_url) = cfg.upstream_proxy.as_deref() {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => {
                    client_builder = client_builder.proxy(proxy);
                }
                Err(e) => {
                    return Err(format!("invalid GATEWAY_UPSTREAM_PROXY: {e}").into());
                }
            }
        }
        let client = client_builder.build()?;
        let ldap = crate::service::ldap::LdapSettings::from(&*cfg);
        // M4：熔断器参数取自 AppConfig（cfg 随后移入 state）
        let breaker_cfg = crate::service::breaker::BreakerConfig {
            failure_threshold: cfg.breaker_failure_threshold,
            open_secs: cfg.breaker_open_secs,
            failure_rate: cfg.breaker_failure_rate,
            min_requests: cfg.breaker_min_requests,
            window_secs: cfg.breaker_window_secs,
            success_threshold: cfg.breaker_success_threshold,
        };

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
            breaker: Arc::new(crate::service::breaker::Breaker::new_with(breaker_cfg)),
            limiter: Arc::new(RateLimiter::new()),
            ldap: Arc::new(RwLock::new(ldap)),
            extra_body_enabled: Arc::new(RwLock::new(true)),
            active_requests: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            api_endpoints: Arc::new(RwLock::new(crate::store::config::ApiEndpointSettings {
                responses_enabled: true,
                responses_visible: true,
                messages_enabled: true,
                messages_visible: true,
            })),
            usage: Arc::new(UsageCache::new()),
            responses_history: Arc::new(
                crate::service::responses::history::ResponseHistoryStore::new(),
            ),
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
        *self.extra_body_enabled.write() =
            crate::store::config::load_extra_body_enabled(&self.pool).await?;
        *self.api_endpoints.write() =
            crate::store::config::load_api_endpoint_settings(&self.pool).await?;
        self.reload_ldap().await?;
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
