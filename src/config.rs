use std::net::SocketAddr;
use std::path::PathBuf;

use sha2::Digest;

/// 应用配置：全部来自环境变量（dotenvy 加载 .env）
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub database_url: String,
    pub https_addr: SocketAddr,
    pub http_addr: SocketAddr,
    /// HTTP→HTTPS 重定向的 Location 端口；缺省取 https_addr 的端口（容器映射端口不同时需覆盖）
    pub redirect_https_port: Option<u16>,
    /// HTTP 端口行为：true = 301 重定向到 HTTPS；false = 直接服务完整应用（明文）
    pub http_redirect_enabled: bool,
    pub tls_cert: String,
    pub tls_key: String,
    /// AES-256-GCM 主密钥（64 位 hex）
    pub master_key: [u8; 32],
    /// none = 跳过 API Key 鉴权（本地开发）；api_key = 强制校验
    pub auth_mode: AuthMode,
    /// 配置热加载周期（秒）
    pub reload_interval_secs: u64,
    /// 控制台前端静态资源目录（Vue dist；不存在时跳过托管）
    pub web_dir: PathBuf,
    /// JWT 签名密钥（缺省由主密钥派生）
    pub jwt_secret: [u8; 32],
    /// access token 有效期（秒）
    pub access_token_ttl: i64,
    /// refresh token 有效期（秒）
    pub refresh_token_ttl: i64,
    // LDAP 登录（配置驱动 AD/OpenLDAP 兼容）
    pub ldap_url: Option<String>,
    pub ldap_starttls: bool,
    pub ldap_bind_dn: Option<String>,
    pub ldap_bind_password: Option<String>,
    pub ldap_base_dn: Option<String>,
    /// 用户搜索过滤器，{0} 为用户登录名占位
    pub ldap_user_filter: String,
    /// 管理员组 DN 列表（memberOf 匹配，逗号分隔）
    pub ldap_admin_groups: Vec<String>,
    // 种子本地管理员（break-glass）
    pub seed_admin_username: Option<String>,
    pub seed_admin_password: Option<String>,
    // 种子上游（providers 为空时生效）
    pub seed_provider_name: Option<String>,
    pub seed_provider_base_url: Option<String>,
    pub seed_provider_api_key: Option<String>,
    pub seed_model_pattern: Option<String>,
    /// 限流恢复豁免：主体（用户×Key）静默 ≥ 该时长后，恢复执行的首请求
    /// 若被限流规则拒绝则豁免放行（每个空闲间隙至多一次）。0 = 关闭。
    pub rate_idle_exempt_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    None,
    ApiKey,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, String> {
        fn env(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_string())
        }
        fn env_bool(key: &str, default: bool) -> Result<bool, String> {
            match std::env::var(key) {
                Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => Ok(true),
                    "false" | "0" | "no" | "off" | "" => Ok(false),
                    _ => Err(format!("invalid {key}: {v} (expect true/false/1/0)")),
                },
                Err(_) => Ok(default),
            }
        }

        let master_key_hex = std::env::var("GATEWAY_MASTER_KEY")
            .map_err(|_| "GATEWAY_MASTER_KEY is required (64 hex chars)".to_string())?;
        if master_key_hex.len() != 64 {
            return Err("GATEWAY_MASTER_KEY must be exactly 64 hex chars".into());
        }
        let master_key_bytes = hex::decode(&master_key_hex)
            .map_err(|e| format!("GATEWAY_MASTER_KEY is not valid hex: {e}"))?;
        let master_key: [u8; 32] = master_key_bytes
            .try_into()
            .map_err(|_| "GATEWAY_MASTER_KEY must decode to 32 bytes".to_string())?;

        let auth_mode = match env("GATEWAY_AUTH_MODE", "api_key").as_str() {
            "none" => AuthMode::None,
            "api_key" => AuthMode::ApiKey,
            other => return Err(format!("invalid GATEWAY_AUTH_MODE: {other}")),
        };

        let reload_interval_secs: u64 = env("GATEWAY_RELOAD_INTERVAL", "30")
            .parse()
            .map_err(|e| format!("invalid GATEWAY_RELOAD_INTERVAL: {e}"))?;
        if reload_interval_secs == 0 {
            // tokio::time::interval 对 0 周期会 panic
            return Err("GATEWAY_RELOAD_INTERVAL must be >= 1 second".into());
        }
        let access_token_ttl: i64 = env("GATEWAY_ACCESS_TOKEN_TTL", "900")
            .parse()
            .map_err(|e| format!("invalid GATEWAY_ACCESS_TOKEN_TTL: {e}"))?;
        let refresh_token_ttl: i64 = env("GATEWAY_REFRESH_TOKEN_TTL", "2592000")
            .parse()
            .map_err(|e| format!("invalid GATEWAY_REFRESH_TOKEN_TTL: {e}"))?;
        if access_token_ttl <= 0 || refresh_token_ttl <= 0 {
            return Err("GATEWAY_ACCESS/REFRESH_TOKEN_TTL must be > 0 seconds".into());
        }

        let rate_idle_exempt_secs: u64 = env("GATEWAY_RATE_IDLE_EXEMPT_SECS", "60")
            .parse()
            .map_err(|e| format!("invalid GATEWAY_RATE_IDLE_EXEMPT_SECS: {e}"))?;

        Ok(Self {
            database_url: env(
                "GATEWAY_DATABASE_URL",
                "postgres://gateway:gateway@localhost:5432/llm_gateway",
            ),
            https_addr: env("GATEWAY_HTTPS_ADDR", "0.0.0.0:443")
                .parse()
                .map_err(|e| format!("invalid GATEWAY_HTTPS_ADDR: {e}"))?,
            http_addr: env("GATEWAY_HTTP_ADDR", "0.0.0.0:80")
                .parse()
                .map_err(|e| format!("invalid GATEWAY_HTTP_ADDR: {e}"))?,
            redirect_https_port: std::env::var("GATEWAY_REDIRECT_HTTPS_PORT")
                .ok()
                .map(|v| {
                    v.parse()
                        .map_err(|e| format!("invalid GATEWAY_REDIRECT_HTTPS_PORT: {e}"))
                })
                .transpose()?,
            http_redirect_enabled: env_bool("GATEWAY_HTTP_REDIRECT", true)?,
            tls_cert: env("GATEWAY_TLS_CERT", "certs/cert.pem"),
            tls_key: env("GATEWAY_TLS_KEY", "certs/key.pem"),
            master_key,
            auth_mode,
            reload_interval_secs,
            web_dir: PathBuf::from(env("GATEWAY_WEB_DIR", "web/dist")),
            jwt_secret: {
                let hex = env("GATEWAY_JWT_SECRET", "");
                if hex.is_empty() {
                    // 缺省由主密钥确定性派生
                    sha2::Sha256::digest(master_key).into()
                } else {
                    let bytes = hex::decode(&hex)
                        .map_err(|e| format!("GATEWAY_JWT_SECRET is not valid hex: {e}"))?;
                    let mut out = [0u8; 32];
                    if bytes.len() != 32 {
                        return Err("GATEWAY_JWT_SECRET must decode to 32 bytes".into());
                    }
                    out.copy_from_slice(&bytes);
                    out
                }
            },
            access_token_ttl,
            refresh_token_ttl,
            ldap_url: std::env::var("LDAP_URL").ok(),
            ldap_starttls: env("LDAP_STARTTLS", "false") == "true",
            ldap_bind_dn: std::env::var("LDAP_BIND_DN").ok(),
            ldap_bind_password: std::env::var("LDAP_BIND_PASSWORD").ok(),
            ldap_base_dn: std::env::var("LDAP_BASE_DN").ok(),
            ldap_user_filter: env(
                "LDAP_USER_FILTER",
                "(&(objectClass=person)(sAMAccountName={0}))",
            ),
            ldap_admin_groups: std::env::var("LDAP_ADMIN_GROUPS")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            seed_admin_username: std::env::var("SEED_ADMIN_USERNAME").ok(),
            seed_admin_password: std::env::var("SEED_ADMIN_PASSWORD").ok(),
            seed_provider_name: std::env::var("SEED_PROVIDER_NAME").ok(),
            seed_provider_base_url: std::env::var("SEED_PROVIDER_BASE_URL").ok(),
            seed_provider_api_key: std::env::var("SEED_PROVIDER_API_KEY").ok(),
            seed_model_pattern: std::env::var("SEED_MODEL_PATTERN").ok(),
            rate_idle_exempt_secs,
        })
    }
}
