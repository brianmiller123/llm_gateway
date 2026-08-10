//! LDAP 适配器：配置驱动，兼容 AD / OpenLDAP。
//!
//! 认证流程：服务账号 bind（可选，direct-bind 模式可跳过）→ 按 `LDAP_USER_FILTER`
//! 搜索用户 DN → 用户凭据 bind 验证 → 读取属性（mail/displayName/cn/memberOf）。
//! 管理员判定：`LDAP_ADMIN_GROUPS`（组 CN 或完整 DN 逗号分隔）匹配 memberOf；
//! 目录未启用 memberOf overlay 时，对 groupOfNames 做补充组搜索（按 CN）。

use std::time::Duration;

use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use tokio::time::timeout;

use crate::config::AppConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LdapError {
    /// 目录中不存在该用户（业务层可回退本地账号）
    UserNotFound,
    /// 用户凭据验证失败
    BadCredentials,
    /// 配置缺失（未配置 LDAP_URL）
    NotConfigured,
    /// 连接/协议/目录异常
    Transport(String),
}

impl From<ldap3::LdapError> for LdapError {
    fn from(e: ldap3::LdapError) -> Self {
        // 用户凭据 bind 失败 → BadCredentials（区别于连接/协议错误）；49 = invalidCredentials
        if let ldap3::LdapError::LdapResult { result } = &e {
            if result.rc == 49 {
                return Self::BadCredentials;
            }
        }
        Self::Transport(e.to_string())
    }
}

impl std::fmt::Display for LdapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserNotFound => write!(f, "user not found in LDAP"),
            Self::BadCredentials => write!(f, "LDAP bind failed"),
            Self::NotConfigured => write!(f, "LDAP not configured"),
            Self::Transport(e) => write!(f, "LDAP transport error: {e}"),
        }
    }
}

impl std::error::Error for LdapError {}

#[derive(Debug, Clone)]
pub struct LdapIdentity {
    pub dn: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub is_admin: bool,
}

/// 可配置的 LDAP 参数（从 AppConfig 提取，便于测试注入）
#[derive(Debug, Clone)]
pub struct LdapSettings {
    pub url: String,
    pub starttls: bool,
    pub bind_dn: Option<String>,
    pub bind_password: Option<String>,
    pub base_dn: String,
    pub user_filter: String,
    pub admin_groups: Vec<String>,
}

impl From<&AppConfig> for LdapSettings {
    fn from(cfg: &AppConfig) -> Self {
        Self {
            url: cfg.ldap_url.clone().unwrap_or_default(),
            starttls: cfg.ldap_starttls,
            bind_dn: cfg.ldap_bind_dn.clone(),
            bind_password: cfg.ldap_bind_password.clone(),
            base_dn: cfg.ldap_base_dn.clone().unwrap_or_default(),
            user_filter: cfg.ldap_user_filter.clone(),
            admin_groups: cfg.ldap_admin_groups.clone(),
        }
    }
}

impl LdapSettings {
    pub fn is_configured(&self) -> bool {
        !self.url.is_empty()
    }
}

/// 转义 LDAP 过滤器特殊字符（防过滤器注入；LDAP 转义码为小写十六进制）
fn ldap_escape_filter(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\5c"),
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\0' => out.push_str("\\00"),
            _ => out.push(c),
        }
    }
    out
}

/// 完整登录流程（服务账号搜索 + 用户 bind），LDAP 连接带 10s 超时
pub async fn authenticate(
    settings: &LdapSettings,
    username: &str,
    password: &str,
) -> Result<LdapIdentity, LdapError> {
    if !settings.is_configured() {
        return Err(LdapError::NotConfigured);
    }
    if password.is_empty() {
        return Err(LdapError::BadCredentials);
    }

    let work = async {
        let (conn, mut ldap) =
            LdapConnAsync::with_settings(LdapConnSettings::new().set_starttls(settings.starttls), &settings.url).await?;
        ldap3::drive!(conn);

        // 服务账号 bind（可选；direct-bind 模式依赖目录匿名读搜索）
        if let (Some(dn), Some(pw)) = (&settings.bind_dn, &settings.bind_password) {
            ldap.simple_bind(dn, pw).await?;
        }

        let filter = settings
            .user_filter
            .replace("{0}", &ldap_escape_filter(username));
        let attrs = ["dn", "cn", "mail", "displayName", "givenName", "memberOf"];
        let rs = ldap
            .search(&settings.base_dn, Scope::Subtree, &filter, attrs)
            .await?;
        let Some(entry) = rs.0.into_iter().next() else {
            return Err(LdapError::UserNotFound);
        };
        let entry = SearchEntry::construct(entry);
        let dn = entry.dn.clone();

        // 用户凭据 bind（simple_bind 不检查服务器响应码，必须显式校验 rc）
        let bind_res = ldap.simple_bind(&dn, password).await?;
        if bind_res.rc != 0 {
            return Err(LdapError::BadCredentials);
        }
        ldap.unbind().await.ok();

        let member_of = entry.attrs.get("memberOf").cloned().unwrap_or_default();
        let is_admin = match_admin_groups(settings, &member_of, &dn).await?;
        Ok(LdapIdentity {
            dn,
            email: entry.attrs.get("mail").and_then(|v| v.first()).cloned(),
            display_name: entry
                .attrs
                .get("displayName")
                .or_else(|| entry.attrs.get("cn"))
                .and_then(|v| v.first())
                .cloned(),
            is_admin,
        })
    };

    match timeout(Duration::from_secs(10), work).await {
        Ok(r) => r,
        Err(_) => Err(LdapError::Transport("LDAP operation timed out".into())),
    }
}

/// 连接测试：建连 +（可选）服务账号 bind + base_dn 根搜索，
/// 返回 Ok(提示信息) / Err(人类可读失败原因)。10s 超时。
pub async fn test_connection(settings: &LdapSettings) -> Result<String, String> {
    if !settings.is_configured() {
        return Err("LDAP URL 为空".into());
    }
    let work = async {
        let (conn, mut ldap) =
            LdapConnAsync::with_settings(LdapConnSettings::new().set_starttls(settings.starttls), &settings.url)
                .await
                .map_err(|e| format!("连接失败: {e}"))?;
        ldap3::drive!(conn);

        if let (Some(dn), Some(pw)) = (&settings.bind_dn, &settings.bind_password) {
            let bind_res = ldap
                .simple_bind(dn, pw)
                .await
                .map_err(|e| format!("服务账号 bind 失败: {e}"))?;
            if bind_res.rc != 0 {
                return Err(format!(
                    "服务账号 bind 失败: {} (code {})",
                    bind_res.text, bind_res.rc
                ));
            }
        }

        // base_dn 根搜索验证目录可读
        let rs = ldap
            .search(&settings.base_dn, Scope::Base, "(objectClass=*)", ["dn"])
            .await
            .map_err(|e| format!("搜索失败: {e}"))?;
        ldap.unbind().await.ok();
        Ok(format!("连接成功，base DN 可访问（{} 条结果）", rs.0.len()))
    };
    match timeout(Duration::from_secs(10), work).await {
        Ok(r) => r,
        Err(_) => Err("LDAP 操作超时（10s）".into()),
    }
}

/// 管理员判定：memberOf 精确 DN / CN 后缀匹配；无 memberOf 时补充组搜索
async fn match_admin_groups(
    settings: &LdapSettings,
    member_of: &[String],
    user_dn: &str,
) -> Result<bool, LdapError> {
    if settings.admin_groups.is_empty() {
        return Ok(false);
    }
    let cn_suffixes: Vec<String> = settings
        .admin_groups
        .iter()
        .map(|g| {
            g.split(',')
                .map(str::trim)
                .find(|rdn| rdn.starts_with("cn="))
                .map(|rdn| rdn.trim_start_matches("cn=").to_string())
                .unwrap_or_else(|| g.clone())
        })
        .collect();

    for group in &settings.admin_groups {
        if member_of.iter().any(|m| m == group) {
            return Ok(true);
        }
    }
    for m in member_of {
        if cn_suffixes.iter().any(|cn| m.ends_with(&format!("cn={cn}"))) {
            return Ok(true);
        }
    }

    // memberOf 未命中或目录未返回该属性：对 groupOfNames 做补充搜索
    // （admin_groups 可能是完整 DN，只取其 CN 后缀做 (cn=...) 匹配）
    let groups_filter = cn_suffixes
        .iter()
        .map(|cn| format!("(cn={cn})"))
        .collect::<Vec<_>>()
        .join("");
    let filter = format!(
        "(&(objectClass=groupOfNames)(|{groups_filter})(member={user_dn}))"
    );
    tracing::debug!(filter, user_dn, "LDAP admin group check");
    let search = async {
        let (conn, mut ldap) =
            LdapConnAsync::with_settings(LdapConnSettings::new().set_starttls(settings.starttls), &settings.url).await?;
        ldap3::drive!(conn);
        if let (Some(dn), Some(pw)) = (&settings.bind_dn, &settings.bind_password) {
            let r = ldap.simple_bind(dn, pw).await?;
            tracing::debug!(rc = r.rc, "service bind rc");
        }
        let rs = ldap
            .search(&settings.base_dn, Scope::Subtree, &filter, ["dn"])
            .await?;
        tracing::debug!(found = rs.0.len(), "admin group search result");
        Ok::<_, LdapError>(!rs.0.is_empty())
    };
    match timeout(Duration::from_secs(10), search).await {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(LdapError::Transport("LDAP group search timed out".into())),
    }
}
