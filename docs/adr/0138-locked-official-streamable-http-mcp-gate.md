# ADR-0138：锁定官方实现的 Streamable HTTP MCP 门禁

- 状态：Accepted
- 日期：2026-08-17
- 范围：MCP Streamable HTTP、外部契约、Agent Loop、临时依赖治理

## 背景

2026-08-08 曾人工启动 `@modelcontextprotocol/server-everything@2026.7.4`，真实发现过 Accept、SSE、Session ID
和 initialized notification 四个兼容问题并修复。但 ignored test 仍要求操作员手工 `npm pack/install/start`，
没有锁定传递依赖、自动回收 Server 或证明今天还能通过。因此历史记录不能替代当前可重复门禁。

只运行 Model Gateway 的 discovery/call 也不够：它能证明客户端协议，却不能证明外部 Tool 真正穿过 Runtime
Agent Loop、持久 Tool 事件和模型结果回灌。

## 决策

1. 以 `runtime/compat/mcp-server-everything-http/package-lock.json` 固定完整 npm 依赖图。外部 Server 固定为
   `2026.7.4`，其 SDK 固定为 `1.30.0`，lock SHA-256 固定为
   `23e4f0ecd182015ac85c35721acaef30828518c50597cd18be3a8df2c6a8e5aa`。
2. release gate `runtime/scripts/test-mcp-streamable-http-compat.sh` 只在 `mktemp` 目录执行 `npm ci`；使用空环境、
   临时 HOME/TMP/cache、`--ignore-scripts`、随机 loopback 端口。本门禁不在仓库产生 `node_modules`，也不写
   用户 npm cache。
3. Server 以直接 Node 子进程运行。退出时核对 PID command 包含本次绝对入口路径后才停止；完整临时目录总是
   清理。测试只调用官方 `echo` Tool，不触发外部业务副作用。
4. 门禁运行三条 exact ignored test：官方 discovery、真实 echo call + stale catalog digest 拒绝，以及完整
   Runtime Agent Loop 的 discover→模型 Tool Call→Tool Result→第二轮模型→唯一成功终态。
5. 每条测试除 Cargo 成功外，还必须出现精确 test name 和 `1 passed / 0 failed / 0 ignored`。同一守卫补到
   ADR-0137 的 Codex stdio 门禁，防止测试改名后“0 tests”仍返回成功。
6. 第一次用当前生产代码运行外部 discovery/call 为 2/2 通过；新增完整 Agent Loop 修正一次测试脚手架编译
   错误后通过。没有观察到协议 RED，因此不修改生产 MCP 实现。

## 后果

- 正面：MCP 外部证据从单一 Codex stdio fixture 扩展到独立的官方 Streamable HTTP 实现，并覆盖完整 Agent
  Loop，而不是依赖 2026-08-08 的人工记录。
- 正面：npm package 的 `^` 传递依赖不再随运行时间漂移；安装脚本、用户凭据环境和持久 npm cache 不进入门禁。
- 代价：完全临时安装约 106 个锁定 package，每次需要网络下载，耗时高于复用全局 npm cache；这是“不留下
  node_modules/缓存”边界的有意取舍。
- 中性：这是第二个外部 MCP 样本，不代表真实 OAuth、非官方 SDK Server、长连接、Resources/Prompts 分页、
  TLS/client cert、代理或重定向兼容完成。

## 对标

- **Codex `ff352fab6209`**：Streamable HTTP、OAuth、重试、Session 与客户端产品链更完整；其 inspected 测试
  大量使用同一 Rust RMCP 实现内的本地 Server。本门禁补的是另一语言官方实现的锁定外部消费证据，不声称
  替代 Codex 的 transport 广度。
- **OpenClaw `58b4b9430457`**：`mcp-http-fetch.ts` 在 SSRF、TLS/client cert、代理、redirect、same-origin header
  和流 cleanup 上更广。本项目已具备 DNS 全地址检查/固定、禁代理、禁重定向、TLS hostname 校验和有界 SSE；
  当前选择较窄的 fail-closed 网络面，尚无 OpenClaw 的安全代理与 client-cert 适配广度。

## 参考

- `runtime/scripts/test-mcp-streamable-http-compat.sh`
- `runtime/compat/mcp-server-everything-http/`
- `runtime/apps/model-gateway/tests/mcp_real_server_compat.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
- `docs/evidence/2026-08-17-official-streamable-http-mcp-compatibility.md`
