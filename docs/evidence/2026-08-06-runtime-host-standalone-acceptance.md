# runtime-host 最小独立运行验收

日期：2026-08-06
对应决策：[ADR-0035](../adr/0035-standalone-rust-runtime-host.md)

## 交付内容

- `docs/adr/0035-standalone-rust-runtime-host.md` — 独立本地/边缘执行宿主的架构决策
- `runtime/apps/runtime-host/` — 新 crate（lib + CLI）
- `runtime/apps/model-gateway/src/invocation.rs` — 把 `ModelInvocation → ModelRequest` 的解码
  从 gRPC 传输层下沉为传输中立的库函数

`runtime/apps/edge-node` 此前只是 7 行占位符，没有既有本地执行路径被替换或删除。

## 架构要点

本地模式**复用 Worker 执行内核**，不另写一套循环：

```
runtime-host
 ├─ WorkerProcessor      (agent-runtime-worker)  模型—Tool 循环与全部安全不变量
 ├─ ProviderAdapter      (agent-model-gateway)   进程内直调，无 gRPC / 无 mTLS
 ├─ TrustedNativeExecutor(agent-tool-runtime)    可信本地 Tool
 └─ 文件 Checkpoint                               本地状态根
```

因此 `Skill 声明 ∩ 可信 Tool ∩ delegated scopes`、未激活 Tool 不外发、不可用 Tool fail-closed、
审批闸、Checkpoint 绑定，这些在本地与云端是**同一份代码**。本地 Run 的权限只可能更小，不可能更大。

三个刻意的本地取值，均在 ADR 中说明理由：

| 项 | 本地取值 | 理由 |
| --- | --- | --- |
| owner epoch / fencing token / incarnation | 固定本地值 | 用于仲裁竞争 Worker；单写者本地执行无可仲裁 |
| workload identity | 占位、不外发 | 本地不跨进程边界取模型凭证，无身份可出示 |
| model policy id | 固定常量 | 本地只有一个已配置 Provider；**必须跨重启稳定**，否则 Checkpoint 恒不可恢复 |

最后一条是实现期真实踩到的：初版每次生成新的 `model_policy_id`，恢复测试直接报
`worker checkpoint identity does not match the replacement command`。

## 解码器下沉

`decode_invocation` 原本是 `grpc.rs` 的私有函数且返回 `tonic::Status`。若在 runtime-host 复制一份，
本地与云端可能对同一个 Run 向模型发出不同 transcript。改为 `agent_model_gateway::decode_model_invocation`
（传输中立错误类型），gRPC 侧只负责把错误映射回 Status——**逐字保留原有 Status 文案与
`invalid_argument` / `unimplemented` 的区分**，对外契约不变。

## 验收结果

`cargo test -p agent-runtime-host` — 4 passed, 0 failed。每个测试都跑真实宿主 + 真实回环 SSE Provider，
进程内**没有** Java 控制面、PostgreSQL、NATS 或 gRPC。

| 测试 | 证明 |
| --- | --- |
| `local_host_runs_an_agent_to_a_terminal_state_without_any_control_plane` | 无控制面即可执行到终态；事件以 `run.started` 开始、含 `run.succeeded`；Checkpoint 落到磁盘 |
| `a_restarted_local_host_resumes_the_run_from_its_filesystem_checkpoint` | **全新宿主实例**仅凭本地状态根恢复；新 attempt；首事件为 `run.restored`；终态成功 |
| `a_local_run_that_changes_its_instructions_cannot_reuse_the_checkpoint` | 换掉 Agent 指令后恢复被拒——本地恢复与云端一样重算有效状态 |
| `a_local_tool_call_fails_closed_when_no_trusted_executor_is_installed` | 未安装可信执行器时模型的 Tool 调用 fail-closed，不触达任何可执行文件 |

## 门禁

- `cargo fmt --all -- --check` — 通过
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — 零告警
- `cargo test --workspace` — **237 passed, 0 failed, 1 ignored**
  （agent-protocol 46 / agent-runtime-worker 103 / agent-model-gateway 24 / agent-runtime-host 14）

## 长驻宿主与本地 IPC

ADR-0035 第 7 条（客户端可与 Runtime 分离）已实现并验收。

新增 `runtime/apps/runtime-host/src/ipc.rs`：Unix socket 守护进程，行分隔 JSON 协议
（`submit` / `attach` / `list`）。每个 Run 跑在自己的 tokio 任务上，连接与 Run 生命周期无关。

关键设计（均在源码注释中说明理由）：

- **先落盘再广播**。事件先写入 `runs/<run_id>/events.jsonl`，再发给在线客户端。反过来会让
  「已广播但未持久」的事件对在线客户端可见、对重连客户端不可见。
- **先订阅再回放**。`attach` 先订阅实时流再读日志，并按 sequence 去重；反序会丢掉两步之间产生的事件。
- **落后的客户端不跳事件**。实时缓冲有界（1024）；一旦 `Lagged`，从持久日志补齐缺口，而不是留个洞。
- **socket 权限 0600**。能连上这个 socket 的人就能花掉本地宿主持有的 Provider 凭证。

CLI 相应扩展为 `serve` / `submit` / `attach` / `list` / `run`（`run` 为无守护进程的一次性执行）。

### 验收结果

`cargo test -p agent-runtime-host --test local_ipc` — 3 passed, 0 failed。

| 测试 | 证明 |
| --- | --- |
| `a_run_survives_the_client_that_submitted_it_disconnecting_immediately` | 提交后立刻断开整条连接，Run 仍跑到 `succeeded`；全新连接能看到 `run.started` 到 `run.succeeded` 的完整过程 |
| `a_reconnecting_client_replays_the_durable_event_log_from_its_last_sequence` | 带游标重连严格少一条事件且顺序不变——游标回放既不丢也不重 |
| `the_control_socket_is_owner_only` | 控制 socket 权限为 0600 |

实现期踩到的两个真实缺陷（都在测试辅助代码里，但会造成假绿）：

1. `BufReader` 重组连接时会丢弃已缓冲的字节，导致 `Finished` 永远读不到、测试挂起 10 分钟。
2. 用 `std::mem::forget` 保活写半边，使「客户端断开」根本没发生——**测试前提本身是假的**。
   改为持有完整连接对象，drop 时两个半边一起关闭。

## 守护进程重启恢复

「Run 不因进程消失而丢失」此前只兑现了一半：客户端消失 Run 能活，**守护进程消失 Run 就停了**——
Checkpoint 躺在盘上却没人捡起来。本节补齐后一半。

### 新增的持久化生命周期

`LocalRunRecord`（`runs/<run_id>/run.json`，先写后执行）：

| 字段 | 作用 |
| --- | --- |
| `input` | 恢复需要原始输入 |
| `state` | `running` / `finished{status}` / `interrupted{reason}` |
| `owner_epoch` | 已用过的最高 epoch |

`owner_epoch` 是必需的而非装饰：`resume` 原本硬编码 epoch 2，第二次恢复会被
`StaleCheckpointLease` 拒绝。现在 `resume` 显式接受 epoch，恢复时取 `记录值 + 1`。

### 恢复规则

守护进程启动时 `recover_unfinished()`：

- `state != running` → 不碰。**已完成的 Run 绝不重跑。**
- `state == running` 且**有** Checkpoint → 以 `epoch + 1` 恢复
- `state == running` 且**无** Checkpoint → 标记 `interrupted`，**不自动重跑**

最后一条是刻意 fail-closed：没有 Checkpoint 就没有证据说明它已经做过什么，
自动重跑只在 Checkpoint 指明续跑位置时才安全。

`serve` 在接受新工作**之前**先恢复，因此重启不会先收新单再想起旧单。

### 验收结果

`cargo test -p agent-runtime-host --test daemon_recovery` — 2 passed, 0 failed。

崩溃是真实模拟的：守护进程跑在自己的 tokio runtime 上，`Runtime` 被 drop 时其所有任务一并中止；
配套的 Provider 会把第一个调用方永久晾住，模拟死在模型调用中途的守护进程。

| 测试 | 证明 |
| --- | --- |
| `a_restarted_daemon_resumes_a_run_its_predecessor_left_unfinished` | 前任留下 `running` + Checkpoint；替代守护进程接管并跑到 `succeeded`，且 owner epoch 严格增大 |
| `a_restarted_daemon_never_re_executes_a_run_that_already_finished` | 已完成的 Run 记录与事件日志在恢复后**逐字节不变** |

第二个测试同时守住了「恢复不得追加事件」——只断言状态不变会漏掉重跑一轮却恰好同样成功的情况。

## 本地审批闸打通

上一轮 `LocalToolConsent::Ask` 实际是死路。本节修复并验收。

### 修掉的两个缺陷

**1. 停在审批的 Run 被记成 `Finished`**（上一轮引入）。`launch` 把任何 `Ok(outcome)` 都记为完成，
包括 `pending_approval` 非空的情况。后果连锁：持久状态错误 → `recover_unfinished` 永远跳过它 →
配合当时没有批准通道，该 Run 永久不可批准。
修复：新增 `LocalRunState::AwaitingApproval { approval_id, binding_digest }`，与 `Finished` 分开。

**2. 停在审批前没有落 Checkpoint**。`drive` 在 `break` 前不持久化，而 pending approval 只存在于
Checkpoint 里，因此恢复后无 approval 可重绑，批准必然失败。
修复：停泊前先落盘，并在注释中写明「pending approval 只有落盘才可被回答」。

顺带清掉一处坏味道：本地模式原本用 `Uuid::nil()` 当 tenant/workspace/agent-version 身份。
nil UUID 是「缺失」哨兵，`ToolApprovalDecisionCommand::validate` 直接以
`identity must be complete` 拒收。改为固定的非 nil 本地常量。

### 新增能力

IPC 增加 `approve` / `deny` / `cancel`；CLI 同名子命令。

回答一个已停泊的审批 = 以 `epoch + 1` 从 Checkpoint 恢复 → `rebind_recovered_approval`
把审批重绑到新 attempt → `apply_tool_approval` → 继续执行。重绑是必需的：审批是针对已被替换的
那个 attempt 发出的。

`cancel` 只作用于已停泊的 Run；取消正在执行中的 Run 尚未实现，且不会假装支持。

### 验收结果

`cargo test -p agent-runtime-host --test approval_flow` — 5 passed, 0 failed。全部使用真实的
`agent-trusted-workspace-tool` 二进制与真实工作区文件。

| 测试 | 证明 |
| --- | --- |
| `a_run_parked_on_an_approval_is_not_recorded_as_finished` | 停泊状态是 `AwaitingApproval`，不是 `Finished` |
| `approving_over_ipc_lets_the_parked_run_execute_its_tool_and_finish` | 批准后 Tool 真的执行（`tool.execution.started`），终态 `succeeded` |
| `a_restarted_daemon_keeps_a_parked_run_approvable` | 守护进程重启后 Run 仍停泊、仍可批准，批准后 Tool 执行且终态 `succeeded` |
| `denying_over_ipc_never_executes_the_tool` | 拒绝后 Tool **从不执行**，且 Run 以 `succeeded` 收尾（绑定的错误结果回灌给模型），不是失败 |
| `cancelling_a_parked_run_closes_it_without_executing_the_tool` | 取消后状态 `Cancelled`，Tool 从不执行 |

**两个假绿是我自己先写出来又自己抓到的**：`deny` 与 `restart-approvable` 最初只断言
`Finished { .. }`，而审批失败时状态是 `Finished { failed: ... }`，同样匹配——测试会通过但证明的是空的。
收紧为断言具体终态与 `tool.execution.started` 的有无之后，两者立即暴露为红。

## 未完成（明确不声称）

- **Skill 未接入本地模式。** 信任模型**已拍板**：控制面签发、离线携带、本地宿主只持验证公钥、
  本地永不自签（已写入 ADR-0035 第 8 条）。密钥分发与制品导出是独立契约，**尚未实现**；
  在此之前本地 Run 不携带 Skill 快照。当前以「不支持本地 Skill」满足 ADR，而不是以放宽验证满足。
- **本地 Tool 执行路径已实现但无本地自动验收。** `drain_tool_calls` 会走审批 → 执行 → 结果回灌，
  但测试用例只覆盖到「无执行器时 fail-closed」。真正的可信 Tool 执行目前由 Worker 侧的既有测试与
  2026-08-02/08-06 的原生实跑覆盖，二者共用同一份代码。
- **取消只支持已停泊的 Run。** 取消正在执行中的 Run（中止模型调用或 Tool 执行）尚未实现。
- **无并发上限 / 配额 / 调用方认证。** 除 socket 文件权限 0600 外没有任何调用方鉴别。
- **无 Checkpoint 的中断 Run 不会自动重跑**，只被标记 `interrupted`，需要人工重新提交。
- **无子代理**：本地遇到 `agent.spawn` 直接报错拒绝，不静默降级。
- 未做本地并发、多 Run 调度、配额。

## 复现

```
cargo test -p agent-runtime-host --manifest-path runtime/Cargo.toml
```

CLI（需自备回环 Provider）：

```
AGENT_RUNTIME_LOCAL_STATE_ROOT=... AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT=... \
AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT=http://127.0.0.1:PORT/v1/chat/completions \
AGENT_RUNTIME_LOCAL_PROVIDER_MODEL=... AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY=... \
cargo run -p agent-runtime-host -- run "your request"
```

长驻宿主与客户端：

```
cargo run -p agent-runtime-host -- serve            # Runtime，独立进程
cargo run -p agent-runtime-host -- submit "..."     # 返回 run_id 后即可退出
cargo run -p agent-runtime-host -- attach <run-id>  # 可随时重连，可带游标
cargo run -p agent-runtime-host -- list
cargo run -p agent-runtime-host -- approve <run-id>   # 或 deny / cancel
```

未在本文件、日志或仓库中写入任何密钥。
