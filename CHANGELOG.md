# Changelog — observex

本文件记录 `observex` 的用户可见变更，遵循 [Keep a Changelog](https://keepachangelog.com/)
与 [Semantic Versioning](https://semver.org/)。

本仓库代码自 `xhyper.rs` 的 `crates/infra/observex` 抽取而来（抽取时点为 `0.1.4`）。
该工程内的版本线不在本文件中延续，本仓库从 `0.1.0` 重新起算。

## [Unreleased]

### 新增

- `ExportingInstrumentation` 的 `record_*` 转发路径在导出失败（exporter 返回 `ExportError`
  或发生可展开 panic）时，以 `tracing::warn!` 发出结构化日志（`error` / `operation` /
  `signal` / `op` / `forward_failures` 字段，中文消息）。此前该路径的失败仅进入诊断计数器，
  日志面完全静默，无人值守场景下导出失败长期不可见（对抗审查 adversarial-20260922-infra6
  Top10 #4）。日志采用指数采样（第 1 次与第 2^k 次失败记录，日志量 O(log n)）防止下游故障
  叠加高频重试时的日志风暴（R-OBS-004）；诊断计数器仍逐条累计。记录调用的返回语义、
  `flush` / `shutdown` 的错误传播与公开 API 均不变。
- 测试：`tests/export_failure_logging.rs`（捕获 warn 日志文本，断言级别、字段与中文消息）、
  `src/export.rs` 单测 `forward_failure_logs_use_exponential_sampling`（采样计数 seam）。

## [0.1.1] - 2026-09-22

### 新增

- 特性 002 三类测试：`tests/tdd_contracts.rs`（10 个公开入口的行为契约与 TDD-PROBE 红绿表）、
  `tests/sdd_spec.rs`（`docs/标准.md` 五章节 1:1 对照断言）、
  `tests/aidd_boundary.rs`（6 条对抗 / 边界用例，含多字节截断、容量 0、exporter Err/panic 隔离、
  并发 shutdown 计数守恒）。

### 变更

- **内部结构改写（公开 API 与可观察契约均不变）**：按 `docs/module-rules.md` §5.5 的手法，
  把 `src/export.rs` 的两组 impl 下沉为子模块 —— `InMemoryExporter` 的三个 impl 块
  （`Default`、`InMemoryExporter` 本体、`TelemetryExporter for InMemoryExporter`、
  `TelemetryExporter for &InMemoryExporter`）与两个纯计数器辅助（`flush_state` / `add_count`）
  → `src/export/memory.rs`（188 行）；`ExportingInstrumentation` 的四个 impl 块
  （`ExportDiagnostics`、本体两个、`Instrumentation` 实现）与时戳辅助 `now_unix_ms`
  → `src/export/forwarding.rs`（175 行）。门面 `src/export.rs` 保留模块文档、全部类型定义与字段
  （`ExportError` / `SpanEvent` / `MetricEvent` / `TelemetryExporter` trait / `MemExportState` /
  `InMemoryExporterStats` / `InMemoryExporter` / `ExportingInstrumentation` / `ExportDiagnostics` /
  `ExportingInstrumentationStats`）、`DEFAULT_BUFFER_CAPACITY` 常量与**原有内联测试**。
  唯一可见性放宽：`InMemoryExporter::state` 提为 `pub(super)` —— 门面内联测试
  `counter_saturation_is_visible_in_stats` 直接读 `MemExportState` 的字段把计数器推饱和。
  其余项要么是 `pub`，要么只在本模块内互调（`flush_state` / `add_count` / `call_exporter` /
  `now_unix_ms`），**保持原样**。
  `src/export.rs` 生产段 **510 → 164** 行。
  动机：`module-rules` 是元仓库必需检查，且它审计各仓**默认分支**，故当 `export.rs` 距
  `MR-STRUCT-007` 的 800 行 ERROR 阈值只剩 290 行时，任一仓的任意改动都可能卡住元仓库的全部 PR。
  属**纯搬移**（行多重集比对确认零代码行丢失：「仅旧」恰为提级的那条签名；内联测试段与旧文件
  511–960 行**逐字节一致**），56 项测试与 doctest 结果不变。

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
