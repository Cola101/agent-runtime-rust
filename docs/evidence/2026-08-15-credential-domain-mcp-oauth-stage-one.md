# MCP OAuth 凭证域第一阶段证据（2026-08-15）

## 范围与结论

本阶段只修改 Rust Runtime 的 RunExecution、Worker→Model Gateway MCP 契约及 Gateway credential domain。
不启动 Java、Docker、Kubernetes、PostgreSQL、NATS 或外部 OAuth 服务，不写入真实凭据。

已证明的最小闭环：

```text
RunExecution v21 oauth_credential_id
  → Worker Protobuf（只有 UUID）
  → Gateway encrypted store + lease/CAS
  → PKCE code exchange
  → expired token singleflight refresh
  → Gateway Bearer → real loopback MCP discovery
```

关键事实：

- schema 21 才允许非 nil OAuth handle；旧 schema、nil UUID、静态 envelope + OAuth 双模式全部拒绝。
- OAuth handle 进入 MCP server authorization digest；Token 值从未进入 digest、Worker wire、Run 或 Checkpoint。
- 文件记录使用 AES-256-GCM，AAD 绑定 tenant、Server UUID、credential UUID 与 endpoint；目录/文件权限分别
  0700/0600，写入为 `partial → fsync → rename → directory fsync`。
- credential-scoped OS lock 覆盖 exchange/refresh 网络调用；锁内重读、revision CAS 和 intent-before-request
  共同保证多进程 singleflight 与 winner protection。
- 调用方取消不会取消 owned token task；若进程在 `exchanging/refreshing` 后退出，重启转换为
  `authorization_required`，不重放可能已消费的 code/refresh token。
- 本地 revoke 与 refresh 使用同一 lease；rejected access token 只有摘要仍匹配当前 token 时才可改变状态。
- Gateway 二进制可选读取 `AGENT_RUNTIME_MCP_OAUTH_STATE_ROOT` 与
  `AGENT_RUNTIME_MCP_OAUTH_MASTER_KEY_FILE`；主密钥文件是 base64 编码 32 bytes，密钥值不进入环境变量或日志。

## 可执行证据

| 门禁 | 结果 |
| --- | --- |
| `agent-protocol` execution contract | 45 passed；含 v21 handle、降级、nil 与双凭证模式 |
| `agent-model-gateway-protocol` authorization digest | 2 passed；旧 v1 digest 保持稳定，OAuth 使用 v2 domain |
| `agent-model-gateway` 全包 | 78 passed，4 个需外部服务/凭据的 live 用例 ignored |
| OAuth lifecycle | 1 个库内 crash test + 3 个真实回环 integration tests passed |
| Worker wire / admission | 2 个专项测试 passed |
| 独立 Host credential-domain 拒绝 | 1 个专项测试 passed |
| Rust workspace all-targets check | passed |
| Rust workspace 全量（4 threads） | 精确列出 729 项：723 passed，0 failed，6 个外部 live 用例 ignored |
| Clippy / fmt | workspace + all-targets + all-features `-D warnings` passed；fmt check passed |

真实回环测试同时断言：

- code exchange 返回立即到期的 `access-one/refresh-one`；两个并发解析最终都取得 `access-two`，token endpoint
  总请求数为 2（一次 exchange + 一次 refresh），不存在双 refresh。
- MCP Server 的两次现代目录请求都收到 `Bearer access-two`，证明 Token 只在 Gateway 最后一跳使用。
- 递归扫描持久 JSON 未发现 access token、refresh token、authorization code 或 OAuth state。
- 旧 token 的迟到拒绝不能覆盖刷新赢家；当前 token 的拒绝会精确进入 authorization-required。
- 同 credential UUID 换 tenant/Server 得到 Absent，换 endpoint 因 AAD/binding 不匹配而 fail-closed。

## 尚未证明与风险

- Protected Resource Metadata、Authorization Server Metadata、WWW-Authenticate challenge 与 issuer/resource
  一致性 discovery 尚未实现；当前 begin API 需要受信配置提供 authorization/token endpoint 和 public client ID。
- Dynamic Client Registration、Client ID Metadata Document、private client secret、本地 callback listener、管理
  gRPC/CLI/GUI 登录入口尚未实现。
- MCP HTTP 401 尚未自动调用 rejected-token CAS；当前原语已测试，但消费链未闭合。未知副作用 Tool 仍不会
  因认证错误自动重放。
- revoke 当前只保证本地 fail-closed；远端 revocation endpoint 是后续 best-effort 独立事务。
- 文件 adapter 使用 Unix `flock`，仅作为 Mac/Linux 原生证据；Windows 与生产多副本需要等价的外部 CAS/lease
  adapter。未用普通文件锁冒充跨平台安全。
- 尚未用真实外部 OAuth MCP Server 验证 provider-specific metadata、scope、错误和 token rotation 差异。

## 缓存与清理

本阶段保留 `runtime/target` 增量缓存，未执行 `cargo clean`。测试创建的 `agent-mcp-oauth-*` 临时目录由 Drop
清除；验收后 `/tmp` 未发现该前缀残留，也未创建 `node_modules`、`.local` 或日志文件。
