# ADR-0133：Kernel 终态事件与控制收据收敛

- 状态：Accepted
- 日期：2026-08-17
- 范围：Embedded Runtime、gRPC/IPC 控制命令重放、Run 终态投影

## 背景

ADR-0125 规定 Kernel terminal event 是 Run 终态权威，Run record 与控制收据只是随后写入的投影。真实 gRPC
恢复门禁暴露了一个窄窗口：客户端已经从 Event Cursor 看见 `run.succeeded`，同一 Resume command 重放却
偶发返回 Internal。原因不是 Provider 重放，而是执行 finalizer 与重放线程同时更新 receipt：前者正在用
`<command-id>.json.partial` 做耐久替换，后者扫描目录时把这个未提交暂存文件当成非法权威记录；两个写者也
可能竞争同一个暂存路径。

## 决策

1. 对已有 Accepted receipt 的重放，先从 Kernel Event Log 查找 terminal event。存在终态时，只把该事件
   投影到 Run record 和同 Run 的 Accepted receipts；不得重新 dispatch Resume/approval/cancel，也不得由
   adapter 自行合成终态。
2. 终态收敛先取得 64 路有界 Run-id reconciliation shard；若本进程仍有 active execution，再取得该执行的
   `record_gate`。finalizer 和重放因此不能同时写 Run/receipt，同一终态后的并发重放也不能竞争 staging。
3. `control_detached` 和直接嵌入调用的 `control` 共用同一收敛入口，避免网络适配器与进程内调用形成两套
   幂等语义。收敛后重新读取精确 command receipt，并验证 `Completed + terminal status` 后才返回。
4. `control-receipts` 扫描只忽略文件名严格为有效 UUID 加 `.json.partial` 的未提交 staging；其他未知文件、
   非 UTF-8 名称、坏 JSON 或绑定错误继续 fail-closed。崩溃遗留 staging 不是权威记录，不能永久阻断恢复。
5. shard 数量固定，不按 Run 数增长；它只串行低频终态收敛，不进入模型、Tool 或 Event 热路径。

## 对标

- **Codex `ff352fab6209`**：rollout 事件与 Thread 生命周期由单 writer/Thread owner 串行，客户端重连从已提交
  history 恢复，不让 transport retry 创建第二个 Turn。本方案保留事件权威与单 owner 原则，并增加多租户
  command digest、owner epoch 和跨适配器持久 receipt。
- **OpenClaw `58b4b9430457`**：Session/Agent 写入由 SQLite writer queue 与事务串行，commit 后再 publication，
  因而不会暴露同名 JSON staging。其事务存储更成熟；本方案在无数据库嵌入模式下用有界门和 Kernel terminal
  evidence 收敛，不把本地投影错误升级为重复副作用。

## 代价与未覆盖

- 本方案依赖一个 `EmbeddedRuntime` 对 state root 的 Unix 单进程 lease；跨机器 owner、分布式 command
  ledger 和共享文件系统仍未实现。
- 64 路 shard 可能让哈希碰撞的无关 Run 短暂串行，但终态收敛是低频路径，且不会形成按历史增长的锁表。
- 暂存文件目前在下一次同 receipt 写入时覆盖，不主动做目录级 GC；自动 quarantine/repair 仍是运维缺口。
