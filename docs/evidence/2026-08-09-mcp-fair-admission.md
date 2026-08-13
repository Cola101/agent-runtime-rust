# MCP 共享背压与租户公平证据

日期：2026-08-09
范围：Rust Worker library、Model Gateway MCP gRPC、真实 loopback MCP HTTP server；未启动 NATS、Java、PostgreSQL、Docker 或 Kubernetes。

## 先红后绿

1. 新测试先要求 `McpDiscoveryScheduler` 和可注入共享调度器的 gateway client；旧实现不存在这两个入口，
   编译以 `E0432`、`E0599` 失败。
2. 两个真实 Run 共用一个两槽调度器。嘈杂租户先提交四台阻塞服务器，只允许前两台到达；安静租户随后
   排队。释放第一个槽后允许嘈杂租户下一项，释放第二个槽后必须轮到安静租户，而不是嘈杂租户第四项。
   测试通过不含租户标识的 aggregate snapshot 等到两个租户都真实入队，不依赖固定 sleep 猜测时序。
3. 一个单槽 Run 在真实 MCP `tools/list` 中阻塞并触发 200ms 整批 deadline。下一 Run 在一秒内完成真实
   发现，证明取消路径归还容量，没有泄漏 admission permit。

## 本轮门禁

- `mcp_end_to_end`：11 通过，包含 2 个新真实 socket 用例。
- `agent-runtime-worker`：加入 ADR-0043 后测试框架报告 126 通过；扣除 19 个未配置 `TEST_NATS_URL`
  而提前返回的 NATS 用例，实际执行断言 107 个。
- `agent-runtime-worker --all-targets` Clippy `-D warnings` 通过。
- Rustfmt 通过。

## 对标与边界

- **Codex**：MCP 生命周期、stdio、OAuth、缓存和逐服务器配置继续领先；未发现跨租户共享目录发现容量
  与 tenant round-robin。本实现只在这一窄项更适合多租户 Runtime。
- **OpenClaw**：进程命令 lane 的暂停、drain、超时和观测更成熟；lane 之间没有共享总容量和租户轮转。
- **后续进展**：协议中立 discovery supervisor 已由 ADR-0043 与真实慢/快 Run 闭环验证；NATS Worker
  仍未接入，不能据此宣称生产 Worker 已具备多 Run 异步发现吞吐。
