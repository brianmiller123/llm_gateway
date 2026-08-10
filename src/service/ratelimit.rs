use std::collections::HashMap;
use parking_lot::Mutex;
use std::time::Instant;

use crate::error::AppError;
use crate::state::AppState;

/// 进程内令牌桶（单实例下精确；多实例时替换为 Redis 实现）
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
    rpm: f64,
    burst: f64,
}

const MAX_BUCKETS: usize = 10_000;

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 检查并消耗一个令牌；失败返回需等待的秒数
    pub fn check(&self, key: &str, rpm: f64, burst: f64) -> Result<(), f64> {
        let mut map = self.buckets.lock();
        // 简单防膨胀：满员时清空（代价：限流状态重置，可接受）
        if map.len() >= MAX_BUCKETS {
            map.clear();
        }
        let now = Instant::now();
        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            tokens: burst,
            last: now,
            rpm,
            burst,
        });
        if bucket.rpm != rpm || bucket.burst != burst {
            bucket.rpm = rpm;
            bucket.burst = burst;
        }
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.last = now;
        bucket.tokens = (bucket.tokens + elapsed * bucket.rpm / 60.0).min(bucket.burst);

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // 距下一个令牌的秒数
            Err((1.0 - bucket.tokens) / (bucket.rpm / 60.0))
        }
    }

    /// 清除某 key 的桶（登录成功后重置失败计数）
    pub fn reset(&self, key: &str) {
        self.buckets.lock().remove(key);
    }
}

/// 应用限流规则（api_key > user > global，命中即拒）
pub fn apply_rate_limits(
    st: &AppState,
    user_id: Option<i64>,
    key_id: Option<i64>,
) -> Result<(), AppError> {
    let rules = st.rules.read();
    for rule in rules.iter() {
        let bucket_key = match rule.scope.as_str() {
            "global" => Some("global".to_string()),
            "user" => user_id
                .filter(|uid| Some(*uid) == rule.scope_id)
                .map(|uid| format!("user:{uid}")),
            "api_key" => key_id
                .filter(|kid| Some(*kid) == rule.scope_id)
                .map(|kid| format!("key:{kid}")),
            other => {
                tracing::warn!("unknown rate limit scope: {other}");
                None
            }
        };
        if let Some(key) = bucket_key {
            if let Err(retry) = st
                .limiter
                .check(&key, rule.rpm as f64, rule.burst as f64)
            {
                return Err(AppError::RateLimited(retry));
            }
        }
    }
    Ok(())
}
