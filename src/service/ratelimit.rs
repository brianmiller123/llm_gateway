use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::error::AppError;
use crate::state::AppState;

/// 单调时间戳。限流回填必须按"真实流逝时间"计费：
/// Linux 取 CLOCK_BOOTTIME（含系统挂起/睡眠期间的时间），
/// 因为 std Instant 的 CLOCK_MONOTONIC 在机器睡眠时不前进——
/// agent 暂停等待用户确认期间机器休眠时，恢复后令牌会少回填。
#[cfg(target_os = "linux")]
pub fn now_ts() -> Ts {
    use std::sync::LazyLock;
    use std::time::Instant;
    // BOOTTIME 读取失败（极老内核）时退化为进程内单调时钟
    static FALLBACK_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    unsafe {
        let mut tp = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut tp) == 0 {
            return Duration::new(tp.tv_sec.max(0) as u64, tp.tv_nsec.max(0) as u32);
        }
    }
    FALLBACK_ORIGIN.elapsed()
}

/// 非 Linux 平台兜底：进程内单调时钟（不含挂起时间，仅保持编译可用）
#[cfg(not(target_os = "linux"))]
pub fn now_ts() -> Ts {
    use std::sync::LazyLock;
    use std::time::Instant;
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    ORIGIN.elapsed()
}

pub type Ts = Duration;

/// 进程内令牌桶（单实例下精确；多实例时替换为 Redis 实现）
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
    /// 请求主体（u:{user_id}|k:{key_id}）最近一次活动时间（任意结局的请求都计）。
    /// 用于"长时间空闲后恢复"的豁免判定：只有真实静默 ≥ 阈值才算"恢复"。
    identities: Mutex<HashMap<String, Identity>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Ts,
    rpm: f64,
    burst: f64,
}

struct Identity {
    last_active: Ts,
}

const MAX_BUCKETS: usize = 10_000;
/// 身份表防膨胀上限
const MAX_IDENTITIES: usize = 10_000;
/// 桶/身份表防膨胀：满员时清掉 1 小时无活动的条目
const IDLE_EVICT_AFTER: Duration = Duration::from_secs(3600);

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 检查并消耗一个令牌；失败返回需等待的秒数（rpm<=0 且无余量时为无穷）。
    /// 登录等无主体场景使用；不参与恢复豁免。
    pub fn check(&self, key: &str, rpm: f64, burst: f64) -> Result<(), f64> {
        self.check_rules_at(
            now_ts(),
            &[(key.to_string(), rpm, burst)],
            None,
            Duration::ZERO,
        )
    }

    /// 多规则原子检查（api_key > user > global，命中即拒）：
    /// - 两阶段判定：先对全部匹配桶只读回填，全部足额才统一扣减落库；
    ///   任一桶不足即整体拒绝（拒绝路径零扣减——旧实现按序消耗、遇拒绝即
    ///   break，被拒请求会白烧掉前面桶的令牌）。返回值为各不足桶中的最大
    ///   等待秒数（整体可通过的最早时刻；rpm<=0 且无余量时为无穷）。
    /// - identity 给定时记录本次活动，并在"该主体静默 ≥ idle_exempt"时
    ///   豁免本次拒绝（恢复后的首个请求放行，不扣空桶，也不重置任何桶）。
    ///   每个空闲间隙至多豁免一次；持续高频请求永远攒不出间隙，不受影响。
    pub fn check_rules_at(
        &self,
        now: Ts,
        rules: &[(String, f64, f64)],
        identity: Option<&str>,
        idle_exempt: Duration,
    ) -> Result<(), f64> {
        let mut buckets = self.buckets.lock();
        // 防膨胀：满员时先驱逐超过 1 小时未活动的桶；仍满才整体清空
        // （整体清空会重置全部限流状态，攻击者可用随机 key 触发，故仅在驱逐无效时兜底）
        if buckets.len() >= MAX_BUCKETS {
            buckets.retain(|_, b| now.saturating_sub(b.last) < IDLE_EVICT_AFTER);
            if buckets.len() >= MAX_BUCKETS {
                buckets.clear();
            }
        }

        let mut touched: Vec<(String, Bucket, bool)> = Vec::with_capacity(rules.len());
        let mut max_wait = 0.0f64;
        let mut rejected = false;
        for (key, rpm, burst) in rules {
            let mut bucket = buckets.get(key).copied().unwrap_or(Bucket {
                tokens: *burst,
                last: now,
                rpm: *rpm,
                burst: *burst,
            });
            if bucket.rpm != *rpm || bucket.burst != *burst {
                bucket.rpm = *rpm;
                bucket.burst = *burst;
            }
            let elapsed = now.saturating_sub(bucket.last).as_secs_f64();
            bucket.last = now;
            bucket.tokens = (bucket.tokens + elapsed * bucket.rpm / 60.0).min(bucket.burst);
            let ok = bucket.tokens >= 1.0;
            if !ok {
                let wait = if bucket.rpm > 0.0 {
                    (1.0 - bucket.tokens) / (bucket.rpm / 60.0)
                } else {
                    f64::INFINITY
                };
                max_wait = max_wait.max(wait);
                rejected = true;
            }
            touched.push((key.clone(), bucket, ok));
        }
        // 回填落库：拒绝=仅入账本次累计与配置刷新（不扣减）；成功=统一扣减 1 令牌
        for (key, bucket, ok) in &touched {
            let mut b = *bucket;
            if !rejected && *ok {
                b.tokens -= 1.0;
            }
            buckets.insert(key.clone(), b);
        }
        drop(buckets);

        // 主体活动记账 + 豁免判定（判定用旧值，写入新值——同请求内只判一次）
        let exempt_ok = if let Some(id) = identity {
            let mut ids = self.identities.lock();
            if ids.len() >= MAX_IDENTITIES {
                ids.retain(|_, v| now.saturating_sub(v.last_active) < IDLE_EVICT_AFTER);
            }
            let idle_gap_ok = match ids.get(id) {
                Some(prev) => now.saturating_sub(prev.last_active) >= idle_exempt,
                // 首次出现视同长空闲后恢复（冷启动不 429）
                None => true,
            };
            ids.insert(id.to_string(), Identity { last_active: now });
            idle_gap_ok && idle_exempt > Duration::ZERO
        } else {
            false
        };

        match rejected {
            false => Ok(()),
            true => {
                if exempt_ok {
                    tracing::info!(
                        identity = identity.unwrap_or(""),
                        "rate limit resume exemption granted"
                    );
                    Ok(())
                } else {
                    Err(max_wait)
                }
            }
        }
    }

    /// 清除某 key 的桶（登录成功后重置失败计数）
    pub fn reset(&self, key: &str) {
        self.buckets.lock().remove(key);
    }
}

/// 规则匹配（纯函数，便于单测）：作用域（global/user/api_key）× 模型限定 → 桶键列表。
/// 模型限定的规则仅在请求模型名与之精确相等时命中，桶键附加 `|model:` 后缀独立计量；
/// 不限模型的规则对所有请求命中（既有行为），两者叠加时最严者先拒。
pub struct MatchedRule {
    /// 计量桶键（令牌桶与并发共用）
    pub key: String,
    pub rpm: f64,
    pub burst: f64,
    /// 并发上限（0 = 不限）
    pub concurrency: i32,
}

fn matched_rules(
    rules: &[crate::store::rules::RateRule],
    user_id: Option<i64>,
    key_id: Option<i64>,
    model: &str,
) -> Vec<MatchedRule> {
    rules
        .iter()
        .filter_map(|rule| {
            let mut bucket_key = match rule.scope.as_str() {
                "global" => "global".to_string(),
                "user" => user_id
                    .filter(|uid| Some(*uid) == rule.scope_id)
                    .map(|uid| format!("user:{uid}"))?,
                "api_key" => key_id
                    .filter(|kid| Some(*kid) == rule.scope_id)
                    .map(|kid| format!("key:{kid}"))?,
                other => {
                    tracing::warn!("unknown rate limit scope: {other}");
                    return None;
                }
            };
            if let Some(rule_model) = rule.model.as_deref() {
                if rule_model != model {
                    return None;
                }
                bucket_key.push_str("|model:");
                bucket_key.push_str(rule_model);
            }
            Some(MatchedRule {
                key: bucket_key,
                rpm: rule.rpm as f64,
                burst: rule.burst as f64,
                concurrency: rule.concurrency,
            })
        })
        .collect()
}

/// 应用限流规则（api_key > user > global，命中即拒）
pub fn apply_rate_limits(
    st: &AppState,
    user_id: Option<i64>,
    key_id: Option<i64>,
    model: &str,
) -> Result<(), AppError> {
    let token_rules: Vec<(String, f64, f64)> = {
        let rules = st.rules.read();
        matched_rules(&rules, user_id, key_id, model)
            .into_iter()
            .map(|m| (m.key, m.rpm, m.burst))
            .collect()
    };

    // 主体标识：user+key 联合（同一用户多 key 各自独立豁免，粒度贴合"会话恢复"）
    let identity = match (user_id, key_id) {
        (Some(u), Some(k)) => Some(format!("u:{u}|k:{k}")),
        (Some(u), None) => Some(format!("u:{u}")),
        _ => None,
    };
    match st.limiter.check_rules_at(
        now_ts(),
        &token_rules,
        identity.as_deref(),
        Duration::from_secs(st.cfg.rate_idle_exempt_secs),
    ) {
        Ok(()) => Ok(()),
        Err(retry) => {
            // 重试群打散：报头值加 [0,1s) 均匀抖动。被拒客户端若都按相同报头值
            // （rpm=60 下恒为 1s）整秒对齐重试，每秒仅回填 1 令牌时对齐流只会
            // 让同一批客户端互相挤兑；抖动使各端重试到达时刻去相关，显著降低
            // 个别客户端连续落败直至放弃重试的概率。
            let retry = retry + rand::Rng::gen_range(&mut rand::thread_rng(), 0.0..1.0);
            tracing::warn!(
                identity = identity.as_deref().unwrap_or(""),
                model = %model,
                retry_secs = format!("{retry:.1}"),
                "request rate limited"
            );
            Err(AppError::RateLimited(retry))
        }
    }
}

/// 进程内并发上限器（单实例语义；多实例需换 Redis 原子计数）。
/// 桶键与令牌桶同维度复用；计数条目仅在请求在途期间存在（guard 释放归零即移除），
/// 表大小以「在途请求数 × 命中规则数」为上界，无需驱逐。
#[derive(Default)]
pub struct ConcurrencyLimiter {
    counts: Arc<Mutex<HashMap<String, i64>>>,
}

/// RAII 并发名额：Drop 时归还全部占用桶。无并发规则命中时 keys 为空，释放为空操作。
/// 生命周期必须绑定到响应体（bind_guards_to_body）：SSE 流尽或客户端断开才归还，
/// handler 返回 ≠ 请求结束。
#[derive(Debug)]
pub struct ConcurrencyGuard {
    counts: Arc<Mutex<HashMap<String, i64>>>,
    keys: Vec<String>,
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 在途计数观测（测试与管理端 introspection 用）
    pub fn active_in(&self, key: &str) -> i64 {
        self.counts.lock().get(key).copied().unwrap_or(0)
    }

    /// 尝试在全部并发受限（limit > 0）的命中桶各占一个名额。
    /// 全有或全无：任一桶满即整体拒绝（返回 (桶键, 上限)），不部分占坑——
    /// 与令牌桶「拒绝路径零扣减」同一原则。limit <= 0 的规则不参与计量。
    pub fn try_acquire(&self, rules: &[(String, i32)]) -> Result<ConcurrencyGuard, (String, i32)> {
        let mut counts = self.counts.lock();
        for (key, limit) in rules {
            if *limit <= 0 {
                continue;
            }
            if counts.get(key).copied().unwrap_or(0) >= i64::from(*limit) {
                return Err((key.clone(), *limit));
            }
        }
        let keys: Vec<String> = rules
            .iter()
            .filter(|(_, limit)| *limit > 0)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            *counts.entry(key.clone()).or_insert(0) += 1;
        }
        Ok(ConcurrencyGuard { counts: self.counts.clone(), keys })
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let mut counts = self.counts.lock();
        for key in &self.keys {
            if let Some(c) = counts.get_mut(key) {
                *c -= 1;
            }
        }
        // 归零移除：防止表随历史桶键无限增长（与 UserActiveGuard 同策略）
        counts.retain(|_, c| *c > 0);
    }
}

/// 并发桶键去重：同键多条规则取最严（最小）上限。输入顺序无关，输出按键排序。
fn dedup_min_limit(rules: Vec<(String, i32)>) -> Vec<(String, i32)> {
    let mut m = std::collections::BTreeMap::new();
    for (key, limit) in rules {
        m.entry(key)
            .and_modify(|e: &mut i32| {
                if limit < *e {
                    *e = limit;
                }
            })
            .or_insert(limit);
    }
    m.into_iter().collect()
}

/// 应用并发上限（与 apply_rate_limits 同一匹配维度，scope × 模型精确匹配）。
/// 返回的 guard 由调用方绑定到响应体生命周期；Err 即整体拒绝、零占坑。
/// 刻意不参与空闲恢复豁免：并发名额是真实资源占用，豁免会突破上限。

pub fn apply_concurrency_limits(
    st: &AppState,
    user_id: Option<i64>,
    key_id: Option<i64>,
    model: &str,
) -> Result<ConcurrencyGuard, AppError> {
    // DB 不强制 (scope, scope_id, model) 唯一：重复规则会让同一桶键出现多次，
    // check 阶段读同值、increment 阶段双倍占坑，实际并发上限被副本数稀释。
    // 按键去重、取最严（最小）上限。
    let matched: Vec<(String, i32)> = dedup_min_limit({
        let rules = st.rules.read();
        matched_rules(&rules, user_id, key_id, model)
            .into_iter()
            .filter(|m| m.concurrency > 0)
            .map(|m| (m.key, m.concurrency))
            .collect()
    });

    match st.concurrency.try_acquire(&matched) {
        Ok(guard) => Ok(guard),
        Err((key, limit)) => {
            let identity = match (user_id, key_id) {
                (Some(u), Some(k)) => Some(format!("u:{u}|k:{k}")),
                (Some(u), None) => Some(format!("u:{u}")),
                _ => None,
            };
            // 名额何时释放未知（在途请求时长不可知），给 1-2s 短重试提示；
            // 客户端按 429 常规退避即可，抖动防止重试群整秒对齐挤兑
            let retry = 1.0 + rand::Rng::gen_range(&mut rand::thread_rng(), 0.0..1.0);
            tracing::warn!(
                identity = identity.as_deref().unwrap_or(""),
                model = %model,
                bucket = %key,
                limit,
                retry_secs = format!("{retry:.1}"),
                "request concurrency limited"
            );
            Err(AppError::ConcurrentLimited(retry))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::rules::RateRule;

    fn rule(scope: &str, scope_id: Option<i64>, model: Option<&str>) -> RateRule {
        RateRule {
            scope: scope.to_string(),
            scope_id,
            rpm: 60,
            burst: 10,
            model: model.map(str::to_string),
            concurrency: 0,
        }
    }

    fn keys(matched: &[MatchedRule]) -> Vec<&str> {
        matched.iter().map(|m| m.key.as_str()).collect()
    }

    fn conc_rules(pairs: &[(&str, i32)]) -> Vec<(String, i32)> {
        pairs
            .iter()
            .map(|(k, l)| (k.to_string(), *l))
            .collect()
    }

    /// 模型限定规则：仅请求模型精确相等时命中，桶键带模型后缀
    #[test]
    fn model_scoped_rule_matches_exact_model_only() {
        let rules = vec![rule("global", None, Some("gpt-4o"))];
        assert_eq!(
            keys(&matched_rules(&rules, None, None, "gpt-4o")),
            vec!["global|model:gpt-4o"]
        );
        assert!(matched_rules(&rules, None, None, "gpt-4o-mini").is_empty());
        assert!(
            matched_rules(&rules, None, None, "GPT-4O").is_empty(),
            "大小写敏感"
        );
    }

    /// 不限模型的规则对所有请求命中（既有行为不变）
    #[test]
    fn unscoped_rule_matches_all_models() {
        let rules = vec![rule("global", None, None)];
        for m in ["a", "b", ""] {
            assert_eq!(keys(&matched_rules(&rules, None, None, m)), vec!["global"]);
        }
    }

    /// 作用域 × 模型叠加：user 规则命中本人且桶键独立；他人不命中
    #[test]
    fn user_scoped_model_rule() {
        let rules = vec![rule("user", Some(7), Some("claude-3"))];
        assert_eq!(
            keys(&matched_rules(&rules, Some(7), None, "claude-3")),
            vec!["user:7|model:claude-3"]
        );
        assert!(matched_rules(&rules, Some(8), None, "claude-3").is_empty());
        assert!(matched_rules(&rules, Some(7), None, "gpt-4o").is_empty());
    }

    /// 重复桶键去重取最严上限：DB 不约束 (scope, scope_id, model) 唯一时，
    /// 同键多副本不得双倍占坑
    #[test]
    fn dedup_concurrency_keys_takes_min_limit() {
        let out = dedup_min_limit(conc_rules(&[("global", 6), ("global", 3), ("user:7", 2)]));
        assert_eq!(out, conc_rules(&[("global", 3), ("user:7", 2)]));
        assert!(dedup_min_limit(conc_rules(&[])).is_empty());
    }

    /// 同请求同时命中限模型与不限模型规则 → 两个桶（最严者先拒）
    #[test]
    fn model_rule_stacks_with_general_rule() {
        let rules = vec![
            rule("global", None, None),
            rule("user", Some(1), Some("m2")),
        ];
        assert_eq!(
            keys(&matched_rules(&rules, Some(1), None, "m2")),
            vec!["global", "user:1|model:m2"]
        );
        // 换模型：只剩不限模型的规则
        assert_eq!(
            keys(&matched_rules(&rules, Some(1), None, "m1")),
            vec!["global"]
        );
    }

    fn rules_global(rpm: f64, burst: f64) -> Vec<(String, f64, f64)> {
        vec![("global".to_string(), rpm, burst)]
    }

    /// 突发耗尽后拒绝，并给出正确的 Retry-After
    #[test]
    fn burst_drain_then_reject_with_retry_after() {
        let rl = RateLimiter::new();
        let now = Duration::ZERO;
        for i in 0..5 {
            assert!(
                rl.check_rules_at(now, &rules_global(60.0, 5.0), None, Duration::ZERO)
                    .is_ok(),
                "burst 内第 {i} 个请求应放行"
            );
        }
        let err = rl
            .check_rules_at(now, &rules_global(60.0, 5.0), None, Duration::ZERO)
            .unwrap_err();
        // rpm=60 → 1 token/s，桶空 → 恰好 1s
        assert!((err - 1.0).abs() < 1e-9, "retry after ~1s, got {err}");
    }

    /// 拒绝路径零泄漏：慢桶拒绝时，快桶令牌不得被白烧。
    /// global rpm=0/burst=2（永不回填）+ user rpm=6/burst=1（10s 攒 1 令牌）：
    /// 首请求放行后 user 桶空；旧实现每次拒绝都先扣掉 global 令牌（rpm=0
    /// 永不回填）→ 慢桶回满后仍被 global 无限拒绝；两阶段判定下 global
    /// 令牌原样保留，慢桶攒够即整体放行。
    #[test]
    fn rejection_does_not_leak_other_buckets() {
        let rl = RateLimiter::new();
        let rules = vec![
            ("global".to_string(), 0.0, 2.0),
            ("user:7".to_string(), 6.0, 1.0),
        ];
        // 双桶初始满额 → 放行一次（global 2→1，user 1→0）
        assert!(
            rl.check_rules_at(Duration::ZERO, &rules, None, Duration::ZERO)
                .is_ok()
        );
        for _ in 0..3 {
            let err = rl
                .check_rules_at(Duration::ZERO, &rules, None, Duration::ZERO)
                .unwrap_err();
            assert!(
                (err - 10.0).abs() < 1e-9,
                "慢桶 rpm=6 → 等待 10s，got {err}"
            );
        }
        assert!(
            rl.check_rules_at(Duration::from_secs(10), &rules, None, Duration::ZERO)
                .is_ok(),
            "global 令牌未被泄漏，慢桶回填后应整体放行"
        );
    }

    /// Retry-After 取所有不足桶的最大等待（整体可过的最早时刻），而非首个拒绝桶
    #[test]
    fn retry_after_is_max_across_failing_buckets() {
        let rl = RateLimiter::new();
        // 两只空桶：rpm=60 → 等 1s；rpm=6 → 等 10s
        let rules = vec![
            ("global".to_string(), 60.0, 0.0),
            ("user:7".to_string(), 6.0, 0.0),
        ];
        let err = rl
            .check_rules_at(Duration::ZERO, &rules, None, Duration::ZERO)
            .unwrap_err();
        assert!((err - 10.0).abs() < 1e-9, "应取最大等待 10s，got {err}");
    }

    /// 短等待：回填不足 1 个令牌，仍拒绝且等待时间按比例缩短
    #[test]
    fn short_wait_still_limited_but_retry_shrinks() {
        let rl = RateLimiter::new();
        let mut now = Duration::ZERO;
        for _ in 0..5 {
            rl.check_rules_at(now, &rules_global(1.0, 5.0), None, Duration::ZERO)
                .ok();
        }
        now += Duration::from_secs(10); // rpm=1 → 10s 只回填 1/6 令牌
        let err = rl
            .check_rules_at(now, &rules_global(1.0, 5.0), None, Duration::ZERO)
            .unwrap_err();
        assert!((err - 50.0).abs() < 1e-6, "还差 5/6 令牌 → 50s, got {err}");
    }

    /// 超长等待：空闲期间配额不被扣减，桶按真实流逝时间回满
    #[test]
    fn long_idle_refills_bucket_to_burst() {
        let rl = RateLimiter::new();
        let mut now = Duration::from_secs(1_000);
        for _ in 0..5 {
            rl.check_rules_at(now, &rules_global(1.0, 5.0), None, Duration::ZERO)
                .ok();
        }
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), None, Duration::ZERO)
                .is_err(),
            "桶已耗尽"
        );
        now += Duration::from_secs(600); // 等待用户确认 10 分钟
        for i in 0..5 {
            assert!(
                rl.check_rules_at(now, &rules_global(1.0, 5.0), None, Duration::ZERO)
                    .is_ok(),
                "长空闲后第 {i} 个请求应放行（空闲回填到 burst）"
            );
        }
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), None, Duration::ZERO)
                .is_err(),
            "超出 burst 仍拒绝"
        );
    }

    /// 恢复豁免：主体长空闲期间共享桶（global）被他人耗尽，
    /// 恢复后的首个请求放行；紧随其后的请求恢复正常限流
    #[test]
    fn resume_exemption_grants_first_request_after_idle() {
        let rl = RateLimiter::new();
        let mut now = Duration::from_secs(1_000);
        let alice = "u:1|k:1";
        let bob = "u:2|k:2";

        // alice 暂停前正常活动一次，进入长空闲
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(alice),
                Duration::from_secs(60)
            )
            .is_ok()
        );
        now += Duration::from_secs(30);
        // bob（其他会话）耗尽全局桶
        for _ in 0..5 {
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(bob),
                Duration::from_secs(60),
            )
            .ok();
        }
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(bob),
                Duration::from_secs(60)
            )
            .is_err()
        );

        // alice 静默 30s < 60s：无豁免，429
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(alice),
                Duration::from_secs(60)
            )
            .is_err(),
            "空闲不足阈值不应豁免"
        );
        // alice 重新静默 60s+（模拟等待用户确认），bob 期间再次耗尽全局桶
        now += Duration::from_secs(120);
        for _ in 0..5 {
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(bob),
                Duration::from_secs(60),
            )
            .ok();
        }
        // alice 恢复：首请求豁免放行
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(alice),
                Duration::from_secs(60)
            )
            .is_ok(),
            "长空闲后恢复的首请求应豁免"
        );
        // 第二个请求立即 429（豁免每空闲间隙仅一次；rpm=1 下桶仍空）
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(alice),
                Duration::from_secs(60)
            )
            .is_err(),
            "豁免只覆盖首个请求"
        );
    }

    /// 高频请求永远攒不出空闲间隙 → 豁免不触发，超出配额的请求全部被拒；
    /// 被拒请求不扣令牌，30s 间隔的持续压制收敛为"通过率=rpm"的交替模式
    #[test]
    fn sustained_hammering_never_gets_exempt() {
        let rl = RateLimiter::new();
        let mut now = Duration::from_secs(1_000);
        let attacker = "u:9|k:9";

        // 阶段一：同一时刻 8 连发（间隙 0 < 60s 阈值）→ burst 5 之外全拒
        for i in 0..8 {
            let r = rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(attacker),
                Duration::from_secs(60),
            );
            if i < 5 {
                assert!(r.is_ok(), "burst 内第 {i} 发应放行");
            } else {
                assert!(r.is_err(), "第 {i} 发必须被限流且不得豁免（无空闲间隙）");
            }
        }

        // 阶段二：30s 间隔持续 20 发（仍 < 60s 阈值，豁免永不触发）。
        // 需求 2/min > 供给 1/min → 必然出现拒绝；且通过数不超过
        // 令牌守恒上限：burst(5) + 总回填(elapsed_min)
        let start = now;
        let mut passes = 0usize;
        for _ in 0..20 {
            if rl
                .check_rules_at(
                    now,
                    &rules_global(1.0, 5.0),
                    Some(attacker),
                    Duration::from_secs(60),
                )
                .is_ok()
            {
                passes += 1;
            }
            now += Duration::from_secs(30);
        }
        let elapsed_min = (now - start).as_secs_f64() / 60.0;
        let cap = 5 + elapsed_min as usize;
        assert!(
            passes <= cap,
            "10 分钟内通过 {passes} 发，超过 burst+回填上限 {cap}"
        );
        assert!(passes < 20, "持续超速压制下不可能全部通过");
    }

    /// 豁免阈值 0 = 功能关闭
    #[test]
    fn exemption_disabled_when_threshold_zero() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        let a = "u:1|k:1";
        let b = "u:2|k:2";
        for _ in 0..5 {
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(b), Duration::ZERO)
                .ok();
        }
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(a), Duration::ZERO)
                .is_err(),
            "idle_exempt=0 时首次出现也不豁免"
        );
    }

    /// 多规则：每个匹配桶各扣一个令牌；首个拒绝即返回
    #[test]
    fn multi_rule_each_bucket_consumed() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        let rules = vec![
            ("user:1".to_string(), 60.0, 1.0),
            ("global".to_string(), 60.0, 1.0),
        ];
        assert!(rl.check_rules_at(now, &rules, None, Duration::ZERO).is_ok());
        // 两个桶各剩 0 令牌
        let err = rl
            .check_rules_at(now, &rules, None, Duration::ZERO)
            .unwrap_err();
        assert!((err - 1.0).abs() < 1e-9);
        // 单独验证 user:1 桶也已扣减（独立调用直接拒绝）
        let user_rule = vec![("user:1".to_string(), 60.0, 1.0)];
        assert!(
            rl.check_rules_at(now, &user_rule, None, Duration::ZERO)
                .is_err()
        );
    }

    /// rpm<=0：不产生 NaN，等待时间为无穷（由错误层钳制 Retry-After）
    #[test]
    fn rpm_zero_yields_infinite_wait_not_nan() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        // burst=1：首发消耗唯一令牌，此后 rpm=0 永不回填
        assert!(
            rl.check_rules_at(now, &rules_global(0.0, 1.0), None, Duration::ZERO)
                .is_ok()
        );
        let err = rl
            .check_rules_at(now, &rules_global(0.0, 1.0), None, Duration::ZERO)
            .unwrap_err();
        assert!(err.is_infinite(), "rpm=0 耗尽后应无限等待, got {err}");
        assert!(!err.is_nan());
    }

    /// 冷启动：全新主体面对耗尽的共享桶，首请求豁免（部署/重启后不被旧状态卡死）
    #[test]
    fn cold_start_identity_gets_single_pass() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        let old = "u:8|k:8";
        for _ in 0..5 {
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(old),
                Duration::from_secs(60),
            )
            .ok();
        }
        let fresh = "u:7|k:7";
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(fresh),
                Duration::from_secs(60)
            )
            .is_ok(),
            "全新主体首请求应豁免"
        );
        assert!(
            rl.check_rules_at(
                now,
                &rules_global(1.0, 5.0),
                Some(fresh),
                Duration::from_secs(60)
            )
            .is_err()
        );
    }

    /// 规则参数热更新（rpm/burst 变化）沿用原语义：即时生效、令牌余量保留
    #[test]
    fn rule_change_keeps_remaining_tokens() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        rl.check_rules_at(now, &rules_global(60.0, 5.0), None, Duration::ZERO)
            .ok();
        // 缩容到 burst=2：现有 4 令牌被钳到 2
        rl.check_rules_at(now, &rules_global(60.0, 2.0), None, Duration::ZERO)
            .ok();
        rl.check_rules_at(now, &rules_global(60.0, 2.0), None, Duration::ZERO)
            .ok();
        assert!(
            rl.check_rules_at(now, &rules_global(60.0, 2.0), None, Duration::ZERO)
                .is_err()
        );
    }

    #[test]
    fn now_ts_is_monotonic() {
        let a = now_ts();
        std::thread::sleep(Duration::from_millis(5));
        let b = now_ts();
        assert!(b > a, "BOOTTIME 时钟必须单调递增");
    }

    /// 并发占坑与释放：满员拒绝并报出 (桶键, 上限)；释放一个名额即可再进
    #[test]
    fn concurrency_acquire_reject_and_release() {
        let cl = ConcurrencyLimiter::new();
        let rules = conc_rules(&[("global", 2)]);
        let g1 = cl.try_acquire(&rules).unwrap();
        let _g2 = cl.try_acquire(&rules).unwrap();
        let err = cl.try_acquire(&rules).unwrap_err();
        assert_eq!(err, ("global".to_string(), 2), "满员拒绝须报出桶键与上限");
        assert_eq!(cl.active_in("global"), 2);
        drop(g1);
        assert_eq!(cl.active_in("global"), 1, "释放一个名额后应可再进");
        let g3 = cl.try_acquire(&rules).unwrap();
        assert!(cl.try_acquire(&rules).is_err(), "重新占满后应拒绝");
        drop(g3);
    }

    /// 多规则叠加：全有或全无——任一桶满整体拒绝，其余桶不得被部分占坑
    /// （拒绝路径零占用，与令牌桶「拒绝路径零扣减」同一原则）
    #[test]
    fn concurrency_multi_rule_all_or_nothing() {
        let cl = ConcurrencyLimiter::new();
        let rules = conc_rules(&[("global", 1), ("user:7", 5)]);
        let g = cl.try_acquire(&rules).unwrap();
        assert_eq!(cl.active_in("global"), 1);
        assert_eq!(cl.active_in("user:7"), 1);
        assert!(cl.try_acquire(&rules).is_err(), "global 满应整体拒绝");
        assert_eq!(
            cl.active_in("user:7"),
            1,
            "拒绝路径不得在 user 桶占坑"
        );
        drop(g);
        assert!(cl.try_acquire(&rules).is_ok(), "全部释放后应可重新占满");
    }

    /// concurrency = 0 的规则不参与并发计量（不会因 0 上限而恒拒）
    #[test]
    fn concurrency_zero_limit_unlimited() {
        let cl = ConcurrencyLimiter::new();
        let rules = conc_rules(&[("global", 0)]);
        for _ in 0..100 {
            drop(cl.try_acquire(&rules).unwrap());
        }
        assert!(cl.try_acquire(&rules).is_ok(), "0 上限 = 不限，不得拒绝");
        assert_eq!(cl.active_in("global"), 0, "0 上限的桶不计数");
    }

    /// guard 全部释放后计数归零且条目移除（防表随历史桶键膨胀）
    #[test]
    fn concurrency_entries_removed_when_all_released() {
        let cl = ConcurrencyLimiter::new();
        let rules = conc_rules(&[("global", 3), ("user:7", 3)]);
        let g = cl.try_acquire(&rules).unwrap();
        drop(g);
        assert_eq!(cl.active_in("global"), 0);
        assert_eq!(cl.active_in("user:7"), 0);
        assert!(
            cl.counts.lock().is_empty(),
            "归零条目应移除而非留 0 值占位"
        );
    }

    /// matched_rules 把规则的 concurrency 带入匹配结果（模型限定规则同样生效）
    #[test]
    fn matched_rules_carry_concurrency() {
        let mut user_rule = rule("user", Some(7), Some("gpt-4o"));
        user_rule.concurrency = 6;
        let mut global_rule = rule("global", None, None);
        global_rule.concurrency = 3;
        let matched = matched_rules(&[user_rule, global_rule], Some(7), None, "gpt-4o");
        assert_eq!(matched.len(), 2);
        let by_key: HashMap<&str, i32> = matched
            .iter()
            .map(|m| (m.key.as_str(), m.concurrency))
            .collect();
        assert_eq!(by_key["user:7|model:gpt-4o"], 6, "模型限定规则的并发上限随匹配透传");
        assert_eq!(by_key["global"], 3, "不限模型规则的并发上限随匹配透传");
        // 模型不匹配：模型限定规则不出现
        let matched_other = matched_rules(&[rule("user", Some(7), Some("gpt-4o"))], Some(7), None, "other");
        assert!(matched_other.is_empty());
    }
}
