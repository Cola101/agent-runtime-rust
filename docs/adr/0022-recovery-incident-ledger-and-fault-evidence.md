# ADR-0022：恢复事故台账与可判定故障证据

## Status

Accepted

## Context

ADR-0015 已完成 SAFE Checkpoint 的跨 Worker 围栏恢复，ADR-0021 已完成计划内安全下线，但控制面此前只返回一次
`ReconcileResult`。当存在安全 Checkpoint、却暂时没有替代容量时，过期 attempt 会静默停留；系统无法回答故障从何时
开始、当前卡在容量还是恢复命令、是否超过 15 分钟，以及 `run.restored` 是否真正结束了恢复。

Codex 的 shutdown report 会区分完成、提交失败和超时，rollout 则保存恢复边界。OpenClaw 的 restart lifecycle 会记录
drain blocker、restart recovery context 和恢复结果。这些机制提供了可观测生命周期，但都没有多租户 Scheduler、跨 Worker
attempt 和 Workspace fencing，因此不能直接作为 PaaS 的权威恢复记录。

## Decision

1. PostgreSQL V13 新增受 RLS 与复合外键保护的 `recovery_incidents`。事故保存失败 attempt、Worker/incarnation、控制面
   最后确认健康时间、检测时间、当前恢复 attempt、状态与完成时间；同一租户 Run 同时只允许一条未完成事故。
2. 状态固定为 `waiting_capacity → recovery_requested → recovered`；恢复 attempt 在 `run.restored` 前已明确终止时进入
   `terminated`，无法安全重放时进入 `indeterminate`。已有未完成事故在连续恢复失败时复用，保留最初故障时钟，
   不允许每次换 Worker 都重置 SLO。
3. accepted dispatch 过期且存在 SAFE Checkpoint、但没有健康容量时必须写入 `waiting_capacity`，不得继续静默等待。
   选到替代 Worker 后绑定新 attempt；只有新 attempt 的 `run.restored` 持久化成功才将事故标为 `recovered`，显式终态
   则关闭为 `terminated`/`indeterminate`，不得留下永久未完成事故。
4. 15 分钟恢复时钟以控制面实际收到最后一次心跳的时间开始，不使用 Worker 自报时间。V13 增加
   `last_heartbeat_received_at`，调度健康过滤和恢复 SLO 均使用该字段，防止节点时钟漂移延长健康窗口。
5. Repository 提供按租户的 `RecoverySloSnapshot`：未完成数、超时数、等待容量数、已请求恢复数和最老事故时长。
   本 ADR 不新增绕过 RLS 的全租户查询；后续 ADR-0023 以不含租户标识的事务汇总桶实现平台级指标。
6. 故障证据分层：750ms 缩放租约测试证明 Reconciler 在 2 秒预算内创建恢复 attempt；真实 NATS 容器 pause/resume
   证明发布失败会 fail-closed 且相同消息 ID 可安全重试；真实 JetStream 重投递配合一次性 Checkpoint Store
   `Unavailable` 注入，证明恢复命令不丢失并最终产生 `run.restored`。
7. 这些证据不等于生产 15 分钟 SLO。启用 Worker HPA 前仍必须在真实 Kubernetes 集群执行 Pod/节点丢失、PDB、
   NATS 集群和 Checkpoint Gateway 实例组合故障，并保留原始计时与事故台账导出。

## Consequences

### Positive

- “无替代容量”从不可见等待变成可查询、可告警、不会重置时钟的持久事故。
- 恢复成功以控制面接受 `run.restored` 为准，不以发出命令或 HTTP 200 冒充完成。
- Worker 时钟漂移不能污染健康调度与恢复 SLO。
- NATS 与 Checkpoint Gateway 的短暂故障已有可重复的传输层恢复证据。

### Negative

- V13 增加写放大与恢复状态维护；长期事故需要后续保留和告警策略。
- 当前快照是按租户读取，尚未接 Prometheus、告警路由和运维控制台。
- 缩放租约与容器级故障测试不能代替云集群、多可用区和 1000 活跃 Run 的恢复验收。

## Alternatives Considered

- **只从日志计算恢复时间**：日志可能丢失、乱序且没有租户复合外键，不能作为权威 SLO 证据，拒绝。
- **从 Reconciler 检测时间开始计时**：会隐藏 heartbeat freshness 和 reconcile poll 的检测延迟，拒绝。
- **信任 Worker `occurred_at`**：节点时钟可漂移或被篡改，会延长或缩短健康窗口，拒绝。
- **每个恢复 attempt 新建事故**：连续失败会重置 SLO，掩盖真实用户中断时长，拒绝。
- **没有容量时不改任何状态**：无法告警，也无法区分容量不足与 Reconciler 未运行，拒绝。

## References

- Codex：`codex-rs/core/src/thread_manager.rs` 的 `ThreadShutdownReport`
- Codex：`codex-rs/core/src/session/mod.rs` 的 rollout resume / shutdown 边界
- OpenClaw：`src/cli/gateway-cli/run-loop.ts` 的 restart recovery context
- OpenClaw：`src/gateway/server-close.ts` 的 drain blocker 与有界关闭
- 本平台：`V13__recovery_incidents.sql`、`JdbcSchedulerRepository.java`
- 本平台：`JdbcSchedulerRepositoryIntegrationTest`、`NatsJetStreamMessageBusIntegrationTest`
- 本平台：`runtime/apps/worker/tests/transport.rs`
