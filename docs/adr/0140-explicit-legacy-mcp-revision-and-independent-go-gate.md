# ADR-0140：显式旧版 MCP 修订与独立 Go 实现门禁

- 状态：Accepted
- 日期：2026-08-17
- 范围：MCP 版本协商、Run 身份、stdio、独立实现兼容、发布门禁

## 背景

ADR-0137—0139 已覆盖 Codex strict fixture、官方 TypeScript SDK 参考 Server 与两个公开生产部署，但仍没有一个
实现栈已知独立于官方 SDK 的完整 Agent Loop。`mark3labs/mcp-filesystem-server v0.11.1` 使用独立维护的
`mark3labs/mcp-go v0.32.0`，并固定选择 MCP `2025-03-26`。

首次将该 Server 接入独立 Rust Host 得到真实 RED：Server 能启动，但 Runtime 因只接受 `2025-06-18` 而在模型
出网前形成唯一 `run.failed`：

```text
stdio MCP server selected unsupported protocol version 2025-03-26
```

简单接受任意 Server 返回版本会让恢复时的线协议脱离 Run/Checkpoint 身份，不能用于多租户 Runtime。

## 决策

1. `McpProtocolRevision` 新增显式 `2025-03-26`，RunExecution 升级到 schema 22；schema 21 及以下不得携带该值。
2. 本地 stdio 新增 `stdio_2025_03_26` 配置。客户端发送并要求 Server 返回同一修订；不自动接受 Server 选择的
   其他旧版修订。
3. Model Gateway gRPC、HTTP initialize 与 stdio initialize 共用同一显式修订；两个旧版修订都禁止携带 2026
   MRTR client capabilities。
4. 默认 `stdio` 与旧 JSON 配置继续表示 `2025-06-18`，既有 transport authority digest 不改变。
5. 新发布门禁固定 filesystem Server commit/tree、`go.mod`/`go.sum` 摘要与 `mcp-go v0.32.0`，所有源码、HOME、
   Go module/build cache 和二进制只存在于受控临时目录。
6. Agent allowlist 只暴露 `list_allowed_directories`，并由运营方 override 为 `Pure`；写、移动、删除和文件读取
   Tool 不进入模型目录。
7. 测试必须完成真实模型→Go MCP Tool→Tool Result→模型第二轮→唯一终态，并验证精确 `1 passed`，不允许零
   测试假绿。

## 后果

- 正面：首次用已知非官方实现栈完成完整 Agent Loop，并修复了真实旧版协议兼容缺口。
- 正面：版本便利性不覆盖恢复权威；协议修订进入 Run/Checkpoint binding，Server 选择漂移会失败关闭。
- 代价：操作者必须显式选择 `stdio_2025_03_26`；本阶段不实现 Codex 式自动 legacy negotiation。
- 代价：该门禁依赖 GitHub、Go module proxy 与本机 Go，不进入默认离线 workspace。
- 中性：只证明一个独立 Go 实现、一个只读 Tool 和 stdio；不证明真实 OAuth、长期连接或整个 MCP 生态。

## 对标

- **Codex `ff352fab6209`**：modern discovery 可从已知 legacy 列表接受 Server 选择的 `2025-03-26`，并在
  initialized session 保存实际修订。本项目吸收“实际修订必须已知”的原则，但为多租户恢复选择在接纳前由
  配置显式冻结，而不是运行中自动降级。
- **OpenClaw `58b4b9430457`**：loopback MCP 明确支持 `2025-03-26`/`2024-11-05`，Agent bundle 测试也覆盖
  `2025-03-26` initialize。本项目补上该旧版兼容，仍落后其 Gateway/integration 生命周期与生态广度。

## 来源

- <https://github.com/mark3labs/mcp-filesystem-server>
- <https://github.com/mark3labs/mcp-go>
- <https://github.com/orgs/modelcontextprotocol/discussions/364>
- `runtime/scripts/test-mcp-go-filesystem-compat.sh`
- `runtime/compat/mcp-go-filesystem/README.md`
- `docs/evidence/2026-08-17-independent-mcp-go-compatibility.md`
