//! 进程内每渠道熔断器（M3，cc-switch `circuit_breaker.rs` 子集 + P1-16 错误率维度）。
//!
//! 语义：某 provider 连续 `failure_threshold` 次可重试失败（429/5xx/408/传输错误）
//! → 熔断 `open_secs`；或窗口内错误率 ≥ `failure_rate`（最少 `min_requests` 个
//! 请求样本，cc-switch error_rate 0.6 / min 10 同款）→ 熔断——慢性劣化（错误率
//! 高但从不连续 4 败）不再永久漏判。期间降级链跳过该渠道（全部渠道均熔断时仍
//! 放行兜底，避免"全部熔断 → 整体 502"的可用性倒挂）。成功即复位连续计数；
//! 半开窗口到期后单探测放行；M4：需连续 `success_threshold` 次成功才闭合
//! （cc-switch success_threshold=2 同款——单次成功闭合会让抖动上游在开-半开间
//! 震荡），不足则重开窗口等待下一个探测。
//! 参数经 AppConfig（环境变量）可调，默认值与此前常量一致。
//! 状态仅存进程内存：重启/热加载清零，不持久化（熔断是短窗启发式，非计费数据）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// M4：熔断器参数（环境变量可调；默认与 cc-switch circuit_breaker 对齐）
#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// 连续失败熔断阈值（cc-switch failure_threshold=4）
    pub failure_threshold: u32,
    /// 熔断窗口秒数（cc-switch timeout=60；本网关历史默认 30 保留）
    pub open_secs: u64,
    /// 窗口错误率熔断阈值（P1-16；cc-switch error_rate=0.6）
    pub failure_rate: f64,
    /// 错误率判定的最小样本数（cc-switch min_requests=10）
    pub min_requests: u64,
    /// 统计窗口秒数（到达即整体重置的衰减窗口，避免永久累积）
    pub window_secs: u64,
    /// M4：半开需连续成功次数才闭合（cc-switch success_threshold=2）
    pub success_threshold: u32,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 4,
            open_secs: 30,
            failure_rate: 0.6,
            min_requests: 10,
            window_secs: 120,
            success_threshold: 2,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ProviderBreaker {
    consec_failures: u32,
    open_until: Option<Instant>,
    /// M32：半开单探测名额——窗口到期后只放行一个请求，成功复位、失败重开
    half_open_probe_in_flight: bool,
    /// M4：半开探测已累计的连续成功次数（< success_threshold 时重开窗口）
    half_open_successes: u32,
    /// P1-16：窗口内请求/失败计数（错误率维度）
    window_requests: u64,
    window_failures: u64,
    window_start: Option<Instant>,
}

pub struct Breaker {
    states: Mutex<HashMap<i64, ProviderBreaker>>,
    cfg: BreakerConfig,
}

impl Breaker {
    /// 默认参数构造（测试用）
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            cfg: BreakerConfig::default(),
        }
    }

    /// 按 AppConfig 熔断参数构造（M4）
    pub fn new_with(cfg: BreakerConfig) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            cfg,
        }
    }

    /// 渠道当前是否放行：未熔断 / 熔断窗口已过且探测名额可用（M32：半开单探测，
    /// 窗口到期不再全量放行——雷群效应；cc-switch allow_half_open_probe 同款）
    pub fn allow(&self, provider_id: i64) -> bool {
        let mut states = self.states.lock();
        let pb = states.entry(provider_id).or_default();
        match pb.open_until {
            None => true,
            Some(until) if Instant::now() >= until => {
                if pb.half_open_probe_in_flight {
                    false
                } else {
                    pb.half_open_probe_in_flight = true;
                    true
                }
            }
            Some(_) => false,
        }
    }

    /// 至少一个候选放行时返回放行子集；全部熔断时返回 None（调用方回退全量候选）
    pub fn filter_candidates<'a, T: Clone>(
        &self,
        candidates: &'a [T],
        id_of: impl Fn(&T) -> i64,
    ) -> Option<Vec<T>> {
        let allowed: Vec<T> = candidates
            .iter()
            .filter(|c| self.allow(id_of(c)))
            .cloned()
            .collect();
        if allowed.is_empty() {
            None
        } else {
            Some(allowed)
        }
    }

    pub fn record_success(&self, provider_id: i64) {
        let mut states = self.states.lock();
        let Some(pb) = states.get_mut(&provider_id) else {
            return;
        };
        // 成功即复位连续计数；窗口计数保留
        pb.consec_failures = 0;
        if pb.half_open_probe_in_flight {
            // M4：半开探测成功累计；达到 success_threshold 才闭合（cc-switch 同款——
            // 单次成功闭合会让抖动上游在开-半开间震荡），不足则释放探测名额并
            // 重开窗口等待下一探测
            pb.half_open_probe_in_flight = false;
            pb.half_open_successes += 1;
            if pb.half_open_successes >= self.cfg.success_threshold {
                pb.open_until = None;
                pb.half_open_successes = 0;
            } else {
                pb.open_until = Some(Instant::now() + Duration::from_secs(self.cfg.open_secs));
            }
        } else {
            pb.open_until = None;
        }
        self.decay_window(pb);
        pb.window_requests += 1;
    }

    pub fn record_failure(&self, provider_id: i64) {
        let mut states = self.states.lock();
        let pb = states.entry(provider_id).or_default();
        if pb.half_open_probe_in_flight {
            // M32：探测请求失败 → 重开完整窗口（cc-switch 半开失败语义）
            pb.half_open_probe_in_flight = false;
            pb.half_open_successes = 0;
            pb.open_until = Some(Instant::now() + Duration::from_secs(self.cfg.open_secs));
            pb.consec_failures = self.cfg.failure_threshold;
            return;
        }
        self.decay_window(pb);
        pb.consec_failures += 1;
        pb.window_requests += 1;
        pb.window_failures += 1;
        // P1-16：连续失败 OR 窗口错误率（最小样本）→ 熔断
        let rate_open = pb.window_requests >= self.cfg.min_requests
            && (pb.window_failures as f64) / (pb.window_requests as f64) >= self.cfg.failure_rate;
        if pb.consec_failures >= self.cfg.failure_threshold || rate_open {
            pb.open_until = Some(Instant::now() + Duration::from_secs(self.cfg.open_secs));
            pb.consec_failures = 0;
            pb.window_requests = 0;
            pb.window_failures = 0;
            pb.window_start = None;
        }
    }

    /// 衰减窗口：窗口到期整体重置（简单且无界安全的滑动近似）
    fn decay_window(&self, pb: &mut ProviderBreaker) {
        match pb.window_start {
            Some(start)
                if Instant::now().duration_since(start)
                    >= Duration::from_secs(self.cfg.window_secs) =>
            {
                pb.window_requests = 0;
                pb.window_failures = 0;
                pb.window_start = Some(Instant::now());
            }
            Some(_) => {}
            None => pb.window_start = Some(Instant::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opens_after_consecutive_failures_and_recovers() {
        let b = Breaker::new();
        for _ in 0..4 {
            assert!(b.allow(1), "熔断前始终放行");
            b.record_failure(1);
        }
        assert!(!b.allow(1), "连续 4 次失败后熔断");
        // 非探测成功即复位
        b.record_success(1);
        assert!(b.allow(1));
    }

    #[test]
    fn non_consecutive_failures_do_not_open() {
        let b = Breaker::new();
        for _ in 0..10 {
            b.record_failure(1);
            b.record_success(1);
        }
        assert!(b.allow(1), "交替成功失败不累积");
    }

    #[test]
    fn filter_candidates_all_open_falls_back_to_all() {
        let b = Breaker::new();
        let cands = vec![("a", 1i64), ("b", 2i64)];
        for _ in 0..4 {
            b.record_failure(1);
            b.record_failure(2);
        }
        // 全部熔断 → None（调用方回退全量，避免整体不可用）
        assert!(b.filter_candidates(&cands, |c| c.1).is_none());
        // 2 号复位后 → 只放行 2 号
        b.record_success(2);
        let allowed = b.filter_candidates(&cands, |c| c.1).expect("subset");
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].0, "b");
    }

    /// M4：半开需连续 success_threshold 次探测成功才闭合（cc-switch success_threshold=2）；
    /// 单次成功不足时释放探测名额并重开窗口，避免开-半开震荡。
    /// 窗口时间用私有状态直接推进（测试不真实等待）。
    fn expire_window(b: &Breaker, provider_id: i64) {
        let mut states = b.states.lock();
        if let Some(pb) = states.get_mut(&provider_id) {
            pb.open_until = Some(Instant::now() - Duration::from_secs(1));
        }
    }

    #[test]
    fn half_open_requires_success_threshold_probes_to_close() {
        let b = Breaker::new_with(BreakerConfig {
            success_threshold: 2,
            ..BreakerConfig::default()
        });
        for _ in 0..4 {
            b.record_failure(1);
        }
        assert!(!b.allow(1), "熔断中");
        expire_window(&b, 1);
        assert!(b.allow(1), "半开探测放行");
        b.record_success(1);
        // 1 次成功 < success_threshold=2：探测名额已释放但窗口重开未到期
        assert!(!b.allow(1), "1 次成功不足，窗口重开");
        expire_window(&b, 1);
        assert!(b.allow(1), "第二个探测放行");
        b.record_success(1);
        assert!(b.allow(1), "2 次成功闭合");
    }

    /// 半开探测失败：重开完整窗口，清零成功计数
    #[test]
    fn half_open_probe_failure_reopens_full_window() {
        let b = Breaker::new_with(BreakerConfig {
            success_threshold: 2,
            ..BreakerConfig::default()
        });
        for _ in 0..4 {
            b.record_failure(1);
        }
        expire_window(&b, 1);
        assert!(b.allow(1), "半开探测放行");
        b.record_failure(1);
        assert!(!b.allow(1), "探测失败 → 重开窗口，不再放行");
        expire_window(&b, 1);
        assert!(b.allow(1), "窗口到期后下一个探测再次放行");
        b.record_success(1);
        // 1 次成功不足 → 重开窗口；到期后第 2 次探测成功才闭合
        assert!(!b.allow(1), "1 次成功不足，窗口重开");
        expire_window(&b, 1);
        assert!(b.allow(1), "第二个探测放行");
        b.record_success(1);
        assert!(b.allow(1), "连续两次成功闭合");
    }

    /// P1-16：慢性劣化（错误率 ≥ 0.6 但从不连续 4 败）→ 错误率维度熔断
    /// （cc-switch error_rate 0.6 / min 10 请求同款语义）
    #[test]
    fn chronic_error_rate_opens_without_consecutive_failures() {
        let b = Breaker::new();
        // (f,f,s)×3 = 6 败/9 请求；再补 f,f → 8 败/11 请求 = 0.727 ≥ 0.6 且样本 ≥ 10
        // → 错误率维度熔断；连续失败峰值 2 < 4，走不到连续阈值路径
        for _ in 0..3 {
            b.record_failure(1);
            b.record_failure(1);
            b.record_success(1);
        }
        b.record_failure(1);
        b.record_failure(1);
        assert!(!b.allow(1), "窗口错误率 ≥ 0.6 → 熔断");
    }

    /// P1-16：低错误率不误熔（0.5 < 0.6，样本足够）
    #[test]
    fn low_error_rate_does_not_open() {
        let b = Breaker::new();
        for _ in 0..20 {
            b.record_failure(1);
            b.record_success(1);
        }
        assert!(b.allow(1), "50% 错误率 < 60% 阈值");
    }
}
