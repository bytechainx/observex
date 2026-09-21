#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! SDD 规格对照（特性 002）：把 `docs/标准.md` 的章节条款转成可执行断言。
//!
//! // SPEC-MAP: S-1 | 1. 定位 | assert_positioning
//! // SPEC-MAP: S-2 | 2. 字段治理 | assert_field_governance
//! // SPEC-MAP: S-3 | 3. 进程内 sink | assert_in_process_sink
//! // SPEC-MAP: S-4 | 4. 失败与并发 | assert_failure_and_concurrency
//! // SPEC-MAP: S-5 | 5. 验收 | assert_acceptance

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use instrumentationx::Instrumentation;
use observex::{
    allows_production_observability_claim, claims_otel_export_complete,
    counting_is_production_metrics, policy_summary, sanitize_op, CountingInstrumentation,
    ExportError, ExportingInstrumentation, InMemoryExporter, MetricEvent, PrefixedInstrumentation,
    SpanEvent, TelemetryExporter, TracingInstrumentation, DEFAULT_BUFFER_CAPACITY, MAX_OP_BYTES,
};

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

/// 全失败导出器：逐调用返回 `Unavailable`。
#[derive(Default)]
struct AlwaysErr {
    calls: AtomicUsize,
}

impl TelemetryExporter for AlwaysErr {
    fn export_spans(&self, _spans: &[SpanEvent]) -> Result<(), ExportError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ExportError::Unavailable)
    }
    fn export_metrics(&self, _metrics: &[MetricEvent]) -> Result<(), ExportError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ExportError::Unavailable)
    }
    fn flush(&self) -> Result<(), ExportError> {
        Err(ExportError::Unavailable)
    }
    fn shutdown(&self) -> Result<(), ExportError> {
        Err(ExportError::Unavailable)
    }
}

/// 可展开 panic 导出器。
struct PanicSpans;

impl TelemetryExporter for PanicSpans {
    fn export_spans(&self, _spans: &[SpanEvent]) -> Result<(), ExportError> {
        panic!("同步 exporter panic 边界");
    }
    fn export_metrics(&self, _metrics: &[MetricEvent]) -> Result<(), ExportError> {
        Ok(())
    }
    fn flush(&self) -> Result<(), ExportError> {
        Ok(())
    }
    fn shutdown(&self) -> Result<(), ExportError> {
        Ok(())
    }
}

/// S-1：定位——`Instrumentation` 的 tracing 实现 + 有界进程内 sink，不是 OTEL。
#[test]
fn assert_positioning() {
    // 实现共享契约，可经 `dyn` 消费。
    let instrumentation = TracingInstrumentation::new();
    let dyn_ref: &dyn Instrumentation = &instrumentation;
    dyn_ref.record_retry("op", 0);
    dyn_ref.record_circuit_open("op");
    dyn_ref.record_circuit_close("op");

    // 诚实性：不宣称 OTEL SDK / OTLP / 远端持久化。
    assert!(!claims_otel_export_complete());
    assert!(!counting_is_production_metrics());
    let summary = policy_summary();
    assert!(summary.contains("otel-sdk=no"));
    assert!(summary.contains("otlp=no"));
    assert!(summary.contains("bounded-sink=in-process"));
}

/// S-2：字段治理——三条记录路径共用同一 `sanitize_op` 语义。
#[test]
fn assert_field_governance() {
    let malicious = format!("{}\n\r\t\0secret-suffix", "配".repeat(80));
    let sanitized = sanitize_op(&malicious);
    assert!(sanitized.len() <= MAX_OP_BYTES);
    assert!(!sanitized.chars().any(char::is_control));
    assert!(!sanitized.contains("secret-suffix"));
    assert!(sanitized.is_char_boundary(sanitized.len()));
    // trim + 空值回落。
    assert_eq!(sanitize_op("  api.fetch  "), "api.fetch");
    assert_eq!(sanitize_op("\0 \t\r \0"), "_");

    // 直接路径（Tracing 语义的替换实现 `CountingInstrumentation` 不记录 op；
    // 这里经 ExportingInstrumentation 的 span 名观测共享清理路径）。
    let counting = CountingInstrumentation::new();
    let exporter = InMemoryExporter::with_capacity(8);
    let exporting = ExportingInstrumentation::new(&counting, &exporter);
    exporting.record_retry(&malicious, 1);
    assert_eq!(
        exporter.buffered_spans()[0].name,
        format!("retry:{sanitized}")
    );

    // 前缀路径：`api.` 拼接后再次清理，最终仍受同一上限与字符约束。
    let counting = CountingInstrumentation::new();
    let exporter = InMemoryExporter::with_capacity(8);
    let prefixed =
        PrefixedInstrumentation::new("api", ExportingInstrumentation::new(&counting, &exporter));
    prefixed.record_retry(&malicious, 1);
    let expected = sanitize_op(&format!("api.{sanitized}"));
    let name = &exporter.buffered_spans()[0].name;
    assert_eq!(name, &format!("retry:{expected}"));
    assert!(name.len() <= MAX_OP_BYTES + "retry:".len());
    assert!(!name.contains("secret-suffix"));
}

/// S-3：进程内 sink——容量、批次原子性、统计快照、flush 与幂等关闭。
#[test]
fn assert_in_process_sink() {
    // 默认容量与显式容量。
    assert_eq!(DEFAULT_BUFFER_CAPACITY, 1_024);
    assert_eq!(
        InMemoryExporter::new().stats().capacity_per_signal,
        DEFAULT_BUFFER_CAPACITY
    );
    let exporter = InMemoryExporter::with_capacity(2);
    assert_eq!(exporter.stats().capacity_per_signal, 2);

    // span 与 metric 容量相互独立。
    exporter.export_spans(&[span("s1")]).expect("span 1");
    exporter.export_metrics(&[metric("m1")]).expect("metric 1");

    // 超限：整批拒绝、原缓冲不变、dropped 记整批长度。
    assert_eq!(
        exporter.export_spans(&[span("s2"), span("s3")]),
        Err(ExportError::BufferFull)
    );
    assert_eq!(exporter.buffered_spans(), vec![span("s1")]);
    assert_eq!(exporter.stats().dropped_spans, 2);

    // 恰满：容量边界可接受。
    exporter.export_spans(&[span("s2")]).expect("恰满");

    // 同一临界区的一致快照。
    let stats = exporter.stats();
    assert_eq!(stats.buffered_spans, 2);
    assert_eq!(stats.buffered_metrics, 1);
    assert!(!stats.counters_saturated);
    assert!(!stats.is_shutdown);

    // flush 只清进程内缓冲并累计 flushed。
    exporter.flush().expect("flush");
    let flushed = exporter.stats();
    assert_eq!(flushed.buffered_spans, 0);
    assert_eq!(flushed.flushed_spans, 2);
    assert_eq!(flushed.flushed_metrics, 1);

    // shutdown：先计入 flushed 再关闭；重复调用幂等；之后 export/flush 返回 Shutdown。
    exporter.export_spans(&[span("pending")]).expect("pending");
    exporter.shutdown().expect("shutdown");
    let closed = exporter.stats();
    assert_eq!(closed.flushed_spans, 3);
    assert_eq!(closed.buffered_spans, 0);
    assert!(closed.is_shutdown);
    exporter.shutdown().expect("重复 shutdown");
    assert_eq!(exporter.stats(), closed);
    assert_eq!(exporter.export_spans(&[]), Err(ExportError::Shutdown));
    assert_eq!(exporter.export_metrics(&[]), Err(ExportError::Shutdown));
    assert_eq!(exporter.flush(), Err(ExportError::Shutdown));
}

/// S-4：失败与并发——inner 先记录、Err/panic 内化、并发容量守恒。
#[test]
fn assert_failure_and_concurrency() {
    // exporter 返回 Err：inner 已记录，诊断累计，业务调用返回 ()。
    let counting = CountingInstrumentation::new();
    let exporting = ExportingInstrumentation::new(&counting, AlwaysErr::default());
    exporting.record_retry("op", 1);
    exporting.record_circuit_open("op");
    exporting.record_circuit_close("op");
    assert_eq!(counting.retry_count(), 1, "失败不得影响 inner 记录");
    assert_eq!(counting.open_count(), 1);
    assert_eq!(counting.close_count(), 1);
    let stats = exporting.export_stats();
    assert!(stats.failed_export_calls >= 4, "失败调用应被诊断");
    assert_eq!(stats.panicked_export_calls, 0);
    assert!(
        stats.unconfirmed_spans >= 3,
        "unconfirmed 应覆盖涉及的事件数"
    );
    assert_eq!(exporting.flush(), Err(ExportError::Unavailable));

    // 可展开 panic 被隔离：记录路径不 panic，仅计诊断。
    let counting = CountingInstrumentation::new();
    let exporting = ExportingInstrumentation::new(&counting, PanicSpans);
    exporting.record_circuit_open("op");
    assert_eq!(counting.open_count(), 1);
    assert_eq!(exporting.export_stats().panicked_export_calls, 1);

    // 并发：容量守恒（buffered + dropped == 总提交数）。
    const THREADS: usize = 8;
    const PER_THREAD: usize = 32;
    const CAPACITY: usize = 64;
    let exporter = Arc::new(InMemoryExporter::with_capacity(CAPACITY));
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for thread_id in 0..THREADS {
        let exporter = Arc::clone(&exporter);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for item in 0..PER_THREAD {
                let event = span(format!("{thread_id}-{item}"));
                let result = exporter.export_spans(std::slice::from_ref(&event));
                assert!(matches!(result, Ok(()) | Err(ExportError::BufferFull)));
            }
        }));
    }
    for handle in handles {
        handle.join().expect("线程");
    }
    let stats = exporter.stats();
    assert_eq!(stats.buffered_spans, CAPACITY);
    assert_eq!(
        stats.buffered_spans + stats.dropped_spans,
        THREADS * PER_THREAD
    );
}

/// S-5：验收——§5 声明的覆盖域各取一条可执行断言。
#[test]
fn assert_acceptance() {
    // 控制字符与多字节边界。
    let zh = "配置服务";
    for max in [0usize, 1, 3, 4, 7, 12] {
        let truncated = observex::truncate_op(zh, max);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.len() <= max);
    }
    // 容量 0 / 恰满 / 超限。
    let zero = InMemoryExporter::with_capacity(0);
    assert_eq!(zero.export_spans(&[]), Ok(()));
    assert_eq!(
        zero.export_spans(&[span("x")]),
        Err(ExportError::BufferFull)
    );
    let one = InMemoryExporter::with_capacity(1);
    assert_eq!(one.export_spans(&[span("ok")]), Ok(()));
    assert_eq!(
        one.export_spans(&[span("over")]),
        Err(ExportError::BufferFull)
    );
    // 批次原子拒绝。
    assert_eq!(one.buffered_spans(), vec![span("ok")]);
    // 计数饱和字段可见（外部只能观测「未饱和」常态；饱和路径由 crate 内单测覆盖）。
    assert!(!one.stats().counters_saturated);
    // exporter Err 隔离。
    let counting = CountingInstrumentation::new();
    let exporting = ExportingInstrumentation::new(&counting, AlwaysErr::default());
    exporting.record_retry("op", 1);
    assert_eq!(counting.retry_count(), 1);
    assert!(exporting.export_stats().failed_export_calls >= 1);
    // shutdown 自动计数与幂等关闭。
    let exporter = InMemoryExporter::with_capacity(4);
    exporter.export_spans(&[span("pending")]).expect("pending");
    exporter.shutdown().expect("shutdown");
    let snapshot = exporter.stats();
    assert_eq!(snapshot.flushed_spans, 1);
    assert!(snapshot.is_shutdown);
    exporter.shutdown().expect("幂等");
    assert_eq!(exporter.stats(), snapshot);
    // 诚实性断言。
    assert!(!allows_production_observability_claim(
        observex::tier_tracing()
    ));
}
