# ADR-0137：哈希固定的跨项目 MCP 兼容门禁

- 状态：Accepted
- 日期：2026-08-17
- 范围：MCP 2026 stdio、Runtime Host、外部兼容证据、参考源码治理

## 背景

Runtime 已有大量自写 HTTP/stdio 回环 MCP server，能证明内部协议一致性，但不能证明客户端与外部实现兼容。
`standalone_run.rs` 虽然已有一条调用 Codex 严格 MCP 2026 stdio server 的 ignored test，过去仍要求人工先构建
一个二进制路径；没有固定参考 commit、源文件摘要或脏文件检查，结果难以重现，也可能误把修改过的 fixture
当作 Codex 证据。

直接复制 fixture 到本仓库同样无效：复制后双方可以一起漂移，它只会退化为第二个自写 mock。

## 决策

1. 新增显式 release gate `runtime/scripts/test-codex-mcp-2026-compat.sh`。门禁只接受 Codex
   `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`，并校验 fixture 文件无未提交修改且 SHA-256 精确为
   `02224a4a998359a1e35c15ab489bcb3463dbdd0a0cec23428e8d15f06ec6b3d8`。
2. 不复制上游源码。一个最小 dev-only Cargo package 的 build script 只在显式传入已验证源路径时，将该原始
   文件读入 Cargo `OUT_DIR` 并编译；普通 workspace 构建没有该环境变量时生成退出码 2 的 fail-closed stub。
3. 外部门禁运行既有真实 Runtime Agent Loop：回环模型选择 Codex `echo` Tool；严格 server 返回 MCP 2026
   `input_required`；第一 Host 退出；replacement Host 以原 request state 和绑定回答恢复；最终同一 Run 成功。
4. 测试继续 `#[ignore]`，因为普通 clone 不应依赖另一个仓库。只有固定脚本成功才算外部兼容证据；普通
   `cargo test --workspace` 中的 ignored 计数不得解释为通过。
5. 构建复用本仓库 `runtime/target`，不创建 `node_modules`、常驻服务、凭据目录或第二套构建缓存。
   脚本同时支持普通 checkout 与 Git worktree、macOS `shasum` 与 Linux `sha256sum`，所有 Cargo 调用使用
   `--locked`，避免兼容门禁隐式改变依赖解析。
6. ADR-0138 审查外部门禁时补充精确测试计数：Cargo 成功之外必须出现目标 test name 和
   `1 passed / 0 failed / 0 ignored`，防止测试改名后零测试仍返回成功。

## 后果

- 正面：当前 MCP 2026 stdio、metadata、elicitation、opaque request state 与 Host replacement 已由 Codex 自己
  用于测试其客户端的严格 fixture 交叉验证，不再只是本项目客户端与服务端互相同意。
- 正面：commit、文件摘要和 clean-tree 三重固定能区分“参考实现升级”与“本项目回归”，升级必须显式复审。
- 代价：本地需要有对应 Codex checkout；普通 CI 不自动获得该证据，release/compatibility job 必须显式运行。
- 中性：这是一个外部样本，不是兼容矩阵完成。Streamable HTTP、多个第三方 MCP、真实 OAuth provider、长稳
  分页、限流和网络断流仍未验证。

## 被否决方案

- **复制 Codex fixture 到本仓库**：会失去跨项目独立性，并引入来源同步与 NOTICE 债务。
- **unset 时静默跳过并返回成功**：会让未执行测试看起来通过；现在 ordinary test 保持 ignored，fixture stub
  也明确失败。
- **构建整个 `codex-rmcp-client` package**：其普通依赖面远大于 fixture 实际使用的 `anyhow/serde_json`，会
  无意义扩大 M1 Pro 的构建时间和缓存。
- **改用自写严格 server**：已有这类测试，不能新增外部兼容证据。

## 对标

- **Codex `ff352fab6209`**：直接使用其 `rmcp-client/src/bin/test_mcp_2026_stdio_server.rs`，因此本阶段对齐的是
  Codex 自己的 `server/discover` metadata、2026 `input_required` 和 continuation 语义，而非根据文档猜测。
- **OpenClaw `58b4b9430457`**：当前 inspected MCP HTTP 路径主要依赖 SDK 的 2025-06-18 transport，并在 SSRF、
  TLS、代理、redirect 和 same-origin header 处理上更成熟；未找到等价的 2026 stdio strict fixture。故本项目在
  这一窄协议样本上更前，但 OpenClaw 的 HTTP 网络防护和真实集成广度仍领先。

## 参考

- `runtime/scripts/test-codex-mcp-2026-compat.sh`
- `runtime/compat/codex-mcp-2026-fixture/`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
- `docs/runtime-compatibility-matrix.md`
- `docs/evidence/2026-08-17-codex-mcp-2026-cross-project-compatibility.md`
