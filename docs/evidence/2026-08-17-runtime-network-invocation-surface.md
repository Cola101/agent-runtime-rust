# 证据：Runtime 网络调用契约第一段（ADR-0123）

- 日期：2026-08-17
- 机器：M1 Pro 16GB，macOS darwin 25.5.0
- 未使用：Docker、虚拟机、Kubernetes、Java、PostgreSQL、NATS、外部 API Key、真实厂商服务

## 复核出来的事实

不是从文档表格读的，是查代码得到的：

| 检查 | 结果 |
| --- | --- |
| `runtime.proto::RuntimeControl` 服务端实现 | **0** |
| `runtime.proto::RuntimeControl` 客户端引用 | **0** |
| 编译 `runtime.proto` 的 crate | **0**（连 build.rs 都没有） |
| `openapi.yaml` 的 Rust 实现 | **0**（全工作区 axum 只被 `runtime-health` 用于健康端口） |
| `runtime-host` 生产监听 | 仅 Unix domain socket；`TcpListener` 全在 `#[cfg(test)]` 内 |

即：**要跑一次 Run，只有把 `EmbeddedRuntime` 编进自己进程，或在同一台机器上连 Unix socket 两条路。**

## 测量结果

| 门禁 | 结果 |
| --- | --- |
| `--test grpc_invocation_identity` | **8 passed, 0 failed**, TEST_EXIT=0 |
| `--test grpc_invocation_loop` | **1 passed, 0 failed**, TEST_EXIT=0 |
| `cargo fmt --all -- --check` | FMT_CHECK_EXIT=0 |
| `clippy --workspace --all-targets --all-features -D warnings` | CLIPPY_EXIT=0 |
| `cargo test --workspace -- --test-threads=4` | 见下 |

### 全量门禁——**不是绿的**

`cargo test --workspace --no-fail-fast -- --test-threads=4`（fullgate19）：

**126 二进制、790 passed、2 failed、6 ignored、`CARGO_EXIT=101`**

两条失败**都是墙钟预算断言，都不是正确性失败**，且都与本轮改动无关：

| 失败 | 测得值 | 判定 |
| --- | --- | --- |
| `sixty_four_sessions_keep_one_thousand_waits_bounded_and_tenant_fair`（`process_wait_multi_session_capacity.rs:438`） | p50=1.048s（门禁 <1s），p95=1.162s（<2s 通过），p100=1.165s（<4s 通过） | **已知未决问题**，本会话此前已记录并明确挂起等用户决定，不得私自放宽 |
| `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded`（`embedded_retention.rs:1223`） | `scan_elapsed >= 2s` | 本轮首次浮现，见下 |

第二条的取证：

- 该测试**前面所有正确性断言全部通过**（`run_directories_after == 16`、墓碑计数、
  `terminal_ledger_bytes < 2 MiB`），失败的只有末尾的 `scan_elapsed < 2s`。
- **单独重跑通过**：`TEST_EXIT=0`，耗时 114.11 秒；而在全量并行下同一测试耗时 216.75 秒，
  接近两倍。2 秒扫描预算是被这个负载差挤爆的。
- **本轮改动没有任何路径能影响它**：`RuntimeInvocationGrpcService` 在 `embedded_retention.rs` 中
  从未被构造，新模块被编译但不被执行；`embedded.rs`、`retention.rs` 与 Run 状态路径未改一行。
- 它与第一条属于**同一类问题**：在不受控的并行负载下断言墙钟预算。这是该类问题的第二个实例。

**两条都没有被修改、放宽或排除。** 按既定纪律，性能预算表达的是意图，改动它是用户的决定，不是我的。

本轮改动自身的门禁：`--test grpc_invocation_identity` 8/8、`--test grpc_invocation_loop` 1/1、
fmt `EXIT=0`、clippy `--workspace --all-targets --all-features -D warnings` `EXIT=0`。

机器状态：target 8.5G（新 crate 与 tonic 依赖使其自 5.8G 增长），磁盘 46Gi 可用（下限 20Gi）。

## 真实闭环证明了什么

`a_network_caller_submits_observes_and_completes_a_real_run`：调用方**只持有一个 TCP 地址和一个
bearer token**，全程没有任何指向 Runtime 的进程内 handle。

1. `Submit` 返回被接纳的 `run_id`
2. 反复 `ReadEvents` 分页前进，**按 typed 边界停止**，不靠猜事件列表
3. 终态边界为 `Terminal { status: "succeeded" }`
4. 观察到的事件类型：`run.started`、`model.provider.selected`、`model.output.delta`、`run.succeeded`
5. 从同一个面重放整段事件，读回模型真实输出

Provider 是真实回环 HTTP/SSE 服务，所以 Run 是**真的执行了**，不是模拟。

## 身份边界证明了什么

8 项，每项都是一次真实的 gRPC 调用：

- 无 token → `unauthenticated`
- **Run 形状 token 携带 `runtime.invoke` → `permission_denied`**（不是 `unauthenticated`：token 有效，
  重新认证也不会把 Run 变成运维）
- 运维 token 但只有 `mcp.oauth.admin` → `unauthenticated`（管理凭证不蕴含开 Run）
- 请求体命名他租户 → `permission_denied`
- 未注册 Profile → `permission_denied`，且消息为固定文本（不确认其是否存在）
- `ReadEvents` 与 `Control` 与 `Submit` 同等认证（只有写路径检查的面会让 token 读到别人的 transcript）
- 未知 control action → `invalid_argument`，不进入持久命令路径
- 任何拒绝消息都不含 state root、`/var`、`/tmp`

## 过程中两次自己的错误

**一、Run 形状测试最初为假绿的反面。** 首跑得到 `unauthenticated` 而非 `permission_denied`。
原因是我伪造的 schema-4 claims 把 `model_policy_digest` 留成空串，而 schema 4 要求它是 sha256——
token 在到达**形状检查之前**就以 `InvalidClaims` 被拒了。那样测的是「我造的 token 不合法」，
不是「形状被拒」。补成除形状外完全合法后才真正命中边界。测试文件里记了这一点。

**二、`Debug` 被用作线路取值。** 初版用 `format!("{:?}").to_lowercase()` 产出状态字符串。
`RunStatus::WaitingApproval` 会变成 `waitingapproval`，而系统其余部分一律写 `waiting_approval`；
更糟的是 `LocalRunState` 是 `#[serde(tag = "state")]` 的带数据枚举，`Cancelling { reason }` 会把
**操作员填写的 reason 文本**写进一个被文档描述为状态标记的字段。改为从 serde 取规范标记。

## 没证明什么——**阶段 1 未完成**

- **binary 未接线。** `main.rs` 仍只绑 Unix socket。**部署出来的 Runtime 不提供这个面。**
  测试跑在真实 TCP 上，但服务端由测试进程自己拉起。
- **没有 TLS/mTLS。** `grpc-security` 已有材料类型，本段未接。接线前不得在回环之外暴露。
- **控制路径只证明了拒绝。** 审批、取消、resume 的成功端到端，以及跨进程崩溃恢复，未在该面验证。
- 无流式订阅，调用方只能分页轮询。无 Java SDK。

## 进度

**总体进度不变，仍为 70–75%。** 这是边界层，不属于并发、真实厂商、跨平台、生产持久层四类证据中的任何一类。
