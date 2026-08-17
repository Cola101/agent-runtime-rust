# ADR-0126：公开 MCP 输入回答与版本绑定

- 状态：Accepted
- 日期：2026-08-17
- 范围：RuntimeInvocation、Kernel 事件、Embedded Runtime 控制命令、MCP 2026 MRTR

## 背景

独立 Host 已能在 MCP 2026 `input_required` 后持久暂停，并由替代 Host 应用回答。但公开网络调用方需要
`input_id`、`input_version`、`binding_digest` 和完整 request set 才能构造 `ResolveMcpInput`。旧的
`mcp.input.required` 事件没有显式携带 `input_version`，内部测试只能写死 `1`；这会迫使 Java、CLI 或 GUI
从 Rust 实现细节猜版本。

另一个边界错误是：JSON 结构合法但版本或 binding 形状非法的控制命令被归入内部 `Configuration`，gRPC
返回 `Internal`。这是调用方可修复的输入错误，不是 Runtime 故障。

## 决策

1. 新增独立常量 `MCP_INPUT_VERSION`。它既不等同于 `McpInputRequired` 文档 schema，也不等同于
   resolution command schema；调用方必须从公开事件读取并原样回送。
2. Kernel 的 `mcp.input.required` payload 在完整 `input` 之外显式携带 `input_version`。这是向后兼容的
   事件字段增加；旧消费者可忽略，新消费者不再猜版本。
3. `McpInputResolutionCommand::validate_for`、Worker 恢复校验和 Embedded 控制预检共用同一常量，禁止三个
   路径各自写死版本。
4. 结构合法但身份、版本、binding 或有界 action 非法的命令返回独立 `InvalidControlCommand`；网络适配器
   将其映射为不泄漏内部细节的 `InvalidArgument`。状态冲突、存储损坏和其他 Runtime 故障不借此降级为
   调用方错误。
5. 验收必须从网络消费者视角完成：第一 Runtime 在 suspended 后整体消失；替代 Runtime 仅凭公开事件
   重建回答。错误版本在 Tool continuation 前拒绝；正确版本恢复同一 Tool，最终提交唯一 Run 终态。

## 对标

- **Codex `ff352fab6209`**：`ElicitationRequestRouter` 以 `(server_name, request_id)` 路由 pending oneshot，
  向客户端发出 form/url elicitation 事件，未知或已释放请求拒绝。其交互类型、客户端链和策略面更成熟；
  inspected router 是进程内 pending map，本 ADR 不据此声称 Codex 已提供跨进程 elicitation 恢复。
- **OpenClaw `58b4b9430457`**：当前 inspected 源码提供 opt-in MCP Apps extension、独立 sandbox origin 与
  app-to-server bridge，但未发现同口径 `input_required` 回答路由。它的 MCP Apps 产品面更宽，不能拿来
  证明本项目 MRTR 恢复语义。
- 本项目的窄面增强是 tenant/Run/Checkpoint/owner epoch/事件版本共同绑定，并能跨 Runtime replacement；
  这服务于多租户无状态调用方，不代表 MCP 生态总体领先。

## 代价与未覆盖

- `action_json` 仍是协议中立文档，不等于 Java SDK 已生成；消费者目前需要按公开 JSON schema 构造回答。
- 本轮使用真实 loopback gRPC、HTTP/SSE Provider 和 MCP Server，但没有外部 MCP 产品兼容或跨机器证据。
- URL elicitation、Decline/Cancel 已有内部 Host 证据，本轮网络专项只覆盖 form Accept；后续兼容矩阵不得把
  这一条扩写成全部交互模式已完成。
