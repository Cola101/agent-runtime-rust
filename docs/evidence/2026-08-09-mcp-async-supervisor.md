# MCP 异步发现监督器闭环

日期：2026-08-09
范围：Rust Worker library、真实 loopback MCP HTTP、Gateway gRPC、签名 Skill 与 Kernel ModelInvocation；未启动 NATS、Java、PostgreSQL、Docker 或 Kubernetes。

## 先红后绿

1. 测试先导入 `McpDiscoverySupervisor`、`McpDiscoveryUpdate`，旧实现不存在，编译以 `E0432` 失败。
2. 慢 Run 进入真实 MCP `tools/list` 并由 Semaphore 阻塞；在它未返回时启动快 Run。快 Run 必须在一秒
   监督器窗口内先产生 `Ready`，而不是等待慢 Run。
3. 两个命令先由真实 `WorkerProcessor::accept` 接纳，并携带重新计算摘要和 Ed25519 签名的 MCP Skill。
   快 Run 的异步目录随后通过正式挂载函数进入 Kernel；`prepare_model_invocation` 最终只出现
   `mcp:fast/fast_tool`，证明不是只测网络返回值。
4. 同一 attempt 的第二次启动明确拒绝；取消慢 Run 后收到精确 attempt 的 `Cancelled`，后台任务不触碰
   Kernel 可变状态。

## 门禁

- `mcp_end_to_end`：该阶段为 12 通过；ADR-0044 协调器阶段现为 13 通过。
- `agent-runtime-worker`：该阶段框架报告 126 通过、实际执行断言 107 个；ADR-0044 阶段现为
  128 通过、实际执行断言 109 个。
- `agent-runtime-worker --all-targets` Clippy `-D warnings` 通过。
- Rustfmt 通过。

## 对标结论

- **Codex**：MCP stdio、OAuth、连接恢复、缓存和 required/optional server 明显领先；未发现同层级的
  跨 Run、Kernel-neutral 完成监督器。
- **OpenClaw**：command lane 的 pause、drain、timeout、active snapshot 更成熟；本实现只在
  “后台网络任务不得并发修改 Kernel”这一多租户 Runtime 边界上更严格。
- **后续进展**：ADR-0044 已让协议中立协调器完成目录挂载、Kernel Start 与恢复校验；NATS adapter
  仍未接入，消息确认顺序仍待真实传输证明。
