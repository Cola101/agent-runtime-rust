# ADR-0101：持久 Process Close Intent 与跨 Host 收敛

状态：Accepted（2026-08-12）

## 决策

1. `process.close` 在发出 TERM/KILL 之前先原子持久化 schema-1 交互收据。该收据是关闭意图，必须精确绑定
   tenant、canonical Workspace、run、原 attempt、Tool Call、binding digest、Session、参数摘要及
   stdout/stderr cursor。
2. Process Session Manifest 继续作为资源状态真相。关闭执行先将 Manifest 推进为
   `Terminating/Closed`，再通过 identity lease 围栏执行 TERM→KILL；资源确认消失后才发布
   `Terminated/Closed`。交互收据与终态 Manifest 共同构成可恢复结果，避免另造第二份终态账本。
3. 替代 Host 只能在关闭收据完整且与原 Tool 请求逐字段一致时继续同一关闭意图。Session 仍为 Running
   时完成首次关闭，处于 Terminating 时继续身份围栏回收，已经 `Terminated/Closed` 时只读取终态结果；
   不重新询问模型，也不创建第二个 Process Session。
4. 缺失、损坏、摘要不匹配、跨 tenant/Workspace、来源 attempt/call/binding 不一致，以及终态不是
   `Terminated/Closed` 时均返回不可恢复，由 Worker 保持 `run.indeterminate`。自然退出发生在 close 前时
   没有待恢复副作用，直接保留自然终态，不伪造关闭收据。
5. `process.interrupt` 仍是信号型非幂等操作。没有可证明的收敛终态前，不沿用 close 的恢复规则，也不把
   “信号已发送”等价为目标进程已处理。

## 原因

- 关闭不是可安全重复的普通 Tool 调用，但它具有可收敛的资源终态。先记录意图、再依赖持久资源身份继续
  终止，比把所有崩溃都永久标记为 indeterminate 更有用，也比无凭据重发 close 更安全。
- 多租户 Runtime 不能依赖旧 Host 的内存 handle。恢复资格必须来自持久、摘要保护且租户隔离的事实。
- 复用交互收据中的历史字段 `committed_at` 仅表示该收据已持久化的时间；对 `close` 不代表终止已经完成，
  完成事实只由 `Terminated/Closed` Manifest 给出。

## 对标判断

- Codex `unified_exec` 已有成熟 `exec_command`/`write_stdin`、有界 yield、事件和跨平台执行后端；本次检查的
  handler 主链依赖当前 `unified_exec_manager`，未证明 CLI/Host 替换后以租户绑定 close intent 恢复结果。
- OpenClaw `process kill/remove` 优先调用进程内 supervisor，失败时按 PID tree kill，并立即更新或删除
  Gateway session。其 Node relay、owner/viewer、Windows 和设备生命周期更成熟，但不是同口径的 durable
  multi-tenant close receipt。
- 本平台只在“跨 Host、租户绑定的关闭收敛”这一窄面更明确；不据此宣称整体进程执行能力领先。

## 验收边界

- 真实进程必须忽略 TERM，使首个 Host 能稳定中断在身份围栏终止阶段；替代 Host 必须完成同一 close，
  返回 `Terminated/Closed`，且进程启动次数仍为 1、Provider Tool 序列仍只有 start/close。
- 一般 NonIdempotent Tool 的响应丢失仍必须进入 `indeterminate`，证明恢复接口没有扩大到通用副作用。
- `agent-tool-runtime`、`agent-runtime-host` 全量与 Clippy `-D warnings`、Rust 格式必须通过。
- 本 ADR 不包含 Windows ConPTY、OpenClaw Node relay/viewer、GUI、Java、NATS 或 Linux cgroup live 验收。
