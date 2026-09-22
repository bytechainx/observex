#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! 转发导出失败的结构化日志契约。
//!
//! 验证 `ExportingInstrumentation` 的 `record_*` 转发路径在 exporter 返回 Err 或发生
//! 可展开 panic 时发出 `tracing::warn!` 结构化日志（`error` / `operation` / `signal` /
//! `op` / `forward_failures` 字段，中文消息），使无人值守场景下的导出失败可见；
//! 同时业务记录语义不变（标准.md §4：Err 被内化且不改业务记录）。
//!
//! 本文件刻意只含一个测试函数并以 scoped subscriber 全程包裹：独立测试进程内无并发
//! 订阅者切换，避免 tracing callsite 兴趣缓存被「无 subscriber 首触发」污染
//! （根因见 `src/lib.rs` 中 `ensure_global_default` 的注释）。

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use instrumentationx::Instrumentation;
use observex::{
    CountingInstrumentation, ExportError, ExportingInstrumentation, MetricEvent, SpanEvent,
    TelemetryExporter,
};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;

/// 共享日志捕获缓冲（与 `src/lib.rs` 单测同款 seam）。
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

/// 恒返回 `Unavailable` 的 exporter。
struct AlwaysErr;

impl TelemetryExporter for AlwaysErr {
    fn export_spans(&self, _spans: &[SpanEvent]) -> Result<(), ExportError> {
        Err(ExportError::Unavailable)
    }
    fn export_metrics(&self, _metrics: &[MetricEvent]) -> Result<(), ExportError> {
        Err(ExportError::Unavailable)
    }
    fn flush(&self) -> Result<(), ExportError> {
        Err(ExportError::Unavailable)
    }
    fn shutdown(&self) -> Result<(), ExportError> {
        Err(ExportError::Unavailable)
    }
}

/// `export_spans` 发生可展开 panic 的 exporter（在包装边界被隔离）。
struct PanicsOnSpans;

impl TelemetryExporter for PanicsOnSpans {
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

/// 每个场景使用独立实例：指数采样是 per-instance 状态，独立实例保证各自的首条
/// 失败必然记录日志（第 1 次失败必 log），断言互不干扰。
#[test]
fn export_forward_failures_emit_structured_warn() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_level(true)
        .with_target(false)
        .without_time()
        .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
        .finish();
    let _guard = subscriber.set_default();

    // 场景 1：exporter 恒 Err —— record_retry 产生 spans + metrics 两条失败（第 1、2 次均记录）。
    let counting = CountingInstrumentation::new();
    let retrying = ExportingInstrumentation::new(&counting, AlwaysErr);
    retrying.record_retry("log-contract-retry", 3);
    // 业务记录语义不变：inner 已执行。
    assert_eq!(counting.retry_count(), 1);

    // 场景 2：circuit_open / circuit_close 各自独立实例，首条失败必记录。
    let open_counting = CountingInstrumentation::new();
    let opening = ExportingInstrumentation::new(&open_counting, AlwaysErr);
    opening.record_circuit_open("log-contract-open");
    let close_counting = CountingInstrumentation::new();
    let closing = ExportingInstrumentation::new(&close_counting, AlwaysErr);
    closing.record_circuit_close("log-contract-close");

    // 场景 3：exporter panic 被隔离后同样记录日志（error 为 Panicked 的 Display）。
    let panic_counting = CountingInstrumentation::new();
    let panicking = ExportingInstrumentation::new(&panic_counting, PanicsOnSpans);
    panicking.record_circuit_open("log-contract-panic");
    assert_eq!(panicking.export_stats().panicked_export_calls, 1);

    drop(_guard);
    let text = capture.text();

    // 级别与中文消息。
    assert!(text.contains("WARN"), "缺少 WARN 级别日志: {text}");
    assert!(
        text.contains("内部遥测导出失败"),
        "缺少中文失败消息: {text}"
    );
    // 结构化字段：operation / signal / op / error / forward_failures
    // （fmt subscriber 对字符串字段值带引号渲染，数值与 %error 的 Display 不带）。
    assert!(
        text.contains("operation=\"record_retry\""),
        "缺少 record_retry 操作字段: {text}"
    );
    assert!(
        text.contains("signal=\"spans\"") && text.contains("signal=\"metrics\""),
        "缺少信号类型字段: {text}"
    );
    assert!(
        text.contains("operation=\"record_circuit_open\""),
        "缺少 circuit_open 日志: {text}"
    );
    assert!(
        text.contains("operation=\"record_circuit_close\""),
        "缺少 circuit_close 日志: {text}"
    );
    assert!(
        text.contains("op=\"log-contract-retry\"")
            && text.contains("op=\"log-contract-open\"")
            && text.contains("op=\"log-contract-close\""),
        "缺少业务 op 字段: {text}"
    );
    assert!(
        text.contains("error=遥测导出器内部不可用"),
        "缺少 Err 场景的 error 字段: {text}"
    );
    assert!(
        text.contains("error=遥测导出器发生可展开 panic")
            && text.contains("op=\"log-contract-panic\""),
        "缺少 panic 场景的 error 字段: {text}"
    );
    assert!(
        text.contains("forward_failures=1"),
        "缺少累计失败次数字段: {text}"
    );
}
