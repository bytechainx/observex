//! 观测边界策略（非 OTEL）。

/// 是否宣称 OTEL 导出完成。
#[must_use]
pub const fn claims_otel_export_complete() -> bool {
    false
}

/// Counting 是否生产 metrics。
#[must_use]
pub const fn counting_is_production_metrics() -> bool {
    false
}

/// 策略摘要。
#[must_use]
pub fn policy_summary() -> &'static str {
    "tracing-min; counting=test-only; bounded-sink=in-process; otel-sdk=no; otlp=no"
}

/// 观测实现级别。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservabilityTier {
    /// tracing info 最小面。
    TracingMin,
    /// 进程内计数（测试）。
    CountingTest,
    /// OTEL（未交付）。
    OtelDeferred,
}

/// TracingInstrumentation 所属级别。
#[must_use]
pub fn tier_tracing() -> ObservabilityTier {
    ObservabilityTier::TracingMin
}

/// CountingInstrumentation 所属级别。
#[must_use]
pub fn tier_counting() -> ObservabilityTier {
    ObservabilityTier::CountingTest
}

/// 是否可宣称「生产可观测完成」。
#[must_use]
pub fn allows_production_observability_claim(tier: ObservabilityTier) -> bool {
    match tier {
        ObservabilityTier::TracingMin => false,
        ObservabilityTier::CountingTest => false,
        ObservabilityTier::OtelDeferred => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_honesty() {
        assert!(!claims_otel_export_complete());
        assert!(!counting_is_production_metrics());
        assert!(policy_summary().contains("bounded-sink=in-process"));
        assert_eq!(tier_tracing(), ObservabilityTier::TracingMin);
        assert_eq!(tier_counting(), ObservabilityTier::CountingTest);
        assert!(!allows_production_observability_claim(tier_tracing()));
        assert!(!allows_production_observability_claim(tier_counting()));
        assert!(!allows_production_observability_claim(
            ObservabilityTier::OtelDeferred
        ));
        let _ = format!("{:?}", ObservabilityTier::TracingMin);
        for _ in 0..30 {
            assert!(!claims_otel_export_complete());
        }
    }
}
