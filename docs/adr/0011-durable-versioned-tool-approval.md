# ADR-0011：版本化持久审批与定向恢复

## 状态

Accepted

## 决策

Tool 审批请求必须先作为 `approval.required` Run Event 写入 PostgreSQL，并同时保存不可变的
`tenant_id + run_id + attempt_id + worker_id + tool_call_id + binding_digest`。Run 只有在该事务成功后
才进入 `waiting_approval`。

审批 API 使用期望版本执行一次性 `allow_once` 或 `deny`。成功决定会递增版本并在同一事务写入
Outbox；控制面不得在此时直接把 Run 改回 `running`。Outbox 命令定向当前 Worker，携带五分钟
有效期及完整绑定摘要。Worker 校验目标和摘要后发布 `run.resumed`；拒绝还会发布带原 Tool Call ID
的错误 `tool.result`。只有这些 Worker 事件持久化后，控制面的 Run 状态才反映实际执行进度。

Beta 不提供 `allow_run`。它需要独立的、可撤销且有作用域的策略记录，不能借一次审批暗中扩大后续
Tool 权限。

## 理由

- Codex 将拒绝转换为可回灌模型的 Tool 输出，避免一次人工拒绝破坏整个 Agent 回合；本平台保留
  该语义，并增加数据库版本与多租户身份绑定。
- OpenClaw 的 `system.run` 会把延迟审批绑定到原始执行计划，并在执行前复核路径或脚本漂移；本平台
  先以摘要覆盖调用、effect、sandbox 和 delegated scope，后续 Executor 仍需补实体快照复核。
- JetStream 至少一次投递要求决定处理可重放。控制面版本锁防止重复决定，Worker 回执缓存使同一命令
  重投产生同一组 Event ID。

## 已落地

- PostgreSQL V6 审批绑定字段、复合 dispatch 外键、唯一 Tool Call 约束与 RLS。
- `POST /v1/approvals/{approvalId}:decide`、`approvals:write` Scope、版本冲突和跨租户隐藏。
- `tool.approval.decided` Outbox 与 Worker 专属 JetStream subject。
- Rust 审批命令契约、有效期校验、allow/deny、重复投递回执及拒绝结果回灌。
- `waiting_approval` Run 可定向取消；`run.resumed` 后才恢复数据库运行态。
- Run 恢复时，pending 审批原子重绑 replacement attempt；若 approved/denied 决定已持久化但旧 Worker
  尚未接收，则同一 approval ID、版本、binding digest 和决定会定向重发给新 attempt。为避免把会话级
  授权在新进程中隐式扩权，`allow_session` 的恢复线协议收敛为当前调用的 `allow_once`。

## 边界

- pending approval、transcript 和恢复回执已经进入 Checkpoint；最新决定的恢复重绑已落地。多个历史审批
  不会被批量重放，只选择来源 attempt 最新且与当前调用绑定的记录。
- 受限容器执行与自动下一模型回合后来已由 ADR-0012 补齐；Kata Provider 和跨进程执行回执仍未实现。
- 摘要尚未包含解析后的 cwd、可执行文件和脚本实体摘要，因此仍落后 OpenClaw 的执行前漂移复核。

## 参考源码

- Codex：`codex-rs/core/src/tools/approvals.rs`
- Codex：`codex-rs/core/src/tools/router.rs`
- Codex：`codex-rs/core/src/tools/parallel.rs`
- OpenClaw：`src/node-host/invoke-system-run.ts`
