//! [`ExportingInstrumentation`] 的四个 impl 块与时戳辅助。
//!
//! 从门面 `export.rs` 下沉（`MR-STRUCT-007` 腾余量）。类型的**定义与字段**留在门面；
//! `call_exporter` 与 `now_unix_ms` 只在本模块内被调用，**保持私有**。
//! 本文件**无任何可见性调整**。

use super::*;

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
