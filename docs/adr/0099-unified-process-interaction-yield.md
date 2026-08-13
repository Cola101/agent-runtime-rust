# ADR-0099：统一 Process Start / Write / Wait 的有界 Yield

状态：Accepted（2026-08-12）

## 决策

1. `process.start` 与 `process.write` 新增可选 `yield_time_ms`；`process.wait` 保留必填的
   `yield_time_ms`。三者复用同一个共享观察器和 cursor 判定，不增加每个 Run 独立的文件轮询。
2. 省略 `yield_time_ms` 时维持原有立即返回语义，不引入隐藏等待。显式等待必须在 1—300000ms 内，且
   不得超过 Run 冻结的 Tool execution timeout；`start` / `write` 在产生副作用之前完成该校验。
3. `start` 从 stdout/stderr cursor 0 等待首批输出或终态；`write` 从调用者提交的 cursor 等待后续输出或
   终态。任何 cursor 前进、终态、取消、yield 到期或 Tool timeout 都在同一次 Tool Call 内返回。
4. Tool 实现摘要单独加入 implementation version 8；Manifest schema 保持 7，避免只为交互 API 演进破坏
   已持久会话迁移，同时确保旧审批/执行绑定不会静默接受新实现。

## 对标判断

- Codex `unified_exec` 的 `exec_command` 与 `write_stdin` 都原生支持 yield。本平台现已对齐一次 Tool Call
  启动/写入并等待输出，同时保留多租户、Workspace、durable cursor、共享 observer 与 Host replacement。
- OpenClaw 的 `exec` 支持 `yieldMs`，但 `process write` 立即返回，读取结果需要后续 `process poll`。本平台
  在内核交互回合数和持久 cursor 上更完整；OpenClaw 的 Node relay、viewer/owner 与 Windows 支持仍领先。

## 验收边界

- 真实延迟 shell 必须证明 start-yield 和 write-yield 均在同一调用返回首批输出，Agent Loop 不得生成
  `process.poll` / `process.wait` busy-poll 回合。
- 原有 pure wait、Host replacement、64 Session / 1024 wait 容量、取消和超时门禁必须保持通过。
- 本 ADR 不处理 Host 在副作用已接受但 yield 结果尚未持久返回时崩溃的收据恢复，也不实现 Windows
  ConPTY、GUI、Java 控制面或云边 Node relay。
