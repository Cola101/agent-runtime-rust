# 原生 PTY Agent Loop 证据（2026-08-11）

## RED / GREEN

1. `process_start_tty_allocates_a_real_terminal` 首次失败于 `tty/cols/rows` 未知字段；实现 `openpty + setsid/TIOCSCTTY` 后，真实 child 的 `[ -t 0 ] && [ -t 1 ]` 转绿。
2. `process_write_sends_input_to_the_live_terminal` 首次稳定失败为旧 FIFO `ENXIO`；写入切到 PTY master 后，真实 shell 返回 `got:from-model`。
3. `replacement_manager_never_claims_a_live_pty_was_reattached` 首次得到错误的 `Reattached`；加入启动前 PTY marker 与保守回收后，replacement 返回 `Indeterminate`。

## 真实闭环

`runtime-host/tests/process_session_loop.rs` 的回环模型实际调用：

```text
process.start {tty:true, cols:100, rows:32}
→ process.poll 观察 terminal=yes / ready
→ process.write "agent-loop\n"
→ process.poll 观察 got:agent-loop
→ process.close
→ 模型成功终止
```

该链只启动本地 Rust Runtime、HTTP 回环模型和真实 Unix child；没有 Java、NATS、数据库、Docker 或 Kubernetes。

## 对标结论

- Codex：已对齐 Unix 真 PTY、交互输入、进程组与增量输出的核心语义；缺 `unified_exec` 的 resize、统一 output buffer/yield、跨平台 backend 与产品集成。
- OpenClaw：已对齐真 PTY 与 tree kill；缺 `TerminalSessionManager` 的 resize、pause/resume 回压、scrollback detach/attach、Windows 与 Node relay。
- 本平台：普通 pipe 会话仍可跨 Host reattach；PTY 在不可重新打开 master 时明确回收并 `indeterminate`。这是诚实的故障语义，不是完整恢复能力。

## 门禁

- `agent-tool-runtime`：87 通过，0 失败。
- 真实 PTY Runtime Host Agent Loop：1/1 通过；同文件全部 3/3 通过。
- Rust 全工作区：571 通过，0 失败，6 个外部 live 用例显式忽略。
- `cargo fmt --all -- --check` 与目标包 `cargo clippy --all-targets -- -D warnings` 通过。

## 下一缺口

实现最小独立 PTY session supervisor，由 supervisor 持有 master；Runtime Host 通过 Unix socket 进行身份校验后的 write/resize/poll，并增加有界输出回压与 replacement reconnect。完成前不得把当前 PTY 表述为可跨 Host 续接。
