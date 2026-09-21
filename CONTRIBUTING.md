# CONTRIBUTING.md — 贡献指南（observex）

本文件面向贡献者，汇总本地门禁与提交约定。
AI Agent 的工作约定另见 [`AGENTS.md`](./AGENTS.md)；术语与领域语言见 [`CONTEXT.md`](./CONTEXT.md)。

## 开发流程

- 本仓库是**独立的单 crate 仓库**，不依赖 `xhyper.rs` 主工程及其内部 crate（`kernel` / `contracts` 等）。
- 但它对共享契约 crate `instrumentationx` 有 `path` 依赖：

  ```toml
  instrumentationx = { version = "0.1.0", path = "../instrumentationx" }
  ```

  因此本地开发、门禁与打包都要把 `observex` 与 `instrumentationx` 两个仓库放在**同级目录**。
- substantial 变更走 feature branch → PR → review → merge，**禁止直接 push `main`**。
- `main` 已启用分支保护：要求 PR + 必需检查 `fmt / clippy / test`，
  `required_approving_review_count = 0`（单人也能合并），禁止强推与删除。
- 合并方式固定为 **create a merge commit**。注意仓库设置是
  `merge_commit_title = MERGE_MESSAGE` + `merge_commit_message = PR_TITLE`，因此
  `gh pr merge` 必须显式传 `--subject` 与 `--body`，否则会产出通用
  `Merge pull request #N from …` 标题。
- 提交信息遵循 Conventional Commits（`feat:` / `fix:` / `docs:` / `ci:` / `chore:` / `refactor:`），
  描述用简体中文。

## 本地门禁（P0 三件套）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

本仓库无启用中的 feature（`[features] default = []`），无需 `--all-features` 变体。

元数据完整性门禁（**不发布 crates.io**，此命令只校验打包元数据）：

```bash
cargo package --no-verify --allow-dirty --offline \
  --config 'patch.crates-io.instrumentationx.path="../instrumentationx"'
```

`instrumentationx` **不发布到 crates.io**，`cargo package` 无法从 registry 解析该依赖，
必须用上面的 `--config patch` 指向同级本地 checkout；该覆盖是**长期约定**，不随版本演进移除。

## 复用口径（不发布 crates.io）

- 本 crate **不发布到 crates.io**，仅以 GitHub 源码 / git 依赖形式复用。
- 文档与元数据中不得出现「可独立发布」「可直接 `cargo publish`」等表述，
  也不得放置 crates.io / docs.rs 徽章与外链。
- `Cargo.toml` 的 `documentation` 指向 `https://github.com/bytechainx/observex#readme`。
- 消费方引入方式（README「安装」小节为准）：因 `path` 内部依赖，需两个仓库同级 checkout 后以
  `path` 形式引入：

  ```toml
  [dependencies]
  observex = { path = "../observex" }
  instrumentationx = { path = "../instrumentationx" }
  ```

## 开发约定

- 注释、文档、错误消息使用**简体中文**；标识符保持英文。
- 错误类型：`thiserror` 派生枚举 `ExportError` + `#[non_exhaustive]`。
- **所有真实记录路径必须统一经过 `sanitize_op`**（移除控制字符、空值回落 `_`、按 UTF-8 字节边界
  限长 `MAX_OP_BYTES`）；清理**不是** PII/secret 检测，也不是 allowlist。
- 新增导出必须走 `lib.rs` 的显式 re-export，**禁止 glob 导出**。
- `TelemetryExporter` 是**同步非阻塞**接口：实现必须快速返回，不得等待外部 I/O 或无界阻塞；
  包装层只隔离 exporter 的可展开 panic，不隔离阻塞。
- 不在库代码里裸 `unwrap()`（`[lints.clippy]` 已 `deny` `unwrap_used` / `expect_used` / `panic` /
  `unreachable` / `todo` / `unimplemented`）；`#![forbid(unsafe_code)]`。
- 所有 `pub` 项必须有中文 `///` 文档（`missing_docs` 已 `deny`）。
- MSRV `1.71`、edition `2021`。
- 集成测试**必须离线运行**，不触碰真实网络与远端导出。

## 提交前自检清单

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo test --all-targets` 通过
- [ ] `cargo package --no-verify --allow-dirty --offline --config 'patch.crates-io.instrumentationx.path="../instrumentationx"'` 通过
- [ ] 新增 `pub` 项都有中文 `///` 文档，且经 `lib.rs` 显式 re-export
- [ ] 文档中无「可独立发布」/ crates.io / docs.rs 表述
- [ ] 文档未宣称 OpenTelemetry / OTLP / 远端持久化能力
