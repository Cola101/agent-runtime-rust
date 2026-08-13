# Durable MCP MRTR evidence — 2026-08-11

## 已验证

- `RunExecution` v19 冻结 `2026-07-28` 与 `elicitation` delegated scope；降级或未委派均拒绝。
- 真实 TCP MCP 回环完成 `server/discover → tools/list → tools/call(input_required) →
  tools/call(inputResponses)`，续传使用新 JSON-RPC ID，opaque state 原样返回。
- 独立 Host 的真实回环 Agent Loop 在第一次 Tool 调用后进入 `Suspended`；替代 Host 从
  Checkpoint 接受用户确认，继续原 Tool，向模型回灌结果并 `Succeeded`。
- 事件顺序包含 `mcp.input.required`、`mcp.input.resolved`、
  `mcp.input.continuation.started`、`tool.result`、`run.succeeded`。
- Worker 恢复测试证明：续传前可安全恢复；续传已发出且 effect 为 `Unknown` 时终止为
  `indeterminate`，不会自动重放。

## 未验证

- 云端 Worker ↔ Model Gateway gRPC 的 MRTR payload 与恢复调度尚未接线。
- MCP 2025 held-open elicitation 继续默认拒绝；sampling 与 roots 仍未授权。
- 尚未用外部公共 MCP 2026 服务做长稳、断流和多轮兼容矩阵。

## 本地命令

所有构建使用独立临时 `CARGO_TARGET_DIR`，没有启动 Docker、Java、PostgreSQL 或 NATS。
