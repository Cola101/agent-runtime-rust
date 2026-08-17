# Kernel 终态与控制收据收敛证据（2026-08-17）

## 稳定 RED

网络恢复测试先完成真实 replacement Resume，并确认 Run 已通过 Event Cursor 到达 `succeeded`、Provider
总调用数为 2。随后把 durable receipt 恢复为崩溃窗口中的 `Accepted`，并在同目录留下合法
`<command-id>.json.partial` 未提交暂存文件。修复前同一 command 重放稳定失败：

```text
terminal evidence must reconcile an accepted receipt beside a staging file:
Status { code: Internal, message: "the Runtime could not complete this request" }
```

这不是测试超时：Kernel terminal event 已提交，失败发生在 receipt 投影扫描。此前全组偶发的同一 Internal
因此被还原为可重复的持久化竞态，而不是继续归类成无法解释的 flake。

## GREEN

- `control_detached` 与直接 `control` 对既有 Accepted receipt 共用终态收敛入口。
- 64 路固定 Run shard 串行同终态的并发重放；仍 active 时再取得 execution `record_gate`，与 finalizer 的
  Run/receipt 写入互斥。
- terminal event 是唯一终态权威；收敛只投影 Run 与同 Run Accepted receipts，不重新 dispatch Resume、
  approval 或 cancel。
- UUID `.json.partial` 被识别为未提交 staging 并忽略；非法 staging 名、未知文件和坏权威记录继续拒绝。
- 返回前重新读取精确 receipt，必须为 `Completed` 且状态匹配 Kernel terminal event。

## 已执行门禁

- 精确网络 Resume/replay RED→GREEN：1/1。
- `agent-runtime-host --test grpc_invocation_recovery`：2/2。
- `agent-runtime-host --test embedded_control`：9/9；覆盖审批、取消、并发命令、二次 owner 崩溃和存储失败。
- 最终 `cargo test -p agent-runtime-host`：223 通过、0 失败、1 个外部 Codex MCP fixture 显式忽略；
  gRPC Resume/replay 在完整并发套件中保持通过。
- `cargo clippy -p agent-runtime-host --all-targets --all-features -- -D warnings` 与
  `cargo fmt --all -- --check`：通过。

## 对标与边界

Codex 通过 Thread owner 与 rollout writer 串行 event/lifecycle；OpenClaw 通过 SQLite writer queue 和事务避免
JSON staging 暴露。本实现增加多租户 command digest、owner epoch 与持久 receipt，但只证明一个
`EmbeddedRuntime` 持有 state-root lease 的本机边界。跨机器 command ledger、共享文件系统和自动 staging GC
仍未实现。
