#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! observex 公开面：Copy/Clone/Debug + 别名。

use instrumentationx::Instrumentation;
use observex::{ObservexInstrumentation, TracingInstrumentation};

#[test]
fn full_instrumentation_surface() {
    let a = TracingInstrumentation::new();
    let b = TracingInstrumentation;
    let c = ObservexInstrumentation::default();
    let d = a; // Copy
    for i in [&a, &b, &c, &d] {
        i.record_retry("op", 1);
        i.record_circuit_open("op");
        i.record_circuit_close("op");
    }
    assert!(format!("{a:?}").contains("TracingInstrumentation"));
}
