# ADR-0108：协议中立的持久 Runtime 控制命令

状态：Accepted（2026-08-14）

## 背景

`EmbeddedRuntime` 已能在同一 Rust 进程预注册多个租户 Profile，并提供 execute/resume/event replay；Unix
daemon 也有审批、取消和恢复能力。但二者的控制语义分叉：嵌入接口没有审批或取消，且不维护 durable
Run record；daemon 的命令又绑定 Unix socket。Java、CLI 与未来 GUI 若各自补一套控制流程，会产生不同的
owner epoch、审批绑定和崩溃恢复语义。

## 决策

1. 在 `EmbeddedRuntime` 下建立 schema v1 的 `RuntimeControlCommand`。命令固定 `command_id`、完整
   `RuntimeInvocationContext`、`run_id`、`expected_owner_epoch` 和 action；action 包含 `resume`、精确
   `decide_approval`、带原因的 `cancel`，以及精确绑定 input ID、版本和摘要的 `resolve_mcp_input`。
2. 调用方认证、签名和用户授权由 Java、CLI、桌面或其他 transport adapter 负责；Rust Runtime 仍校验
   预注册 Profile、完整 invocation、Run 所有权、owner epoch、approval ID、target Run 和 binding digest。
3. 每个已接受命令在 Profile 的 state root 写一份可重建完整命令的摘要绑定收据，状态为
   `accepted → completed`。收据保存 action 与 expected owner epoch，并由重建后的 canonical command
   校验摘要；相同 command ID 只能重放完全相同的命令，改写 action、身份或 epoch 会失败。收据和 Run record 使用
   `write + fsync + rename + directory fsync`，读取错误除 NotFound 外全部 fail-closed。
4. 每个 `(invocation, run_id)` 在任何收据或状态变更前取得唯一进程内执行所有权。并发审批只能有一个
   所有者；并发取消共享同一 cancellation token 和 per-Run record gate，所有已接受的取消收据最终收敛。
5. `execute` 先建立 durable `LocalRunRecord`；执行结果映射为 Running、AwaitingApproval、AwaitingMcpInput、
   Finished 或 Cancelled。活动取消先持久化 `Cancelling` 再触发 token；等待审批/MCP 的 Run 可直接持久
   终止；失去所有者但仍有 Checkpoint 的 Run 以更高 owner epoch 恢复并立即取消。
6. approval/resume 必须存在 Checkpoint，并推进 owner epoch。审批决定必须匹配 durable pending Tool；
   同一 `accepted` 命令在执行者再次崩溃后可以继续，但不得绕过 Run 冻结的 Provider 尝试、预算或副作用
   恢复规则。

## 对标判断

- Codex app-server 已有成熟 `thread/resume`、`turn/interrupt` 与客户端审批请求，Turn 中断还能结束等待中的
  command approval；本阶段吸收其统一 Thread/Turn 控制面思想，但没有复制 OpenAI Responses 绑定。当前
  本平台多出的窄面是完整 tenant/application/workload/Workspace identity、owner epoch 和 command digest
  收据；Codex 的客户端协议、交互体验和跨平台产品链仍领先。
- OpenClaw Node Host 以 invoke ID、active invoke map、AbortController、input sequence 和 Gateway pending
  invoke 管理在线调用，审批策略与 Node relay 更完整；其 inspected Node invoke 主链以在线内存状态为主。
  本平台增加了跨 Host 的本地持久 command receipt 和 Run record，但尚无 OpenClaw 同等级 Gateway、Node
  运维、实时 relay、动态 inventory 或跨平台覆盖。

## 验收与边界

- 审批、活动取消、崩溃恢复、重复命令、错误 binding、过期 epoch、command ID 改写和并发所有权均有
  真实 HTTP/SSE Runtime 行为测试，不使用 mock 控制接口。
- 一个已写 `accepted` 的 resume 命令在第二个执行所有者崩溃后，以相同 command ID 和更高 owner epoch
  恢复成功；该用例显式给出三次同 Provider 尝试预算。
- 最终全 Rust workspace 667 通过、0 失败、6 个外部 live 用例显式忽略；8 线程压力曾出现一次既有 PTY
  identity ambiguous，exact 10/10 与 4 线程全套通过，仍保留为高并发稳定性风险。
- 本 ADR 不提供外部认证、远端 transport、分布式 command ledger、GUI 或 Java SDK；也不宣称多进程可
  同时共享同一 state root。Edge 本轮不在实施范围。
- Unix daemon/CLI 的复用与兼容迁移由后续 ADR-0109 固定；它不改变本 ADR 的协议中立内核边界。
