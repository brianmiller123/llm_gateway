use std::collections::HashMap;
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
        self.check_rules_at(now_ts(), &[(key.to_string(), rpm, burst)], None, Duration::ZERO)
    }

    /// 多规则原子检查（api_key > user > global，命中即拒）：
    /// - 所有匹配桶逐个消耗令牌，首个拒绝即终止；
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

        let mut rejected: Option<(String, f64)> = None;
        for (key, rpm, burst) in rules {
            let bucket = buckets.entry(key.clone()).or_insert(Bucket {
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

            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
            } else {
                let wait = if bucket.rpm > 0.0 {
                    (1.0 - bucket.tokens) / (bucket.rpm / 60.0)
                } else {
                    f64::INFINITY
                };
                rejected = Some((key.clone(), wait));
                break;
            }
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
            ids.insert(
                id.to_string(),
                Identity {
                    last_active: now,
                },
            );
            idle_gap_ok && idle_exempt > Duration::ZERO
        } else {
            false
        };

        match rejected {
            None => Ok(()),
            Some((key, wait)) => {
                if exempt_ok {
                    tracing::info!(
                        rule_key = %key,
                        identity = identity.unwrap_or(""),
                        "rate limit resume exemption granted"
                    );
                    Ok(())
                } else {
                    Err(wait)
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
fn matched_rules(
    rules: &[crate::store::rules::RateRule],
    user_id: Option<i64>,
    key_id: Option<i64>,
    model: &str,
) -> Vec<(String, f64, f64)> {
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
            Some((bucket_key, rule.rpm as f64, rule.burst as f64))
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
    let matched = {
        let rules = st.rules.read();
        matched_rules(&rules, user_id, key_id, model)
    };

    // 主体标识：user+key 联合（同一用户多 key 各自独立豁免，粒度贴合"会话恢复"）
    let identity = match (user_id, key_id) {
        (Some(u), Some(k)) => Some(format!("u:{u}|k:{k}")),
        (Some(u), None) => Some(format!("u:{u}")),
        _ => None,
    };
    match st.limiter.check_rules_at(
        now_ts(),
        &matched,
        identity.as_deref(),
        Duration::from_secs(st.cfg.rate_idle_exempt_secs),
    ) {
        Ok(()) => Ok(()),
        Err(retry) => {
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
        }
    }

    fn keys(matched: &[(String, f64, f64)]) -> Vec<&str> {
        matched.iter().map(|(k, _, _)| k.as_str()).collect()
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
        assert!(matched_rules(&rules, None, None, "GPT-4O").is_empty(), "大小写敏感");
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

    /// 同请求同时命中限模型与不限模型规则 → 两个桶（最严者先拒）
    #[test]
    fn model_rule_stacks_with_general_rule() {
        let rules = vec![rule("global", None, None), rule("user", Some(1), Some("m2"))];
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
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(alice), Duration::from_secs(60))
                .is_ok()
        );
        now += Duration::from_secs(30);
        // bob（其他会话）耗尽全局桶
        for _ in 0..5 {
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(bob), Duration::from_secs(60))
                .ok();
        }
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(bob), Duration::from_secs(60))
                .is_err()
        );

        // alice 静默 30s < 60s：无豁免，429
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(alice), Duration::from_secs(60))
                .is_err(),
            "空闲不足阈值不应豁免"
        );
        // alice 重新静默 60s+（模拟等待用户确认），bob 期间再次耗尽全局桶
        now += Duration::from_secs(120);
        for _ in 0..5 {
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(bob), Duration::from_secs(60))
                .ok();
        }
        // alice 恢复：首请求豁免放行
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(alice), Duration::from_secs(60))
                .is_ok(),
            "长空闲后恢复的首请求应豁免"
        );
        // 第二个请求立即 429（豁免每空闲间隙仅一次；rpm=1 下桶仍空）
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(alice), Duration::from_secs(60))
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
        let err = rl.check_rules_at(now, &rules, None, Duration::ZERO).unwrap_err();
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
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(old), Duration::from_secs(60))
                .ok();
        }
        let fresh = "u:7|k:7";
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(fresh), Duration::from_secs(60))
                .is_ok(),
            "全新主体首请求应豁免"
        );
        assert!(
            rl.check_rules_at(now, &rules_global(1.0, 5.0), Some(fresh), Duration::from_secs(60))
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
}
