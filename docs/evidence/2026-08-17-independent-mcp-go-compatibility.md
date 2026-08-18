# 独立 mcp-go 实现兼容证据（2026-08-17）

## 外部实现

| 项 | 固定值 |
| --- | --- |
| Server | `mark3labs/mcp-filesystem-server v0.11.1` |
| commit | `5646396f50ba144b9dd1ca9d088db0ac08cab3f8` |
| Git tree | `8dcf90035679d3f7a9ed509f941efdd36d9abe85` |
| protocol stack | `github.com/mark3labs/mcp-go v0.32.0` |
| wire revision | `2025-03-26` |
| license | MIT |

`mcp-go` 是独立 Go 实现；MCP 官方 Go SDK 的设计讨论明确说明曾参考该项目。本仓库不复制或发布其源码，门禁
只在临时目录 clone 固定 commit，并以原 `go.sum` 和 Go checksum database 验证依赖。

## RED

生产代码修改前，真实 Server 进入独立 Host 后形成：

```text
McpServerDiscoveryStatus {
  health: Unavailable,
  error: "stdio MCP server selected unsupported protocol version 2025-03-26"
}
status: Failed
event_types: ["run.failed"]
```

这证明失败发生在真实 initialize 协商，不是根据源码推断的潜在差异。

## 修复与 GREEN

- RunExecution schema 22 显式冻结 `McpProtocolRevision::V2025_03_26`。
- stdio 与 Model Gateway initialize 发送并验证同一预期修订。
- schema 21 downgrade、legacy revision drift 与 legacy MRTR capability 均失败关闭。
- Agent 只看见 `mcp:mcp_go/list_allowed_directories`，参数为空，不读取或修改任何文件。

```text
runtime/scripts/test-mcp-go-filesystem-compat.sh

running 1 test
test mcp_go_filesystem_server_completes_an_agent_loop ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
verified mark3labs/mcp-filesystem-server@v0.11.1
with mcp-go@v0.32.0
```

内部门禁另外证明：schema 22 round-trip、schema 21 downgrade 拒绝、Gateway 显式修订解析、相同修订成功
initialize、Server 选择另一旧修订时拒绝，以及 Runtime Host 的 JSON 配置入口能显式解析
`stdio_2025_03_26`。2026-08-18 复验结果：执行契约 **46/46**、Model Gateway lib **24/24**、Runtime Host
lib **32/32**、JSON 配置精确测试 **1/1**、Clippy `-D warnings` 与 fmt 均通过。

## 安全与垃圾边界

- 无凭据、用户数据、Workspace 源码或外部系统写入。
- 所有 Go HOME、module cache、build cache、源码和二进制都在一个受控 `mktemp` 目录。
- Go module cache 的只读文件先恢复 owner 写权限，再在受控目录原地删除；不移动到 macOS Trash。
- 审查中发现旧 HTTP 门禁曾在 Trash 留下 6 个临时目录（约 195 MiB），已精确清除，并将该门禁改为相同的
  原地删除策略。
- 门禁结束后未发现 `agent-runtime-mcp-go.*`/`agent-runtime-mcp-http.*` 临时目录或 Server 进程。

## 证据边界

- 当前只有一个独立实现栈、一个 stdio Server 和一个纯读取 authority 的 Tool。
- 没有验证该版本的 Resources/Prompts、写 Tool、取消、progress 或长期 session。
- 没有真实 OAuth Server、真实厂商 Provider 或跨机器恢复。
- 因此该阶段关闭“已知非官方 SDK/手写实现为零”的缺口，但总体 Rust 内核仍维持 70–75%。
