# MCP MRTR gRPC 轮次桥证据（2026-08-11）

## 断点

修复前，真实 MCP 服务返回 `input_required` 后，Model Gateway 的 `call_tool` 把它转换为
“caller has no MRTR continuation path”协议错误；gRPC 响应只能表达完成结果。Worker 已有
Checkpoint 和 `resume_with_mcp_input`，但数据无法穿过这个接缝。

RED 测试 `the_worker_gateway_path_preserves_a_modern_mcp_input_round` 实际经过四层 socket/
进程内边界：

```text
真实 TCP MCP 2026
  → Model Gateway MCP HTTP client
  → Model Gateway gRPC service
  → Worker gRPC client
  → FederatedToolExecutor
```

旧实现稳定失败为 `ToolExecutionError::Engine`，消息明确指出 gRPC 调用者没有 MRTR 路径，
证明测试命中的是产品断点，而不是测试脚手架。

## GREEN 事实

- 首轮成为 typed `McpInputRequired`：`round=1`、一个 form request，并保留包含空格、Unicode
  与换行的 opaque state。
- 第二轮使用新的 gRPC/MCP request，发送 `round=2`、相同 state 和用户响应。
- 真实 MCP 只观察到两次 `tools/call`，request ID 不同；第二次精确收到
  `inputResponses.confirmation.content.confirmed=true`。
- 完成内容穿过同一路径返回 `FederatedToolExecutor`，没有被当成 transport error。
- 旧非 MRTR `call_tool` 入口仍会在需要用户输入时 fail-closed。

## 门禁

- 目标 RED：旧实现以 “caller has no MRTR continuation path” 失败。
- 目标 GREEN：1/1；Gateway MCP 13/13、gRPC identity 5/5、Worker MCP socket 14/14。
- 全工作区：571 项中 566 通过、0 失败、5 个外部 live 用例显式忽略。
- `cargo fmt --all -- --check`、`cargo check --workspace --all-targets`、
  `cargo clippy --workspace --all-targets -- -D warnings` 全部通过。

## 未证明

这证明 gRPC 轮次契约和 Worker ToolExecutor 适配，不证明 NATS recovery poll、Java 控制面、
外部用户审批服务或完整云端产品闭环。独立 Runtime 仍可在没有这些组件时运行。
