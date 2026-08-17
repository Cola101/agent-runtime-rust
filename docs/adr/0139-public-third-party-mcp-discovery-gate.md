# ADR-0139：公开第三方 MCP 只读发现门禁

- 状态：Accepted
- 日期：2026-08-17
- 范围：真实公网 MCP、动态目录、无凭据兼容、外部证据分层

## 背景

ADR-0137/0138 已覆盖 Codex strict stdio fixture 与本地启动的 MCP 官方 Streamable HTTP 参考实现，但二者都
是测试型参考 Server。`mcp-archery`、`mcp-email` 和 inspected OpenClaw fixtures 也直接依赖官方 TypeScript
SDK，不能证明真实运营环境或独立部署的网络/协议差异。

公开生产 MCP 更能暴露 CDN、TLS、真实 DNS、动态目录和服务端部署差异，但它不可按本仓库方式固定二进制
摘要，也可能限流或临时不可用。因此必须把“live 部署兼容”与“实现栈独立”分开记录。

## 决策

1. 新增显式 live gate `runtime/scripts/test-mcp-public-discovery-compat.sh`，固定两个官方公开端点：
   - Context7：`https://mcp.context7.com/mcp`
   - Microsoft Learn：`https://learn.microsoft.com/api/mcp`
2. 两个运营方的官方清单/文档都声明 Streamable HTTP、公开访问或无需 Key；门禁仍显式 unset 本项目兼容测试
   能读取的 bearer/auth 环境变量，避免本机登录让测试假绿。
3. 门禁只执行 initialize、initialized notification 和 `tools/list`。不调用任何第三方 Tool，不发送 Workspace、
   用户输入、源码、凭据或模型内容。
4. 每个端点必须返回非空目录，所有 Tool input schema 必须是对象，限定名必须进入本地 namespace，catalog
   digest 必须为 64 位。脚本还要求精确 test name、`1 passed / 0 failed / 0 ignored` 和两个端点各自的
   `compat:` 证据行。
5. live gate 保持 ignored，不进入普通离线 workspace。公网故障或限流必须如实失败，不能自动降级为 skip。
6. 当前生产客户端第一次同时运行两端点即通过，没有协议 RED，因此不修改生产代码。

## 后果

- 正面：外部兼容不再只来自测试 fixture；真实 TLS/DNS/CDN 后的两个独立运营方生产服务都消费了本项目的
  Streamable HTTP discovery。
- 正面：测试严格停在只读目录边界，避免为了兼容证据调用会查询外部数据或产生未知副作用的 Tool。
- 代价：远端版本和实现摘要不受本项目控制；该门禁证明“在此时此刻可互操作”，不是长期可重现的二进制证据。
- 中性：Context7 公开源码使用 MCP Server helper，Microsoft Learn 服务端实现栈未公开验证。因此本阶段证明
  运营方/部署多样性，**不声称**已经证明非官方 SDK 或手写协议实现多样性。

## 对标

- **Codex `ff352fab6209`**：具备成熟 Streamable HTTP/OAuth 客户端、动态 Tool 目录和 CLI 配置；Microsoft
  官方文档也将 Codex 列为可连接客户端。本项目本阶段对齐的是动态发现与真实公网消费，Codex 的用户配置、
  OAuth 和长期连接产品链仍领先。
- **OpenClaw `58b4b9430457`**：公开 integrations、Auth Profile、代理/TLS/redirect 与连接生命周期更广；本项目
  对每个真实目录继续施加 tenant namespace、对象 schema 和 digest 冻结，适合多租户 Run，但不替代 OpenClaw
  的生态运营经验。

## 官方来源

- <https://github.com/upstash/context7/blob/master/server.json>
- <https://github.com/upstash/context7/blob/master/packages/mcp/README.md>
- <https://learn.microsoft.com/en-us/training/support/mcp>
- <https://learn.microsoft.com/en-us/training/support/mcp-developer-reference>

## 参考

- `runtime/scripts/test-mcp-public-discovery-compat.sh`
- `runtime/apps/model-gateway/tests/mcp_real_server_compat.rs`
- `docs/evidence/2026-08-17-public-third-party-mcp-discovery.md`
