//! 极简进程内计数器（/metrics 文本导出；无外部依赖）。
//! 计数自进程启动累计，内存态，重启清零是有意为之。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// 进程级指标集合：通常构造一次后经 Arc 共享
#[derive(Debug, Default)]
pub struct Metrics {
    /// 已接受的连接总数
    pub conn_accepted: AtomicU64,
    /// 连接级错误总数（读/写/超时等）
    pub conn_err: AtomicU64,
    /// TLS 握手失败总数
    pub tls_handshake_fail: AtomicU64,
    /// 客户端中断总数（流式响应中途断开）
    pub client_abort_total: AtomicU64,
    /// 上游错误总数（含按 provider 细分）
    pub upstream_err_total: AtomicU64,
    /// 熔断器打开次数
    pub breaker_open_total: AtomicU64,
    /// 按 provider_id 累计的上游错误；毒化时读侧降级为空
    upstream_err_by_provider: Mutex<HashMap<i64, u64>>,
}

impl Metrics {
    /// 全零初始状态
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次上游错误：总数 +1，同时累计到对应 provider
    pub fn record_upstream_error(&self, provider_id: i64) {
        self.upstream_err_total.fetch_add(1, Ordering::Relaxed);
        // 锁毒化时静默跳过逐 provider 统计，总数已计入，绝不 panic
        if let Ok(mut map) = self.upstream_err_by_provider.lock() {
            *map.entry(provider_id).or_insert(0) += 1;
        }
    }

    /// 记录一次熔断器打开
    pub fn record_breaker_open(&self) {
        self.breaker_open_total.fetch_add(1, Ordering::Relaxed);
    }

    /// 逐 provider 上游错误快照；锁毒化时返回空表（绝不 panic）
    pub fn upstream_err_by_provider(&self) -> HashMap<i64, u64> {
        self.upstream_err_by_provider
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default()
    }

    /// 渲染 Prometheus 文本格式（每个指标带 HELP/TYPE 头行）
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        counter(
            &mut out,
            "gateway_conn_accepted_total",
            "已接受的连接总数",
            self.conn_accepted.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "gateway_conn_error_total",
            "连接级错误总数",
            self.conn_err.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "gateway_tls_handshake_fail_total",
            "TLS 握手失败总数",
            self.tls_handshake_fail.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "gateway_client_abort_total",
            "客户端中断总数",
            self.client_abort_total.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "gateway_upstream_error_total",
            "上游错误总数（含按 provider 细分）",
            self.upstream_err_total.load(Ordering::Relaxed),
        );
        // 同一指标族的带标签样本紧跟在总数之后，按 provider_id 升序保证输出稳定
        let mut by_provider: Vec<(i64, u64)> =
            self.upstream_err_by_provider().into_iter().collect();
        by_provider.sort_unstable_by_key(|(provider_id, _)| *provider_id);
        for (provider_id, count) in by_provider {
            let _ = writeln!(
                out,
                "gateway_upstream_error_total{{provider_id=\"{provider_id}\"}} {count}"
            );
        }
        counter(
            &mut out,
            "gateway_breaker_open_total",
            "熔断器打开次数",
            self.breaker_open_total.load(Ordering::Relaxed),
        );
        out
    }
}

/// 追加一个 counter 指标族的 HELP/TYPE 头行与当前样本
fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    // String 写入不会失败，忽略返回值即可
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_upstream_error_accumulates_total_and_per_provider() {
        let metrics = Metrics::new();
        metrics.record_upstream_error(7);
        metrics.record_upstream_error(7);
        metrics.record_upstream_error(9);
        assert_eq!(metrics.upstream_err_total.load(Ordering::Relaxed), 3);
        let by_provider = metrics.upstream_err_by_provider();
        assert_eq!(by_provider.get(&7), Some(&2));
        assert_eq!(by_provider.get(&9), Some(&1));
    }

    #[test]
    fn render_emits_family_headers_and_provider_samples() {
        let metrics = Metrics::new();
        metrics.record_upstream_error(42);
        metrics.record_breaker_open();
        let text = metrics.render();
        assert!(text.contains("# TYPE gateway_conn_accepted_total counter\n"));
        assert!(text.contains("\ngateway_breaker_open_total 1\n"));
        assert!(text.contains("\ngateway_upstream_error_total 1\n"));
        assert!(text.contains("gateway_upstream_error_total{provider_id=\"42\"} 1\n"));
    }

    #[test]
    fn poisoned_lock_reads_as_empty_and_never_panics() {
        let metrics = Metrics::new();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = metrics.upstream_err_by_provider.lock().unwrap();
            panic!("poison the lock on purpose");
        }));
        assert!(poisoned.is_err());
        assert!(metrics.upstream_err_by_provider().is_empty());
        // 写侧同样不 panic，总数照常累计
        metrics.record_upstream_error(1);
        assert_eq!(metrics.upstream_err_total.load(Ordering::Relaxed), 1);
        assert!(metrics.upstream_err_by_provider().is_empty());
    }
}
