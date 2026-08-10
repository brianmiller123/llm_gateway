//! 控制台业务：LDAP/本地登录、JWT 会话签发与刷新、登出。
//!
//! 登录顺序：配置了 LDAP 时先尝试目录认证 —— 目录明确返回"用户不存在"才回退
//! 本地账号（break-glass 管理员）；密码错误直接拒绝，不回退，避免枚举。

use chrono::Utc;
use sha2::Digest;

use crate::error::AppError;
use crate::service::ldap::{self, LdapError};
use crate::service::session;
use crate::state::AppState;
use crate::store::{tokens, users};

/// 登录成功后的会话（access + refresh + 用户信息）
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub user: users::UserRow,
}

pub async fn login(st: &AppState, username: &str, password: &str) -> Result<Session, AppError> {
    if username.trim().is_empty() || password.is_empty() {
        return Err(AppError::BadRequest("username and password are required".into()));
    }

    let ldap_settings = st.ldap.read().clone();
    if ldap_settings.is_configured() {
        match ldap::authenticate(&ldap_settings, username, password).await {
            Ok(identity) => {
                let user = users::upsert_ldap_user(
                    &st.pool,
                    username,
                    identity.email.as_deref(),
                    identity.display_name.as_deref(),
                    &identity.dn,
                    identity.is_admin,
                )
                .await
                .map_err(AppError::internal)?;
                return issue_session(st, user).await;
            }
            Err(LdapError::UserNotFound) => {
                tracing::info!(username, "user not in LDAP, falling back to local account");
            }
            Err(LdapError::NotConfigured) => {}
            Err(LdapError::BadCredentials) => {
                return Err(AppError::Unauthorized("invalid username or password".into()));
            }
            Err(LdapError::Transport(e)) => {
                tracing::warn!(username, error = %e, "LDAP unavailable; falling back to local account");
            }
        }
    }

    // 本地账号（LDAP 用户无 password_hash，天然拒绝）
    let user = users::find_by_username(&st.pool, username)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::Unauthorized("invalid username or password".into()))?;
    let Some(hash) = &user.password_hash else {
        return Err(AppError::Unauthorized("invalid username or password".into()));
    };
    if !session::verify_password(password, hash) {
        return Err(AppError::Unauthorized("invalid username or password".into()));
    }
    issue_session(st, user).await
}

/// 会话签发：状态检查 → last_login → access + refresh
async fn issue_session(st: &AppState, user: users::UserRow) -> Result<Session, AppError> {
    if user.status != 1 {
        return Err(AppError::Forbidden("account disabled".into()));
    }
    users::touch_last_login(&st.pool, user.id)
        .await
        .map_err(AppError::internal)?;

    let access_token = session::issue_access(&st.cfg.jwt_secret, &user, st.cfg.access_token_ttl)?;
    let (refresh_token, refresh_hash) = session::new_refresh_token();
    let expires_at = Utc::now() + chrono::Duration::seconds(st.cfg.refresh_token_ttl);
    tokens::insert(&st.pool, user.id, &refresh_hash, expires_at)
        .await
        .map_err(AppError::internal)?;
    Ok(Session {
        access_token,
        refresh_token,
        user,
    })
}

/// 刷新：校验旧 token（未吊销/未过期）→ 用户状态/版本 → 旋转（吊销旧的，发新的）
pub async fn refresh(st: &AppState, refresh_token: &str) -> Result<Session, AppError> {
    let hash = hex::encode(sha2::Sha256::digest(refresh_token.as_bytes()));
    let row = tokens::find_active(&st.pool, &hash)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::Unauthorized("invalid or expired refresh token".into()))?;

    let user = users::find_by_id(&st.pool, row.user_id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::Unauthorized("user not found".into()))?;
    if user.status != 1 {
        tokens::revoke(&st.pool, row.id).await.map_err(AppError::internal)?;
        return Err(AppError::Forbidden("account disabled".into()));
    }

    // 旋转：旧 token 一次性
    tokens::revoke(&st.pool, row.id).await.map_err(AppError::internal)?;
    issue_session(st, user).await
}

pub async fn logout(st: &AppState, refresh_token: &str) -> Result<(), AppError> {
    let hash = hex::encode(sha2::Sha256::digest(refresh_token.as_bytes()));
    if let Some(row) = tokens::find_active(&st.pool, &hash)
        .await
        .map_err(AppError::internal)?
    {
        tokens::revoke(&st.pool, row.id).await.map_err(AppError::internal)?;
    }
    Ok(())
}
