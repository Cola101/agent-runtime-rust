# 持久 Process Wait 证据（2026-08-11）

## RED / GREEN

1. 真实 Agent Loop 首次在 `process.wait` 未进入 Skill snapshot 时失败，错误为 Tool 未激活；注册 Pure Tool
   后，真实 child 延迟 1 秒输出，单次 wait 返回 `delayed-ready` 与 terminal state，没有模型侧 poll 循环。
2. wait 首次允许模型请求超过 Run-frozen Tool timeout；新增双重校验后，100ms wait 在 20ms Tool budget
   下于执行前拒绝，不以内部长等待绕过执行策略。
3. Host 在第二个 `tool.execution.started`（wait）已进入持久事件后被 abort；replacement 恢复同一 Run，
   child start 总计一次，Provider 给出的 start/wait Tool Call 也各一次，最终 Run 成功。

## 输出背压复核

- supervisor reader 只有一个 8KiB 栈 buffer，读取后同步 write+flush 到 durable log；没有 channel、ring 或
  WebSocket send queue。磁盘阻塞会停止 PTY read，由内核 PTY 缓冲反压生产者。
- noisy PTY 既有真实门禁仍在冻结输出预算精确终止进程组；`process.wait` 只消费既有有界 poll 输出，不创建
  第二份 scrollback。
- OpenClaw 的 4MiB/512KiB 高低水位直接读取连接 `bufferedAmount`，适用边界是 live viewer 传输，不是当前
  Runtime 的持久日志层。因此本轮明确不实现伪造的 Kernel pause/resume 状态。

## 质量门禁

- Rust 全工作区共 587 项：581 通过、0 失败、6 个外部 live 用例显式忽略。
- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 与差异门禁通过。
- 最终复核无 Cargo、PTY supervisor 或 child 残留；临时 Cargo target 与 Graphify 输出均删除。

## 对标快照

- Codex `ff352fab6209`：`codex-rs/core/src/tools/handlers/unified_exec` 和 `utils/pty` 已提供统一 yield/write、
  bounded mpsc/broadcast、Unix/Windows PTY；本平台仍缺 start+yield、write+yield 和跨平台 backend。
- OpenClaw `58b4b9430457`：`src/gateway/terminal/output-flow-control.ts` 的 pause/resume 与高低水位服务于
  WebSocket viewer；本平台的窄优势是 wait 与 durable cursor/Checkpoint/Host replacement 绑定。
- 下一项固定为事件驱动共享 wait 与 1000 并发容量证明，不进入 GUI 或控制面。
