# 2026-08-15 有界 MCP Resources/Prompts 证据

## 已验证

| 契约边界 | 可执行证据 |
| --- | --- |
| HTTP 2025 | 真实 Streamable HTTP Server 完成 Resources list/read、Prompts list/get，并保留 opaque cursor |
| HTTP 2026 | 同一协议中立返回类型跨 `server/discover` + 现代请求路径工作 |
| stdio 2025/2026 | 两个真实 `/bin/sh` MCP 子进程在持久会话内完成四个操作并正常退出 |
| Gateway→Worker | 完整 invocation identity、schema 4 workload token、Server snapshot 与目录摘要穿过真实 gRPC |
| 多租户拒绝 | token 未授权的 Server ID 在发往远端 MCP 前返回 gRPC `PERMISSION_DENIED` |
| capability 漂移 | HTTP operation session 撤销 Resources 后，读取前返回 `CatalogChanged`，零次 `resources/list` |
| 消费端 fail-closed | Worker 独立拒绝未知 response schema、超页、超 content/message、未知 role 与畸形 JSON |
| 资源上限 | 单页 64、cursor 2 KiB、resource content 16、prompt argument/message 32、远端总响应 256 KiB |

四个操作不自动遍历 Server cursor 链，也不把截断页伪装成完整页。HTTP 每次 operation session 重验能力；
stdio 复用当前已协商的持久会话，并重验冻结目录与 capability。Gateway 是云端 sealed credential 的唯一解封域，
Worker、Checkpoint、事件与日志均不获得凭证明文。

## 门禁结果

```text
cargo test -p agent-model-gateway -- --test-threads=8                 passed
cargo test -p agent-runtime-worker --test mcp_end_to_end -- --test-threads=8
                                                                    15 passed
cargo test -p agent-runtime-host --lib -- --test-threads=8          15 passed
cargo test --workspace -- --test-threads=8                          712 passed, 6 ignored
cargo clippy --workspace --all-targets --all-features -- -D warnings passed
cargo fmt --all --check                                              passed
git diff --check                                                     passed
```

全工作区精确列出 718 项测试，其中 6 项需要外部 live 服务而显式忽略；本轮没有把 ignored 当作通过。
全量门禁未启动 Docker、Java、PostgreSQL、NATS 或用户系统服务。`runtime/target` 最终约 17 GiB，作为 Cargo
可复用增量缓存保留，未执行 `cargo clean`；未发现 `.local`、`node_modules`、日志、临时测试目录或残留进程。

## 源码锚点

- 协议中立类型：`runtime/crates/protocol/src/lib.rs`
- HTTP parser、上限与 operation-session 门禁：`runtime/apps/model-gateway/src/mcp.rs`
- workload identity 与 gRPC：`runtime/apps/model-gateway/src/mcp_grpc.rs`
- Worker consumer contract：`runtime/apps/worker/src/mcp_gateway.rs`
- stdio 持久会话：`runtime/apps/runtime-host/src/stdio_mcp.rs`
- wire 真相：`contracts/proto/model_gateway.proto`
- HTTP 行为：`runtime/apps/model-gateway/tests/mcp_federation.rs`
- 认证全链：`runtime/apps/worker/tests/mcp_end_to_end.rs`

## 对标判断

- **Codex**：已对齐实际 Resources list/read 和模型无关的 typed result；本项目把完整 tenant/application/
  Workspace/Run 身份、Server authorization digest 与硬上限放进共享 Runtime 边界。Codex 仍领先 Resource
  Templates、模型可自主选择的只读内核 Tool、OAuth refresh/persist 与成熟 MCP session 管理。
- **OpenClaw**：已对齐 Resources list/read 与 Prompts list/get。OpenClaw 默认自动遍历所有页，适合单用户
  facade 的便利，但共享多租户 Runtime 默认采用单页 cursor 和整页硬上限更安全。OpenClaw 的成熟 session/
  Apps 生命周期和真实生态覆盖仍领先。

## 尚未验证与风险

1. 尚无 Runtime-owned 模型入口；Agent Loop 不能自主选择读取 Resource/Prompt，只有嵌入方、Worker 和 Host API
   能调用。不能把 backend 完成误报成 Codex 同等产品体验。
2. Resource Templates 尚未实现；Codex 在这一面明确领先。
3. OAuth onboarding、PKCE、refresh/persist/revoke 与真实授权 Server 尚未实现。
4. 真实外部 MCP Server 的长稳分页尚未实跑；尤其 HTTP 2025 Server 是否把 cursor 绑定具体 session，需要
   外部兼容矩阵验证。若绑定，必须引入有界 session/cursor lease，不能静默重启后继续 raw cursor。
5. Prompt content 当前以有界 JSON 保留前向兼容；未来模型入口必须增加内容数据等级、预算与事件审计，不能
   直接把远端 Prompt 提升为 system 权威。

> 后续状态：第 1、2 项以及第 5 项中的模型入口/Prompt 权威边界已由 ADR-0117 和
> `2026-08-15-runtime-owned-mcp-read-tools-and-resource-templates.md` 完成；OAuth、真实外部分页与内容分级仍未完成。
