# MCP Checkpoint 精确恢复

日期：2026-08-09
范围：纯 Rust `WorkerProcessor`、MCP federation、NATS 恢复适配；不依赖 Java、PostgreSQL、Docker 或 Kubernetes。

## 已证实的原缺陷

Checkpoint schema 5 只保存原生 Tool 目录摘要。MCP 定义是逐 Run 挂载的，不在 Worker 全局
`tool_definitions` 中，因此恢复后变化过的 MCP 目录仍可重新挂载，并继续执行旧审批。

测试先在旧实现上得到真实失败：变化后的目录返回 `Ok(())`，没有产生
`CheckpointToolCatalogMismatch`。

## 修复后的不变量

- schema 7 保存 `qualified tool name -> frozen catalog digest` 以及有效 `McpDiscoveryPolicy`。
- 恢复后必须重新发现并精确重建同一组 Tool 和执行器，才能继续模型、审批或 Tool 工作。
- 同一目录若通过不同的并发上限、单服务器 deadline 或整批 deadline 重新发现，同样拒绝恢复。
- 工具消失、新增、目录摘要变化、执行器实现摘要不符或联邦客户端缺失均 fail-closed。
- 旧 schema 5 无法证明原目录，schema 6 无法证明发现策略，MCP Run 均明确拒绝恢复；普通非 MCP
  旧 Checkpoint 仍兼容。
- 发现与挂载是公开 Rust 能力，`NatsWorker` 只负责传输编排；独立 Runtime Host 可走同一路径。

## 真实闭环测试

纯 Rust 测试完成：模型提出 MCP Tool → 逐次审批 → Checkpoint → 新 worker/attempt/owner epoch 恢复
→ 拒绝变化目录 → 精确目录重新挂载 → 审批重绑 → 执行器实际返回结果。

另一条真实 socket 测试使用完全相同的 MCP 目录，以并发上限 4 建立 Checkpoint、以并发上限 2 恢复；
旧实现错误接受，schema 7 实现后明确拒绝。

门禁结果：

- `cargo test -p agent-runtime-worker`：测试框架报告 120 通过、0 失败；其中 19 个 NATS 用例因
  `TEST_NATS_URL` 未配置而在测试体内提前返回，实际执行断言的用例为 101 个，不能把前者当作 NATS 验收。
- 其中真实 loopback socket 的 MCP → Gateway → gRPC → Worker 测试：9 通过。
- `cargo clippy -p agent-runtime-worker --all-targets -- -D warnings`：通过。

## 对标结论

- Codex 的 `PreparedMcpCall` 在调用时持有精确 client/tool/catalog revision，并在目录 revision 变化时拒绝。
  本实现已达到同一调用期不变量，并额外覆盖跨 Worker Checkpoint 恢复。
- OpenClaw 使用 `mcpResumeHash` 防止 CLI session 在 MCP 稳定拓扑变化后复用；它绑定的是规范化配置。
  本实现绑定真实发现到的 Tool 名、目录摘要与有效发现策略，对多租户 Runtime 恢复更严格。
- 仍落后两者之处：Codex/OpenClaw 的本地 stdio/CLI MCP 生态更广；本实现当前只有 HTTP federation。

## 尚未证明

- 未对真实公网第三方 MCP 做长时间稳定性与 OAuth 验收。
- DNS 固定路径目前禁用环境代理；显式代理尚未做到“由代理连接但仍保持目标地址固定”。
- NATS 恢复适配已接线，但本机没有 `TEST_NATS_URL`，19 个 NATS 用例会提前返回；目录漂移分支
  尚无真实 NATS 故障注入证据。核心恢复闭环只由不依赖 NATS 的 Rust 测试证明。
