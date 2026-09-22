//! [`InMemoryExporter`] 的三个 impl 块与两个纯计数器辅助。
//!
//! 从门面 `export.rs` 下沉（`MR-STRUCT-007` 腾余量）。`InMemoryExporter` /
//! `MemExportState` / `InMemoryExporterStats` 的**定义与字段**留在门面（子模块可访问父模块
//! 私有项，故字段无需提级）；`flush_state` / `add_count` 只在本模块内被调用，**保持私有**。
//! 唯一放宽的是 `state()`：门面内联测试 `counter_saturation_is_visible_in_stats` 直接读
//! `MemExportState` 的字段把它推饱和，故提为 `pub(super)`。

use super::*;

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

    pub(super) fn state(&self) -> MutexGuard<'_, MemExportState> {
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
