#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! CountingInstrumentation 本地验证路径（非 OTEL）。
use instrumentationx::Instrumentation;
use observex::{CountingInstrumentation, PrefixedInstrumentation};

fn main() {
    let c = CountingInstrumentation::new();
    let p = PrefixedInstrumentation::new("demo", &c);
    p.record_retry("op", 1);
    println!("retries={}", c.retry_count());
    assert_eq!(c.retry_count(), 1);
}
