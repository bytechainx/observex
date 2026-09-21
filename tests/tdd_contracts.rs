#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! TDD 行为契约（特性 002）。
//!
//! 逐公开入口的先红后绿：下表每个入口先在 `/tmp` 变异副本上观测红、再在本树观测绿；
//! 红绿过程（变异描述 + 复现命令）见 PR 描述。
//!
//! // TDD-PROBE: TracingInstrumentation::new | 变异：record_* 去掉 sanitize_op 调用 | 红=tracing_new_emits_sanitized_events | 绿=tracing_new_emits_sanitized_events
//! // TDD-PROBE: CountingInstrumentation::new | 变异：record_* 不再递增计数器 | 红=counting_new_counts_via_trait | 绿=counting_new_counts_via_trait
//! // TDD-PROBE: CountingInstrumentation::retry_count | 变异：retry_count 返回 open 计数 | 红=retry_count_tracks_attempts | 绿=retry_count_tracks_attempts
//! // TDD-PROBE: ExportingInstrumentation::new | 变异：record_* 不调用 inner | 红=exporting_new_wires_inner_and_exporter | 绿=exporting_new_wires_inner_and_exporter
//! // TDD-PROBE: ExportingInstrumentation::flush | 变异：flush 只计数不清空缓冲 | 红=flush_drains_buffer | 绿=flush_drains_buffer
//! // TDD-PROBE: ExportingInstrumentation::shutdown | 变异：shutdown 不先 flush（丢待处理计数） | 红=shutdown_flushes_and_is_idempotent | 绿=shutdown_flushes_and_is_idempotent
//! // TDD-PROBE: InMemoryExporter::new | 变异：DEFAULT_BUFFER_CAPACITY 改为 0 | 红=memory_exporter_default_capacity | 绿=memory_exporter_default_capacity
//! // TDD-PROBE: InMemoryExporter::stats | 变异：stats 的 dropped 字段返回 buffered 值 | 红=stats_snapshot_is_consistent | 绿=stats_snapshot_is_consistent
//! // TDD-PROBE: ops::sanitize_op | 变异：去掉 128 字节上限截断 | 红=sanitize_op_bounds_and_strips_control_chars | 绿=sanitize_op_bounds_and_strips_control_chars
//! // TDD-PROBE: policy::allows_production_observability_claim | 变异：对 TracingMin 返回 true | 红=no_tier_allows_production_claim | 绿=no_tier_allows_production_claim

use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once};

use instrumentationx::Instrumentation;
use observex::{
    CountingInstrumentation, ExportError, ExportingInstrumentation, InMemoryExporter, MetricEvent,
    SpanEvent, TelemetryExporter, TracingInstrumentation, DEFAULT_BUFFER_CAPACITY,
};
use tracing_subscriber::fmt::MakeWriter;

fn span(name: impl Into<String>) -> SpanEvent {
    SpanEvent {
        name: name.into(),
        start_unix_ms: 0,
        attributes: Vec::new(),
    }
}

fn metric(name: impl Into<String>) -> MetricEvent {
    MetricEvent {
        name: name.into(),
        value: 1,
        attributes: Vec::new(),
    }
}

/// 捕获 tracing 输出的测试替身。
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 安装一次性全局 no-op subscriber，避免 callsite 兴趣缓存被污染（与 crate 内测试同策）。
fn ensure_global_default() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(io::sink)
                .with_ansi(false)
                .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
                .finish(),
        );
    });
}

fn with_capture(f: impl FnOnce()) -> String {
    use tracing_subscriber::util::SubscriberInitExt;

    ensure_global_default();
    let cap = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(cap.clone())
        .with_ansi(false)
        .with_target(false)
        .without_time()
        .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
        .finish();
    let guard = subscriber.set_default();
    f();
    drop(guard);
    let _ = cap.make_writer().flush();
    cap.text()
}

/// `TracingInstrumentation::new`：经 `dyn` 记录三类事件，且 `op` 已被清理。
#[test]
fn tracing_new_emits_sanitized_events() {
    let out = with_capture(|| {
        let instrumentation = TracingInstrumentation::new();
        let dyn_ref: &dyn Instrumentation = &instrumentation;
        let malicious = format!("{}\nsecret-suffix", "配".repeat(80));
        dyn_ref.record_retry(&malicious, 3);
        dyn_ref.record_circuit_open("api.op");
        dyn_ref.record_circuit_close("api.op");
    });
    for needle in ["retry", "circuit_open", "circuit_close"] {
        assert!(out.contains(needle), "应捕获 {needle:?}；实际输出：\n{out}");
    }
    assert!(
        !out.contains("secret-suffix"),
        "未经清理的 op 不得进入 tracing"
    );
}

/// `CountingInstrumentation::new`：三类调用各自计数，并透传最近 attempt。
#[test]
fn counting_new_counts_via_trait() {
    let counting = CountingInstrumentation::new();
    let dyn_ref: &dyn Instrumentation = &counting;
    dyn_ref.record_retry("op", 2);
    dyn_ref.record_circuit_open("op");
    dyn_ref.record_circuit_close("op");
    assert_eq!(counting.retry_count(), 1);
    assert_eq!(counting.open_count(), 1);
    assert_eq!(counting.close_count(), 1);
    assert_eq!(counting.last_attempt(), 2);
}

/// `CountingInstrumentation::retry_count`：计数累积与 reset 归零。
#[test]
fn retry_count_tracks_attempts() {
    let counting = CountingInstrumentation::new();
    assert_eq!(counting.retry_count(), 0);
    for attempt in 1..=3 {
        counting.record_retry("op", attempt);
    }
    assert_eq!(counting.retry_count(), 3, "retry_count 应只反映重试次数");
    assert_eq!(counting.open_count(), 0, "重试不得计入 open");
    assert_eq!(counting.last_attempt(), 3);
    counting.reset();
    assert_eq!(counting.retry_count(), 0);
    assert_eq!(counting.last_attempt(), 0);
}

/// `ExportingInstrumentation::new`：inner 先记录，exporter 收到 span 与 metric。
#[test]
fn exporting_new_wires_inner_and_exporter() {
    let counting = CountingInstrumentation::new();
    let exporter = InMemoryExporter::with_capacity(8);
    let instrumentation = ExportingInstrumentation::new(&counting, &exporter);
    assert_eq!(instrumentation.inner().retry_count(), 0);
    assert!(instrumentation.exporter().buffered_spans().is_empty());

    instrumentation.record_retry("api.op", 4);
    assert_eq!(counting.retry_count(), 1, "inner 必须先被记录");
    let spans = exporter.buffered_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "retry:api.op");
    assert_eq!(exporter.buffered_metrics().len(), 1);
    assert_eq!(instrumentation.export_stats().failed_export_calls, 0);
}

/// `ExportingInstrumentation::flush`：清空缓冲并累计已 flush 计数。
#[test]
fn flush_drains_buffer() {
    let counting = CountingInstrumentation::new();
    let exporter = InMemoryExporter::with_capacity(8);
    let instrumentation = ExportingInstrumentation::new(&counting, &exporter);
    instrumentation.record_retry("api.op", 1);
    instrumentation.record_circuit_open("api.op");
    instrumentation.record_circuit_close("api.op");
    assert_eq!(exporter.buffered_spans().len(), 3);

    instrumentation.flush().expect("flush 应成功");
    assert!(exporter.buffered_spans().is_empty(), "flush 后缓冲必须清空");
    assert!(exporter.buffered_metrics().is_empty());
    let stats = exporter.stats();
    assert_eq!(stats.flushed_spans, 3);
    assert_eq!(stats.flushed_metrics, 1);
    assert!(!stats.is_shutdown);
}

/// `ExportingInstrumentation::shutdown`：先 flush 再关闭，且重复调用幂等。
#[test]
fn shutdown_flushes_and_is_idempotent() {
    let counting = CountingInstrumentation::new();
    let exporter = InMemoryExporter::with_capacity(8);
    let instrumentation = ExportingInstrumentation::new(&counting, &exporter);
    instrumentation.record_retry("api.op", 1);
    instrumentation.record_circuit_open("api.op");

    instrumentation.shutdown().expect("首次关闭");
    let first = exporter.stats();
    assert_eq!(first.flushed_spans, 2, "关闭必须先把待处理数据计入 flushed");
    assert_eq!(first.buffered_spans, 0);
    assert!(first.is_shutdown);

    instrumentation.shutdown().expect("重复关闭应成功");
    assert_eq!(exporter.stats(), first, "重复关闭不得重复计数");
    assert_eq!(
        exporter.export_spans(&[span("late")]),
        Err(ExportError::Shutdown)
    );
}

/// `InMemoryExporter::new`：每类信号默认容量 `DEFAULT_BUFFER_CAPACITY`，可显式覆盖。
#[test]
fn memory_exporter_default_capacity() {
    assert_eq!(DEFAULT_BUFFER_CAPACITY, 1_024);
    let exporter = InMemoryExporter::new();
    assert_eq!(
        exporter.stats().capacity_per_signal,
        DEFAULT_BUFFER_CAPACITY
    );
    assert_eq!(
        InMemoryExporter::default().stats().capacity_per_signal,
        DEFAULT_BUFFER_CAPACITY
    );
    assert_eq!(
        InMemoryExporter::with_capacity(3)
            .stats()
            .capacity_per_signal,
        3
    );
    assert_eq!(exporter.export_spans(&[span("ok")]), Ok(()));
    assert_eq!(exporter.stats().buffered_spans, 1);
}

/// `InMemoryExporter::stats`：同一临界区内给出 buffered/flushed/dropped 一致快照。
#[test]
fn stats_snapshot_is_consistent() {
    let exporter = InMemoryExporter::with_capacity(2);
    exporter.export_spans(&[span("s1")]).expect("首次");
    assert_eq!(
        exporter.export_spans(&[span("s2"), span("s3")]),
        Err(ExportError::BufferFull),
        "容量不足应整批拒绝"
    );
    exporter.export_metrics(&[metric("m1")]).expect("metric");

    let stats = exporter.stats();
    assert_eq!(stats.capacity_per_signal, 2);
    assert_eq!(stats.buffered_spans, 1, "整批拒绝不得留下部分写入");
    assert_eq!(stats.buffered_metrics, 1);
    assert_eq!(stats.dropped_spans, 2, "dropped 应记整批长度");
    assert_eq!(stats.dropped_metrics, 0);

    exporter.flush().expect("flush");
    let after = exporter.stats();
    assert_eq!(after.buffered_spans, 0);
    assert_eq!(after.flushed_spans, 1);
    assert_eq!(after.flushed_metrics, 1);
    assert_eq!(after.dropped_spans, 2, "flush 不得改动 dropped");
}

/// `ops::sanitize_op`：控制字符 / trim / 空值回落 / 128 字节上限与 UTF-8 边界。
#[test]
fn sanitize_op_bounds_and_strips_control_chars() {
    assert_eq!(observex::sanitize_op("   "), "_");
    assert_eq!(observex::sanitize_op("\0 api.fetch \0"), "api.fetch");
    assert_eq!(observex::sanitize_op("api.fetch"), "api.fetch");

    let malicious = format!("{}\nsecret-suffix", "配".repeat(80));
    let sanitized = observex::sanitize_op(&malicious);
    assert!(
        sanitized.len() <= observex::MAX_OP_BYTES,
        "清理结果不得超过 {} 字节",
        observex::MAX_OP_BYTES
    );
    assert!(!sanitized.chars().any(char::is_control));
    assert!(!sanitized.contains("secret-suffix"));
    assert!(
        sanitized.is_char_boundary(sanitized.len()),
        "必须是合法 UTF-8"
    );

    let max = observex::MAX_OP_BYTES;
    assert_eq!(observex::sanitize_op(&"a".repeat(max)).len(), max);
    let over = observex::sanitize_op(&"a".repeat(max + 50));
    assert!(over.len() <= max);
    assert!(over.ends_with('~'), "超长应带截断标记");
}

/// `policy::allows_production_observability_claim`：任何级别都不得宣称生产可观测完成。
#[test]
fn no_tier_allows_production_claim() {
    use observex::ObservabilityTier;

    for tier in [
        ObservabilityTier::TracingMin,
        ObservabilityTier::CountingTest,
        ObservabilityTier::OtelDeferred,
    ] {
        assert!(
            !observex::allows_production_observability_claim(tier),
            "{tier:?} 不得允许生产可观测声明"
        );
    }
    assert!(!observex::allows_production_observability_claim(
        observex::tier_tracing()
    ));
    assert!(!observex::allows_production_observability_claim(
        observex::tier_counting()
    ));
    assert!(!observex::claims_otel_export_complete());
    assert!(!observex::counting_is_production_metrics());
    assert!(observex::policy_summary().contains("otel-sdk=no"));
}
