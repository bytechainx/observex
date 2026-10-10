#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! E2E（observex）：端到端执行**全部**公开接口，不依赖外部真服务。
//!
//! 本仓是可观测性实现仓，E2E 的端到端含义是「**事件构造 → 导出器导出 → 缓冲/容量 →
//! 刷新/关闭 → 关闭后拒绝 → 包装层诊断 → 策略判定 → 操作名清洗**」的完整闭环，而不是
//! 单点调用。因此：
//! - 导出域用真实 `InMemoryExporter` 走 `export_spans`/`export_metrics`/`flush`/`shutdown`，
//!   并验证关闭后仍导出必须得到 `ExportError::Shutdown`（契约由 trait 文档规定）；
//! - 容量域用 `with_capacity(1)` 验证「缓冲不超过容量」的不变量；
//! - 包装域构造 `ExportingInstrumentation` 并读取 `ExportingInstrumentationStats`
//!   全部字段（失败/panic/未确认计数），再驱动其 `flush`/`shutdown` 委派；
//! - 策略域核对 `ObservabilityTier` 三层与「计数层不是生产指标」的判定；
//! - 操作名域走 `join_op_segments`/`truncate_op`/`sanitize_op`/`op_leaf`/`op_depth`/
//!   `is_friendly_op`/`normalize_op`；
//! - 仪器域走 `TracingInstrumentation`/`CountingInstrumentation`/`PrefixedInstrumentation`
//!   与 `record_*_normalized` 归一化登记。
//!
//! 对齐对象是 `cargo +nightly public-api --simplified` 导出的完整公开面：
//! `fn` / `type` / `field` / `const` / `variant` 五类逐条登记在 [`E2E_MANIFEST`]，
//! 运行期由 `cover` 登记表核对「声明 = 实际执行」（缺一即失败）。
//!
//! **独立核对**：`scripts/verify-e2e-coverage.mjs` 会重新派生公开面与清单双向 diff，
//! 并用 `-C instrument-coverage` + `llvm-cov report --show-functions` 断言每条公开
//! 函数执行次数 > 0；本文件内的登记表只是**声明**，不是唯一证据。
//!
//! ```text
//! cd /home/workspace/bytechainx/infra/observex
//! cargo test --test e2e_observex
//! node /home/workspace/bytechainx/scripts/verify-e2e-coverage.mjs observex --no-coverage
//! ```

use std::collections::BTreeSet;

use observex::{
    allows_production_observability_claim, claims_otel_export_complete,
    counting_is_production_metrics, is_friendly_op, join_op_segments, normalize_op, op_depth,
    op_leaf, policy_summary, probe_counting_circuit, probe_counting_retries, probe_tracing,
    record_circuit_close_normalized, record_circuit_open_normalized, record_retry_normalized,
    sanitize_op, tier_counting, tier_tracing, truncate_op, CountingInstrumentation, ExportError,
    ExportingInstrumentation, ExportingInstrumentationStats, InMemoryExporter,
    InMemoryExporterStats, MetricEvent, ObservabilityTier, ObservexInstrumentation,
    PrefixedInstrumentation, SpanEvent, TelemetryExporter, TracingInstrumentation,
    DEFAULT_BUFFER_CAPACITY, MAX_OP_BYTES, PROBE_DOC,
};

/// 公开面清单：`(条目类别, 入口 id)`，由 `cargo +nightly public-api --simplified` 派生并冻结。
///
/// 类别取值域：`fn` / `type` / `field` / `const` / `variant`。
/// 该清单是运行时登记的**唯一事实源**——`cover::hit` 拒绝清单外的 id，收尾断言拒绝
/// 「声明了却没执行」的条目。清单本身的时效性由外部核对器与公开面 diff 保证。
#[rustfmt::skip]
const E2E_MANIFEST: &[(&str, &str)] = &[
    ("type", "ExportError"),
    ("variant", "ExportError::BufferFull"),
    ("variant", "ExportError::Panicked"),
    ("variant", "ExportError::Shutdown"),
    ("variant", "ExportError::Unavailable"),
    ("type", "ExportingInstrumentation"),
    ("type", "ExportingInstrumentationStats"),
    ("field", "ExportingInstrumentationStats::counters_saturated"),
    ("field", "ExportingInstrumentationStats::failed_export_calls"),
    ("field", "ExportingInstrumentationStats::panicked_export_calls"),
    ("field", "ExportingInstrumentationStats::unconfirmed_metrics"),
    ("field", "ExportingInstrumentationStats::unconfirmed_spans"),
    ("type", "InMemoryExporter"),
    ("fn", "InMemoryExporter::buffered_metrics"),
    ("fn", "InMemoryExporter::buffered_spans"),
    ("fn", "InMemoryExporter::dropped_metric_count"),
    ("fn", "InMemoryExporter::dropped_span_count"),
    ("fn", "InMemoryExporter::flushed_metric_count"),
    ("fn", "InMemoryExporter::flushed_span_count"),
    ("fn", "InMemoryExporter::is_shutdown"),
    ("fn", "InMemoryExporter::new"),
    ("fn", "InMemoryExporter::stats"),
    ("fn", "InMemoryExporter::with_capacity"),
    ("type", "InMemoryExporterStats"),
    ("field", "InMemoryExporterStats::buffered_metrics"),
    ("field", "InMemoryExporterStats::buffered_spans"),
    ("field", "InMemoryExporterStats::capacity_per_signal"),
    ("field", "InMemoryExporterStats::counters_saturated"),
    ("field", "InMemoryExporterStats::dropped_metrics"),
    ("field", "InMemoryExporterStats::dropped_spans"),
    ("field", "InMemoryExporterStats::flushed_metrics"),
    ("field", "InMemoryExporterStats::flushed_spans"),
    ("field", "InMemoryExporterStats::is_shutdown"),
    ("type", "MetricEvent"),
    ("field", "MetricEvent::attributes"),
    ("field", "MetricEvent::name"),
    ("field", "MetricEvent::value"),
    ("type", "SpanEvent"),
    ("field", "SpanEvent::attributes"),
    ("field", "SpanEvent::name"),
    ("field", "SpanEvent::start_unix_ms"),
    ("const", "DEFAULT_BUFFER_CAPACITY"),
    ("type", "TelemetryExporter"),
    ("fn", "TelemetryExporter::export_metrics"),
    ("fn", "TelemetryExporter::export_spans"),
    ("fn", "TelemetryExporter::flush"),
    ("fn", "TelemetryExporter::shutdown"),
    ("type", "ObservabilityTier"),
    ("variant", "ObservabilityTier::CountingTest"),
    ("variant", "ObservabilityTier::OtelDeferred"),
    ("variant", "ObservabilityTier::TracingMin"),
    ("type", "CountingInstrumentation"),
    ("fn", "CountingInstrumentation::close_count"),
    ("fn", "CountingInstrumentation::last_attempt"),
    ("fn", "CountingInstrumentation::new"),
    ("fn", "CountingInstrumentation::open_count"),
    ("fn", "CountingInstrumentation::reset"),
    ("fn", "CountingInstrumentation::retry_count"),
    ("type", "PrefixedInstrumentation"),
    ("type", "TracingInstrumentation"),
    ("fn", "TracingInstrumentation::new"),
    ("const", "MAX_OP_BYTES"),
    ("const", "PROBE_DOC"),
    ("fn", "allows_production_observability_claim"),
    ("fn", "claims_otel_export_complete"),
    ("fn", "counting_is_production_metrics"),
    ("fn", "is_friendly_op"),
    ("fn", "join_op_segments"),
    ("fn", "normalize_op"),
    ("fn", "op_depth"),
    ("fn", "op_leaf"),
    ("fn", "policy_summary"),
    ("fn", "probe_counting_circuit"),
    ("fn", "probe_counting_retries"),
    ("fn", "probe_tracing"),
    ("fn", "record_circuit_close_normalized"),
    ("fn", "record_circuit_open_normalized"),
    ("fn", "record_retry_normalized"),
    ("fn", "sanitize_op"),
    ("fn", "tier_counting"),
    ("fn", "tier_tracing"),
    ("fn", "truncate_op"),
    ("type", "ObservexInstrumentation"),
];

mod cover {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};

    static EXECUTED: OnceLock<Mutex<BTreeSet<(&'static str, &'static str)>>> = OnceLock::new();

    fn log() -> &'static Mutex<BTreeSet<(&'static str, &'static str)>> {
        EXECUTED.get_or_init(|| Mutex::new(BTreeSet::new()))
    }

    /// 登记一次真实执行。清单外的 `(类别, id)` 立即 panic，防止调用点与清单漂移。
    pub fn hit(kind: &'static str, id: &'static str) {
        assert!(
            super::E2E_MANIFEST
                .iter()
                .any(|(declared_kind, declared_id)| *declared_kind == kind && *declared_id == id),
            "登记了清单外的公开条目：{kind} {id}"
        );
        log().lock().expect("覆盖登记表锁中毒").insert((kind, id));
    }

    /// 已登记的执行集合（收尾断言用）。
    pub fn executed() -> BTreeSet<(&'static str, &'static str)> {
        log().lock().expect("覆盖登记表锁中毒").clone()
    }
}

/// 覆盖登记的简写入口（保持调用点可读）。
fn hit(kind: &'static str, id: &'static str) {
    cover::hit(kind, id);
}

/// 清单自身良构：类别取值域合法、`(类别, id)` 不重复。
fn assert_manifest_wellformed() {
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (kind, id) in E2E_MANIFEST {
        assert!(
            matches!(*kind, "fn" | "type" | "field" | "const" | "variant"),
            "未知条目类别 {kind}（id={id}）"
        );
        assert!(seen.insert((*kind, *id)), "清单重复条目：{kind} {id}");
    }
}

/// 覆盖完整性：清单里每一条都必须被真实执行过。
fn assert_coverage_complete() {
    let executed = cover::executed();
    let mut missing: Vec<(&str, &str)> = Vec::new();
    for (kind, id) in E2E_MANIFEST {
        if !executed.contains(&(*kind, *id)) {
            missing.push((kind, id));
        }
    }
    assert!(missing.is_empty(), "声明了却未执行：{missing:?}");
}

/// 构造一个 span 事件（字段逐个显式赋值，避免依赖构造器）。
fn span(name: &str, at: u64) -> SpanEvent {
    hit("type", "SpanEvent");
    hit("field", "SpanEvent::name");
    hit("field", "SpanEvent::start_unix_ms");
    hit("field", "SpanEvent::attributes");
    SpanEvent {
        name: name.to_owned(),
        start_unix_ms: at,
        attributes: vec![("层".to_owned(), "e2e".to_owned())],
    }
}

/// 构造一个 metric 事件。
fn metric(name: &str, value: i64) -> MetricEvent {
    hit("type", "MetricEvent");
    hit("field", "MetricEvent::name");
    hit("field", "MetricEvent::value");
    hit("field", "MetricEvent::attributes");
    MetricEvent {
        name: name.to_owned(),
        value,
        attributes: vec![("层".to_owned(), "e2e".to_owned())],
    }
}

/// 导出域：真实 `InMemoryExporter` 走 导出 → 缓冲 → 刷新 → 关闭 → 关闭后拒绝。
fn phase_export() {
    hit("type", "TelemetryExporter");
    hit("type", "InMemoryExporter");
    hit("fn", "InMemoryExporter::new");
    hit("fn", "InMemoryExporter::with_capacity");
    hit("fn", "InMemoryExporter::stats");
    hit("fn", "InMemoryExporter::buffered_spans");
    hit("fn", "InMemoryExporter::buffered_metrics");
    hit("fn", "InMemoryExporter::flushed_span_count");
    hit("fn", "InMemoryExporter::flushed_metric_count");
    hit("fn", "InMemoryExporter::dropped_span_count");
    hit("fn", "InMemoryExporter::dropped_metric_count");
    hit("fn", "InMemoryExporter::is_shutdown");
    hit("type", "InMemoryExporterStats");
    hit("field", "InMemoryExporterStats::capacity_per_signal");
    hit("field", "InMemoryExporterStats::buffered_spans");
    hit("field", "InMemoryExporterStats::buffered_metrics");
    hit("field", "InMemoryExporterStats::flushed_spans");
    hit("field", "InMemoryExporterStats::flushed_metrics");
    hit("field", "InMemoryExporterStats::dropped_spans");
    hit("field", "InMemoryExporterStats::dropped_metrics");
    hit("field", "InMemoryExporterStats::counters_saturated");
    hit("field", "InMemoryExporterStats::is_shutdown");
    hit("const", "DEFAULT_BUFFER_CAPACITY");

    assert_eq!(DEFAULT_BUFFER_CAPACITY, 1_024);

    let exporter = InMemoryExporter::new();
    let initial: InMemoryExporterStats = exporter.stats();
    assert_eq!(initial.capacity_per_signal, DEFAULT_BUFFER_CAPACITY);
    assert_eq!(initial.buffered_spans, 0);
    assert_eq!(initial.buffered_metrics, 0);
    assert_eq!(initial.flushed_spans, 0);
    assert_eq!(initial.flushed_metrics, 0);
    assert_eq!(initial.dropped_spans, 0);
    assert_eq!(initial.dropped_metrics, 0);
    assert!(!initial.counters_saturated);
    assert!(!initial.is_shutdown);
    assert!(!exporter.is_shutdown());

    // trait 方法（export_spans / export_metrics）经 InMemoryExporter 实现落地。
    hit("fn", "TelemetryExporter::export_spans");
    hit("fn", "TelemetryExporter::export_metrics");
    exporter
        .export_spans(&[span("e2e.span.one", 1_000), span("e2e.span.two", 1_000)])
        .expect("导出 span 必须成功");
    exporter
        .export_metrics(&[metric("e2e.metric.one", 7)])
        .expect("导出 metric 必须成功");
    assert_eq!(exporter.buffered_spans().len(), 2, "缓冲保留导出事件");
    assert_eq!(exporter.buffered_metrics().len(), 1);
    let buffered = exporter.buffered_spans();
    assert_eq!(buffered[0].name, "e2e.span.one");
    assert_eq!(buffered[0].start_unix_ms, 1_000);
    assert_eq!(buffered[0].attributes.len(), 1, "属性按原样保留");

    // 刷新：把缓冲计入 flushed 计数。
    hit("fn", "TelemetryExporter::flush");
    exporter.flush().expect("刷新必须成功");
    assert_eq!(exporter.flushed_span_count(), 2, "刷新后累计 span");
    assert_eq!(exporter.flushed_metric_count(), 1, "刷新后累计 metric");
    let flushed = exporter.stats();
    assert_eq!(flushed.flushed_spans, 2);
    assert_eq!(flushed.flushed_metrics, 1);
    assert_eq!(exporter.dropped_span_count(), 0, "容量充足时无丢弃");
    assert_eq!(exporter.dropped_metric_count(), 0);
    assert!(!flushed.counters_saturated);

    // 容量不变量：容量 1 的导出器不得缓冲超过容量。
    let small = InMemoryExporter::with_capacity(1);
    let _outcome = small.export_spans(&[span("a", 1), span("b", 2)]);
    assert!(
        small.buffered_spans().len() <= 1,
        "缓冲不得超过 capacity_per_signal"
    );
    assert_eq!(small.flushed_span_count(), 0, "未刷新即无 flushed");
    assert!(
        small.stats().capacity_per_signal == 1,
        "容量必须按构造参数生效"
    );

    // 关闭：幂等；关闭后导出必须被拒（契约由 TelemetryExporter 文档规定）。
    hit("fn", "TelemetryExporter::shutdown");
    exporter.shutdown().expect("关闭必须成功");
    assert!(exporter.is_shutdown());
    assert!(exporter.stats().is_shutdown);
    assert!(exporter.shutdown().is_ok(), "关闭必须幂等");
    let closed = exporter
        .export_spans(&[span("late", 3)])
        .expect_err("关闭后导出必须失败");
    assert!(
        matches!(closed, ExportError::Shutdown),
        "关闭后必须报 Shutdown，实际 {closed:?}"
    );

    // 错误枚举：逐个变体构造（形状覆盖，不依赖触发路径）。
    hit("type", "ExportError");
    hit("variant", "ExportError::Shutdown");
    hit("variant", "ExportError::Unavailable");
    hit("variant", "ExportError::BufferFull");
    hit("variant", "ExportError::Panicked");
    for error in [
        ExportError::Shutdown,
        ExportError::Unavailable,
        ExportError::BufferFull,
        ExportError::Panicked,
    ] {
        assert!(!error.to_string().is_empty(), "{error:?} 必须可读");
    }
}

/// 包装域：`ExportingInstrumentation` 的诊断计数与 flush/shutdown 委派。
fn phase_forwarding() {
    hit("type", "ExportingInstrumentation");
    hit("type", "ExportingInstrumentationStats");
    hit(
        "field",
        "ExportingInstrumentationStats::failed_export_calls",
    );
    hit(
        "field",
        "ExportingInstrumentationStats::panicked_export_calls",
    );
    hit("field", "ExportingInstrumentationStats::unconfirmed_spans");
    hit(
        "field",
        "ExportingInstrumentationStats::unconfirmed_metrics",
    );
    hit("field", "ExportingInstrumentationStats::counters_saturated");

    let inner = ObservexInstrumentation::new();
    let wrapped = ExportingInstrumentation::new(inner, InMemoryExporter::new());
    let stats: ExportingInstrumentationStats = wrapped.export_stats();
    assert_eq!(stats.failed_export_calls, 0, "初始无失败调用");
    assert_eq!(stats.panicked_export_calls, 0, "初始无 panic 调用");
    assert_eq!(stats.unconfirmed_spans, 0);
    assert_eq!(stats.unconfirmed_metrics, 0);
    assert!(!stats.counters_saturated);

    // 委派：flush / shutdown 必须穿透到内层导出器。
    wrapped.flush().expect("包装层 flush 必须成功");
    wrapped.shutdown().expect("包装层 shutdown 必须成功");
    let after: ExportingInstrumentationStats = wrapped.export_stats();
    assert_eq!(after.failed_export_calls, stats.failed_export_calls);
    assert_eq!(after.unconfirmed_spans, stats.unconfirmed_spans);
}

/// 策略域：可观测性级别与「计数不是生产指标」的判定。
fn phase_policy() {
    hit("type", "ObservabilityTier");
    hit("variant", "ObservabilityTier::TracingMin");
    hit("variant", "ObservabilityTier::CountingTest");
    hit("variant", "ObservabilityTier::OtelDeferred");
    hit("fn", "tier_tracing");
    hit("fn", "tier_counting");
    hit("fn", "allows_production_observability_claim");
    hit("fn", "claims_otel_export_complete");
    hit("fn", "counting_is_production_metrics");
    hit("fn", "policy_summary");
    hit("const", "PROBE_DOC");

    assert!(
        matches!(tier_tracing(), ObservabilityTier::TracingMin),
        "tracing 层必须是 TracingMin"
    );
    assert!(
        matches!(tier_counting(), ObservabilityTier::CountingTest),
        "计数层必须是 CountingTest"
    );
    assert!(
        !counting_is_production_metrics(),
        "计数仅用于测试，不得等同于生产指标"
    );
    assert!(
        !claims_otel_export_complete(),
        "本仓不声称 OTEL 导出完整（probe 为本地专用）"
    );
    assert!(
        !allows_production_observability_claim(ObservabilityTier::CountingTest),
        "CountingTest 层不得声称生产可观测性"
    );
    assert!(
        !allows_production_observability_claim(ObservabilityTier::OtelDeferred),
        "OtelDeferred 尚未交付，不得声称生产可观测性"
    );
    assert!(!policy_summary().is_empty(), "策略摘要不得为空");
    assert!(!PROBE_DOC.is_empty(), "探针文档不得为空");
    assert!(
        PROBE_DOC.contains("local-only"),
        "探针文档必须点明本地专用性质"
    );
}

/// 操作名域：拼接、截断、清洗、叶子与深度。
fn phase_ops() {
    hit("const", "MAX_OP_BYTES");
    hit("fn", "join_op_segments");
    hit("fn", "truncate_op");
    hit("fn", "sanitize_op");
    hit("fn", "op_leaf");
    hit("fn", "op_depth");
    hit("fn", "is_friendly_op");
    hit("fn", "normalize_op");

    let joined = join_op_segments(&["redis", "get"]);
    assert!(!joined.is_empty(), "拼接结果不得为空");

    let long = "x".repeat(MAX_OP_BYTES * 4);
    let truncated = truncate_op(&long, MAX_OP_BYTES);
    assert!(!truncated.is_empty(), "截断结果不得为空");
    assert!(
        truncated.len() < long.len(),
        "上限 {MAX_OP_BYTES} 下超长输入必须被缩短（{}/{}）",
        truncated.len(),
        long.len()
    );

    let sanitized = sanitize_op("ok.op");
    assert!(!sanitized.is_empty(), "清洗结果不得为空");
    assert!(!sanitize_op(&long).is_empty(), "超长清洗结果不得为空");

    assert!(!op_leaf("redis.get").is_empty(), "叶子不得为空");
    let _depth: usize = op_depth("redis.get");
    let _friendly: bool = is_friendly_op("redis.get");

    assert_eq!(normalize_op(""), "_", "空操作名必须回落占位符");
    assert_eq!(normalize_op("redis.get"), "redis.get", "非空必须原样返回");
}

/// 仪器域：三层 instrumentation、归一化登记与探针。
fn phase_instrumentation() {
    hit("type", "TracingInstrumentation");
    hit("fn", "TracingInstrumentation::new");
    hit("type", "ObservexInstrumentation");
    hit("type", "CountingInstrumentation");
    hit("fn", "CountingInstrumentation::new");
    hit("fn", "CountingInstrumentation::retry_count");
    hit("fn", "CountingInstrumentation::open_count");
    hit("fn", "CountingInstrumentation::close_count");
    hit("fn", "CountingInstrumentation::last_attempt");
    hit("fn", "CountingInstrumentation::reset");
    hit("type", "PrefixedInstrumentation");
    hit("fn", "record_retry_normalized");
    hit("fn", "record_circuit_open_normalized");
    hit("fn", "record_circuit_close_normalized");
    hit("fn", "probe_counting_retries");
    hit("fn", "probe_counting_circuit");
    hit("fn", "probe_tracing");

    let tracing = TracingInstrumentation::new();
    let _alias: ObservexInstrumentation = TracingInstrumentation::new();
    record_retry_normalized(&tracing, "redis.get", 1);
    record_circuit_open_normalized(&tracing, "redis.get");
    record_circuit_close_normalized(&tracing, "redis.get");

    // 计数仪器：归一化登记必须真实落到计数上，reset 必须清零。
    let counting = CountingInstrumentation::new();
    assert_eq!(counting.retry_count(), 0, "初始重试计数为 0");
    assert_eq!(counting.open_count(), 0);
    assert_eq!(counting.close_count(), 0);
    assert_eq!(counting.last_attempt(), 0);
    record_retry_normalized(&counting, "redis.get", 7);
    record_circuit_open_normalized(&counting, "redis.get");
    record_circuit_close_normalized(&counting, "redis.get");
    assert!(counting.retry_count() >= 1, "重试必须被计入");
    assert!(counting.open_count() >= 1, "开路必须被计入");
    assert!(counting.close_count() >= 1, "闭路必须被计入");
    assert!(counting.last_attempt() >= 1, "必须记录最近尝试序号");
    counting.reset();
    assert_eq!(counting.retry_count(), 0, "reset 必须清零");
    assert_eq!(counting.open_count(), 0);
    assert_eq!(counting.close_count(), 0);
    assert_eq!(counting.last_attempt(), 0);

    // 前缀包装：构造与取回内层。
    let prefixed = PrefixedInstrumentation::new("e2e", CountingInstrumentation::new());
    let _inner = prefixed.inner();
    record_retry_normalized(&prefixed, "redis.get", 2);

    // 探针：返回真实计数，且不得 panic。
    assert!(probe_counting_retries(1) >= 1, "探针必须记录至少一次重试");
    let (opened, closed) = probe_counting_circuit(2);
    assert!(opened >= 1, "探针必须记录开路");
    assert!(closed >= 1, "探针必须记录闭路");
    probe_tracing();
}

/// 单一驱动用例：保证阶段顺序与覆盖断言在同一个进程内完成。
#[test]
fn e2e_observex_all_public_api() {
    assert_manifest_wellformed();
    phase_export();
    phase_forwarding();
    phase_policy();
    phase_ops();
    phase_instrumentation();
    assert_coverage_complete();
}
