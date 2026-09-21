//! observex —— `tracing` / metrics 封装与有界进程内遥测 sink。
//!
//! [`TracingInstrumentation`] 实现 [`instrumentationx::Instrumentation`]。
//! 另有 [`PrefixedInstrumentation`]、[`CountingInstrumentation`]（本地验证）。
//! [`export`] 提供自定义的有界进程内 sink（[`TelemetryExporter`] /
//! [`InMemoryExporter`] / [`ExportingInstrumentation`]）。它不是 OpenTelemetry API/SDK，也不实现 OTLP。

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable
    )
)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unreachable_pub)]

use std::sync::atomic::{AtomicU64, Ordering};

use instrumentationx::Instrumentation;

pub mod export;
mod ops;
mod policy;
mod surface;
pub use export::{
    ExportError, ExportingInstrumentation, ExportingInstrumentationStats, InMemoryExporter,
    InMemoryExporterStats, MetricEvent, SpanEvent, TelemetryExporter, DEFAULT_BUFFER_CAPACITY,
};
pub use ops::{
    is_friendly_op, join_op_segments, op_depth, op_leaf, sanitize_op, truncate_op, MAX_OP_BYTES,
};
pub use policy::{
    allows_production_observability_claim, claims_otel_export_complete,
    counting_is_production_metrics, policy_summary, tier_counting, tier_tracing, ObservabilityTier,
};
pub use surface::{probe_counting_circuit, probe_counting_retries, probe_tracing, PROBE_DOC};

/// tracing 实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingInstrumentation;

impl TracingInstrumentation {
    /// 构造。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// 兼容别名（与 [`TracingInstrumentation`] 等价）。
pub type ObservexInstrumentation = TracingInstrumentation;

impl Instrumentation for TracingInstrumentation {
    fn record_retry(&self, op: &str, attempt: u32) {
        let op = sanitize_op(op);
        tracing::info!(op = op, attempt = attempt, "retry");
    }
    fn record_circuit_open(&self, op: &str) {
        let op = sanitize_op(op);
        tracing::info!(op = op, "circuit_open");
    }
    fn record_circuit_close(&self, op: &str) {
        let op = sanitize_op(op);
        tracing::info!(op = op, "circuit_close");
    }
}

/// op 名前缀包装。
#[derive(Debug, Clone)]
pub struct PrefixedInstrumentation<I> {
    prefix: String,
    inner: I,
}

impl<I> PrefixedInstrumentation<I> {
    /// 构造。
    #[must_use]
    pub fn new(prefix: impl Into<String>, inner: I) -> Self {
        let prefix = prefix.into();
        let prefix = if prefix.is_empty() {
            prefix
        } else {
            sanitize_op(&prefix)
        };
        Self { prefix, inner }
    }
    /// 内层。
    #[must_use]
    pub fn inner(&self) -> &I {
        &self.inner
    }
    fn qualify(&self, op: &str) -> String {
        let op = sanitize_op(op);
        if self.prefix.is_empty() {
            op
        } else {
            sanitize_op(&format!("{}.{}", self.prefix, op))
        }
    }
}

impl<I: Instrumentation> Instrumentation for PrefixedInstrumentation<I> {
    fn record_retry(&self, op: &str, attempt: u32) {
        self.inner.record_retry(&self.qualify(op), attempt);
    }
    fn record_circuit_open(&self, op: &str) {
        self.inner.record_circuit_open(&self.qualify(op));
    }
    fn record_circuit_close(&self, op: &str) {
        self.inner.record_circuit_close(&self.qualify(op));
    }
}

/// 进程内计数（单测用，非生产 metrics）。
#[derive(Debug, Default)]
pub struct CountingInstrumentation {
    retries: AtomicU64,
    opens: AtomicU64,
    closes: AtomicU64,
    last_attempt: AtomicU64,
}

impl CountingInstrumentation {
    /// 构造。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// 重试次数。
    #[must_use]
    pub fn retry_count(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }
    /// 打开次数。
    #[must_use]
    pub fn open_count(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }
    /// 关闭次数。
    #[must_use]
    pub fn close_count(&self) -> u64 {
        self.closes.load(Ordering::Relaxed)
    }
    /// 最近 attempt。
    #[must_use]
    pub fn last_attempt(&self) -> u64 {
        self.last_attempt.load(Ordering::Relaxed)
    }
    /// 清零。
    pub fn reset(&self) {
        self.retries.store(0, Ordering::Relaxed);
        self.opens.store(0, Ordering::Relaxed);
        self.closes.store(0, Ordering::Relaxed);
        self.last_attempt.store(0, Ordering::Relaxed);
    }
}

impl Instrumentation for CountingInstrumentation {
    fn record_retry(&self, _op: &str, attempt: u32) {
        self.retries.fetch_add(1, Ordering::Relaxed);
        self.last_attempt
            .store(u64::from(attempt), Ordering::Relaxed);
    }
    fn record_circuit_open(&self, _op: &str) {
        self.opens.fetch_add(1, Ordering::Relaxed);
    }
    fn record_circuit_close(&self, _op: &str) {
        self.closes.fetch_add(1, Ordering::Relaxed);
    }
}

impl Instrumentation for &CountingInstrumentation {
    fn record_retry(&self, op: &str, attempt: u32) {
        (*self).record_retry(op, attempt);
    }
    fn record_circuit_open(&self, op: &str) {
        (*self).record_circuit_open(op);
    }
    fn record_circuit_close(&self, op: &str) {
        (*self).record_circuit_close(op);
    }
}

/// 空 op → `"_"`。
#[must_use]
pub fn normalize_op(op: &str) -> &str {
    if op.is_empty() {
        "_"
    } else {
        op
    }
}

/// 清理并限制 `op` 后 retry。
pub fn record_retry_normalized(instr: &dyn Instrumentation, op: &str, attempt: u32) {
    instr.record_retry(&sanitize_op(op), attempt);
}
/// 清理并限制 `op` 后 open。
pub fn record_circuit_open_normalized(instr: &dyn Instrumentation, op: &str) {
    instr.record_circuit_open(&sanitize_op(op));
}
/// 清理并限制 `op` 后 close。
pub fn record_circuit_close_normalized(instr: &dyn Instrumentation, op: &str) {
    instr.record_circuit_close(&sanitize_op(op));
}

#[cfg(test)]
mod tests {
    use super::*;
    use instrumentationx::Instrumentation;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex, Once};
    use tracing_subscriber::fmt::MakeWriter;

    /// 安装一次性全局 no-op subscriber，防并发测试污染 callsite 兴趣缓存。
    ///
    /// 根因：`surface` 模块的 `probe_tracing`（无 subscriber）与 `with_capture`
    /// （scoped subscriber）并发运行时，`probe_tracing` 首次触发 `tracing::info!`
    /// 会让共享的 callsite 经 `Rebuilder::JustOne` → `get_default` 回落。若无全局
    /// default，`get_default` 返回 `NoSubscriber`（`Interest::never`），把该
    /// callsite 的缓存永久标记为 never，导致后续 `tracing_fields_captured` 的事件
    /// 在宏层就被丢弃（只捕获到 retry，丢 circuit_open/close）。
    /// 设置全局 no-op（`register_callsite` 返回 always）后，`get_default` 不再回落
    /// 到 never，缓存不会被污染。
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
    fn with_capture(f: impl FnOnce()) -> String {
        ensure_global_default();
        use tracing::Level;
        use tracing_subscriber::filter::LevelFilter;
        use tracing_subscriber::util::SubscriberInitExt;
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .with_level(true)
            .with_target(false)
            .without_time()
            .with_max_level(LevelFilter::from_level(Level::TRACE))
            .finish();
        // set_default + guard：llvm-cov 并行跑 lib 测试时比 with_default 更稳，避免偶发空捕获。
        let _guard = sub.set_default();
        f();
        drop(_guard);
        let _ = cap.make_writer().flush();
        cap.text()
    }

    #[test]
    fn tracing_and_alias() {
        let _ = with_capture(|| {
            let t = TracingInstrumentation::new();
            t.record_retry("f", 1);
            t.record_circuit_open("f");
            t.record_circuit_close("f");
            let a: ObservexInstrumentation = ObservexInstrumentation::new();
            let b: TracingInstrumentation = a;
            b.record_retry("x", 1);
            let d = TracingInstrumentation;
            let _ = format!("{d:?}");
            let _ = d;
        });
    }

    #[test]
    fn counting_and_prefix() {
        let c = CountingInstrumentation::new();
        let p = PrefixedInstrumentation::new("m", &c);
        p.record_retry("op", 3);
        p.record_circuit_open("op");
        p.record_circuit_close("op");
        assert_eq!(c.retry_count(), 1);
        assert_eq!(c.open_count(), 1);
        assert_eq!(c.close_count(), 1);
        assert_eq!(c.last_attempt(), 3);
        assert_eq!(p.inner().retry_count(), 1);
        let p0 = PrefixedInstrumentation::new("", &c);
        p0.record_retry("z", 1);
        assert_eq!(c.retry_count(), 2);
        c.reset();
        assert_eq!(c.retry_count(), 0);
    }

    #[test]
    fn normalize_helpers() {
        assert_eq!(normalize_op(""), "_");
        assert_eq!(normalize_op("a"), "a");
        let c = CountingInstrumentation::new();
        record_retry_normalized(&c, "", 1);
        record_circuit_open_normalized(&c, "");
        record_circuit_close_normalized(&c, "z");
        assert_eq!(c.retry_count() + c.open_count() + c.close_count(), 3);
        assert!(is_friendly_op("ok"));
        assert!(!is_friendly_op(""));
    }

    #[test]
    fn tracing_fields_captured() {
        let out = with_capture(|| {
            let p = PrefixedInstrumentation::new("api", TracingInstrumentation::new());
            p.record_retry("get", 2);
            p.record_circuit_open("get");
            p.record_circuit_close("get");
        });
        for needle in ["retry", "circuit_open", "circuit_close"] {
            assert!(
                out.contains(needle),
                "expected tracing capture to contain {needle:?}; got:\n{out}"
            );
        }
    }

    #[test]
    fn tracing_record_paths_sanitize_untrusted_op() {
        let suffix = "must-not-reach-tracing";
        let malicious = format!("{}\n{suffix}", "配".repeat(80));
        let expected = sanitize_op(&format!("api.{malicious}"));
        let out = with_capture(|| {
            let p = PrefixedInstrumentation::new("api", TracingInstrumentation::new());
            p.record_retry(&malicious, 1);
        });
        assert!(expected.len() <= MAX_OP_BYTES);
        assert!(!expected.chars().any(char::is_control));
        assert!(out.contains(&expected));
        assert!(!out.contains(suffix));
    }

    #[test]
    fn policy_is_honest() {
        assert!(!claims_otel_export_complete());
        assert!(!counting_is_production_metrics());
        assert!(policy_summary().contains("bounded-sink=in-process"));
    }

    #[test]
    fn exporting_instrumentation_flush_path() {
        let c = CountingInstrumentation::new();
        let exp = InMemoryExporter::new();
        let e = ExportingInstrumentation::new(&c, &exp);
        e.record_retry("x", 2);
        assert!(!exp.buffered_spans().is_empty());
        e.flush().unwrap();
        assert!(exp.buffered_spans().is_empty());
        e.shutdown().unwrap();
    }

    #[test]
    fn dyn_and_ops() {
        let _ = with_capture(|| {
            let t = TracingInstrumentation::new();
            let o: &dyn Instrumentation = &t;
            o.record_retry("d", 1);
            assert_eq!(join_op_segments(&["a", "b"]), "a.b");
            assert!(truncate_op("abcdef", 3).len() <= 3);
        });
    }
}
