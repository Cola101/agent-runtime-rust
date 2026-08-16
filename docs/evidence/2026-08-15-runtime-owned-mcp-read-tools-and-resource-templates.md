# 2026-08-15 Runtime-owned MCP 只读 Tool 与 Resource Templates 证据

## 已验证

| 契约边界 | 可执行证据 |
| --- | --- |
| 模型真实主链 | 独立 Host 的真实三回合 Agent Loop 依次 list/read Resources、list Templates、list/get Prompts，并到达 `run.succeeded` |
| 最小权限 | Server 只有 `mcp:read:knowledge`、没有 `tool:mcp:knowledge`；五个 Runtime Tool 可见且无审批，移除 read scope 后全部不可见 |
| Prompt 权威 | `prompts/get` 返回内容只进入 Tool Result；下一模型请求的 system message 不包含远端 Prompt |
| Resource Templates | HTTP 2025/2026、真实 stdio 2025/2026、Model Gateway gRPC、Worker consumer 与独立 Host 共用协议中立分页类型 |
| 身份与冻结 | 云路径绑定完整 invocation identity、workload token、Server snapshot、directory digest 与精确 capability |
| 边界上限 | page≤64、cursor≤2 KiB、URI/template/name 有界、远端 response≤256 KiB、模型可见序列化结果≤128 KiB |
| 审计与恢复 | 五个读取均走普通 Tool planning、requested/started/result、Checkpoint 与取消路径，没有直接写 transcript 的旁路 |

远端 Tool scope 不会隐式授予内容读取权；Resources/Prompts capability 也不等同授权。结果超限返回确定性
`mcp_model_result_too_large` Tool error，不截断为貌似完整的 JSON。binary Resource 以 Base64 表达并受同一上限。

## 门禁结果

```text
cargo test -p agent-model-gateway --test mcp_federation             19 passed
cargo test -p agent-runtime-worker --test mcp_end_to_end            16 passed
cargo test -p agent-runtime-host --lib <stdio resource/prompt tests>  2 passed
cargo test -p agent-runtime-host --test standalone_run \
  runtime_owned_mcp_read_tools_complete_a_real_agent_loop_without_remote_tool_authority
                                                                       1 passed
cargo test --workspace -- --test-threads=4                           715 passed, 6 ignored
cargo clippy --workspace --all-targets --all-features -- -D warnings passed
cargo fmt --all --check                                               passed
git diff --check                                                      passed
```

全工作区精确列出 721 项，其中 6 项需要外部 live 服务而显式忽略；ignored 不计为通过。本轮没有启动 Docker、
Java、PostgreSQL、NATS 或用户系统服务。Cargo `target` 作为可复用增量缓存保留，未执行 `cargo clean`。

一次 8 线程复验曾使既有 `expired_cooldown_allows_only_one_concurrent_half_open_probe` 的 5 秒等待超时；该用例
单跑通过，整组在 4 线程 13/13 通过，最终全工作区 4 线程也通过。该用例与本轮 MCP 代码无共享状态或调用链，
当前按高并发本机 I/O/调度抖动记录，不把那次失败隐藏成成功，也未为通过门禁而修改其业务实现。

## 源码锚点

- 五个 Tool、动态 scope/capability 规划与执行：`runtime/apps/worker/src/mcp_gateway.rs`
- Worker 模型可见性、Tool 事件和恢复绑定：`runtime/apps/worker/src/lib.rs`
- Resource Templates 协议类型：`runtime/crates/protocol/src/lib.rs`
- HTTP parser 与远端边界：`runtime/apps/model-gateway/src/mcp.rs`
- gRPC provider/consumer：`runtime/apps/model-gateway/src/mcp_grpc.rs`、`runtime/apps/worker/src/mcp_gateway.rs`
- stdio 持久会话：`runtime/apps/runtime-host/src/stdio_mcp.rs`
- 真实主链：`runtime/apps/runtime-host/tests/standalone_run.rs`
- 设计边界：`docs/adr/0117-runtime-owned-mcp-read-tools-and-resource-templates.md`

## 对标判断

- **Codex**：已对齐 `list_mcp_resources`、`read_mcp_resource`、`list_mcp_resource_templates` 的 Runtime-owned
  Tool 模式。当前实现额外要求精确多租户 read scope、完整 invocation identity、冻结目录与模型结果硬上限；
  Codex 仍领先 OAuth、App Server/客户端产品链、真实生态兼容和更广工具面。
- **OpenClaw**：已覆盖其 session facade 的 Resources、Resource Templates 与 Prompts 表面；本项目不自动遍历
  所有分页，以免一个模型调用在共享 Runtime 中制造无界远端工作量。OpenClaw 仍领先长期 session、OAuth、
  Apps、Gateway 运维和跨平台产品能力。

## 尚未验证与风险

1. OAuth onboarding、PKCE、refresh/persist/revoke 与真实授权 Server 尚未实现。
2. HTTP 2025 外部 Server 可能把 cursor 绑定特定 session；真实长稳分页兼容矩阵未验证。
3. 内容分类、租户 DLP 与模型地域策略尚未在 Resource/Prompt 结果上形成独立强制门禁。
4. Roots/Sampling 仍默认关闭；本阶段未扩大反向能力。
