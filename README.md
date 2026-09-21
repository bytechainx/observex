# observex

`observex` 是可观测性**实现** crate：为 `instrumentationx::Instrumentation` 提供 `tracing`
落地、`op` 名清理，以及一个自定义的有界进程内遥测 sink。

- 实现 `instrumentationx::Instrumentation`：`TracingInstrumentation`、`PrefixedInstrumentation`、
  `CountingInstrumentation`、`ExportingInstrumentation`
- 所有真实记录路径统一经过 `sanitize_op`：移除控制字符、空值回落 `_`、按 UTF-8 字节边界限长 128
- 有界进程内 sink：`InMemoryExporter` 的 span / metric 各自独立容量，满载整批拒绝
- exporter 失败隔离：`ExportingInstrumentation` 内化 `ExportError` 与 unwind，并暴露诊断计数
- 策略诚实：明确声明**不是** OpenTelemetry API/SDK，也不实现 OTLP

## 安装

本 crate **不发布到 crates.io**；它与 `instrumentationx` 之间是 path 依赖，需把两个仓库
clone 到同级目录后以 path 引入：

```toml
[dependencies]
observex = { path = "../observex" }
instrumentationx = { path = "../instrumentationx" }
```

## 最小可运行示例

```rust
use instrumentationx::Instrumentation;
use observex::TracingInstrumentation;

fn main() {
    let instrumentation = TracingInstrumentation::new();
    instrumentation.record_retry("db.query", 1);
    instrumentation.record_circuit_open("db.query");
    instrumentation.record_circuit_close("db.query");
}
```

自定义有界 sink（不依赖任何远端服务）：

```rust
use instrumentationx::Instrumentation;
use observex::{CountingInstrumentation, ExportingInstrumentation, InMemoryExporter};

fn main() -> Result<(), observex::ExportError> {
    let inner = CountingInstrumentation::new();
    let exporter = InMemoryExporter::new();
    let instrumentation = ExportingInstrumentation::new(&inner, &exporter);

    instrumentation.record_retry("db.query", 1);
    assert_eq!(inner.retry_count(), 1);

    instrumentation.flush()?;
    instrumentation.shutdown()?;
    Ok(())
}
```

## 能力矩阵

| 类型 | 作用 |
| --- | --- |
| `TracingInstrumentation` | 零字段 `Copy`，三方法落到 `tracing::info!`；无 subscriber 时为 no-op，不 panic |
| `PrefixedInstrumentation<I>` | 给 `op` 加统一前缀后再委托内层实现 |
| `CountingInstrumentation` | 进程内原子计数，用于单测断言（**不是**生产 metrics） |
| `ExportingInstrumentation<I, E>` | 包装内层实现，同时同步写入 `TelemetryExporter` |
| `InMemoryExporter` | 有界进程内 span / metric 缓冲 sink |
| `TelemetryExporter` | 同步、必须快速返回的导出端口 |
| `ExportError` / `ExportingInstrumentationStats` | 导出错误与失败诊断 |
| `sanitize_op` / `truncate_op` / `join_op_segments` / `MAX_OP_BYTES` | `op` 名清理与拼接 |
| `ObservabilityTier` / `policy_summary` | 观测能力层级与诚实策略描述 |

兼容别名：`ObservexInstrumentation` = `TracingInstrumentation`。

## `op` 名治理

`sanitize_op` 是**唯一**的清理入口：移除控制字符、trim、空值回落 `_`，并按 UTF-8 字节边界
截断到 128 字节。它**不是** PII / secret 检测，也不是 allowlist；调用方负责保证 `op` 使用
稳定、低基数的受控业务词汇。

## 有界进程内 sink 语义

- `InMemoryExporter::new()` 对 span 与 metric 各使用 1024 个事件槽位，`with_capacity(n)` 可显式配置
- 单次同类批次容量不足时**整批拒绝**并累计 dropped，原缓冲不变
- `stats()` 在同一 mutex 临界区内生成 buffered / flushed / dropped / shutdown 一致性快照
- `usize` 溢出时计数饱和并由 `counters_saturated` 标记为下界
- `shutdown()` 先计入 flushed 再幂等关闭；重复调用成功且不重复计数
- 容量限制**事件数**，不限制直接调用 exporter 时单个事件字段的字节数

## 生产误用红线

| 禁止 | 原因 |
| --- | --- |
| 宣称 OpenTelemetry / 生产可观测完成 | 仅 `tracing` + 自定义有界进程内 sink |
| 把 `shutdown` 当远端持久化确认 | 它只更新进程内 flushed 计数并清空内存 |
| 在 exporter 内等待外部 I/O | 同步合同要求快速返回；包装层只隔离 unwind panic，不隔离阻塞 |

## Subscriber 故障隔离

`TracingInstrumentation` 只调用 `tracing::info!`：**没有**自定义 subscriber 时为 no-op，不 panic。
若业务安装了阻塞或 panic 的 subscriber，隔离责任在**安装方**——本 crate 不使用 `catch_unwind`
包裹，以免掩盖真实错误。

## 非职责

- OpenTelemetry API/SDK、OTLP、远程导出、采样与持久化
- PII / secret 检测或 `op` allowlist
- 重试 / 熔断 / 限流策略本身（属 `resiliencx`）
- 业务审计（属 `evidence`）

## 门禁

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# instrumentationx 不发布到 crates.io，该覆盖是长期约定；打包时始终显式指向同级的本地 checkout。
cargo package --no-verify --offline \
  --config 'patch.crates-io.instrumentationx.path="../instrumentationx"'
```

## 与 instrumentationx 的关系

本 crate 实现 `instrumentationx::Instrumentation`，依赖以 `path` + `version` 声明。
`instrumentationx` **不发布到 crates.io**（组织内各 crate 均不发布），因此：

- 本地开发：把两个仓库放在同级目录即可正常构建；
- 消费方：同样需要两个仓库同级 checkout，并按上面的 `path` 形式引入；
- `cargo package`：需要上面那条 `--config` 覆盖；该覆盖是长期约定，不随版本演进移除。

## 许可

MIT OR Apache-2.0
