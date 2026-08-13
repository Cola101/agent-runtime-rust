# 持久 Process Close 恢复证据（2026-08-12）

## RED

- 真实 `process.start` 启动一个记录 PID、忽略 TERM 的子进程；模型随后只发一次 `process.close`。
- 测试在 Manifest 已成为 `Terminating`、身份围栏终止尚未完成时中断首个 Runtime Host。
- 旧实现的替代 Host 只产生 `run.restored → run.indeterminate`，无法交付已经接受的 close 结果。这证明缺口
  位于跨 Host 关闭恢复，而不是模型或测试替身。

## GREEN

- close 在副作用前原子写入绑定 tenant、Workspace、run、原 attempt、Tool Call、binding、Session、参数
  摘要和 cursor 的 intent receipt；替代 Host 校验同一收据后继续原 Session 的身份围栏终止。
- 相同真实崩溃用例返回 `Succeeded` 和模型文本 `close receipt recovered`；工作区启动计数只有
  `started` 一行，Provider 调用序列只有 `process.start`、`process.close`，证明没有重启进程或重发 close。
- 自然退出后才收到 close 的旧测试继续返回原终态，不制造 close intent；测试夹具原先把所有 Tool Call
  固定为同一 binding digest，已改为按调用身份和参数生成真实唯一摘要。
- 一般 Workspace 非幂等副作用的替代 Host 测试仍返回 `indeterminate`，Kernel 的 unknown
  non-idempotent 测试也保持通过。

## 质量门禁

- `agent-tool-runtime`：103 通过、0 失败。
- `agent-runtime-host`：125 通过、0 失败、1 个外部 live fixture 按条件忽略。
- `process_session_loop`：9/9 通过，其中新增跨 Host close 恢复真实进程用例。
- `cargo fmt --all -- --check` 通过。
- `cargo clippy -p agent-tool-runtime -p agent-runtime-host --all-targets -- -D warnings` 通过。

## 对标来源与边界

- Codex：`codex-rs/core/src/tools/handlers/unified_exec/{exec_command,write_stdin}.rs`；其统一执行、yield、事件、
  sandbox 与跨平台 backend 仍领先。
- OpenClaw：`src/agents/bash-tools.process.ts`；其 supervisor cancel、PID tree fallback、Node relay、viewer/owner
  与 Windows 生命周期仍领先。
- 本轮已确认优势仅限 tenant/Workspace/原 attempt 绑定的跨 Host close intent 收敛。`process.interrupt`、
  Windows ConPTY、Node relay/viewer、真实 Linux cgroup 和外部控制面均未完成。
