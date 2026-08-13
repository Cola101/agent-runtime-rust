# 统一 Process 交互 Yield 证据（2026-08-12）

## RED

1. `process.start` 的延迟输出测试先失败：输入中的 `yield_time_ms` 被拒绝为未知字段。
2. `process.write` 的延迟回显测试先失败：输入中的 `yield_time_ms` 被拒绝为未知字段。
3. 真实 Agent Loop 测试先在模型可见 schema 处失败：`process.start` 没有暴露有界 yield，证明旧链路仍需
   start 后追加 poll/wait，而不是只缺测试断言。

## GREEN

- `agent-tool-runtime` 全套通过：48 个单元测试及 persistent process、治理、崩溃、回收、64 Session /
  1024 wait 等全部集成门禁为 0 失败。
- `agent-runtime-host` 全套通过：核心、审批、恢复、取消、IPC、多 Provider、真实 Process Agent Loop、
  子代理和并发门禁均为 0 失败；1 个需要外部 Codex fixture 的 live 用例保持显式忽略。
- 新 Agent Loop 真实执行延迟 shell，完成 `process.start(yield) -> process.write(yield) -> process.close`；
  ready 与 echo 分别在原 Tool Call 返回，模型没有发出 `process.poll` 或 `process.wait`。
- `cargo clippy -p agent-tool-runtime -p agent-runtime-host --all-targets -- -D warnings` 与
  `cargo fmt --all --check` 最终通过。
- 最终未发现遗留 Host、PTY supervisor 或测试 shell；`cargo clean` 删除 23757 个可重建文件、共 6.0GiB，
  `runtime/target` 已不存在。

## 对标来源

- Codex：`codex-rs/core/src/tools/handlers/unified_exec.rs`、`write_stdin.rs`、`process_manager.rs`。
- OpenClaw：`src/agents/bash-tools.schemas.ts`、`bash-tools.exec-run.ts`、`bash-tools.process.ts`。
- 结论：本轮对齐 Codex 的 start/write 单调用 yield；相对 OpenClaw，write 后不再强制增加一次模型 poll。
  未验证或未实现项仍是副作用已接受后的丢结果恢复、Windows ConPTY、viewer/owner 和 Node relay。
