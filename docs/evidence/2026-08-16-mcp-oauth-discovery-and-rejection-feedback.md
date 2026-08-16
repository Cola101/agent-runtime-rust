# MCP OAuth 第二阶段证据：discovery 与拒绝反馈（2026-08-16）

## 范围与结论

本阶段只修改 Rust Model Gateway 的 credential domain（`mcp_oauth.rs`）与 MCP HTTP 传输的认证失败分类
（`mcp.rs`）。不启动 Java、Docker、Kubernetes、PostgreSQL、NATS 或外部 OAuth 服务，不写入真实凭据。

已证明的最小闭环：

```text
WWW-Authenticate challenge（同源校验，发请求前）
  → RFC 9728 Protected Resource Metadata
  → RFC 8414 Authorization Server Metadata
  → 冻结进 PendingAuthorization
  → S256 PKCE authorization URL
  → 真实 token exchange
  → Gateway Bearer 调 MCP
  → 真实 401 invalid_token
  → CAS 精确标记 authorization_required（无重放）
```

## 可执行证据

全部命令使用 `--manifest-path /Users/cola/Documents/Code/agent-runtime-platform/runtime/Cargo.toml`。

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| execution contract | `-p agent-protocol --test execution_contract` | 45 passed，0 failed，0 ignored |
| authorization digest | `-p agent-model-gateway-protocol` | 2 passed，0 failed，0 ignored |
| OAuth 第一阶段 lifecycle | `-p agent-model-gateway --test mcp_oauth_lifecycle` | 3 passed，0 failed，0 ignored |
| OAuth 第二阶段 discovery/rejection | `-p agent-model-gateway --test mcp_oauth_discovery` | **13 passed，0 failed，0 ignored** |
| Gateway 全包 | `-p agent-model-gateway` | **89 passed，0 failed，4 ignored**（4 项需外部服务/凭据的 live 用例） |
| Gateway clippy | `clippy -p agent-model-gateway --all-targets --all-features -- -D warnings` | 无输出（干净） |

### 全工作区门禁状态：未通过，原因已定位

`cargo test --workspace -- --test-threads=4` 出现 1 项失败，**与本阶段改动无关**：

```text
test execution_deadline_terminates_the_process_group_without_a_poll ... FAILED
runtime/crates/tool-runtime/tests/process_session_governance.rs:185
panicked: the deadline supervisor left pid 98155 alive
```

取证结论：

- 断言失败后再查该 pid，进程**已经不在**（`ps -p` 无输出）：supervisor 确实杀掉了它，只是晚于断言那一瞬间。
- 单独重跑该测试：passed。
- `--test process_session_governance -- --test-threads=4` 连续 5 次：每次 10 passed，0 failed。
- `agent-tool-runtime` 是 crate，`agent-model-gateway` 是 app；app 依赖 crate 而非相反，本阶段改动不可能影响它。

真实根因是测试的**定点采样**而非超时不足：该用例设 `max_runtime: 150ms`，随后固定 `sleep(500ms)` 并在那一个
瞬间断言 `process_alive` 为假，只留约 350ms 余量。全工作区并行时 supervisor 任务未必能在该窗口内被调度。

正确修法是**取消定点采样**（在有界期限内轮询 `process_alive`，到期仍存活才失败），而不是把 500ms 调大、不是
串行化、也不是删断言。用例名中的 "without_a_poll" 指不轮询 **manager**（`manager.interact`），轮询进程存活不
违反该语义。该修改尚未实施。

## 真实网络请求计数（反假绿）

测试断言的是请求**次数**，而不仅仅是"调用失败了"——只断言失败的话，客户端即使偷偷重试过也照样通过。

| 场景 | 断言 |
| --- | --- |
| challenge → PRM → AS metadata | `hits() >= 2`，两份文档都必须真的被取回 |
| challenge 指向外部 origin | `hits() == 0`，必须在发出任何请求**之前**拒绝 |
| token exchange | `provider.hits() == 1`，恰好一次 |
| 真实 401 invalid_token | MCP server `hits == 1`，认证失败**零重放** |
| 真实 403 | `hits == 1`，且凭证仍为 `Active` |

其余安全断言：metadata body 超 64KiB、单字段超 4KiB、302 重定向、`file://`、`169.254.169.254` 链路本地地址、
含控制字符与超限的 `WWW-Authenticate` 全部 fail-closed；`resource` 与 MCP endpoint 不一致、issuer 与自身
endpoint 不同源均被拒绝。

## 崩溃点与并发

- 冻结绑定：`begin_discovered_authorization` 把 endpoint 写入 PendingAuthorization，`complete_authorization`
  只从记录读取，从不重新解析 metadata。测试以伪造 state 的 callback 断言该路径仍走冻结的 token endpoint。
- 迟到 401：`record_rejected_access_token` 的 CAS 在 digest 不再是当前值时返回 false 且不改状态，刷新赢家不被覆盖。
- 第一阶段既有保证未放宽：owned exchange/refresh 事务、跨进程 singleflight、崩溃后收敛到需重新授权、跨
  tenant/Server/endpoint 复用 fail-closed，均由 `mcp_oauth_lifecycle.rs` 3 项测试继续守护。

## 尚未证明与风险

- **全工作区门禁尚未跑出一次全绿**，上述 flake 修复未实施；在此之前不应宣称第 7 项完成定义达成。
- 真实外部 OAuth MCP Server 的 provider-specific metadata、scope、错误与 token rotation 差异**完全未验证**。
- Dynamic Client Registration、Client ID Metadata Document、private client secret、管理 gRPC/CLI、callback 承载、
  远端 revocation endpoint 均未实现。
- discovery 只取 `authorization_servers` 的第一项，多授权服务器选择策略未定义。
- 401 分类基于 `WWW-Authenticate` 的 `error` 参数；不发该参数、语义又非 token 失效的服务器会被当作 token 被拒。
- 文件 store 仍使用 Unix `flock`，Windows 与生产多副本需要等价外部 CAS/lease adapter。

## 缓存与环境事件

- 起始：`runtime/target` 不存在，磁盘剩余 52Gi。冷编译后 662M → 1.7G → 全量门禁中 2.8G。
- **环境事件**：第二次全量门禁执行期间 `runtime/target` 被**外部**整体删除，cargo 报
  `error writing dependencies ... No such file or directory`，磁盘剩余由 50Gi 跳到 64Gi。本会话未执行
  `cargo clean`，也未删除该目录。源码与 Git 状态未受影响（HEAD 仍 `7bfedae`，改动文件完整）。该事件导致全量
  门禁需要从零重建，是上述"尚未跑出全绿"的直接原因之一。
- 测试创建的 `agent-mcp-oauth-*` 与 `agent-mcp-oauth-disc-*` 临时目录由 `Drop` 清除；未创建 `node_modules`、
  Docker 镜像或项目外长期服务。
