# ADR-0134：Checkpoint 绑定的终态事件发布

- 状态：Accepted
- 日期：2026-08-17
- 范围：Worker Checkpoint、独立 Runtime Host、Root Session、角色子代理

## 背景

Root Session 与角色子代理需要在终态 Event 对调用方可见之前保存完整 transcript，因此现有正确提交顺序是
`terminal Checkpoint → terminal Event → Session head / parent result`。此前只覆盖了 Event 已提交而后续投影尚未
提交的恢复窗口；若进程在第一步和第二步之间退出，Checkpoint 已是终态，Event Log 却没有终态。恢复代码既不能
把终态 Checkpoint 当作普通工作重新执行，也不能凭状态重新生成 event id、timestamp 或 trace id。真实故障测试证明
Root Session 会直接报“event log disagrees with Checkpoint”，子代理则会把终态 Checkpoint 送入普通 restore 并因
身份不匹配失败。

## 决策

1. Worker Checkpoint schema 27 保存 Kernel 已生成的完整 terminal `EventEnvelope`。该 envelope 与 Checkpoint
   一起进入摘要，且 tenant、Session、Run、attempt、sequence、status/type 和 payload digest 必须一致。
2. 提交顺序保持 `Checkpoint → Event → projection`。不把 Event 提前，因为终态可见时必须已经存在能重建
   Session/子代理结果的 transcript；也不引入 SQLite、PostgreSQL 或外部服务作为本地 Run 前置依赖。
3. 替代 Host 只有在 Session active head 或父 Checkpoint 仍精确拥有该工作时才允许收敛。Event Log 已有同一终态
   时幂等返回；没有终态时，只有完整多租户身份一致、sequence 连续且最后序号紧邻 receipt 的前缀才能追加原始
   envelope。已有不同终态、终态后仍有事件、身份漂移或序号缺口全部 fail-closed。
4. 终态子代理不得进入 `WorkerProcessor::restore`。Host 先用当前角色、输入、历史、权限、预算、Tool/Skill/MCP
   目录和新 owner epoch 验证 Checkpoint 的完整执行绑定，再补发缺失 Event、生成父结果收据；模型和 Tool 均不重放。
5. schema 1—26 的终态 Checkpoint仍可在 Event 已经存在时读取。若旧 Checkpoint 的 Event 也缺失，则无法证明
   原始事件身份，必须明确拒绝，不能生成“看起来等价”的新终态。

## 对标

- **Codex `ff352fab6209`**：rollout recorder 以单 writer task 串行写入，并为 Flush/Shutdown 提供明确 ack；
  Thread 恢复从已提交 rollout 重建。本方案保留“先持久、后可见、单一事件身份”原则，但将 receipt 绑定到完整
  多租户执行身份，适配无数据库的嵌入式 Runtime。
- **OpenClaw `58b4b9430457`**：Session transcript 由进程内 writer queue 加 SQLite `BEGIN IMMEDIATE`/WAL
  串行，并在事务内重验快照。其多记录事务、迁移和并发写成熟度仍领先；本方案只为文件模式中经过证明的
  Checkpoint→Event 窗口提供确定性收敛，不声称替代通用事务存储。

## 代价与未覆盖

- 每个新终态 Checkpoint 多保存一个有界 EventEnvelope，并将 Worker Checkpoint schema 提升到 27。
- 该机制依赖 standalone daemon 或 EmbeddedRuntime 对 state root 的单 owner 边界；共享文件系统、跨机器并发
  owner 和硬件掉电仍未验证。
- 本 ADR 关闭的是终态 transcript 发布窗口，不等价于 Event、Checkpoint、Run record、Session 和所有控制收据
  之间已经具备任意多文件原子事务。其他跨文件窗口仍需逐项以副作用风险和可执行故障测试证明。
