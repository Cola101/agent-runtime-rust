# 公开第三方 MCP 只读发现证据（2026-08-17）

## 端点与授权边界

| 运营方 | 官方端点 | 官方声明 | 本轮动作 |
| --- | --- | --- | --- |
| Context7 / Upstash | `https://mcp.context7.com/mcp` | server registry 标记 Streamable HTTP，Authorization 可选 | initialize + `tools/list` |
| Microsoft Learn | `https://learn.microsoft.com/api/mcp` | Streamable HTTP、公开可用、无需鉴权 | initialize + `tools/list` |

官方来源：

- Context7 [`server.json`](https://github.com/upstash/context7/blob/master/server.json) 与
  [MCP README](https://github.com/upstash/context7/blob/master/packages/mcp/README.md)
- Microsoft Learn [MCP overview](https://learn.microsoft.com/en-us/training/support/mcp) 与
  [developer reference](https://learn.microsoft.com/en-us/training/support/mcp-developer-reference)

脚本在启动测试前 unset `AGENT_RUNTIME_MCP_COMPAT_ENDPOINT`、认证 endpoint 和 bearer 变量。测试的
`McpServerRef` 使用空 credential envelope 与 `oauth_credential_id=None`，只调用 `list_tools`；不执行远端 Tool。

## 结果

```text
runtime/scripts/test-mcp-public-discovery-compat.sh

compat: https://mcp.context7.com/mcp -> 2 tools, digest 554a73f5c74637a1
compat: https://learn.microsoft.com/api/mcp -> 3 tools, digest eb7dc272885ec0ca
test discovery_works_against_every_configured_server ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
```

每台服务都返回非空 Tool 目录；所有 schema 可解析为对象，Tool 名称进入 `mcp:compat/` namespace，目录摘要
为 64 位。当前生产客户端首跑两台即通过，没有协议 RED，也没有修改生产代码。

## 证据边界

- 工具数量和 digest 是 2026-08-17 的观测值，不冻结为未来契约；服务应被允许动态更新目录。
- Context7 是独立运营的生产部署，但公开源码仍使用 MCP Server helper；Microsoft Learn 服务端实现栈未公开
  验证。故本轮证明两个运营方/部署的真实兼容，不证明底层 SDK 实现独立。
- 没有调用 `resolve-library-id`、`query-docs`、Microsoft search/fetch 或任何其他远端 Tool。
- 没有覆盖远端 Tool call、真实 OAuth、长期 SSE、通知、分页、限流恢复、代理或 client certificate。
- live gate 依赖公网可用性，不进入默认离线测试；失败不能转成 skip。

外部 MCP 证据目前包含 Codex strict stdio fixture、官方本地 Streamable HTTP reference、Context7 production 和
Microsoft Learn production。总体 Rust 内核进度仍维持 70–75%。
