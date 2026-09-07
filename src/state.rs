use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use sqlx::PgPool;

use crate::config::AppConfig;
use crate::service::ratelimit::RateLimiter;
use crate::service::usage::UsageCache;
use crate::store::access::{UserAccessRule, load_user_access};
use crate::store::rules::{ModelPrice, RateRule, UserQuota, load_prices, load_quotas, load_rules};
use crate::store::upstream::{ModelRoute, Provider, load_providers, load_routes};

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
    /// Coding Plan 运行时：user_id → 候选 Plan 列表（双通道全集，reload 刷新；
    /// 请求期用 store::plans::resolve_plan 按「当前生效时段 + priority 择优」解析）
    pub plans: Arc<RwLock<HashMap<i64, Vec<crate::store::plans::PlanRuntime>>>>,
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
    /// 全局自定义 Header（upstream → 上游请求；response → 客户端响应）
    pub custom_headers: Arc<RwLock<crate::store::config::HeaderSettings>>,
    pub active_requests: Arc<std::sync::atomic::AtomicI64>,
    /// P0-1：Responses previous_response_id 桥接历史（进程内 LRU；仅转换路径
    /// 记录，原生 openai-responses 透传不记录——上游自身有状态）
    pub responses_history: Arc<crate::service::responses::history::ResponseHistoryStore>,
    pub quota_alerts: Arc<Mutex<HashSet<(i64, String)>>>,
    /// 状态页主动健康探测缓存：provider_id → 最近一次探测结果（/1/status，
    /// 非结论性时回退 /v1/models；TTL 30s 内复用；api::status 刷新，见 service::health::snapshot）
    pub provider_health: Arc<tokio::sync::Mutex<HashMap<i64, crate::service::health::ProbeResult>>>,
    /// Plan 阈值告警进程内去重：(user_id, plan_id, period_key, level)
    pub plan_alerts_seen: Arc<Mutex<HashSet<(i64, i64, String, i16)>>>,
}

impl AppState {
    pub async fn init(
        cfg: Arc<AppConfig>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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
            plans: Arc::new(RwLock::new(HashMap::new())),
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
            custom_headers: Arc::new(RwLock::new(crate::store::config::HeaderSettings::default())),
            usage: Arc::new(UsageCache::new()),
            responses_history: Arc::new(
                crate::service::responses::history::ResponseHistoryStore::new(),
            ),
            quota_alerts: Arc::new(Mutex::new(HashSet::new())),
            plan_alerts_seen: Arc::new(Mutex::new(HashSet::new())),
            provider_health: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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

    /// 全量热加载：供应商 / 路由 / 限流规则 / 配额 / 单价 / 访问授权 / LDAP。
    ///
    /// 分节独立应用：任一节加载失败只影响该节（沿用旧值并 error 留痕），不再让
    /// 整体 reload 中途返回、其余节全部冻结——历史上 schema 演进后旧实例的
    /// plan 节 SQL 永久失败，全有或全无的 reload 让成员变更传播整体停摆。
    /// 启动期（init）返回首个错误 fail-fast；周期刷新沿用旧值继续运行。
    pub async fn reload(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut first_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        macro_rules! section {
            ($name:literal, $expr:expr) => {
                if let Err(e) = $expr.await {
                    tracing::error!(section = $name, error = %e, "config reload section failed; keeping previous values");
                    first_err.get_or_insert(e);
                }
            };
        }
        section!("providers", async {
            let v = load_providers(&self.pool).await?;
            *self.providers.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("routes", async {
            let v = load_routes(&self.pool).await?;
            *self.routes.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("rules", async {
            let v = load_rules(&self.pool).await?;
            *self.rules.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("quotas", async {
            let v: HashMap<_, _> = load_quotas(&self.pool)
                .await?
                .into_iter()
                .map(|q| (q.user_id, q))
                .collect();
            *self.quotas.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("prices", async {
            let v: HashMap<_, _> = load_prices(&self.pool)
                .await?
                .into_iter()
                .map(|p| (p.model.clone(), p))
                .collect();
            *self.prices.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("user_access", async {
            let mut v: HashMap<i64, Vec<UserAccessRule>> = HashMap::new();
            for r in load_user_access(&self.pool).await? {
                v.entry(r.user_id).or_default().push(r);
            }
            *self.user_access.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("admin_ids", async {
            let v: HashSet<i64> = crate::store::users::load_admin_ids(&self.pool).await?;
            *self.admin_ids.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("plans", async {
            let v = crate::store::plans::load_plan_runtimes(&self.pool).await?;
            *self.plans.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("extra_body_enabled", async {
            let v = crate::store::config::load_extra_body_enabled(&self.pool).await?;
            *self.extra_body_enabled.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("api_endpoints", async {
            let v = crate::store::config::load_api_endpoint_settings(&self.pool).await?;
            *self.api_endpoints.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        section!("ldap", self.reload_ldap());
        section!("custom_headers", async {
            let v = crate::store::config::load_header_settings(&self.pool).await?;
            *self.custom_headers.write() = v;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
