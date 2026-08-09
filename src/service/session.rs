use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, SaltString};
use argon2::{Argon2, PasswordVerifier};
use chrono::Utc;
use sha2::Digest;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};

use crate::error::AppError;
use crate::store::users::UserRow;

/// JWT 声明（HS256）；sub 为字符串格式的用户 ID（JWT 标准）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub is_admin: bool,
    /// 签发时的 token_version，强制下线后旧 token 失效
    pub tv: i32,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
}

/// argon2id 密码哈希（本地用户）
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hashing cannot fail")
        .to_string()
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn issue_access(secret: &[u8; 32], user: &UserRow, ttl_secs: i64) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub: user.id.to_string(),
        username: user.username.clone(),
        is_admin: user.is_admin,
        tv: user.token_version,
        jti: uuid::Uuid::new_v4().to_string(),
        iat: now,
        exp: now + ttl_secs,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .map_err(AppError::internal)
}

/// 校验签名 + 有效期；用户状态/版本校验由调用方基于返回的声明完成
pub fn verify_access(secret: &[u8; 32], token: &str) -> Result<Claims, AppError> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_required_spec_claims(&["exp", "sub"]);
    decode::<Claims>(token, &DecodingKey::from_secret(secret), &validation)
        .map(|data| data.claims)
        .map_err(|_| AppError::Unauthorized("invalid or expired access token".into()))
}

/// 不透明 refresh token（64 hex），仅存哈希
pub fn new_refresh_token() -> (String, String) {
    let mut buf = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    let token = hex::encode(buf);
    let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    (token, hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_roundtrip() {
        let secret = [7u8; 32];
        let user = crate::store::users::UserRow {
            id: 1,
            username: "alice".into(),
            email: None,
            display_name: None,
            ldap_dn: None,
            source: "local".into(),
            password_hash: None,
            is_admin: false,
            status: 1,
            token_version: 0,
            last_login_at: None,
            created_at: chrono::Utc::now(),
        };
        let token = issue_access(&secret, &user, 900).unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_required_spec_claims(&["exp", "sub"]);
        let raw = jsonwebtoken::decode::<Claims>(&token, &DecodingKey::from_secret(&secret), &validation);
        if let Err(e) = &raw {
            println!("DECODE ERR: kind={:?}", e.kind());
        }
        let claims = verify_access(&secret, &token).unwrap();
        assert_eq!(claims.sub, "1");
        assert_eq!(claims.username, "alice");
    }

    #[test]
    fn token_mismatch_secret_fails() {
        let secret = [7u8; 32];
        let user = crate::store::users::UserRow {
            id: 1,
            username: "alice".into(),
            email: None,
            display_name: None,
            ldap_dn: None,
            source: "local".into(),
            password_hash: None,
            is_admin: false,
            status: 1,
            token_version: 0,
            last_login_at: None,
            created_at: chrono::Utc::now(),
        };
        let token = issue_access(&secret, &user, 900).unwrap();
        let wrong = [8u8; 32];
        assert!(verify_access(&wrong, &token).is_err());
    }
}
