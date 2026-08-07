# ADR-0015：Checkpoint PubAck 与围栏恢复接管

## Status

Accepted

## Context

ADR-0014 定义了 Checkpoint 内容和恢复资格，但未规定 Worker、JetStream、PostgreSQL 与新 Worker 之间的持久化屏障。若事件先于 Checkpoint、恢复命令重复投递，或审批仍绑定旧 attempt，系统可能丢失恢复点、重复副作用或把决定发给失效 Worker。Codex 的 rollout 恢复适合本地线程；OpenClaw 会拒绝不完整 replay，并取消重复 invoke ID 对应的旧进程。多租户 PaaS 还需要数据库权威状态、Workspace fencing 和短期身份轮换。

## Decision

1. Worker 对每个非终态 Kernel 事件依次发布 Run Event 与 v1 Checkpoint；两者都必须获得 JetStream PubAck。Checkpoint 使用独立持久 Subject，消息 ID 用于 Broker 去重。
2. 控制面分别消费 Run Event 与 Checkpoint。Checkpoint 若先到达且尚不能匹配权威 Run sequence，使用延迟 NAK 重试；格式或摘要非法才终止消息。
3. JetStream 默认消息上限约 1 MiB，因此 v1 内联 Checkpoint 原始 payload 限制为 512 KiB，为 Base64 与 JSON 信封留出空间。更大快照必须使用后续对象存储 `payload_ref`，不得提高客户端上限后假定 Broker 可接受。
4. Reconciler 只对 `SAFE` Checkpoint 接管：创建新 attempt，递增 Workspace owner epoch，轮换 fencing token 与工作负载身份，并定向发送 `RunRecoveryCommand`。原 dispatch 标记为 lost，原 Workspace 租约行保留以保证 epoch 不回退。
5. 新 Worker 必须校验租户、Run、Session、不可变 Agent/模型/预算/Scope、Tool Catalog 摘要和更高 fencing 身份，重建 Kernel 后先发布 `run.restored`。
6. 已开始的 `pure`/`idempotent` Tool 在新 attempt 重新发布 `tool.execution.requested`，再经过 started PubAck 执行；`non_idempotent`/`unknown` 的模糊结果不得自动恢复。
7. 等待审批的 Run 在接管事务中把 pending approval 绑定到新 attempt 与 Worker；新 Worker 发布 `approval.rebound`，控制面重建新 attempt 的 Tool Ledger 后，审批决定才能定向发送。
8. 重复的同一恢复命令在 Worker 内按 Checkpoint 摘要与新 attempt 幂等；旧 attempt 的迟到事件因 current attempt 与 fencing 不符而拒绝。

## Consequences

### Positive

- Worker 崩溃后的安全恢复不依赖进程内状态，形成 JetStream、PostgreSQL、Reconciler 与新 Worker 的完整接管链。
- replay-safe Tool 和审批恢复都产生新 attempt 的可审计事实，不复用旧执行账本。
- 乱序跨 Subject 投递不会被误判为永久无效，非法消息仍可快速终止。

### Negative

- 本 ADR 决策时按稳定 Worker ID 排除旧 Worker；该限制已由 ADR-0016 的进程启动实例身份解除。
- 512 KiB 以上快照在对象存储引用实现前不可发布，长会话需要先做压缩或显式暂停。
- 已证明协议与故障路径，但尚未完成 Kubernetes 节点丢失、长时间断流和 15 分钟恢复 SLO 的系统级演练。

## Alternatives Considered

- **事件和 Checkpoint 放进同一消息**：会放大每个增量事件并耦合公开事件与内部状态，不利于独立保留策略。
- **Checkpoint 未匹配时直接 ACK/终止**：跨 Subject 无全局顺序，可能永久丢失合法恢复点。
- **沿用旧审批 attempt**：审批决定会被投递到失效 Worker，并破坏 Tool Ledger 的 attempt 隔离。
- **恢复时直接执行 pure Tool**：缺少新 attempt 的 planned/started 持久化屏障，崩溃后仍无法判定是否已执行。

## References

- Codex：`codex-rs/core/src/session/session.rs` 的 `InitialHistory::Resumed`
- Codex：`codex-rs/core/src/session/handlers.rs` 的 `apply_rollout_reconstruction`
- OpenClaw：`src/agents/embedded-agent-runner/replay-history.ts` 的 dangling/orphan 校验
- OpenClaw：`src/agents/embedded-agent-runner/run.overflow-compaction.test.ts` 的 `replaySafe=false`
- OpenClaw：`src/node-host/runtime.ts` 的重复 invoke 取消
