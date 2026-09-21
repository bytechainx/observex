//! 公开面探测。

use instrumentationx::Instrumentation;

use crate::{CountingInstrumentation, TracingInstrumentation};

/// 探测说明（本地 only）。
pub const PROBE_DOC: &str = "probe is local-only, not OTEL";

/// 探测 TracingInstrumentation 经 trait 调用不 panic。
pub fn probe_tracing() {
    let t = TracingInstrumentation::new();
    let d: &dyn Instrumentation = &t;
    d.record_retry("probe", 0);
    d.record_circuit_open("probe");
    d.record_circuit_close("probe");
}

/// 探测 Counting 递增。
#[must_use]
pub fn probe_counting_retries(n: u32) -> u64 {
    let c = CountingInstrumentation::new();
    for i in 0..n {
        c.record_retry("probe", i);
    }
    c.retry_count()
}

/// 探测 open/close 计数。
#[must_use]
pub fn probe_counting_circuit(n: u32) -> (u64, u64) {
    let c = CountingInstrumentation::new();
    for _ in 0..n {
        c.record_circuit_open("probe");
        c.record_circuit_close("probe");
    }
    (c.open_count(), c.close_count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes() {
        assert!(PROBE_DOC.contains("local"));
        probe_tracing();
        assert_eq!(probe_counting_retries(5), 5);
        for n in 0..25 {
            assert_eq!(probe_counting_retries(n), u64::from(n));
        }
    }

    #[test]
    fn probe_many_rounds() {
        for round in 0..12 {
            probe_tracing();
            let n = (round + 1) * 3;
            assert_eq!(probe_counting_retries(n), u64::from(n));
            let (o, c) = probe_counting_circuit(n);
            assert_eq!(o, u64::from(n));
            assert_eq!(c, u64::from(n));
        }
    }
}
