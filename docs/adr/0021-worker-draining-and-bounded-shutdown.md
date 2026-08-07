# ADR-0021：Worker 单向 Draining 与有界安全下线

## Status

Accepted

## Context

ADR-0020 已区分 `/live` 与 `/ready`，并为 Worker 配置稳定 ID、进程 incarnation、PVC、PDB 和 120 秒
Kubernetes 终止宽限期，但进程没有 SIGTERM 生命周期。计划内滚动、节点排空或缩容会直接终止 Worker；活动 Run
只能等待租约过期后恢复，新任务还可能在终止窗口继续调度到旧进程。

Codex 的 `ThreadManager::shutdown_all_threads_bounded` 会并发、有界地关闭线程，只删除确认关闭的线程；中断路径会
留下持久历史边界。OpenClaw 的 Gateway restart lifecycle 会先原子关闭准入与 readiness，再排空活动 task/run，
超时后标记恢复上下文并执行有界 abort。两者都没有本平台的多租户 Worker 租约，因此不能直接照搬进程关闭逻辑。

## Decision

1. Worker heartbeat v2 以兼容扩展增加 `accepting_work`、`draining_since` 和 `drain_deadline`。旧心跳缺少字段时
   按 `accepting_work=true` 读取；draining 心跳必须同时携带正向、有界的时间窗。
2. SIGTERM/SIGINT 处理器同步完成两件事：将 readiness 置为 false，并关闭进程共享的原子 admission fence。
   它不取消正在提交的 JetStream Future，避免截断“内存状态已推进、accepted/event 尚未获得 PubAck”的事务边界。
3. admission fence 是 incarnation 内的一次性开关。已接受 attempt 的精确重投仍幂等；新执行和恢复命令返回 NAK，
   由控制面在新的可调度 incarnation 上重发。恢复准入只能退出进程并创建新 incarnation。
4. Worker 主循环观察到信号后发布 draining heartbeat，停止拉取新执行和恢复命令，但继续处理当前 attempt 的身份
   续期、取消、审批、模型流和 Tool 结果。活动 assignment 继续随心跳续租，避免正常排空期间被 Reconciler 抢占。
5. PostgreSQL V12 在稳定 Worker 与 incarnation 表同时保存 drain 状态。Scheduler 和 recovery worker 查询只选择
   `accepting_work=true`；容量递增 SQL 再做一次同条件检查。相同 incarnation 的准入状态在数据库中也是单向的，
   后续伪造的 `accepting_work=true` 心跳不能重新进入调度池；较新的 incarnation 可以接管稳定 Worker。
6. 活动 attempt 全部完成后，Worker 发布 `active_runs=0` 的最终 heartbeat 并退出。90 秒默认 drain deadline 到期时，
   Worker 为所有非终态 attempt 再发布一次最新安全 Checkpoint 和 draining heartbeat，然后退出；控制面沿既有
   owner epoch/fencing/SAFE Checkpoint 流程恢复。
7. `AGENT_RUNTIME_DRAIN_GRACE_SECONDS` 允许 1–300 秒。Kubernetes Base 使用 90 秒，
   `terminationGracePeriodSeconds=120`，强制至少保留 10 秒给 Checkpoint、日志和传输 teardown。
8. deadline 后的 Checkpoint 只代表已持久状态边界，不宣称正在执行的外部非幂等 Tool 已安全完成；Tool Ledger
   已记录 started 但无 result 时，既有恢复判定仍进入 `indeterminate`，不得自动重放。

## Consequences

### Positive

- Pod 删除、滚动升级和未来 lease-aware 缩容不再把新 Run 送入即将退出的 Worker。
- readiness、消息准入、数据库调度和 Workspace lease 使用同一 draining 事实，避免各层状态互相矛盾。
- 不安全的 Future cancellation 被一次性原子栅栏替代，保留 JetStream PubAck 与 Kernel 状态推进顺序。
- 正常结束优先；超时后仍留下可审计 Checkpoint，恢复继续受 owner epoch、fencing 和副作用账本约束。

### Negative

- 当前只能在 attempt 边界发布 Checkpoint，不能暂停任意正在运行的容器进程或 Provider HTTP 请求后做内存快照。
- deadline 到期后进程会退出；若 NATS 或 Checkpoint Gateway 同时不可用，只能依赖此前已确认的 Checkpoint。
- Worker HPA 仍未启用。还需要在真实 Kubernetes 集群验证 eviction、PDB、NATS/Gateway 故障组合及 15 分钟恢复
  SLO，才能把该机制用于自动缩容。
- heartbeat v2 使用兼容扩展而非新 subject；跨版本滚动必须先升级控制面再升级 Worker，以便控制面理解 drain 字段。

## Alternatives Considered

- **SIGTERM 直接取消所有模型与 Tool Future**：可能在外部副作用已发生但结果未持久化时制造重复执行，拒绝。
- **仅撤销 Kubernetes readiness**：Scheduler 从 JetStream/数据库选 Worker，不读取 Service Endpoint，仍会下发任务，拒绝。
- **把 draining 表示为 `capacity=active_runs`**：丢失显式生命周期和 deadline，且容量变化可能重新开门，拒绝。
- **收到信号后取消正在 poll 的 Future**：Future 可能已经推进 Processor 却尚未完成 PubAck，不满足取消安全，拒绝。
- **deadline 后继续无限等待**：会超过 Kubernetes/supervisor 的硬终止预算，无法形成可预测运维语义，拒绝。
- **draining 后立刻释放 Workspace lease**：旧进程仍可能产生事件或副作用，会与新所有者并发写入，拒绝。

## References

- Codex：`codex-rs/core/src/thread_manager.rs` 的 `shutdown_all_threads_bounded`
- Codex：`codex-rs/core/src/session/mod.rs` 的 `shutdown_and_wait` / interrupt
- OpenClaw：`src/cli/gateway-cli/run-loop.ts` 的 restart admission fence 与 active work drain
- OpenClaw：`src/gateway/server-close.ts` 的有界 reply/run drain
- 本平台：`runtime/apps/worker/src/main.rs`、`runtime/apps/worker/tests/assignment.rs`
- 本平台：`control-plane/.../JdbcSchedulerRepository.java`、`V12__worker_draining.sql`
- 本平台：`deploy/tests/validate_kubernetes.rb`
