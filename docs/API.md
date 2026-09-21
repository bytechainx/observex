# observex 公开 API

**版本 / 角色**：`observex 0.1.0` · Instrumentation 的 tracing 实现 + 自定义有界进程内 sink

## 公开消费面

`TracingInstrumentation` / 别名 `ObservexInstrumentation`，实现 `instrumentationx::Instrumentation`。

所有真实记录路径都会调用 `sanitize_op`：移除控制字符、空值回落 `_`，并按 UTF-8 字节边界
限制到 `MAX_OP_BYTES`（128）。该操作不检测 PII/secret，也不执行 allowlist。

## 最小用法

```rust
use instrumentationx::Instrumentation;
use observex::TracingInstrumentation;

let i = TracingInstrumentation::new();
i.record_retry("op", 1);
```

## 有界 sink

```rust
use observex::{InMemoryExporter, TelemetryExporter};

let exporter = InMemoryExporter::with_capacity(64); // span/metric 各 64
exporter.flush()?;
let stats = exporter.stats();
assert_eq!(stats.capacity_per_signal, 64);
# Ok::<(), observex::ExportError>(())
```

- `new()` / `default()`：每类信号容量 `DEFAULT_BUFFER_CAPACITY`（1024）。
- 单次 `export_spans` 或 `export_metrics`：容量不足时整批拒绝，返回 `BufferFull` 并累计 dropped。
- span 与 metric 是两次独立调用，跨信号不提供事务原子性。
- flushed/dropped 在 `usize` 范围内精确；溢出时字段饱和，`counters_saturated` 标记其为下界。
- `flush`：把当前缓冲数累计到 flushed 并清空；关闭后返回 `Shutdown`。
- `shutdown`：同一临界区执行 flush-and-close，重复调用成功且不重复计数。

`TelemetryExporter` 是同步非阻塞接口：实现必须快速返回，不得等待外部 I/O 或无界阻塞。
`ExportingInstrumentation` 内化记录路径的 `ExportError`，通过 `catch_unwind` 隔离 exporter 中
可展开（unwind）的 Rust panic，并由 `export_stats()` 返回 failed/panicked/unconfirmed 诊断；
`unconfirmed_*` 表示失败调用涉及、交付状态未知且 wrapper 不重试，不宣称 exporter 实际丢弃；
flush/shutdown 的 unwind panic 转为 `Panicked` 错误。`panic=abort` 不可捕获。该边界没有异步
队列、timeout 或阻塞线程隔离。

## 能力边界

这是自定义进程内数据模型，不是 OpenTelemetry API/SDK、语义约定或 OTLP exporter，也不提供远端持久化。
容量限制事件数，不限制直接 exporter 调用中单个事件字段的字节数。
