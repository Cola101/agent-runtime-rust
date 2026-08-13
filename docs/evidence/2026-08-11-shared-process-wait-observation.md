# 共享 Process Wait 观察证据（2026-08-11）

## RED / GREEN

1. 加入运行指标后，旧实现的 1000 个并发 wait 显示 `active_observers=1000`，容量门禁要求 1，测试真实失败。
   这证明旧的 50ms 循环会把同一份持久状态观察放大到每个等待者。
2. 第一版共享观察器把 active observer 降为 1，但输出后唤醒全部 1000 人耗时 2.399 秒，超过 2 秒门禁；
   原因是每个消费者醒来后仍执行完整 sweep。
3. 观察器承担唯一 sweep 后，等待者改为重新校验 tenant/Workspace 的无恢复锁持久读取；1000 人门禁通过：
   一个 observer，250ms 新增观察不超过 10，真实 `fanout` 输出后全部在 2 秒内完成并看到相同内容。

## 真实边界

- Pipe：真实 `/bin/sh` 子进程从 stdin 收到 `fanout` 并写入 durable stdout，1000 个 wait 共享唤醒。
- PTY：独立 `agent-pty-session-supervisor` 持有 master 并写 durable log，wait 在 2 秒门禁内看到
  `got:pty-event`；没有依赖 Host 持有 PTY fd。
- 取消：最后一个 wait 的 `CancellationToken` 取消后，wait 在 1 秒内返回 `Cancelled`，active waiter 和
  observer 都收敛为 0。
- 恢复：既有真实 Host replacement 测试在 durable `tool.execution.started` 后 abort 原 Host；新 Host
  恢复同一 wait，child start 和 Provider start/wait Tool Call 都各一次。
- 时序：事件驱动可能先返回 `running + 新输出`，而不是旧轮询偶然得到的 `exited + 新输出`。测试现在验证
  “输出或终态任一发生即返回”，并独立验证短进程随后收敛终态。

## 质量门禁

- `persistent_process_session`：15 项通过，包括 1000 wait、取消回收和外部 PTY。
- `process_session_loop`：5 项通过，包括无模型 busy-poll 与 Host replacement。
- Rust 全工作区共 590 项：584 通过、0 失败、6 个外部 live 用例显式忽略。
- `cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt --all -- --check` 通过。
- RED 失败留下的两个精确测试进程组已终止；最终无 child/supervisor/Host 残留，Rust `target`、Graphify
  输出和本轮临时目录均删除。`cargo clean` 移除 10.2GiB，仓库最终占用 32MiB。

## 对标快照

- Codex `ff352fab6209`：`core/src/unified_exec/process_manager.rs` 使用 `Notify/watch` 等待 output/close/cancel，
  `exec-server/src/process.rs` 还提供有界 replay + live broadcast。我们的共享唤醒已对齐，但统一 exec API、
  Windows backend 和成熟 output event log 仍落后。
- OpenClaw `58b4b9430457`：`src/node-host/pty-command.ts` 将 PTY pause 到异步 chunk emit 完成，Gateway 还有
  bufferedAmount 水位。本平台只在持久 Kernel 层做共享观察，viewer 与 Node relay 仍明显落后。
