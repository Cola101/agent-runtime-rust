# ADR-0014：版本化 Worker Checkpoint 与恢复资格判定

## Status

Accepted

## Context

Tool Execution Ledger 已能判断 Worker 丢失后是否存在模糊副作用，但只保存执行事实不能重建模型上下文、待处理 Tool 和审批状态。Codex 从持久 rollout 重建会话；OpenClaw 在 replay 前检查 Tool Call/Result 配对并保留副作用失效标记。多租户 Runtime 还必须确保恢复发生在新租约下，且恢复使用的 Tool/Skill 能力没有漂移。

## Decision

1. Worker Checkpoint v1 将 Kernel 状态、单调 sequence、Protobuf 编码 transcript、待规划 Tool、未完成执行请求、已开始执行标记、待审批请求和 Tool Catalog 摘要封装进带 SHA-256 摘要的 `CheckpointSnapshot`。
2. Checkpoint 不持久化工作负载令牌或 Provider 凭证；恢复 attempt 必须获得新签发的短期身份。
3. 跨 Worker 恢复必须使用新的 `attempt_id`、更高 `owner_epoch` 和不同的 `fencing_token`，同时保持 tenant、Run、Session、Workspace、AgentVersion、ModelPolicy、输入摘要、预算和 delegated scopes 不变。
4. Tool Catalog 摘要必须完全一致。已开始但未完成的 `non_idempotent` 或 `unknown` Tool 禁止自动恢复；`pure` 和 `idempotent` Tool 可回到待执行状态，并在新 attempt 重新产生 started 事实。
5. Kernel 恢复保持 Run sequence，并在新 attempt 产生 `run.restored` 事件，避免用第二个 `run.started` 冒充初次启动。
6. PostgreSQL `run_checkpoints` 是恢复资格的权威索引：RLS、复合 dispatch 外键、16 MiB 存储防护上限、payload 摘要、Kernel 摘要、Tool Catalog 摘要和租约身份全部持久化；JetStream v1 内联传输另限制为 512 KiB。
7. 控制面仅将“checkpoint sequence 等于 Run 最新持久事件、状态一致、租约一致且无模糊非幂等 started Tool”的记录判为 `SAFE`。任何缺失、滞后或副作用模糊均不得派发恢复。

## Consequences

### Positive

- transcript、Tool Call/Result 配对和事件序号可以在新 Worker 上重建。
- 恢复同时受内容摘要、能力版本和 Workspace fencing 约束，不能用旧租约或漂移后的 Tool 实现继续执行。
- 控制面与 Worker 分别做资格预检和最终校验，形成双重 fail-closed 边界。

### Negative

- Checkpoint 包含对话正文；必须按内容保留和删除策略处理，未来大对象应迁移到对象存储并只在 PostgreSQL 保存摘要引用。
- replay-safe Tool 在恢复后可能再次执行，因此幂等声明必须由平台契约和测试证明，不能只信任 Skill 作者标签。
- 自动派发与恢复传输由 ADR-0015 接通；大于 512 KiB 的快照仍须等对象存储引用契约，当前会 fail-closed。

## Alternatives Considered

- **只重放 Run Event**：公开事件不一定包含完整模型消息、Tool schema 和内部队列，无法无损恢复。
- **只保存 transcript**：会丢失已规划/已开始 Tool 与审批边界，可能重复副作用。
- **允许 Tool Catalog 漂移后恢复**：相同 Tool 名称可能已经对应不同权限或实现，破坏原审批和调用摘要。
- **沿用旧 attempt/租约**：无法隔离迟到的旧 Worker 事件，违反 fencing 设计。

## References

- Codex：`codex-rs/core/src/session/session.rs` 的 `InitialHistory::Resumed`
- Codex：`codex-rs/core/src/session/handlers.rs` 的 rollout reconstruction
- Codex：`codex-rs/core/src/session/rollout_reconstruction_tests.rs`
- OpenClaw：`src/agents/embedded-agent-runner/replay-history.ts`
- OpenClaw：`src/agents/embedded-agent-runner/run.overflow-compaction.test.ts`
- OpenClaw：`src/node-host/runtime.ts` 的重复 invoke 清理
