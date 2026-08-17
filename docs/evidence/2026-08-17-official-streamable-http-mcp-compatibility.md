# 官方 Streamable HTTP MCP 外部兼容证据（2026-08-17）

## 外部权威与供应链边界

- Server：`@modelcontextprotocol/server-everything@2026.7.4`
- MCP SDK：`@modelcontextprotocol/sdk@1.30.0`
- npm lockfile v3 SHA-256：`23e4f0ecd182015ac85c35721acaef30828518c50597cd18be3a8df2c6a8e5aa`
- 安装：`npm ci --omit=dev --ignore-scripts`，完整依赖图由仓库 lockfile 固定
- 运行环境：空环境变量、临时 HOME/TMP/npm cache、随机 `127.0.0.1` 端口

Server、`node_modules`、npm cache 和日志只存在于 `mktemp` 目录。脚本退出时核对精确 PID identity 后关闭
Server，并删除完整目录；本门禁在仓库内只保留 manifest、lockfile 与说明文件。

## 真实消费链

门禁不是仅探活：

1. Model Gateway 对官方 Server 完成 initialize、SSE/JSON 协商、Session ID 回送、initialized notification 和
   `tools/list`，确认 `mcp:everything/echo` 及其对象 schema。
2. Model Gateway 调用官方 `echo`，读回官方 Tool Result；随后用未冻结 catalog digest 调用同一 Tool，客户端在
   出网前拒绝，证明多租户 Run 的目录冻结没有因外部实现而失效。
3. 独立 Rust Runtime Host 连接同一外部 Server；回环模型自主选择 `mcp:everything/echo`，Runtime 产生持久
   Tool Result，第二轮模型收到结果并提交唯一 `run.succeeded`。

## 结果

```text
runtime/scripts/test-mcp-streamable-http-compat.sh

test discovery_works_against_the_reference_server ... ok
test result: ok. 1 passed; 0 failed; 0 ignored

test a_tool_call_round_trips_against_the_reference_server ... ok
test result: ok. 1 passed; 0 failed; 0 ignored

test official_streamable_http_server_completes_an_agent_loop ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
```

当前生产协议第一次重跑 discovery/call 即 2/2 通过，没有协议 RED。新增 Agent Loop 测试第一次失败发生在编译
期：测试误把固定响应 Provider 的 `JoinHandle<()>` 当作捕获请求列表；改为“Provider 成功服务两轮 + Runtime
持久 `tool.result`/唯一终态”这组真实断言后通过。没有修改生产 MCP 代码。

门禁同时检查每个 exact test 的名称与汇总数量；测试被删除或改名不能以 Cargo 的零测试成功冒充兼容通过。
同一守卫已回补 Codex MCP 2026 stdio 门禁并重跑 1/1 通过。

## 未验证

- Server 与 SDK 都来自 MCP 官方项目；尚缺非官方 SDK、其他语言或手写协议的第三个独立实现。
- 没有真实 OAuth Server、授权登录、refresh/revoke 或 client registration。
- 没有覆盖长期 SSE、Last-Event-ID、服务器主动 notification、Resources/Templates/Prompts 的外部分页。
- loopback 门禁没有验证公网 TLS、client certificate、环境代理、安全重定向或网络断流。
- 模型仍是确定性回环 Provider，因此不构成真实模型厂商兼容证据。

MCP 外部样本现在为两个：Codex strict 2026 stdio 与官方 Streamable HTTP。生态兼容仍未完成，总体 Rust 内核
进度继续维持 70–75%。
