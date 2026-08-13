# ADR-0091：无状态 MCP MRTR gRPC 轮次桥

状态：Accepted（2026-08-11）

## 决策

1. `McpFederation.CallTool` 保持 unary；每一次 MCP MRTR 都使用新的 gRPC 请求，不让
   Model Gateway 持有等待用户的连接、回调或内存 waiter。
2. 首轮请求的 `input_continuation_json` 为空；后续轮携带协议中立
   `McpInputContinuation` JSON。Gateway 只负责把它翻译成 MCP 2026 的
   `requestState + inputResponses`。
3. 响应只能二选一：`content_json/is_error` 表示完成，`input_required_json` 表示需要
   用户输入。两类结果同时出现时 Worker fail-closed。
4. 每轮继续携带并验证 tenant、Run、attempt、Worker incarnation、完整 server snapshot
   和短期 workload token；用户输入不会改变原 Tool authority。
5. 旧 `call_tool` 调用者若收到 input-required 必须显式失败；只有调用
   `call_tool_round` / `resume_with_mcp_input` 的路径可以续传，避免静默丢弃用户交互。

## 理由

Model Gateway 是凭据和外连边界，不是 Run 状态源。把用户等待状态放进 Gateway 会使
进程替换后无法恢复，也会让凭据进程承担会话编排。无状态轮次桥让 Worker/Host 的
Checkpoint 保持唯一恢复依据，同时复用 Gateway 的凭据隔离、目录冻结和出站限制。

## 边界

- 已验证：真实 TCP MCP → Model Gateway HTTP client → gRPC service → Worker client →
  `FederatedToolExecutor` 的两轮输入与完成结果。
- 未验证：NATS recovery poll 自动消费输入决定并调度 continuation；该适配器不属于当前
  独立 Rust Kernel 的运行前提。
- 本 ADR 不启用 MCP 2025 held-open elicitation、sampling、roots、OAuth 或 GUI。
