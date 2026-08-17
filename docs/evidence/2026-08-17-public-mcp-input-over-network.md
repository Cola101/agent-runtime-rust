# 公开 MCP 输入网络闭环证据（2026-08-17）

## RED

1. 新网络消费者只从 `mcp.input.required` 事件提取回答参数。旧事件没有 `input_version`，测试在发送控制
   命令前失败，证明内部测试写死 `1` 不能代表公开契约完整。
2. 补事件版本后，消费者故意回送 `input_version=2`。Runtime 正确拒绝，但 gRPC 返回 `Internal` 而不是
   可行动的 `InvalidArgument`。

## GREEN

- `mcp.input.required` 同时携带完整 pending input 与独立 `input_version=1`；调用方无需读取状态目录或引用
  Rust 类型。
- 第一 Runtime、gRPC Server、执行任务和 state-root owner 随独立 Tokio Runtime 一起消失；替代 Runtime
  读取原事件，suspended Run 不自动重放 Tool。
- 错误版本返回 `InvalidArgument`；公开边界仍为 `Suspended`，没有 `mcp.input.resolved`，且 MCP
  `tools/call` 仍只有第一次，证明拒绝既未改变 Run，也未越过 continuation 边界。
- 正确回答绑定原 `input_id + input_version + binding_digest + request key`；同一 Tool 进行第二轮调用，
  `requestState` 字节保持不变，随后出现 `mcp.input.resolved`、`mcp.input.continuation.started`、`tool.result`
  和唯一 `run.succeeded`。`run.started` 仍只有一次。

## 已执行门禁

- `agent-runtime-host --test grpc_invocation_mcp_input`：1/1。
- `agent-protocol --test mcp_mrtr_contract`：2/2。
- `agent-kernel`：41/41。
- `agent-runtime-worker --test federated_tools`：9/9。
- 既有 gRPC identity 8/8、control 3/3、approval 1/1、recovery 1/1。
- 最终 `cargo test -p agent-runtime-host`：191 通过、0 失败、1 个外部 Codex MCP fixture 显式忽略。
- Runtime Host 与 Worker 的 `clippy --all-targets --all-features -D warnings`、全 Runtime `fmt --check`：通过。

## 结论边界

本轮证明的是一个 M1 Pro 本机真实网络与真实进程替换形状，不是 mock `EmbeddedRuntime::control`。未使用
Java、GUI、Edge、Docker、PostgreSQL、NATS 或外部凭据。它不证明跨机器、真实外部 MCP Server、URL
elicitation 网络面或 Java SDK；总体 Runtime 进度仍保持 70–75%。
