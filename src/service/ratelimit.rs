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

/// 进程内令牌桶（单实例下精确；多实例时替换为 Redis 实现）。
/// 仅用于控制台登录防爆破（login:user:{username} / login:ip:{ip}）；
/// 代理请求路径的限流即并发上限（apply_concurrency_limits），不走令牌桶。
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Ts,
    rpm: f64,
    burst: f64,
}

const MAX_BUCKETS: usize = 10_000;
/// 桶表防膨胀：满员时清掉 1 小时无活动的条目
const IDLE_EVICT_AFTER: Duration = Duration::from_secs(3600);

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 检查并消耗一个令牌；不足返回需等待的秒数（rpm<=0 且无余量时为无穷）。
    /// 回填速率 rpm/60 每秒，按真实流逝时间计（含系统挂起期间，见 now_ts）。
    pub fn check(&self, key: &str, rpm: f64, burst: f64) -> Result<(), f64> {
        self.check_at(now_ts(), key, rpm, burst)
    }

    /// check 的时间注入变体（测试确定性；生产路径走 check）
    pub fn check_at(&self, now: Ts, key: &str, rpm: f64, burst: f64) -> Result<(), f64> {
        let mut buckets = self.buckets.lock();
        // 防膨胀：满员时先驱逐超过 1 小时未活动的桶；仍满才整体清空
        // （整体清空会重置全部限流状态，攻击者可用随机 key 触发，故仅在驱逐无效时兜底）
        if buckets.len() >= MAX_BUCKETS {
            buckets.retain(|_, b| now.saturating_sub(b.last) < IDLE_EVICT_AFTER);
            if buckets.len() >= MAX_BUCKETS {
                buckets.clear();
            }
        }
        let mut bucket = buckets.get(key).copied().unwrap_or(Bucket {
            tokens: burst,
            last: now,
            rpm,
            burst,
        });
        // 参数热更新即时生效，令牌余量保留
        if bucket.rpm != rpm || bucket.burst != burst {
            bucket.rpm = rpm;
            bucket.burst = burst;
        }
        let elapsed = now.saturating_sub(bucket.last).as_secs_f64();
        bucket.last = now;
        bucket.tokens = (bucket.tokens + elapsed * bucket.rpm / 60.0).min(bucket.burst);
        let ok = bucket.tokens >= 1.0;
        if ok {
            bucket.tokens -= 1.0;
        }
        buckets.insert(key.to_string(), bucket);
        match ok {
            true => Ok(()),
            false => {
                // 拒绝路径零扣减：仅入账本次累计与配置刷新
                let wait = if rpm > 0.0 {
                    (1.0 - bucket.tokens) / (rpm / 60.0)
                } else {
                    f64::INFINITY
                };
                Err(wait)
            }
        }
    }

    /// 清除某 key 的桶（登录成功后重置失败计数）
    pub fn reset(&self, key: &str) {
        self.buckets.lock().remove(key);
    }
}

/// 规则匹配（纯函数，便于单测）：作用域（global/user/api_key）× 模型限定 →
/// (并发桶键, 并发上限) 列表。模型限定的规则仅在请求模型名与之精确相等时命中，
/// 桶键附加 `|model:` 后缀独立计量；不限模型的规则对所有请求命中，
/// 同键多规则叠加时由 apply_concurrency_limits 去重取最严上限。
/// user 规则 scope_id 为 NULL 时通配所有用户（桶键仍按 user:{uid} 独立计量，
/// 一条规则实现「每用户 N 并发」）；指定 scope_id 则仅该用户命中。
fn matched_rules(
    rules: &[crate::store::rules::RateRule],
    user_id: Option<i64>,
    key_id: Option<i64>,
    model: &str,
) -> Vec<(String, i32)> {
    rules
        .iter()
        .filter_map(|rule| {
            let mut bucket_key = match rule.scope.as_str() {
                "global" => "global".to_string(),
                "user" => {
                    // 未鉴权请求（user_id=None）不匹配任何 user 规则
                    let uid = user_id?;
                    if let Some(sid) = rule.scope_id {
                        if uid != sid {
                            return None;
                        }
                    }
                    format!("user:{uid}")
                }
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
            Some((bucket_key, rule.concurrency))
        })
        .collect()
}

/// 进程内并发上限器（单实例语义；多实例需换 Redis 原子计数）。
/// 桶键按 scope × 模型精确匹配；计数条目仅在请求在途期间存在（guard 释放归零即移除），
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
        Ok(ConcurrencyGuard {
            counts: self.counts.clone(),
            keys,
        })
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

/// 应用并发上限（scope × 模型精确匹配）。
/// 返回的 guard 由调用方绑定到响应体生命周期；Err 即整体拒绝、零占坑。
pub fn apply_concurrency_limits(
    st: &AppState,
    user_id: Option<i64>,
    key_id: Option<i64>,
    model: &str,
) -> Result<ConcurrencyGuard, AppError> {
    // DB 不强制 (scope, scope_id, model) 唯一：重复规则会让同一桶键出现多次，
    // try_acquire 阶段双倍占坑，实际并发上限被副本数稀释。
    // 按键去重、取最严（最小）上限。
    let matched: Vec<(String, i32)> = dedup_min_limit({
        let rules = st.rules.read();
        matched_rules(&rules, user_id, key_id, model)
            .into_iter()
            .filter(|(_, limit)| *limit > 0)
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
            // 名额何时释放未知（在途请求时长不可知），给 ~6s 重试提示；
            // 客户端按 429 常规退避即可，抖动防止重试群整秒对齐挤兑
            //（5.5-6.5s 区间，向上取整后 Retry-After 恒为 6）
            let retry = 5.5 + rand::Rng::gen_range(&mut rand::thread_rng(), 0.0..1.0);
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
            model: model.map(str::to_string),
            concurrency: 0,
        }
    }

    fn keys(matched: &[(String, i32)]) -> Vec<&str> {
        matched.iter().map(|(k, _)| k.as_str()).collect()
    }

    fn conc_rules(pairs: &[(&str, i32)]) -> Vec<(String, i32)> {
        pairs.iter().map(|(k, l)| (k.to_string(), *l)).collect()
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

    /// user 通配规则（scope_id=NULL）：所有用户命中且桶键按 uid 独立；
    /// 未鉴权不命中；与指定用户规则同键叠加时产出同一桶键（去重后不双计）
    #[test]
    fn user_wildcard_rule_matches_any_user_per_uid() {
        let rules = vec![rule("user", None, None)];
        assert_eq!(keys(&matched_rules(&rules, Some(7), Some(100), "m")), vec!["user:7"]);
        assert_eq!(keys(&matched_rules(&rules, Some(8), None, "m")), vec!["user:8"]);
        assert!(
            matched_rules(&rules, None, None, "m").is_empty(),
            "未鉴权请求不匹配 user 规则"
        );
        // 通配规则 + 指定用户规则同键：组合层（apply_concurrency_limits 的
        // dedup_min_limit）合并为单桶取最严，不双倍占坑
        let stacked = vec![rule("user", None, None), rule("user", Some(7), None)];
        assert_eq!(
            keys(&dedup_min_limit(matched_rules(&stacked, Some(7), None, "m"))),
            vec!["user:7"]
        );
        assert_eq!(
            keys(&dedup_min_limit(matched_rules(&stacked, Some(8), None, "m"))),
            vec!["user:8"]
        );
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

    /// 突发耗尽后拒绝，并给出正确的等待秒数（登录限流：Retry-After 来源）
    #[test]
    fn burst_drain_then_reject_with_retry_after() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        for i in 0..5 {
            assert!(
                rl.check_at(now, "login:ip:1.2.3.4", 60.0, 5.0).is_ok(),
                "burst 内第 {i} 次应放行"
            );
        }
        let err = rl.check_at(now, "login:ip:1.2.3.4", 60.0, 5.0).unwrap_err();
        // rpm=60 → 1 token/s，桶空 → 恰好 1s
        assert!((err - 1.0).abs() < 1e-9, "retry after ~1s, got {err}");
    }

    /// 超长等待：空闲期间不扣减，桶按真实流逝时间回满
    #[test]
    fn long_idle_refills_bucket_to_burst() {
        let rl = RateLimiter::new();
        let mut now = Duration::from_secs(1_000);
        for _ in 0..5 {
            rl.check_at(now, "login:user:alice", 1.0, 5.0).ok();
        }
        assert!(
            rl.check_at(now, "login:user:alice", 1.0, 5.0).is_err(),
            "桶已耗尽"
        );
        now += Duration::from_secs(600);
        for i in 0..5 {
            assert!(
                rl.check_at(now, "login:user:alice", 1.0, 5.0).is_ok(),
                "长空闲后第 {i} 次应放行（空闲回填到 burst）"
            );
        }
        assert!(
            rl.check_at(now, "login:user:alice", 1.0, 5.0).is_err(),
            "超出 burst 仍拒绝"
        );
    }

    /// rpm<=0：不产生 NaN，等待时间为无穷（由错误层钳制 Retry-After）
    #[test]
    fn rpm_zero_yields_infinite_wait_not_nan() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        // burst=1：首发消耗唯一令牌，此后 rpm=0 永不回填
        assert!(rl.check_at(now, "login:user:bob", 0.0, 1.0).is_ok());
        let err = rl.check_at(now, "login:user:bob", 0.0, 1.0).unwrap_err();
        assert!(err.is_infinite(), "rpm=0 耗尽后应无限等待, got {err}");
        assert!(!err.is_nan());
    }

    /// 参数热更新（rpm/burst 变化）即时生效、令牌余量保留
    #[test]
    fn rule_change_keeps_remaining_tokens() {
        let rl = RateLimiter::new();
        let now = Duration::from_secs(1_000);
        rl.check_at(now, "login:user:carol", 60.0, 5.0).ok();
        // 缩容到 burst=2：现有 4 令牌被钳到 2
        rl.check_at(now, "login:user:carol", 60.0, 2.0).ok();
        rl.check_at(now, "login:user:carol", 60.0, 2.0).ok();
        assert!(rl.check_at(now, "login:user:carol", 60.0, 2.0).is_err());
    }

    /// 登录成功后 reset 清桶：紧随其后的登录不受上一主体失败计数影响
    #[test]
    fn reset_clears_login_bucket() {
        let rl = RateLimiter::new();
        for _ in 0..10 {
            rl.check("login:user:dave", 10.0, 10.0).ok();
        }
        assert!(rl.check("login:user:dave", 10.0, 10.0).is_err());
        rl.reset("login:user:dave");
        assert!(
            rl.check("login:user:dave", 10.0, 10.0).is_ok(),
            "reset 后应立即可用"
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
        assert_eq!(cl.active_in("user:7"), 1, "拒绝路径不得在 user 桶占坑");
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
        assert!(cl.counts.lock().is_empty(), "归零条目应移除而非留 0 值占位");
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
        let by_key: HashMap<&str, i32> = matched.iter().map(|(k, l)| (k.as_str(), *l)).collect();
        assert_eq!(
            by_key["user:7|model:gpt-4o"], 6,
            "模型限定规则的并发上限随匹配透传"
        );
        assert_eq!(by_key["global"], 3, "不限模型规则的并发上限随匹配透传");
        // 模型不匹配：模型限定规则不出现
        let matched_other = matched_rules(
            &[rule("user", Some(7), Some("gpt-4o"))],
            Some(7),
            None,
            "other",
        );
        assert!(matched_other.is_empty());
    }
}
