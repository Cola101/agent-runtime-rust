# 独立 Rust Host 直连 MCP 与恢复闭环

日期：2026-08-09
范围：`runtime-host`、Worker MCP 协调器、Model Gateway HTTP MCP 客户端、
Checkpoint schema 9；未启动 Java、PostgreSQL、NATS、Docker、Kubernetes 或独立 gRPC Gateway。

## 真实闭环

```mermaid
flowchart LR
    H["Standalone Host"] --> D["真实 MCP tools/list"]
    D --> M["真实模型 HTTP 请求"]
    M --> A["allow-once 审批"]
    A --> T["真实 MCP tools/call"]
    T --> R["Tool result 回灌模型"]
    R --> C["Checkpoint schema 9"]
    C --> H2["新 Host 恢复"]
```

- 首次 Run 的模型只能在发现后看到 `mcp:local/search`，自主发起 Tool Call；真实 MCP Server
  收到一次 `tools/call`，结果进入下一轮模型历史，Run 成功。
- 新 Host 只复用本地状态根目录，重新发现相同目录后先产生 `run.restored`，恢复后的模型仍看到
  冻结 Tool；已完成 Tool Call 没有重放，服务器调用计数保持为 1。
- 整条路径由 Host 进程内调用协议中立 Backend，没有启动 gRPC Gateway 或其他平台服务。

## 先红后绿

1. 独立 MCP 测试首先因 `LocalMcpServerConfig` 和配置字段不存在而编译失败；加入测试脚手架后，
   Provider 在首轮请求里看不到 MCP Tool，形成行为性 RED。
2. 引入 `McpFederationBackend`、开放服务器专用 HTTP 客户端和 Host Coordinator 接线后，发现、调用、
   结果回灌与恢复测试转绿。
3. 随后用第二个真实 MCP Server 暴露完全相同目录。恢复最初错误成功，证明旧 Checkpoint 只绑定目录，
   没绑定远端权威。
4. Checkpoint schema 9 加入 server ID、名称、endpoint 和 credential envelope 摘要后，换端点恢复在模型
   调用前 fail-closed；旧 MCP Checkpoint 因缺少权威证明直接拒绝。

## 安全与运行边界

- 本地 Backend 只接受空 credential envelope；不会获得云端 RSA 私钥或解封能力。
- 直接 HTTP 复用 Model Gateway 的 no-proxy、禁止重定向、DNS/地址约束、请求超时、响应和目录上限。
- Tool 仍经过签名 Skill 声明、delegated scope、逐次审批和 frozen catalog digest，不因本地直连而扩权。
- Checkpoint 同时冻结 Runtime policy、Tool catalog、发现策略和 MCP server authority。

## 门禁

- `agent-runtime-host`：21 项实际测试通过，其中 7 项 standalone；两项 MCP 测试使用真实 loopback
  Model/MCP HTTP socket。
- `agent-runtime-worker`：框架报告 128 通过；19 个外部 NATS 用例因未配置 `TEST_NATS_URL` 提前
  返回，实际执行断言 109 项；MCP 真实 socket 13/13。
- `agent-model-gateway`：49 项通过，4 项显式忽略的公共外部 MCP 测试不计为通过。
- `cargo clippy -p agent-runtime-host -p agent-runtime-worker -p agent-model-gateway --all-targets -- -D warnings`：通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过；未发现 `agent-tool-runtime-*` 临时目录。

## 对标结论

- **Codex**：stdio/remote stdio、HTTP、OAuth、缓存、重连、required/optional server、Resources/Apps
  均领先；本实现的协议中立 Backend 和跨 Worker authority/catalog/policy Checkpoint 是不同约束下的优势。
- **OpenClaw**：stdio/SSE/Streamable HTTP、OAuth、请求者作用域连接、轮换检测、session Runtime
  idle/LRU/dispose 治理均领先；本实现只在跨恢复不可变权威绑定与 Kernel 单写围栏上更强。
- **下一缺口**：先补 standalone 的 stdio transport 与进程树回收，并保持同一审批/Checkpoint 语义；
  OAuth 和运行时连接缓存随后推进，不转向 GUI 或 Java 控制面。
