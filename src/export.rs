//! 自定义进程内遥测导出面。
//!
//! 本模块不是 OpenTelemetry API/SDK，不实现 OTLP，也不承诺 OpenTelemetry 的信封或生命周期语义。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use instrumentationx::Instrumentation;

use crate::sanitize_op;

/// [`InMemoryExporter`] 每类信号的默认事件容量。
pub const DEFAULT_BUFFER_CAPACITY: usize = 1_024;

/// 导出错误。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExportError {
    /// 导出器已关闭。
    #[error("遥测导出器已关闭")]
    Shutdown,
    /// 内部不可用。
    #[error("遥测导出器内部不可用")]
    Unavailable,
    /// 当前信号缓冲容量不足；整批事件均未写入。
    #[error("遥测导出器缓冲区已满")]
    BufferFull,
    /// 导出器发生可展开（unwind）的 Rust panic，已在包装边界隔离。
    #[error("遥测导出器发生可展开 panic")]
    Panicked,
}

/// 自定义简化 span 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEvent {
    /// 操作名。
    pub name: String,
    /// 开始时间（unix ms，可选近似）。
    pub start_unix_ms: u64,
    /// 属性（扁平）。
    pub attributes: Vec<(String, String)>,
}

/// 简化 metric 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricEvent {
    /// 指标名。
    pub name: String,
    /// 数值。
    pub value: i64,
    /// 属性。
    pub attributes: Vec<(String, String)>,
}

/// 同步遥测导出器。
///
/// 每个方法都在调用线程执行。实现必须快速返回，不得等待外部 I/O 或执行无界阻塞；允许有界的
/// 短临界区本地同步。第三方实现违反该约束时，调用线程仍可能被阻塞。
/// [`ExportingInstrumentation`] 会内化记录路径的 [`ExportError`]，并将可展开（unwind）的 Rust
/// panic 转为诊断计数；`panic=abort` 不可捕获。
/// 本 trait 不提供线程隔离、超时、重试，也不是 OpenTelemetry exporter 接口。
pub trait TelemetryExporter: Send + Sync {
    /// 同步导出 spans；批次原子性由实现定义。
    fn export_spans(&self, spans: &[SpanEvent]) -> Result<(), ExportError>;
    /// 同步导出 metrics；批次原子性由实现定义。
    fn export_metrics(&self, metrics: &[MetricEvent]) -> Result<(), ExportError>;
    /// 完成实现定义的刷新。
    fn flush(&self) -> Result<(), ExportError>;
    /// 关闭；后续 export 应失败；幂等。
    fn shutdown(&self) -> Result<(), ExportError>;
}

#[derive(Debug)]
struct MemExportState {
    capacity_per_signal: usize,
    spans: Vec<SpanEvent>,
    metrics: Vec<MetricEvent>,
    /// flush 后累计处置的 span 数。
    flushed_spans: usize,
    flushed_metrics: usize,
    dropped_spans: usize,
    dropped_metrics: usize,
    counters_saturated: bool,
    shutdown: bool,
}

/// [`InMemoryExporter`] 的一致性统计快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InMemoryExporterStats {
    /// 每类信号各自独立的容量。
    pub capacity_per_signal: usize,
    /// 当前缓冲 span 数。
    pub buffered_spans: usize,
    /// 当前缓冲 metric 数。
    pub buffered_metrics: usize,
    /// 已 flush 的 span 累计数。
    pub flushed_spans: usize,
    /// 已 flush 的 metric 累计数。
    pub flushed_metrics: usize,
    /// 因 span 缓冲容量不足而整批丢弃的 span 累计数。
    pub dropped_spans: usize,
    /// 因 metric 缓冲容量不足而整批丢弃的 metric 累计数。
    pub dropped_metrics: usize,
    /// 是否有累计计数超过 `usize` 表示范围。
    ///
    /// 为 `true` 时，至少一个 flushed/dropped 字段已饱和为 `usize::MAX`，只能解释为下界。
    pub counters_saturated: bool,
    /// 是否已关闭。
    pub is_shutdown: bool,
}

/// 有界进程内 sink。
///
/// span 与 metric 各自拥有 `capacity_per_signal` 个槽位。单次 `export_spans` 或
/// `export_metrics` 容量不足时整批拒绝、原缓冲不变，并累计对应 dropped 数；跨两种信号的
/// 多次调用不具备事务原子性。`shutdown` 在同一临界区先把待处理数计入 flushed，再清空并关闭。
/// 数据只存在当前进程内，不落盘、不远程发送；本类型不是 OpenTelemetry SDK/OTLP exporter。
/// 容量限制的是事件数量；直接调用 exporter 时，调用方提供的事件字段字节数另行占用内存。
#[derive(Debug)]
pub struct InMemoryExporter {
    inner: Mutex<MemExportState>,
}

/// 包装内层 [`Instrumentation`]，将清理后的事件同步写入导出器。
///
/// exporter 返回的 [`ExportError`] 与可展开（unwind）的 Rust panic 都不会改变记录调用的返回；
/// `panic=abort` 不可捕获。inner 始终先执行，失败会进入 [`ExportingInstrumentationStats`]。
/// `record_*` 转发路径的导出失败无法经 `Result` 传播给调用方，会以 `tracing::warn!` 发出
/// 结构化日志（指数采样防日志风暴，精确总数见诊断计数器）。
/// 由于 [`TelemetryExporter`] 是同步接口，违反非阻塞合同的第三方实现仍会阻塞当前线程。
/// 本类型不提供异步队列、线程隔离或超时。
pub struct ExportingInstrumentation<I, E> {
    inner: I,
    exporter: E,
    diagnostics: ExportDiagnostics,
}

#[derive(Debug, Default)]
struct ExportDiagnostics {
    snapshot_lock: Mutex<()>,
    failed_export_calls: AtomicU64,
    panicked_export_calls: AtomicU64,
    unconfirmed_spans: AtomicU64,
    unconfirmed_metrics: AtomicU64,
    counters_saturated: AtomicBool,
    /// 转发路径（`record_*`）累计导出失败次数，作为日志指数采样的依据。
    forward_failures: AtomicU64,
    /// 实际发出的转发失败 warn 日志条数；与 `forward_failures` 的差即被采样抑制数。
    logged_forward_failures: AtomicU64,
}

/// [`ExportingInstrumentation`] 的导出失败诊断快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportingInstrumentationStats {
    /// 返回错误或发生 unwind panic 的 exporter 调用数。
    pub failed_export_calls: u64,
    /// 其中发生 unwind panic 并被隔离的 exporter 调用数。
    pub panicked_export_calls: u64,
    /// 失败调用涉及且 wrapper 不重试、交付状态未知的 span 事件数。
    pub unconfirmed_spans: u64,
    /// 失败调用涉及且 wrapper 不重试、交付状态未知的 metric 事件数。
    pub unconfirmed_metrics: u64,
    /// 是否有诊断计数超过 `u64` 表示范围；为真时饱和值只能解释为下界。
    pub counters_saturated: bool,
}

mod forwarding;
mod memory;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::CountingInstrumentation;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

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

    #[test]
    fn record_export_flush_shutdown() {
        let counting = CountingInstrumentation::new();
        let exporter = InMemoryExporter::new();
        let instr = ExportingInstrumentation::new(&counting, &exporter);
        // 访问器
        let _ = instr.inner();
        let _ = instr.exporter();
        assert_eq!(
            instr.export_stats(),
            ExportingInstrumentationStats {
                failed_export_calls: 0,
                panicked_export_calls: 0,
                unconfirmed_spans: 0,
                unconfirmed_metrics: 0,
                counters_saturated: false,
            }
        );
        instr.record_retry("op", 1);
        instr.record_circuit_open("op");
        instr.record_circuit_close("op");
        assert_eq!(counting.retry_count(), 1);
        assert!(!exporter.buffered_spans().is_empty());
        assert!(!exporter.buffered_metrics().is_empty());
        instr.flush().unwrap();
        assert!(exporter.buffered_spans().is_empty());
        assert_eq!(exporter.flushed_span_count(), 3);
        assert_eq!(exporter.flushed_metric_count(), 1);
        instr.shutdown().unwrap();
        instr.shutdown().unwrap(); // idempotent
        assert!(exporter.is_shutdown());
        assert_eq!(exporter.export_spans(&[]), Err(ExportError::Shutdown));
        assert_eq!(exporter.export_metrics(&[]), Err(ExportError::Shutdown));
        assert_eq!(exporter.flush(), Err(ExportError::Shutdown));
        assert_eq!(ExportError::Shutdown.to_string(), "遥测导出器已关闭");
        assert_eq!(ExportError::Unavailable.to_string(), "遥测导出器内部不可用");
        assert_eq!(ExportError::BufferFull.to_string(), "遥测导出器缓冲区已满");
        assert_eq!(
            ExportError::Panicked.to_string(),
            "遥测导出器发生可展开 panic"
        );
    }

    #[test]
    fn capacity_is_per_signal_and_batches_are_all_or_nothing() {
        let exporter = InMemoryExporter::with_capacity(2);
        assert_eq!(exporter.stats().capacity_per_signal, 2);

        exporter.export_spans(&[span("kept")]).unwrap();
        assert_eq!(
            exporter.export_spans(&[span("rejected-1"), span("rejected-2")]),
            Err(ExportError::BufferFull)
        );
        assert_eq!(exporter.buffered_spans(), vec![span("kept")]);
        assert_eq!(exporter.dropped_span_count(), 2);

        exporter
            .export_metrics(&[metric("m1"), metric("m2")])
            .unwrap();
        assert_eq!(
            exporter.export_metrics(&[metric("m3")]),
            Err(ExportError::BufferFull)
        );
        assert_eq!(
            exporter.buffered_metrics(),
            vec![metric("m1"), metric("m2")]
        );
        assert_eq!(exporter.dropped_metric_count(), 1);

        exporter.flush().unwrap();
        exporter
            .export_spans(&[span("reused-1"), span("reused-2")])
            .unwrap();
        let stats = exporter.stats();
        assert_eq!(stats.buffered_spans, 2);
        assert_eq!(stats.flushed_spans, 1);
        assert_eq!(stats.flushed_metrics, 2);
        assert_eq!(stats.dropped_spans, 2);
        assert_eq!(stats.dropped_metrics, 1);
    }

    #[test]
    fn zero_capacity_rejects_non_empty_batches_but_accepts_empty_batches() {
        let exporter = InMemoryExporter::with_capacity(0);
        exporter.export_spans(&[]).unwrap();
        exporter.export_metrics(&[]).unwrap();
        assert_eq!(
            exporter.export_spans(&[span("x")]),
            Err(ExportError::BufferFull)
        );
        assert_eq!(
            exporter.export_metrics(&[metric("x")]),
            Err(ExportError::BufferFull)
        );
        let stats = exporter.stats();
        assert_eq!(stats.buffered_spans, 0);
        assert_eq!(stats.buffered_metrics, 0);
        assert_eq!(stats.dropped_spans, 1);
        assert_eq!(stats.dropped_metrics, 1);
    }

    #[test]
    fn shutdown_flushes_pending_data_and_is_idempotent() {
        let exporter = InMemoryExporter::with_capacity(4);
        exporter.export_spans(&[span("s1"), span("s2")]).unwrap();
        exporter.export_metrics(&[metric("m1")]).unwrap();

        exporter.shutdown().unwrap();
        let first = exporter.stats();
        assert_eq!(first.buffered_spans, 0);
        assert_eq!(first.buffered_metrics, 0);
        assert_eq!(first.flushed_spans, 2);
        assert_eq!(first.flushed_metrics, 1);
        assert!(first.is_shutdown);

        exporter.shutdown().unwrap();
        assert_eq!(exporter.stats(), first);
        assert_eq!(
            exporter.export_spans(&[span("late")]),
            Err(ExportError::Shutdown)
        );
        assert_eq!(
            exporter.export_metrics(&[metric("late")]),
            Err(ExportError::Shutdown)
        );
        assert_eq!(exporter.flush(), Err(ExportError::Shutdown));
        assert_eq!(exporter.stats(), first);
    }

    #[test]
    fn concurrent_exports_remain_bounded_and_accounted() {
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
            handle.join().unwrap();
        }
        let stats = exporter.stats();
        assert_eq!(stats.buffered_spans, CAPACITY);
        assert_eq!(stats.dropped_spans, THREADS * PER_THREAD - CAPACITY);
        assert_eq!(
            stats.buffered_spans + stats.dropped_spans,
            THREADS * PER_THREAD
        );
    }

    #[test]
    fn concurrent_shutdown_accounts_every_accepted_event() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 100;
        let exporter = Arc::new(InMemoryExporter::with_capacity(THREADS * PER_THREAD));
        let barrier = Arc::new(Barrier::new(THREADS + 1));
        let mut handles = Vec::new();
        for thread_id in 0..THREADS {
            let exporter = Arc::clone(&exporter);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let first = span(format!("shutdown-{thread_id}-0"));
                assert_eq!(exporter.export_spans(std::slice::from_ref(&first)), Ok(()));
                barrier.wait();
                barrier.wait();
                for item in 1..PER_THREAD {
                    let event = span(format!("shutdown-{thread_id}-{item}"));
                    assert_eq!(
                        exporter.export_spans(std::slice::from_ref(&event)),
                        Err(ExportError::Shutdown)
                    );
                }
            }));
        }
        barrier.wait();
        barrier.wait();
        exporter.shutdown().unwrap();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        let stats = exporter.stats();
        assert_eq!(stats.flushed_spans, THREADS);
        assert_eq!(stats.buffered_spans, 0);
        assert_eq!(stats.dropped_spans, 0);
        assert!(stats.is_shutdown);
    }

    #[test]
    fn counter_saturation_is_visible_in_stats() {
        let exporter = InMemoryExporter::with_capacity(0);
        exporter.state().dropped_spans = usize::MAX;
        assert_eq!(
            exporter.export_spans(&[span("overflow")]),
            Err(ExportError::BufferFull)
        );
        let stats = exporter.stats();
        assert_eq!(stats.dropped_spans, usize::MAX);
        assert!(stats.counters_saturated);
    }

    #[test]
    fn poisoned_state_is_recovered_without_losing_buffer_contract() {
        let exporter = Arc::new(InMemoryExporter::with_capacity(2));
        exporter.export_spans(&[span("before-poison")]).unwrap();
        let poisoner = Arc::clone(&exporter);
        let result = std::thread::spawn(move || {
            let _state = poisoner.inner.lock().unwrap();
            panic!("制造 mutex poison 以验证恢复路径");
        })
        .join();
        assert!(result.is_err());

        exporter.export_spans(&[span("after-poison")]).unwrap();
        assert_eq!(exporter.stats().buffered_spans, 2);
        exporter.shutdown().unwrap();
        assert_eq!(exporter.stats().flushed_spans, 2);
    }

    #[test]
    fn exporting_paths_use_only_sanitized_op() {
        let counting = CountingInstrumentation::new();
        let exporter = InMemoryExporter::with_capacity(8);
        let instr = ExportingInstrumentation::new(&counting, &exporter);
        let suffix = "must-not-reach-export";
        let malicious = format!("{}\n\r\t\0\u{7f}\u{85}{suffix}", "配".repeat(80));
        instr.record_retry(&malicious, 7);
        instr.record_circuit_open(&malicious);
        instr.record_circuit_close(&malicious);

        assert_eq!(counting.retry_count(), 1);
        let spans = exporter.buffered_spans();
        let metrics = exporter.buffered_metrics();
        assert_eq!(spans.len(), 3);
        assert_eq!(metrics.len(), 1);
        for value in spans.iter().map(|event| event.name.as_str()).chain(
            metrics[0]
                .attributes
                .iter()
                .map(|(_, value)| value.as_str()),
        ) {
            assert!(!value.chars().any(char::is_control));
            assert!(!value.contains(suffix));
        }
        let op = &metrics[0].attributes[0].1;
        assert!(op.len() <= crate::MAX_OP_BYTES);
        assert_eq!(op, &sanitize_op(&malicious));
    }

    #[derive(Default)]
    struct AcceptsThenErrors {
        calls: AtomicUsize,
        accepted_spans: AtomicUsize,
        accepted_metrics: AtomicUsize,
    }

    impl TelemetryExporter for AcceptsThenErrors {
        fn export_spans(&self, spans: &[SpanEvent]) -> Result<(), ExportError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.accepted_spans
                .fetch_add(spans.len(), Ordering::Relaxed);
            Err(ExportError::Unavailable)
        }

        fn export_metrics(&self, metrics: &[MetricEvent]) -> Result<(), ExportError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.accepted_metrics
                .fetch_add(metrics.len(), Ordering::Relaxed);
            Err(ExportError::Unavailable)
        }

        fn flush(&self) -> Result<(), ExportError> {
            Err(ExportError::Unavailable)
        }

        fn shutdown(&self) -> Result<(), ExportError> {
            Err(ExportError::Unavailable)
        }
    }

    #[test]
    fn accepted_then_error_is_unconfirmed_without_changing_recording() {
        let counting = CountingInstrumentation::new();
        let instr = ExportingInstrumentation::new(&counting, AcceptsThenErrors::default());
        instr.record_retry("op", 1);
        instr.record_circuit_open("op");
        instr.record_circuit_close("op");
        assert_eq!(counting.retry_count(), 1);
        assert_eq!(counting.open_count(), 1);
        assert_eq!(counting.close_count(), 1);
        assert_eq!(instr.exporter().calls.load(Ordering::Relaxed), 4);
        assert_eq!(instr.exporter().accepted_spans.load(Ordering::Relaxed), 3);
        assert_eq!(instr.exporter().accepted_metrics.load(Ordering::Relaxed), 1);
        assert_eq!(instr.flush(), Err(ExportError::Unavailable));
        assert_eq!(instr.shutdown(), Err(ExportError::Unavailable));
        assert_eq!(
            instr.export_stats(),
            ExportingInstrumentationStats {
                failed_export_calls: 6,
                panicked_export_calls: 0,
                unconfirmed_spans: 3,
                unconfirmed_metrics: 1,
                counters_saturated: false,
            }
        );
    }

    #[test]
    fn exporter_diagnostic_saturation_is_visible() {
        let instr = ExportingInstrumentation::new(
            CountingInstrumentation::new(),
            AcceptsThenErrors::default(),
        );
        instr
            .diagnostics
            .failed_export_calls
            .store(u64::MAX, Ordering::Relaxed);
        instr.record_circuit_open("op");
        let stats = instr.export_stats();
        assert_eq!(stats.failed_export_calls, u64::MAX);
        assert_eq!(stats.unconfirmed_spans, 1);
        assert!(stats.counters_saturated);
    }

    #[test]
    fn forward_failure_logs_use_exponential_sampling() {
        // 日志风暴防护（R-OBS-004）：同一实例 5 次转发失败，仅第 1、2、4 次记录日志；
        // 诊断计数器与采样计数不受日志采样影响，精确总数始终可查。
        let instr = ExportingInstrumentation::new(
            CountingInstrumentation::new(),
            AcceptsThenErrors::default(),
        );
        for _ in 0..5 {
            instr.record_circuit_open("sampling-op");
        }
        assert_eq!(instr.export_stats().failed_export_calls, 5);
        assert_eq!(
            instr.diagnostics.forward_failures.load(Ordering::Relaxed),
            5
        );
        assert_eq!(
            instr
                .diagnostics
                .logged_forward_failures
                .load(Ordering::Relaxed),
            3
        );
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PanicAt {
        Spans,
        Metrics,
        Flush,
        Shutdown,
    }

    struct Panics {
        at: PanicAt,
        accepted_spans: AtomicUsize,
        accepted_metrics: AtomicUsize,
    }

    impl Panics {
        fn new(at: PanicAt) -> Self {
            Self {
                at,
                accepted_spans: AtomicUsize::new(0),
                accepted_metrics: AtomicUsize::new(0),
            }
        }

        fn boundary(&self, boundary: PanicAt) -> Result<(), ExportError> {
            assert!(self.at != boundary, "同步 exporter panic 边界");
            Ok(())
        }
    }

    impl TelemetryExporter for Panics {
        fn export_spans(&self, spans: &[SpanEvent]) -> Result<(), ExportError> {
            self.accepted_spans
                .fetch_add(spans.len(), Ordering::Relaxed);
            self.boundary(PanicAt::Spans)
        }

        fn export_metrics(&self, metrics: &[MetricEvent]) -> Result<(), ExportError> {
            self.accepted_metrics
                .fetch_add(metrics.len(), Ordering::Relaxed);
            self.boundary(PanicAt::Metrics)
        }

        fn flush(&self) -> Result<(), ExportError> {
            self.boundary(PanicAt::Flush)
        }

        fn shutdown(&self) -> Result<(), ExportError> {
            self.boundary(PanicAt::Shutdown)
        }
    }

    #[test]
    fn accepted_then_unwind_is_unconfirmed_and_isolated() {
        let spans_counting = CountingInstrumentation::new();
        let spans = ExportingInstrumentation::new(&spans_counting, Panics::new(PanicAt::Spans));
        spans.record_retry("op", 1);
        assert_eq!(spans_counting.retry_count(), 1);
        assert_eq!(spans.exporter().accepted_spans.load(Ordering::Relaxed), 1);
        assert_eq!(spans.exporter().accepted_metrics.load(Ordering::Relaxed), 1);
        assert_eq!(spans.export_stats().panicked_export_calls, 1);
        assert_eq!(spans.export_stats().unconfirmed_spans, 1);

        let metrics_counting = CountingInstrumentation::new();
        let metrics =
            ExportingInstrumentation::new(&metrics_counting, Panics::new(PanicAt::Metrics));
        metrics.record_retry("op", 1);
        assert_eq!(metrics_counting.retry_count(), 1);
        assert_eq!(metrics.exporter().accepted_spans.load(Ordering::Relaxed), 1);
        assert_eq!(
            metrics.exporter().accepted_metrics.load(Ordering::Relaxed),
            1
        );
        assert_eq!(metrics.export_stats().panicked_export_calls, 1);
        assert_eq!(metrics.export_stats().unconfirmed_metrics, 1);

        let flush = ExportingInstrumentation::new(
            CountingInstrumentation::new(),
            Panics::new(PanicAt::Flush),
        );
        assert_eq!(flush.flush(), Err(ExportError::Panicked));
        assert_eq!(flush.export_stats().panicked_export_calls, 1);

        let shutdown = ExportingInstrumentation::new(
            CountingInstrumentation::new(),
            Panics::new(PanicAt::Shutdown),
        );
        assert_eq!(shutdown.shutdown(), Err(ExportError::Panicked));
        assert_eq!(shutdown.export_stats().panicked_export_calls, 1);
    }
}
