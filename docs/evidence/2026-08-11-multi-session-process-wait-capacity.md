# 多 Session Process Wait 容量证据（2026-08-11）

## RED / GREEN

1. 64 个真实 Session、1024 个 wait 的前四轮试跑中，第 4 轮在关闭第 56 个 Session 时出现
   `Operation not permitted (os error 1)`。检查发现旧逻辑在身份租约释放后仍无条件向记录的 PGID 发信号，
   PGID 退出/复用窗口既会产生该错误，也可能误伤无关进程组。
2. 新增确定性单元测试：创建一个未持有的 `identity.lock`，再把一个独立进程组放在旧 PGID 位置。旧实现
   真实返回 `EPERM`，证明没有身份租约也会操作该组。
3. 加入身份围栏后，租约释放直接 no-op；TERM/KILL 与有界回收都复核租约，未释放的最终残留返回
   `Indeterminate`。第二个 RED 进一步证明“PGID 不存在但身份仍由后代持有”不能算成功；修复后两个确定性
   测试均转绿，容量测试随后连续 10/10 轮通过。

## 容量结果

- 8 个 tenant、64 个独立 Workspace、64 个真实 shell Session、每 Session 16 个 wait，共 1024 个并发 wait。
- 注册稳定时为 1024 waiter / 64 observer；最终 250ms 取证为 295 次持久文件观察，低于 512 门禁。
- 每 Session 取消 1 个等待者后，64 个取消均在 2 秒内完成，其余 960 个等待者和 64 个 observer 不受影响。
- 64 路输入并发写入后，最终取证为 p50 905.30ms、p95 982.02ms、p100 995.13ms；全部 Session 和 tenant
  收到各自输出。连续 10 轮总测试耗时分别落在 7.87—8.43 秒，均通过。
- 最后一个 waiter 返回后 observer 收敛为 0；64 个 Session 全部关闭。测试 panic guard 也以身份锁围栏，
  不会按已释放的 PGID 清理无关进程。

## 回归门禁

- `persistent_process_session`：15/15 通过；两个 PTY 用例在一次高负载并行回归中瞬时失败，分别单跑和整套
  重跑均通过，未当作产品通过证据掩盖，保留为后续 flake 观察项。
- `process_session_capacity`、`process_session_governance`、`process_session_process_crash`、
  `process_tree_reaping` 和本容量测试合计 34/34 通过。
- Rust 全工作区共 593 项：587 通过、0 失败、6 个外部 live 用例显式忽略。
- `cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt --all -- --check` 通过；外部 live 用例
  仍不计入本地内核闭环。
- 最终没有 child/supervisor/Host 或测试临时目录残留；`cargo clean` 移除 8.9GiB 构建缓存，Graphify 输出
  已删除，仓库占用恢复为 32MiB。

## 对标快照

- Codex `ff352fab6209`：`MAX_UNIFIED_EXEC_PROCESSES=64`，进程 output/state 使用
  `Notify/watch/broadcast`，容量表可清理候选项。本平台已对齐 64 容量和共享唤醒，但 live 多租户进程不做
  LRU 淘汰；统一 exec/yield 和跨平台实现仍落后。
- OpenClaw `58b4b9430457`：Node Host progress writer 使用串行队列、sequence、heartbeat 和 pause/resume；
  本平台的 durable cursor、tenant/Workspace 校验与 identity lease 属于不同层。Node relay、viewer 和 Windows
  仍由 OpenClaw 领先。
