# ADR-0090：可恢复的 MCP 2026 多轮用户输入

状态：Accepted（2026-08-11）

## 决策

1. `RunExecution` schema 19 显式冻结 MCP wire revision 与客户端能力；旧命令默认
   `2025-06-18 + 空客户端能力`，不能隐式升级。
2. 新式 HTTP 采用 MCP `2026-07-28` 的无会话 Multi Round-Trip Requests（MRTR）。
   `input_required` 被当作数据返回，不保持服务器到客户端的反向请求连接。
3. `requestState` 按字节原样持久化；每轮最多 8 个请求、10 轮、64 KiB state 和
   128 KiB 请求/响应。表单不得索取 password、secret、token、API key、private key
   或 credential；敏感流程必须使用 HTTPS URL elicitation。
4. Worker Checkpoint schema 25 分别保存 `pending_mcp_input` 与
   `resolved_mcp_input`。询问用户前、回答落地后、续传发出前三个边界均先发布事件并
   保存 Checkpoint。
5. 续传发出后进程失联时，仅 `Pure/Idempotent` Tool 可重放；
   `NonIdempotent/Unknown` 进入 `indeterminate`。
6. 独立 Rust Host、Unix socket IPC 和 CLI 暴露同一恢复语义。ADR-0091 已把同一轮次
   contract 接入 Worker↔Model Gateway gRPC；NATS recovery poll 的自动续传调度仍未验证，
   不得把“gRPC 轮次可传输”扩大成完整云端恢复闭环。

## 理由

旧版 `elicitation/create` 依赖进程内反向请求路由，Host 崩溃或换实例后无法恢复等待者。
MRTR 将延续状态交给调用方持久化，更适合本项目的 Checkpoint、owner epoch 和多租户
执行模型。

Codex 已支持 MCP 2025 elicitation 与 2026 MRTR，但其旧反向请求路由主要保存在进程内；
本实现额外绑定 Run、Tool、摘要与副作用恢复。OpenClaw 当前参考源码没有等价的 MCP
客户端 MRTR 闭环。以上仅说明本决策在恢复边界上的适配性，不代表整体能力领先。
