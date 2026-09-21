# Changelog — observex

本文件记录 `observex` 的用户可见变更，遵循 [Keep a Changelog](https://keepachangelog.com/)
与 [Semantic Versioning](https://semver.org/)。

本仓库代码自 `xhyper.rs` 的 `crates/infra/observex` 抽取而来（抽取时点为 `0.1.4`）。
该工程内的版本线不在本文件中延续，本仓库从 `0.1.0` 重新起算。

## [Unreleased]

### 新增

- 特性 002 三类测试：`tests/tdd_contracts.rs`（10 个公开入口的行为契约与 TDD-PROBE 红绿表）、
  `tests/sdd_spec.rs`（`docs/标准.md` 五章节 1:1 对照断言）、
  `tests/aidd_boundary.rs`（6 条对抗 / 边界用例，含多字节截断、容量 0、exporter Err/panic 隔离、
  并发 shutdown 计数守恒）。

## [0.1.0] - 2026-09-21

### 新增

- 从 `xhyper.rs` 抽取为独立 crate，移除对内部 crate `kernel` 与 `contracts` 的依赖。
- `TracingInstrumentation` 改为实现共享契约 `instrumentationx::Instrumentation`。
- `CountingInstrumentation`、`PrefixedInstrumentation`、`ExportingInstrumentation`、
  `InMemoryExporter` 与 `TelemetryExporter` 首次以独立 crate 形式提供。
- `sanitize_op` / `truncate_op` / `join_op_segments` / `op_depth` / `op_leaf` 等
  `op` 名治理函数首次独立提供。

### 说明

**不是** OpenTelemetry API/SDK，不实现 OTLP。`InMemoryExporter` 只是有界进程内 sink，
`flush()` 与 `shutdown()` 不代表写盘或远端确认。
