# 持久 Process 交互收据证据（2026-08-12）

## RED

1. 真实 `process.start` 在进程已写入启动标记、Host 于 start-yield 返回前中断后，替代 Host 得到
   `run.indeterminate`。这证明旧 Worker 把所有已启动 NonIdempotent Tool 一律终止，无法利用已有 Manifest。
2. 真实 `process.write` 在目标进程已写入唯一输入标记、Host 于 write-yield 返回前中断后，替代 Host 同样
   无法交付结果。单靠 Manifest 的 `stdin_write_intent` 不能证明字节已送达，因此未将 intent 当作成功。

## GREEN

- start 恢复使用唯一、摘要保护的 Session Manifest，替代 Host 返回原 Session；工作区启动标记始终只有
  一行，Provider 调用序列只有 `process.start`、`process.close`。
- write 在发送成功后原子写入一份交互收据；替代 Host 从原 cursor 交付同一 Session。工作区输入标记始终
  只有 `write-once` 一行，Provider 调用序列只有 start、write、close；相同测试连续 10/10 轮通过。
- `process_session_loop` 8/8 通过；`persistent_process_session` 17/17 通过；一般 Workspace 非幂等写入的
  旧恢复测试仍稳定产生 `run.indeterminate`，没有把通用 Tool 错当作可恢复 Process 操作。
- `agent-tool-runtime` 全量 103 通过、0 失败；`agent-runtime-host` 全量 124 通过、0 失败、1 个外部 Codex
  live fixture 显式忽略。Clippy `-D warnings` 与 `cargo fmt --all --check` 均通过。
- 最终未发现遗留 Runtime Host、PTY supervisor 或测试进程；`cargo clean` 删除 26270 个可重建文件、共
  7.0GiB，`runtime/target` 已不存在。

## 对标来源与边界

- Codex：`codex-rs/core/src/tools/handlers/unified_exec/{exec_command,write_stdin,process_manager}.rs`。
- OpenClaw：`src/agents/bash-tools.{exec-run,process,schemas}.ts` 及 finished-retention/poll 测试。
- 已确认优势仅限本轮窄面：多租户、Workspace 和原 attempt 绑定的跨 Host 收据恢复。Codex 的跨平台执行、
  sandbox/事件产品链，以及 OpenClaw 的 Node relay、viewer/owner 和 Windows 仍领先。
