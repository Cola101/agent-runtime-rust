# ADR-0016：稳定 Worker 身份与进程启动实例

## Status

Accepted

## Context

Worker ID 表示一个由控制面登记的稳定节点或设备，进程重启后必须保持不变。若执行、取消和审批只按
Worker ID 定向，旧进程与新进程可能同时消费同一命令；若调度器直接排除稳定 Worker ID，同一设备重启后
又不能接管自己的安全 Checkpoint。Codex 的 `ThreadManager` 能从 rollout 恢复本地 Thread，但不处理
分布式执行进程身份；OpenClaw 的 Node Host 用 `activeInvokes` 取消相同 invoke ID 的旧进程，能避免进程
遗留，但该映射只存在于当前 Node Host 进程内。

多租户 Runtime 需要同时表达“这是什么节点”和“这是该节点的哪次启动”，并把两者贯穿调度、消息路由、
持久化确认与恢复资格。

## Decision

1. `worker_id` 是稳定节点身份；Worker 每次启动生成新的 UUIDv7 `worker_incarnation_id`，不得从上次进程复用。
2. PostgreSQL `runtime_workers` 保存稳定节点及当前实例快照；`runtime_worker_incarnations` 保存每次启动的容量、
   心跳和历史。`run_dispatches` 与 pending `approvals` 通过复合外键绑定具体 `(worker_id, incarnation_id)`。
3. RunExecution、RunExecutionAccepted、Heartbeat、Cancellation 和 Approval Decision 升级为 v2。v2 缺少启动
   实例一律拒绝；v1 仅在滚动升级期间保留读取兼容，控制面不再发布 v1 定向命令。
4. JetStream 定向主题固定为
   `runtime.execution.worker.{worker_id}.incarnation.{incarnation_id}.{command}.v2`。不同启动实例使用不同 durable
   consumer，旧进程无法消费发给新进程的执行、恢复、取消或审批命令。
5. accepted 事件、assignment 心跳续租、终态容量归还都必须精确匹配启动实例。不同 incarnation 的 current
   切换按 PostgreSQL 首次见到该实例的顺序单向前进，而不是按客户端心跳时间；迟到的旧实例心跳可以写入
   历史，但不能重新夺回 current，也不能续租新实例的 assignment。
6. Workspace lease 的 `owner_id` 继续使用稳定 Worker ID，写所有权由 owner epoch 与 fencing token 区分；
   启动实例只负责进程寻址，不替代 Workspace fencing。
7. SAFE Checkpoint 恢复只排除原 `(worker_id, incarnation_id)`，因此同一稳定节点的新实例可作为候选；恢复仍创建
   新 attempt、递增 owner epoch、轮换 fencing token 与短期工作负载令牌。
8. 边缘 gRPC 契约采用同一身份模型：NodeHello/Heartbeat 携带 node incarnation，RunLease/Cancellation 明确目标实例。

## Consequences

### Positive

- 节点重启不再需要更换稳定 ID，同一设备可安全接管自己的 Checkpoint。
- 旧进程、迟到消息和重复投递无法跨启动实例续租、接受或执行新命令。
- 容量与心跳既保留历史证据，又有单一 current incarnation 供调度器选择。
- 身份寻址、Workspace 写围栏与 Run attempt 各司其职，避免用一个 UUID 混合三种生命周期。

### Negative

- v1/v2 滚动升级需要同时维护读取兼容，且发布方、Consumer、数据库迁移必须原子协调。
- 当前工作负载令牌仍绑定 tenant/run/attempt/稳定 Worker/ModelPolicy 与租约到期时间，尚未额外绑定 incarnation；
  Worker 会拒绝错实例命令，但 Model Gateway 的纵深校验将在后续令牌 v2 中补齐。
- 节点注册、设备证明、吊销和实例异常抖动策略尚未实现；数据库 current incarnation 不能替代真实设备认证。

## Alternatives Considered

- **每次启动生成新的 Worker ID**：会丢失稳定设备身份、节点策略和审计连续性，也无法表达“同设备恢复”。
- **只在内存中记录进程实例**：控制面或 Worker 重启后证据消失，无法支持跨进程恢复和迟到消息拒绝。
- **把 incarnation 当作 Workspace owner**：实例重启会改变设备所有权语义，仍不能替代 owner epoch 与 fencing token。
- **同一 Worker Subject 内检查 payload**：旧进程仍会抢占并 NAK/终止新实例消息，造成不必要的红elivery 和阻塞。

## References

- Codex：`codex-rs/core/src/thread_manager.rs` 的内存 Thread 映射、rollout resume 与 fork
- OpenClaw：`src/node-host/runtime.ts` 的 `activeInvokes` replacement abort 与 identity-safe cleanup
- OpenClaw：`src/node-host/runtime.test.ts` 的重复 invoke、输入序号和 cancel 测试
- 本平台：`V9__worker_incarnations.sql`、`execution_contract.rs`、`assignment.rs`、`transport.rs`
