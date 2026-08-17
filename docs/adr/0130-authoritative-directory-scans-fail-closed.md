# ADR-0130：权威目录扫描失败关闭

- 状态：Accepted
- 日期：2026-08-17
- 范围：独立 Rust Runtime 的 Run 恢复、Session 归属、终态提交

## 背景

独立 Runtime 使用每个预注册 Profile 的本地 state root 保存 Run、Session、Checkpoint 与事件。此前
`list_run_records` 和 `find_active_session_turn` 都把 `read_dir` 的任意失败当成空目录，并用
`flatten()` 跳过逐项读取错误。结果是损坏、权限错误或瞬时 I/O 故障可能被误报成“没有待恢复 Run”或
“没有所属 Session”。后一种情况还可能允许 Runtime 发布终态事件，却没有先确认 root Session 的终态
Checkpoint 约束。

## 决策

1. 只有 `NotFound` 表示权威命名空间尚不存在，并返回空列表或 `None`。
2. `NotADirectory`、`PermissionDenied`、其他 I/O 错误以及目录迭代中的单项读取错误全部返回 typed
   `StateRoot` 错误，不允许转换为“空”。
3. 聚合恢复继续以 Profile 为故障隔离单元：损坏 Profile 进入 failure report，其他租户 Profile 继续按
   round-robin 计划恢复；损坏 Profile 不能被记为扫描成功且零 Run。
4. Session 归属无法验证时，不提交任何 Run 终态事件。调用方得到存储错误，修复存储后再由恢复入口接管。
5. 非 UUID 文件名仍视为不受 Runtime 管理的目录项并忽略。这与无法读取一个目录项不同；本轮不把任意
   外部文件升级为 Runtime 权威。

## 对标

- **Codex `ff352fab6209`**：rollout loader 对打开文件和逐行读取的 I/O 错误使用 `?` 传播；对已经成功
  读取的 JSON 解析错误则计数并跳过。其 rollout 是单用户 Thread 历史，容错边界不等同于本项目直接决定
  多租户恢复和副作用状态的 Event/Run 权威。本项目只吸收“存储 I/O 不伪装成空”的语义，不照搬坏记录跳过。
- **OpenClaw `58b4b9430457`**：Session transcript 读路径直接执行 SQLite 查询，写路径使用进程内 writer
  queue、`BEGIN IMMEDIATE` 事务和 snapshot revalidation；查询、JSON 解码、冲突或事务错误都向调用方传播。
  它的存储引擎与迁移体系更成熟。本项目保持无外部数据库的嵌入式边界，但在权威扫描失败语义上与其对齐。

## 代价与未覆盖

- 一个无法读取的目录项会阻断该 Profile 的恢复或终态提交。这是保守选择，运维侧必须修复或隔离损坏，
  不能用可用性掩盖状态不确定。
- 本轮没有实现自动 quarantine、状态修复工具、介质校验或跨机器共享存储。
- Session record 当前仍是本地文件制品；其落盘耐久提交协议与 Run record/Checkpoint 的一致化另行处理。
