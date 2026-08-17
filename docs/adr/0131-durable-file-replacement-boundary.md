# ADR-0131：本地权威文件的耐久替换边界

- 状态：Accepted
- 日期：2026-08-17
- 范围：独立 Rust Runtime 的 Session、Run、Checkpoint、控制收据、子代理结果、Tool 处置与保留账本

## 背景

独立 Runtime 的 Event Log 已以换行作为提交标记，并在 append 后 `sync_data()`；Run record、Checkpoint、
Embedded control receipt 和 retention ledger 也已经使用“临时文件同步→rename→父目录同步”。但这些实现散落
在三个模块，Session record、子代理结果与模糊 Tool reconciliation 仍只有 `write→rename`。进程退出后文件
通常可见，并不能证明主机掉电后目录项和文件内容已经提交。

审计还发现模型路由 journal 与 Provider 健康文件也使用较弱写法。把所有更新直接升级为每次强同步后，
1000 Run 门禁从约 116 秒退化到超过 200 秒并突破 90/180 秒硬阈值。模型路由在一次普通 Turn 内有多次
高频状态推进，不能用若干独立文件 fsync 模拟事务日志。

## 决策

1. 新增唯一 `durable_file::replace` 原语，固定执行：创建父目录→创建可预测 staging→完整写入→
   `sync_all(staging)`→关闭文件→原子 rename→Unix 父目录 `sync_all()`。任一步错误都返回 typed
   `StateRoot`，不得继续宣称提交成功。
2. Session record、Run record、Checkpoint、Embedded control receipt、retention ledger/segment、子代理结果
   与 Tool reconciliation 共用该原语。原有 Run/Checkpoint/receipt/retention 只去重实现，不改变提交顺序。
3. Event Log 保持 ADR-0129 的追加式协议；它不是整文件替换，不能强行复用本原语。
4. Provider 健康是可重建的路由缓存，本轮不升级为强提交权威。其丢失可能降低短期路由质量，但不能放宽
   单 Run 已冻结的候选、尝试预算或副作用围栏。
5. 模型路由 journal 是恢复权威，但本轮明确保留为未完成项。下一阶段应改成有界追加 WAL 或等价组提交，
   将“Provider 调用前的 inflight fence”和“响应 staging commit”变成少量 durable record；不得靠放宽容量
   门禁，也不得继续给每次投影更新追加独立 fsync。该后续项已由 ADR-0132 完成。

## 对标

- **Codex `ff352fab6209`**：rollout recorder 使用单 writer task、内存 pending queue、显式 persist/flush ack，
  写失败会重开并重试；检查到的普通 rollout `flush` 路径调用 Tokio file flush，没有把每个记录升级成独立
  `sync_all`。它的串行 writer 与恢复模式成熟，但其单用户历史文件不等同于本项目的多租户副作用权威。
- **OpenClaw `58b4b9430457`**：Session/Agent 状态使用 SQLite WAL、`synchronous=NORMAL`、
  `BEGIN IMMEDIATE` 和 commit 后 publication，把多行变化收敛到一个事务边界。它在高频状态组提交、并发写、
  完整性检查和 quarantine 上领先。本项目仍保持“无外部服务即可运行”，但下一阶段应吸收 WAL/组提交结构，
  而不是继续扩散 JSON 整文件替换。

## 代价与未覆盖

- 本轮的操作记录器证明调用顺序和错误传播，不是硬件断电或存储控制器撒谎测试；macOS `sync_all` 也不等于
  对所有硬件声明绝对持久。
- 多个权威文件之间仍无原子事务。Checkpoint→terminal Event→Session head 的两个终态窗口已由 ADR-0134
  使用摘要绑定的原始 Event receipt 收敛，但不能等价为任意多记录数据库事务。
- 可预测 staging 文件假设每个 state root 只有一个进程 owner；EmbeddedRuntime 在 Unix 已持有 `flock`，
  普通 LocalRuntimeHost 的跨进程写者治理仍需继续收口。Windows rename/目录同步语义未验证。
- 模型路由 journal 的有界 WAL 已由 ADR-0132 完成；自动 quarantine/repair、真实硬件掉电和跨机器共享存储
  仍未完成。

## 备选方案

- **所有 JSON 投影逐次强同步**：正确性直观，但实测击穿本地容量门禁，拒绝。
- **立即把独立 Runtime 改为 SQLite**：事务与 WAL 成熟，但本轮会扩大依赖、迁移和并发模型；先完成文件边界
  收口并单独设计高频 journal。
- **维持全部 `write→rename`**：进程崩溃通常可恢复，但无法支撑已对外确认的 Session/控制/副作用证据，拒绝。
