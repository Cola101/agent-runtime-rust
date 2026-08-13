# MCP 2026 stdio 与 URL elicitation 兼容证据（2026-08-11）

## RED

`modern_stdio_mcp_input_round_trip_survives_host_replacement` 首次运行在配置解析处稳定失败：Runtime 只接受 `streamable_http`、`streamable_http2026` 和旧 `stdio`，没有现代 stdio transport。继续接线前，真实子进程又以 `server/discover` 不受支持关闭，证明断点确实位于 transport/协议接缝。

## GREEN

- 内置 JSONL MCP 2026 子进程实际收到 `server/discover`、两次 `tools/list` 和首轮 `tools/call`，返回 form `input_required`。
- 第一 Host 完整关闭后，replacement Host 启动新的 MCP 进程，用原 `requestState` 和用户回答完成第二轮；marker 精确为 `started`、`continued` 各一次。
- HTTP URL elicitation 在 replacement 后完成；服务端确认回答只有 `action=accept`，没有 `content`，外部授权内容未进入 Runtime。
- 从 Codex revision `ff352fab6209` 单独构建其 `test_mcp_2026_stdio_server`，以 `modern` 模式作为外部服务运行；完整模型→Tool→input-required→Host replacement→continuation→Tool Result→模型终态测试 1/1 通过。

## 对标结论

- Codex：现代 stdio wire contract 已用其严格参考服务实跑对齐；本平台额外提供 Run-bound Checkpoint、delegated scope 与副作用模糊结果保护。
- OpenClaw：当前 inspected revision 未找到等价客户端 MRTR；其 Node、PTY、应用/渠道与跨平台生命周期仍明显领先。
- 下一内核目标不扩展控制面：优先补齐本地 PTY/持久进程 Tool 的流式输出、resize、时限、取消和恢复语义，对齐 Codex `unified_exec` 与 OpenClaw Node PTY。

## 门禁

- Codex 外部严格 stdio Agent Loop：1/1 通过。
- Rust 全工作区：568 通过、0 失败、6 个外部 live 用例显式忽略。
- `cargo fmt --all -- --check`、`cargo check --workspace --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings` 全部通过。
- 测试与 Codex 构建缓存均位于独立 `/tmp` 目录；验收后 15.4 GiB 缓存和两处 Graphify 输出已删除，仓库内没有 `runtime/target`。
