# ADR-0132：有界模型路由预写日志

- 状态：Accepted
- 日期：2026-08-17
- 范围：独立 Rust Runtime 的 Provider 路由、重试、故障转移与响应恢复

## 背景

模型路由状态决定替代 Host 是否可以再次调用 Provider，因此不是普通缓存。ADR-0131 的首次实现把每次状态
推进都改成整文件强替换，1000 Run 保留门禁从约 116 秒退化到 201.76 秒。恢复弱写虽然找回性能，却留下
两个错误边界：Provider 出站围栏和已经收到的响应可能在进程或主机故障后消失。

一次正常模型调用还会依次推进 inflight、staged response、事件观察和 completion。继续堆整文件 fsync
会把可靠性成本按状态数线性放大；完全不提交中间状态则可能重复 Provider 请求或丢掉已收到的响应。

## 决策

1. 每个模型请求使用一个单写、有界、全状态记录的追加 WAL。每行包含 `record_version`、连续 `revision` 与
   完整 `journal`，最终换行是记录提交标记；EOF 唯一未换行尾部可在下次 append 前截断，已换行坏记录、
   空记录、超限记录和 revision 缺口全部 fail-closed。
2. 第一条记录和第 32 条之后的 compaction 使用 ADR-0131 的
   `write→sync file→rename→sync parent`；其余记录一次 append 后 `sync_data()`。compaction 将当前完整状态
   重写为 revision 1，不保留无界历史。
3. 单条记录上限 8 MiB，WAL 最多 32 条，并在读取前用文件元数据执行总大小上限，避免先把无界损坏文件读入
   内存。路径不存在与路径不可读/非普通文件严格区分，只有 `NotFound` 可以创建新 WAL。
4. WAL 固定 Run、模型请求摘要、路由策略摘要和候选链。相邻记录还必须满足状态单调约束：候选游标、失败与
   重试列表、观察计数、同候选尝试数、Provider 选择和 completion 不得回退；staged response 只能保持不变，
   或在 completion 时清空。连续 revision 但状态回滚同样视为损坏。
5. V1/V2 单 JSON snapshot 在任何 Provider 出站前校验并原子迁移成一条 V3 WAL；V3 snapshot 不冒充旧版本。
   已完成请求为新 attempt 归档时使用 rename 后父目录同步，不能只依赖 rename 可见性。
6. 提交点按恢复职责收敛。普通成功路径固定为四条记录：Provider 出站前 inflight fence、完整响应 staging、
   Event+Checkpoint 已提交后的选择观察、最终 completion。纯 attempt-id 变化、等待结束和候选跳过不单独
   制造 fsync；它们必须在下一条真正的恢复权威记录中一起提交。

## 对标

- **Codex `ff352fab6209`**：rollout recorder 使用单 writer task、pending queue、persist/flush acknowledgement
  和失败重开，适合统一会话历史。本方案吸收其单写与显式提交确认，但不把高频 Provider 路由混进完整会话
  rollout，避免扩大多租户恢复权威。
- **OpenClaw `58b4b9430457`**：Session/Agent 状态依赖 SQLite WAL、`synchronous=NORMAL`、
  `BEGIN IMMEDIATE` 和 commit 后 publication，在并发事务、迁移、完整性检查和组提交上仍更成熟。本方案
  吸收 WAL/compaction 思路，保持嵌入式 Runtime 无数据库和外部服务即可完成一次 Run。

## 代价与未覆盖

- 全状态记录会重复序列化 staged response；32×8 MiB 是硬上界而非期望工作集。后续若响应体继续增大，应
  使用内容寻址对象，而不是放宽 WAL。
- compaction 保留恢复所需的当前状态，不是审计历史；审计仍由 Event Log 承担。
- 只有单 state-root owner/single writer 得到保证；共享文件系统、多进程写、介质位翻转、硬件断电和 Windows
  目录同步尚未验证。多个权威文件之间也仍无数据库式原子事务。
- Provider 健康仍是可重建缓存；本 ADR 只保护单 Run 已冻结的候选、尝试预算和响应。

## 备选方案

- **每次状态整文件强替换**：正确但已用容量门禁证明不可接受，拒绝。
- **立即引入 SQLite**：事务能力成熟，但改变嵌入依赖、迁移和部署边界；保留为并发写需求出现后的适配器。
- **只持久最终响应**：无法在出站前围栏 Provider，也无法区分“请求没发出”和“响应丢失”，拒绝。
