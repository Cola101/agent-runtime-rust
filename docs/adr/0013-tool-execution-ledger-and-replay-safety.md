# ADR-0013：Tool Execution Ledger 与安全重放边界

## Status

Accepted

## Context

受限 Tool Runtime 已能完成模型—Tool—模型闭环，但 Worker 在外部副作用发生后崩溃时，控制面此前只知道 dispatch 已被接受，无法区分 Tool 尚未启动、正在执行或已经返回。重新从原输入执行可能重复收费、发送消息或修改外部系统；直接把所有故障归为通用 `indeterminate` 又缺少可审计的具体调用证据。

Codex 通过持久 rollout 重建会话历史；OpenClaw 在 replay 前校验 Tool Call/Result 配对，并在潜在副作用后把 replay 标记为无效。本平台需要把这些恢复语义放进多租户 PostgreSQL 权威状态，而不是依赖单机文件或 Gateway 内存。

## Decision

1. 新增按 `(tenant_id, run_id, attempt_id, tool_call_id)` 唯一的 PostgreSQL `tool_executions` 账本，状态为 `planned → started → completed`。
2. 账本保存不可变 `binding_digest`、副作用类别、沙箱类别和原始执行请求，并通过复合外键绑定 dispatch，通过事件外键绑定 requested/started/result 三个事实。
3. Worker 必须先发布 `tool.execution.started` 并取得 JetStream PubAck，才允许启动容器进程。Tool Result 继续使用 PubAck 作为下一模型回合的持久化屏障。
4. `tool.execution.started` 和 `tool.result` 都携带原执行摘要；控制面在同一数据库事务中校验状态和摘要，错误绑定不会写入 Run Event 或推进 sequence。
5. 租约到期时，若存在未完成的 `non_idempotent` 或 `unknown` 执行，Run 进入 `indeterminate`，事件必须包含 call ID、binding digest、effect 和 `replay_safe=false`。
6. 在完整 transcript/checkpoint 恢复尚未实现前，即使 Tool 是 pure/idempotent，也不允许从原始用户输入盲目重放整个 Run；故障会明确终止并说明缺少 checkpoint。

## Consequences

### Positive

- 外部副作用前后都有稳定、可审计、租户隔离的持久证据。
- JetStream 重投和伪造的错误 Tool Result 无法越过绑定摘要与状态机校验。
- Reconciler 能指出具体模糊调用，而不是只给出笼统 Worker 丢失原因。
- 为后续跨 Worker checkpoint 恢复和幂等 Tool 重试提供权威决策输入。

### Negative

- 每次 Tool 执行增加一个 started 事件和一次持久化往返。
- 当前仍不能恢复模型 transcript、待审批状态和多个未完成 Tool；安全策略因此偏保守。
- JetStream 已持久但 PostgreSQL Consumer 尚未处理时存在短暂最终一致窗口，恢复判定必须等待事件消费追平。

### Neutral

- 策略拒绝和人工拒绝不启动外部执行；它们仍以模型可见 Tool Result 结束，但不冒充 started 状态。

## Alternatives Considered

- **Worker 丢失后直接重跑原输入**：无法证明模型会生成相同调用，会重复非幂等副作用。
- **只写 Worker 本地 SQLite/文件日志**：Pod 或边缘设备丢失后不可用，也不能作为多租户控制面的权威审计源。
- **本阶段一次实现完整事件重放恢复**：需要同时稳定 transcript、Tool/Skill 版本、模型策略和 Workspace checkpoint，范围过大；先建立不可绕过的执行事实层。

## References

- Codex：`codex-rs/core/src/session/handlers.rs` 的 rollout reconstruction
- OpenClaw：`src/agents/embedded-agent-runner/replay-history.ts` 的 Tool Call/Result replay invariant
- OpenClaw：`src/agents/embedded-agent-runner/run.overflow-compaction.test.ts` 的 side-effect replay invalidation
