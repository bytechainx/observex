# observex Agent 指南

> 本文件为 AI Agent 在本仓库工作时的入口指南。

## 项目定位

可观测性实现：`instrumentationx::Instrumentation` 的 tracing 实现、op 名清理与有界进程内遥测 sink。它**不是** OpenTelemetry API/SDK，也不实现 OTLP。

## 技术栈

- Rust edition 2021, rust-version 1.71
- 关键依赖: `instrumentationx`（契约 trait）、`thiserror` 2、`tracing` 0.1；dev-dependencies: `tracing-subscriber`
- `instrumentationx` 为 path 依赖（`../instrumentationx`），是不发布到 crates.io 的共享契约 crate
- 零业务耦合，不依赖 kernel/contracts 等私有 crate
- crate 级 lint：`unwrap_used` / `expect_used` / `panic` / `unreachable` / `todo` / `unimplemented` 全部 `deny`（测试代码经 `cfg_attr(test)` 豁免）

## 代码结构

```text
src/
├── lib.rs      # TracingInstrumentation / PrefixedInstrumentation / CountingInstrumentation + 受控 re-export
├── export.rs   # 有界进程内 sink：TelemetryExporter / InMemoryExporter / ExportingInstrumentation
├── ops.rs      # op 名清理：sanitize_op / truncate_op / join_op_segments / MAX_OP_BYTES
├── policy.rs   # 可观测性分层与「生产可用」断言策略（ObservabilityTier / policy_summary）
└── surface.rs  # 本地验证探针（probe_tracing / probe_counting_*）
```

- `#![forbid(unsafe_code)]`、`#![deny(missing_docs)]`、`#![deny(unreachable_pub)]`

## 开发约定

- 注释与文档使用简体中文；标识符保持英文
- 错误：thiserror 枚举 + `#[non_exhaustive]` + Result 别名
- 禁止裸 `unwrap()`（库代码；lint 已 deny）
- 所有 `pub` 项必须有中文 `///` 文档（`missing_docs` 已 deny）
- 新增导出必须走 `lib.rs` 的显式 re-export，禁止 glob 导出

## 门禁三件套（P0）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## 相关文档

- 组织 Rust 规范：`~/org-config/rulesets/rust/RULES.md`
- API 文档：`docs/API.md`
- 标准与验收：`docs/标准.md`
- 术语与领域语言：`CONTEXT.md`
- 贡献指南：`CONTRIBUTING.md`
- 变更记录：`CHANGELOG.md`
- 基准测试：`benches/hot_path.rs`
