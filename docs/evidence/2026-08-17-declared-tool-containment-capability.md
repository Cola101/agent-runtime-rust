# 证据：声明式 Tool 容器边界能力（ADR-0122）

- 日期：2026-08-17
- 机器：M1 Pro 16GB，macOS darwin 25.5.0
- 未使用：Docker、虚拟机、Kubernetes、Java、PostgreSQL、NATS、外部 API Key、真实厂商服务

## 修的是什么

两处**契约与实际行为不符**，都是复核代码时发现的，不是文档表格里写着的。

### 一、`TrustedNative` 在非 macOS 上静默降级

`runtime/crates/tool-runtime/src/lib.rs` 原形状：

```rust
#[cfg(target_os = "macos")]
let (program, args) = { /* seatbelt::wrap_launch(...) */ };
#[cfg(not(target_os = "macos"))]
let (program, args) = (self.executable.clone(), self.definition.fixed_args.clone());
```

非 macOS 上以**裸进程**启动：无 seatbelt、无 landlock、无任何替代边界。而
`runtime/apps/runtime-host/src/lib.rs:2624` 与 `:2688` 注册 `workspace.write_text`、`shell.exec`
和进程会话可执行文件时**没有任何 `#[cfg]` 门**——descriptor 仍写 `TrustedNative`，
implementation digest 仍绑定，审批仍 `Ask`，没有任何一处报告边界未生效。

挡住它的只有 `docs/implementation-status.md` 里的一句话。

### 二、实现摘要分不出可写与只读工具

两个执行器的 implementation digest 都把 `workspace_access` 写死为 `"read_only"`，而启动路径使用真实值
（`seatbelt.rs:86` 据此决定是否加入 `WRITABLE_WORKSPACE`；容器执行器据此决定 bind mount 是否 `readonly`）。
`WorkspaceAccess::ReadWrite` 在仓库内有 9 处真实使用。

**同一个二进制、同样的参数，一个能写 Workspace、一个不能，摘要完全相同。**

## 测量结果

| 门禁 | 结果 |
| --- | --- |
| `--test containment_capability` | **8 passed, 0 failed**, TEST_EXIT=0 |
| `-p agent-tool-runtime`（整 crate） | 15 二进制，**116 passed, 0 failed, 0 ignored**, TEST_EXIT=0 |
| `cargo fmt --all -- --check` | FMT_CHECK_EXIT=0 |
| `clippy --workspace --all-targets --all-features -D warnings` | CLIPPY_EXIT=0 |
| `cargo test --workspace -- --test-threads=4` | 见下 |

### 全量门禁

`cargo test --workspace -- --test-threads=4`（fullgate17）：

**122 二进制、783 passed、0 failed、6 ignored、`CARGO_EXIT=0`**

对照本轮开始前的 fullgate15（121 / 775 / 0 / 6）：二进制 **+1**（新增测试文件），
通过 **+8**（新增 8 个测试）。差值精确等于新增量，因此本轮改动没有连带改变任何既有测试的结果——
而摘要变更是本轮最大的风险面，这个吻合是它没有外溢的证据。

首次门禁 `CARGO_EXIT=101`（编译失败，见下），修复后重跑通过。两次门禁的退出码都是单独
`echo $?` 取得，未经管道。

`docs/evidence/2026-08-15-*` 记录的延迟门禁偶发失败本轮未复现；该问题仍未解决，与本轮无关。

机器状态：target 5.8G，磁盘 51Gi 可用（下限 20Gi）。测试结束后无残留进程、端口、临时目录或测试密钥。

## 编译器抓到的第三处

首次全量门禁 `CARGO_EXIT=101`：

```
error[E0004]: non-exhaustive patterns:
  `&ToolExecutionError::UnsupportedContainment(_)` not covered
```

`runtime/apps/worker/src/lib.rs:10289` 的 `tool_execution_error_code` 是穷尽匹配。这暴露出一个**必须显式
决定、不能默认**的语义问题：新错误落到哪一类失败。

答案是 `deterministic_failure_result()`。`record_tool_execution_failure`
（`worker/src/lib.rs:8064`）的规则是：**未被分类为确定性未执行**的失败，若工具是
`NonIdempotent`/`Unknown`，一律收敛为 `run.indeterminate`——也就是叫人去查副作用有没有落地的那条分支。

容器边界拒绝发生在 Workspace 解析之前、任何进程创建之前，**可证明什么都没发生**。若不显式加入
该分类，系统会对一件确定没发生的事说「可能发生了」，并派人去核对一个不存在的副作用。

因此 `UnsupportedContainment` 进入 `deterministic_failure_result()`，形成脱敏 Tool Result。
缺失的保证名**不进入模型可见内容**（测试有反向断言），只经错误 Display 和事件码到达操作员。

## 测试证明了什么，没证明什么

**证明了：**

- 缺任一保证被**具名**拒绝（`workspace_write_confinement` / `credential_read_denial` /
  `network_egress_denial` / `containment_backend` 四条各自断言）
- 本机声明 `MacosSeatbelt` 且**实际** `prepare()` 产出 `sandbox-exec` 包装与 `-p` profile 参数——
  声明与行为一致，不是只声明
- 读写与只读工具的 implementation digest **不再相同**（此条在修复前必然失败，是真回归测试）
- 同一 definition 的摘要仍然稳定
- 确定性未执行不收敛为 `indeterminate`，且保证名不泄漏给模型

**没证明：**

- **没有任何 Linux 或 Windows 上的实测。** 这台机器上 `cfg!(target_os = "macos")` 恒真，
  非 macOS 分支从未被执行过。缺失路径是靠**构造的能力集合**证明的（ADR-0072 的既有模式：
  `validate_governance` 把 capabilities 作为参数），这证明的是校验器，不是那些平台的真实行为。
- 没有新增任何非 macOS 隔离能力。Linux landlock 仍未实现。
- 拒绝发生在 `prepare()` 而非注册处，因此 Linux 上工具仍会进入模型目录并可能被审批后才失败。
  见 ADR-0122「风险与后续」。

## 进度

**总体进度不变，仍为 70–75%。** 本轮修的是可信度，不是能力。按 `implementation-status.md` 的口径，
只有并发、真实厂商、跨平台、生产持久层四类证据能推动百分比；本轮不属于其中任何一类——
让「跨平台缺失」变得诚实，不等于跨平台。
