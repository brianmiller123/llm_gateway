use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::state::AppState;
use crate::store::keys::find_by_prefix;

/// 常数时间字节比较（等长输入）
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 提取并校验 Bearer API Key，返回 (user_id, api_key_id)
///
/// auth_mode = None 时直接放行（本地开发）。
pub async fn authenticate(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<(Option<i64>, Option<i64>), AppError> {
    if st.cfg.auth_mode == crate::config::AuthMode::None {
        return Ok((None, None));
    }

    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Auth("missing Authorization header".into()))?;
    let key = auth
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::Auth("authorization scheme must be Bearer".into()))?;
    if key.len() < 16 {
        return Err(AppError::Auth("invalid api key".into()));
    }

    // 12 位前缀定位（避免全表哈希扫描）
    let prefix = &key[..12];
    let row = find_by_prefix(&st.pool, prefix)
        .await
        .map_err(AppError::internal)?;
    let Some(row) = row else {
        tracing::warn!(prefix, "api key not found");
        return Err(AppError::Auth("invalid api key".into()));
    };

    // 常数时间比较哈希（长度固定 64 hex，防时序侧信道；原 != 为非常数时间）
    let digest = hex::encode(Sha256::digest(key.as_bytes()));
    if !constant_time_eq(digest.as_bytes(), row.key_hash.as_bytes()) {
        tracing::warn!(prefix, "api key hash mismatch");
        return Err(AppError::Auth("invalid api key".into()));
    }
    if let Some(exp) = row.expires_at {
        if exp <= chrono::Utc::now() {
            return Err(AppError::Auth("api key expired".into()));
        }
    }
    if row.user_status != 1 {
        return Err(AppError::Auth("account disabled".into()));
    }
    Ok((Some(row.user_id), Some(row.id)))
}
