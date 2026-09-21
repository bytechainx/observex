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

impl Default for InMemoryExporter {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_CAPACITY)
    }
}

impl InMemoryExporter {
    /// 以 [`DEFAULT_BUFFER_CAPACITY`] 作为每类信号容量构造。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 以显式的每类信号容量构造；`0` 会拒绝所有非空批次。
    #[must_use]
    pub fn with_capacity(capacity_per_signal: usize) -> Self {
        Self {
            inner: Mutex::new(MemExportState {
                capacity_per_signal,
                spans: Vec::new(),
                metrics: Vec::new(),
                flushed_spans: 0,
                flushed_metrics: 0,
                dropped_spans: 0,
                dropped_metrics: 0,
                counters_saturated: false,
                shutdown: false,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, MemExportState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 在同一把锁下读取完整统计，避免多个独立访问器之间发生竞态。
    #[must_use]
    pub fn stats(&self) -> InMemoryExporterStats {
        let state = self.state();
        InMemoryExporterStats {
            capacity_per_signal: state.capacity_per_signal,
            buffered_spans: state.spans.len(),
            buffered_metrics: state.metrics.len(),
            flushed_spans: state.flushed_spans,
            flushed_metrics: state.flushed_metrics,
            dropped_spans: state.dropped_spans,
            dropped_metrics: state.dropped_metrics,
            counters_saturated: state.counters_saturated,
            is_shutdown: state.shutdown,
        }
    }

    /// 当前缓冲 spans。
    #[must_use]
    pub fn buffered_spans(&self) -> Vec<SpanEvent> {
        self.state().spans.clone()
    }

    /// 当前缓冲 metrics。
    #[must_use]
    pub fn buffered_metrics(&self) -> Vec<MetricEvent> {
        self.state().metrics.clone()
    }

    /// 已 flush 的 span 累计数。
    #[must_use]
    pub fn flushed_span_count(&self) -> usize {
        self.state().flushed_spans
    }

    /// 已 flush 的 metric 累计数。
    #[must_use]
    pub fn flushed_metric_count(&self) -> usize {
        self.state().flushed_metrics
    }

    /// 因容量不足而整批丢弃的 span 累计数。
    #[must_use]
    pub fn dropped_span_count(&self) -> usize {
        self.state().dropped_spans
    }

    /// 因容量不足而整批丢弃的 metric 累计数。
    #[must_use]
    pub fn dropped_metric_count(&self) -> usize {
        self.state().dropped_metrics
    }

    /// 是否已 shutdown。
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.state().shutdown
    }
}

fn flush_state(state: &mut MemExportState) {
    let (flushed_spans, spans_saturated) = add_count(state.flushed_spans, state.spans.len());
    let (flushed_metrics, metrics_saturated) =
        add_count(state.flushed_metrics, state.metrics.len());
    state.flushed_spans = flushed_spans;
    state.flushed_metrics = flushed_metrics;
    state.counters_saturated |= spans_saturated || metrics_saturated;
    state.spans.clear();
    state.metrics.clear();
}

fn add_count(current: usize, amount: usize) -> (usize, bool) {
    current
        .checked_add(amount)
        .map_or((usize::MAX, true), |value| (value, false))
}

impl TelemetryExporter for InMemoryExporter {
    fn export_spans(&self, spans: &[SpanEvent]) -> Result<(), ExportError> {
        let mut g = self.state();
        if g.shutdown {
            return Err(ExportError::Shutdown);
        }
        if spans.len() > g.capacity_per_signal.saturating_sub(g.spans.len()) {
            let (dropped, saturated) = add_count(g.dropped_spans, spans.len());
            g.dropped_spans = dropped;
            g.counters_saturated |= saturated;
            return Err(ExportError::BufferFull);
        }
        g.spans.extend(spans.iter().cloned());
        Ok(())
    }

    fn export_metrics(&self, metrics: &[MetricEvent]) -> Result<(), ExportError> {
        let mut g = self.state();
        if g.shutdown {
            return Err(ExportError::Shutdown);
        }
        if metrics.len() > g.capacity_per_signal.saturating_sub(g.metrics.len()) {
            let (dropped, saturated) = add_count(g.dropped_metrics, metrics.len());
            g.dropped_metrics = dropped;
            g.counters_saturated |= saturated;
            return Err(ExportError::BufferFull);
        }
        g.metrics.extend(metrics.iter().cloned());
        Ok(())
    }

    fn flush(&self) -> Result<(), ExportError> {
        let mut g = self.state();
        if g.shutdown {
            return Err(ExportError::Shutdown);
        }
        flush_state(&mut g);
        Ok(())
    }

    fn shutdown(&self) -> Result<(), ExportError> {
        let mut g = self.state();
        if g.shutdown {
            return Ok(());
        }
        flush_state(&mut g);
        g.shutdown = true;
        Ok(())
    }
}

/// 包装内层 [`Instrumentation`]，将清理后的事件同步写入导出器。
///
/// exporter 返回的 [`ExportError`] 与可展开（unwind）的 Rust panic 都不会改变记录调用的返回；
/// `panic=abort` 不可捕获。inner 始终先执行，失败会进入 [`ExportingInstrumentationStats`]。
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

impl ExportDiagnostics {
    fn add(&self, counter: &AtomicU64, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        let previous = counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(amount))
            })
            .unwrap_or(u64::MAX);
        if previous.checked_add(amount).is_none() {
            self.counters_saturated.store(true, Ordering::Relaxed);
        }
    }

    fn record_failure(&self, spans: usize, metrics: usize, panicked: bool) {
        let _snapshot = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.add(&self.failed_export_calls, 1);
        self.add(&self.unconfirmed_spans, spans);
        self.add(&self.unconfirmed_metrics, metrics);
        if panicked {
            self.add(&self.panicked_export_calls, 1);
        }
    }

    fn stats(&self) -> ExportingInstrumentationStats {
        let _snapshot = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ExportingInstrumentationStats {
            failed_export_calls: self.failed_export_calls.load(Ordering::Relaxed),
            panicked_export_calls: self.panicked_export_calls.load(Ordering::Relaxed),
            unconfirmed_spans: self.unconfirmed_spans.load(Ordering::Relaxed),
            unconfirmed_metrics: self.unconfirmed_metrics.load(Ordering::Relaxed),
            counters_saturated: self.counters_saturated.load(Ordering::Relaxed),
        }
    }
}

impl<I, E> ExportingInstrumentation<I, E> {
    /// 构造。
    #[must_use]
    pub fn new(inner: I, exporter: E) -> Self {
        Self {
            inner,
            exporter,
            diagnostics: ExportDiagnostics::default(),
        }
    }

    /// 内层 instrumentation。
    #[must_use]
    pub fn inner(&self) -> &I {
        &self.inner
    }

    /// 导出器。
    #[must_use]
    pub fn exporter(&self) -> &E {
        &self.exporter
    }

    /// 读取同一诊断临界区内的原子计数一致性快照。
    ///
    /// `unconfirmed_*` 表示失败调用涉及且本 wrapper 不重试的事件；exporter 可能已产生部分
    /// 副作用，因此这些计数不代表实际丢弃量。
    #[must_use]
    pub fn export_stats(&self) -> ExportingInstrumentationStats {
        self.diagnostics.stats()
    }
}

impl<I, E> ExportingInstrumentation<I, E>
where
    E: TelemetryExporter,
{
    fn call_exporter(
        &self,
        spans: usize,
        metrics: usize,
        call: impl FnOnce() -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        match catch_unwind(AssertUnwindSafe(call)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.diagnostics.record_failure(spans, metrics, false);
                Err(error)
            }
            Err(_) => {
                self.diagnostics.record_failure(spans, metrics, true);
                Err(ExportError::Panicked)
            }
        }
    }

    fn export_spans(&self, spans: &[SpanEvent]) -> Result<(), ExportError> {
        self.call_exporter(spans.len(), 0, || self.exporter.export_spans(spans))
    }

    fn export_metrics(&self, metrics: &[MetricEvent]) -> Result<(), ExportError> {
        self.call_exporter(0, metrics.len(), || self.exporter.export_metrics(metrics))
    }

    /// 刷新导出器。
    pub fn flush(&self) -> Result<(), ExportError> {
        self.call_exporter(0, 0, || self.exporter.flush())
    }

    /// 关闭导出器（幂等）。
    pub fn shutdown(&self) -> Result<(), ExportError> {
        self.call_exporter(0, 0, || self.exporter.shutdown())
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl<I, E> Instrumentation for ExportingInstrumentation<I, E>
where
    I: Instrumentation,
    E: TelemetryExporter,
{
    fn record_retry(&self, op: &str, attempt: u32) {
        let op = sanitize_op(op);
        self.inner.record_retry(&op, attempt);
        let span = SpanEvent {
            name: format!("retry:{op}"),
            start_unix_ms: now_unix_ms(),
            attributes: vec![("attempt".into(), attempt.to_string())],
        };
        let metric = MetricEvent {
            name: "retry".into(),
            value: i64::from(attempt),
            attributes: vec![("op".into(), op)],
        };
        let _ = self.export_spans(std::slice::from_ref(&span));
        let _ = self.export_metrics(std::slice::from_ref(&metric));
    }

    fn record_circuit_open(&self, op: &str) {
        let op = sanitize_op(op);
        self.inner.record_circuit_open(&op);
        let span = SpanEvent {
            name: format!("circuit_open:{op}"),
            start_unix_ms: now_unix_ms(),
            attributes: Vec::new(),
        };
        let _ = self.export_spans(std::slice::from_ref(&span));
    }

    fn record_circuit_close(&self, op: &str) {
        let op = sanitize_op(op);
        self.inner.record_circuit_close(&op);
        let span = SpanEvent {
            name: format!("circuit_close:{op}"),
            start_unix_ms: now_unix_ms(),
            attributes: Vec::new(),
        };
        let _ = self.export_spans(std::slice::from_ref(&span));
    }
}

impl TelemetryExporter for &InMemoryExporter {
    fn export_spans(&self, spans: &[SpanEvent]) -> Result<(), ExportError> {
        (*self).export_spans(spans)
    }
    fn export_metrics(&self, metrics: &[MetricEvent]) -> Result<(), ExportError> {
        (*self).export_metrics(metrics)
    }
    fn flush(&self) -> Result<(), ExportError> {
        (*self).flush()
    }
    fn shutdown(&self) -> Result<(), ExportError> {
        (*self).shutdown()
    }
}

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
