#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! observex 热路径：Instrumentation 记录。
use std::hint::black_box;
use std::time::Instant;

use instrumentationx::Instrumentation;
use observex::TracingInstrumentation;

fn iters() -> u32 {
    if std::env::args().any(|a| a == "--quick") {
        5_000
    } else {
        200_000
    }
}

fn main() {
    let n = iters();
    let instr = TracingInstrumentation::new();
    for _ in 0..n.min(50) {
        instr.record_retry("w", 1);
    }
    let start = Instant::now();
    for i in 0..n {
        instr.record_retry("bench_op", i);
        if i % 3 == 0 {
            instr.record_circuit_open("bench_op");
        }
        if i % 5 == 0 {
            instr.record_circuit_close("bench_op");
        }
        let _ = black_box(i);
    }
    let elapsed = start.elapsed();
    println!(
        "bench_observex_record: iters={n} total={elapsed:?} per_iter={:?}",
        elapsed / n
    );
}
