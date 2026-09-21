#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! AIDD 对抗 / 边界用例（特性 002）。
//!
//! 候选由 AI 生成，逐条人工复核后仅保留「结论=保留」项；丢弃项登记于 PR 描述。
//!
//! // AIDD: 多字节 op 在任意字节上限截断 | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §2 UTF-8 字节上限且落在字符边界 | 结论=保留
//! // AIDD: 容量 0 的 sink | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §3 容量 0 拒绝非空批次 | 结论=保留
//! // AIDD: exporter 恒返回 Err | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §4 Err 被内化且不改业务记录 | 结论=保留
//! // AIDD: exporter 发生可展开 panic | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §4 panic 在包装边界隔离 | 结论=保留
//! // AIDD: 多线程接受事件后 shutdown | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §3 shutdown 在同一临界区先 flush 计数 | 结论=保留
//! // AIDD: 空串与全空白 op | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §2 空值回落 `_` | 结论=保留

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use instrumentationx::Instrumentation;
use observex::{
    is_friendly_op, join_op_segments, op_depth, op_leaf, sanitize_op, truncate_op,
    CountingInstrumentation, ExportError, ExportingInstrumentation, InMemoryExporter, MetricEvent,
    SpanEvent, TelemetryExporter,
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

/// 边界：多字节 op 在任意字节上限截断，结果必须仍是合法 UTF-8 且不超限。
#[test]
fn multibyte_truncation_stays_on_boundary() {
    let zh = "配置服务";
    assert_eq!(zh.len(), 12);
    for max in 0usize..=14 {
        let truncated = truncate_op(zh, max);
        assert!(
            truncated.is_char_boundary(truncated.len()),
            "max={max} 结果必须是合法 UTF-8：{truncated:?}"
        );
        assert!(truncated.len() <= max, "max={max} 结果超限：{truncated:?}");
        if max == 0 {
            assert!(truncated.is_empty());
        } else if max < zh.len() {
            assert!(
                truncated.ends_with('~'),
                "max={max} 应带截断标记：{truncated:?}"
            );
        } else {
            assert_eq!(truncated, zh);
        }
    }
}

/// 边界：容量 0 的 sink——拒绝一切非空批次，但仍接受空批次。
#[test]
fn zero_capacity_rejects_non_empty_batches() {
    let exporter = InMemoryExporter::with_capacity(0);
    assert_eq!(exporter.export_spans(&[]), Ok(()));
    assert_eq!(exporter.export_metrics(&[]), Ok(()));
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

/// 边界：exporter 恒返回 Err——不得改变业务记录、不得 panic。
#[test]
fn exporter_error_does_not_change_recording() {
    let counting = CountingInstrumentation::new();
    let exporting = ExportingInstrumentation::new(&counting, AlwaysErr::default());

    exporting.record_retry("op", 1);
    exporting.record_circuit_open("op");
    exporting.record_circuit_close("op");

    assert_eq!(counting.retry_count(), 1);
    assert_eq!(counting.open_count(), 1);
    assert_eq!(counting.close_count(), 1);
    assert_eq!(exporting.exporter().calls.load(Ordering::Relaxed), 4);
    let stats = exporting.export_stats();
    assert_eq!(stats.failed_export_calls, 4);
    assert_eq!(stats.panicked_export_calls, 0);
    assert_eq!(stats.unconfirmed_spans, 3);
    assert_eq!(stats.unconfirmed_metrics, 1);
}

/// 边界：exporter 发生可展开 panic——在包装边界隔离，记录路径不 panic。
#[test]
fn exporter_panic_is_isolated() {
    let counting = CountingInstrumentation::new();
    let exporting = ExportingInstrumentation::new(&counting, PanicSpans);
    exporting.record_circuit_open("op");
    exporting.record_circuit_close("op");
    assert_eq!(counting.open_count(), 1);
    assert_eq!(counting.close_count(), 1);
    let stats = exporting.export_stats();
    assert_eq!(stats.panicked_export_calls, 2);
    assert_eq!(stats.failed_export_calls, 2);
    // PanicSpans 的 flush 不 panic，故包装层返回 Ok。
    assert_eq!(exporting.flush(), Ok(()));
}

/// 边界：多线程接受事件后 shutdown——每个已接受事件都必须被计入 flushed。
#[test]
fn concurrent_shutdown_accounts_every_event() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 16;
    let exporter = Arc::new(InMemoryExporter::with_capacity(THREADS * PER_THREAD));
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for thread_id in 0..THREADS {
        let exporter = Arc::clone(&exporter);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for item in 0..PER_THREAD {
                let event = span(format!("{thread_id}-{item}"));
                assert_eq!(exporter.export_spans(std::slice::from_ref(&event)), Ok(()));
            }
        }));
    }
    for handle in handles {
        handle.join().expect("线程");
    }
    exporter.shutdown().expect("shutdown");
    let stats = exporter.stats();
    assert_eq!(stats.flushed_spans, THREADS * PER_THREAD);
    assert_eq!(stats.buffered_spans, 0);
    assert_eq!(stats.dropped_spans, 0);
    assert!(stats.is_shutdown);
}

/// 边界：空串与全空白 op——统一回落 `_` 且层级视为 1。
#[test]
fn empty_and_whitespace_op_normalization() {
    assert_eq!(sanitize_op(""), "_");
    assert_eq!(sanitize_op("   "), "_");
    assert_eq!(sanitize_op("\0 \t\r \0"), "_");
    assert_eq!(op_depth(""), 1);
    assert_eq!(op_depth("   "), 1);
    assert_eq!(op_leaf(""), "_");
    assert!(!is_friendly_op(""));
    assert!(!is_friendly_op("   "));
    assert_eq!(join_op_segments(&["a", "", " b "]), "a.b");
    assert_eq!(join_op_segments(&[]), "");
    // 空 op 经记录路径也不得 panic。
    let counting = CountingInstrumentation::new();
    let exporter = InMemoryExporter::with_capacity(4);
    let exporting = ExportingInstrumentation::new(&counting, &exporter);
    exporting.record_retry("", 1);
    exporting.record_circuit_open("");
    exporting.record_circuit_close("");
    assert_eq!(exporter.buffered_spans()[0].name, "retry:_");
}
