# Codex MCP 2026 跨项目兼容证据（2026-08-17）

## 权威来源

- Codex checkout：`ff352fab6209dc0f9d13fc0036ed3f9404682b2c`
- 原始 fixture：`codex-rs/rmcp-client/src/bin/test_mcp_2026_stdio_server.rs`
- SHA-256：`02224a4a998359a1e35c15ab489bcb3463dbdd0a0cec23428e8d15f06ec6b3d8`
- fixture 文件 `git status --porcelain` 为空

脚本在编译前逐项验证以上事实。源码不进入本仓库，只在显式门禁的 Cargo `OUT_DIR` 中生成构建输入。

## 真实消费链

测试不是 discovery-only：回环模型发出 `mcp:codex/echo` Tool Call；Codex strict server 验证 2026 metadata，
返回带 `requestState=stdio-state` 的 `input_required`。第一 Runtime Host 退出并释放进程；replacement Host 从
Checkpoint 恢复同一 Run，以绑定的 form Accept 回送 `inputResponses`，Codex server 自行断言 request state、
action 与 content，随后返回 Tool Result，模型第二轮完成 Run。

覆盖的外部语义：

- `server/discover` 与 `io.modelcontextprotocol/*` metadata；
- `tools/list`/`tools/call` 的 MCP 2026 complete/input-required 形状；
- opaque request state 和 form response；
- stdio 子进程生命周期、Host replacement、Checkpoint 与唯一 Run 终态。

## 结果

```text
runtime/scripts/test-codex-mcp-2026-compat.sh
running 1 test
test codex_mcp_2026_stdio_server_completes_a_recoverable_agent_loop ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 39 filtered out
```

第一次直接编译并运行 exact upstream source 即通过，没有发现协议缺陷，因此本阶段没有伪造 RED 或修改生产
协议代码。新增的是可重复、fail-closed 的外部证据门禁。另行验证：未提供 fixture source 时兼容 package 构建
stub，执行固定退出 2；它不能被误当成外部 server。

## 本轮门禁

- `cargo check --workspace --all-targets --locked`：通过；新增兼容 package 不破坏普通 workspace 构建。
- compat package `cargo clippy --all-targets -- -D warnings`：通过。
- 无 fixture source 的普通构建：通过；生成的 stub 执行退出码精确为 2，错误消息精确指向固定脚本。
- 固定脚本在普通 Codex checkout 上最终重跑：1 通过、0 失败；脚本同时支持 Git worktree。
- `cargo fmt --check`、shell 语法检查与 `git diff --check`：通过。
- 未启动外部服务、未使用凭据、未创建第二套 Cargo target 或项目临时目录。

## 未验证

- 这是 N=1 的外部实现，不能称兼容矩阵完成。
- 没有覆盖 Codex legacy/fallback/oversized modes、Streamable HTTP、Resources/Prompts 分页或长稳断流。
- 没有覆盖真实 OAuth MCP Server、认证 token、redirect/SSRF/TLS/proxy 组合。
- 没有使用真实模型厂商；模型回环只负责确定性触发外部 MCP Tool。
- 普通全包仍显式 ignore 该测试；只有固定脚本的输出才是本条 evidence。

总体 Rust 内核进度继续维持 70–75%。下一兼容目标是第二个独立 MCP 实现和三类真实 Provider 矩阵，不把
单个 Codex fixture 外推为生态完成。
