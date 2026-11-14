use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

use crate::error::AppError;
use crate::state::AppState;
use crate::store::keys::{ApiKeyRow, find_all_by_prefix};

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

/// 在同前缀候选中定位哈希匹配的 Key：逐候选常数时间比对，命中即返回。
/// 前缀碰撞时多行同前缀，只查一行会把另一把合法 Key 永久 401（旧 fetch_optional 行为）
fn match_candidate<'a>(rows: &'a [ApiKeyRow], key: &str) -> Option<&'a ApiKeyRow> {
    // 长度固定 64 hex，防时序侧信道；逐候选比对不跨候选共享状态
    let digest = hex::encode(Sha256::digest(key.as_bytes()));
    rows.iter()
        .find(|row| constant_time_eq(digest.as_bytes(), row.key_hash.as_bytes()))
}

/// F11：缓存条目是否在 TTL 内（now 注入，sleep-free 单测）
fn cache_fresh(at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(at) < ttl
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

    // M7：Anthropic 客户端（Claude Code / Claude SDK）默认发 `x-api-key` 而非
    // Bearer；Authorization 缺失时回退读取 x-api-key（同一 Key 库校验）
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "));
    let key = bearer
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|k| !k.is_empty())
        })
        .ok_or_else(|| {
            AppError::Auth("missing Authorization: Bearer header or x-api-key header".into())
        })?;
    if key.len() < 16 {
        return Err(AppError::Auth("invalid api key".into()));
    }

    // 12 位前缀定位（避免全表哈希扫描）；前缀仅 12 hex 会碰撞 → 取全部候选逐行比对。
    // F11：候选组走进程内前缀缓存（TTL = cfg.auth_cache_secs，0 = 关闭直查
    // DB；每次 reload tick 全量清空）——热路径免逐请求 DB SELECT。吊销/禁用
    // 最迟 TTL + 一个 reload tick 内生效，对 5s 量级延迟换取主链路免查询是
    // 接受的折衷；sha256 行匹配仍在逐候选常数时间比对，缓存只省定位查询。
    let prefix = &key[..12];
    let ttl = Duration::from_secs(st.cfg.auth_cache_secs);
    let rows = if ttl.is_zero() {
        find_all_by_prefix(&st.pool, prefix)
            .await
            .map_err(AppError::internal)?
    } else {
        let now = Instant::now();
        let cached = st
            .auth_cache
            .lock()
            .get(prefix)
            .filter(|(at, _)| cache_fresh(*at, now, ttl))
            .map(|(_, rows)| rows.clone());
        match cached {
            Some(rows) => rows,
            None => {
                let rows = find_all_by_prefix(&st.pool, prefix)
                    .await
                    .map_err(AppError::internal)?;
                st.auth_cache
                    .lock()
                    .insert(prefix.to_string(), (now, rows.clone()));
                rows
            }
        }
    };
    if rows.is_empty() {
        tracing::warn!(prefix, "api key not found");
        return Err(AppError::Auth("invalid api key".into()));
    }
    let Some(row) = match_candidate(&rows, key) else {
        tracing::warn!(prefix, "api key hash mismatch");
        return Err(AppError::Auth("invalid api key".into()));
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, key_hash: &str) -> ApiKeyRow {
        ApiKeyRow {
            id,
            user_id: 7,
            key_hash: key_hash.to_string(),
            expires_at: None,
            user_status: 1,
        }
    }

    #[test]
    fn prefix_collision_reaches_second_candidate() {
        // 两把 Key 同 12 位前缀：第一行哈希不匹配，必须比到第二行
        // （旧 fetch_optional 只取任意一行，另一把合法 Key 会永久 401）
        let key = "sk-gateway-0123456789abcdef";
        let rows = [
            row(1, &hex::encode(sha2::Sha256::digest(b"another-key"))),
            row(2, &hex::encode(sha2::Sha256::digest(key.as_bytes()))),
        ];
        let matched = match_candidate(&rows, key).expect("second row should match");
        assert_eq!(matched.id, 2);
    }

    #[test]
    fn no_candidate_matches_returns_none() {
        let rows = [row(1, "deadbeef")];
        assert!(match_candidate(&rows, "sk-gateway-0123456789abcdef").is_none());
    }

    // ---------- F11：前缀缓存（TTL，0 = 关闭） ----------

    use std::time::{Duration, Instant};

    /// 构造鉴权测试态：断连池（触 DB 即快速失败——据此证明缓存命中/关闭
    /// 时是否真的绕开了 DB）+ 注入的 auth_mode/TTL
    fn auth_fixture(auth_cache_secs: u64) -> AppState {
        let mut st = AppState::test_fixture();
        let mut cfg = (*st.cfg).clone();
        cfg.auth_mode = crate::config::AuthMode::ApiKey;
        cfg.auth_cache_secs = auth_cache_secs;
        st.cfg = std::sync::Arc::new(cfg);
        st
    }

    fn bearer(key: &str) -> axum::http::HeaderMap {
        axum::http::HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {key}").parse().unwrap(),
        )])
    }

    fn cached_rows(key: &str) -> Vec<ApiKeyRow> {
        vec![row(
            42,
            &hex::encode(sha2::Sha256::digest(key.as_bytes())),
        )]
    }

    #[test]
    fn cache_fresh_ttl_boundary() {
        let at = Instant::now();
        let ttl = Duration::from_secs(5);
        assert!(cache_fresh(at, at + Duration::from_secs(1), ttl), "TTL 内命中");
        assert!(
            cache_fresh(at, at + ttl - Duration::from_millis(1), ttl),
            "TTL 边界前一瞬仍命中"
        );
        assert!(!cache_fresh(at, at + ttl, ttl), "越过 TTL 未命中");
    }

    /// 缓存命中：断连池下仍成功返回——证明热路径未触 DB（候选组来自缓存）
    #[tokio::test]
    async fn authenticate_serves_from_cache_within_ttl() {
        let key = "sk-gateway-cache-hit-key-0001";
        let st = auth_fixture(60);
        st.auth_cache
            .lock()
            .insert(key[..12].to_string(), (Instant::now(), cached_rows(key)));

        let (user_id, key_id) = authenticate(&st, &bearer(key))
            .await
            .expect("缓存命中必须免 DB 查询直接通过");
        assert_eq!(user_id, Some(7));
        assert_eq!(key_id, Some(42));
    }

    /// TTL 过期：回源 DB——断连池下必然失败，证明过期条目不再被信任
    #[tokio::test]
    async fn expired_cache_entry_falls_back_to_db() {
        let key = "sk-gateway-cache-exp-key-002";
        let st = auth_fixture(60);
        st.auth_cache.lock().insert(
            key[..12].to_string(),
            (Instant::now() - Duration::from_secs(61), cached_rows(key)),
        );

        assert!(
            authenticate(&st, &bearer(key)).await.is_err(),
            "过期缓存必须回源 DB（断连池下失败即证回源）"
        );
    }

    /// 关闭模式（auth_cache_secs = 0）：即使缓存有匹配条目也直查 DB——
    /// 断连池下失败即证缓存被绕开
    #[tokio::test]
    async fn disabled_cache_mode_goes_straight_to_db() {
        let key = "sk-gateway-cache-off-key-03";
        let st = auth_fixture(0);
        st.auth_cache
            .lock()
            .insert(key[..12].to_string(), (Instant::now(), cached_rows(key)));

        assert!(
            authenticate(&st, &bearer(key)).await.is_err(),
            "0 = 关闭缓存，必须直查 DB"
        );
    }
}
