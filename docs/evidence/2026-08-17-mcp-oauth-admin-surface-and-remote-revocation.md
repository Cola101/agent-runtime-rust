# MCP OAuth 管理面与远端撤销证据（2026-08-17）

## 范围与结论

本阶段修改 Model Gateway 的 OAuth 管理 gRPC 与 RFC 7009 远端撤销，另外修复了在全量门禁下暴露的两个
`agent-tool-runtime` 缺陷。不启动 Java、Docker、Kubernetes、PostgreSQL、NATS 或外部 OAuth 服务，不写入真实凭据。

已证明的最小闭环：

```text
operator token（mcp.oauth.admin）
  → McpOauthAdmin gRPC
  → credential domain（与 federation 共享同一 coordinator）
  → 本地 Revoked 原子提交 → 释放 lease
  → 有界 best-effort RFC 7009 POST
  → remote_confirmed 如实回报，失败不复活本地
```

## 可执行证据

全部命令使用 `--manifest-path /Users/cola/Documents/Code/agent-runtime-platform/runtime/Cargo.toml`。

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| 管理面身份 | `-p agent-model-gateway --test mcp_oauth_admin_identity` | **4 passed，0 failed，0 ignored** |
| discovery / 拒绝 / 撤销 | `-p agent-model-gateway --test mcp_oauth_discovery` | **15 passed，0 failed，0 ignored** |
| Gateway 全包 | `-p agent-model-gateway` | **97 passed，0 failed，4 ignored** |
| tool-runtime 全包 | `-p agent-tool-runtime` | **109 passed，0 failed，0 ignored** |
| provider 偏差矩阵与并发交叉 | `-p agent-model-gateway --test mcp_oauth_provider_deviations` | **25 passed，0 failed，0 ignored**（并发两项以 6 次 × 8 线程复跑确认） |
| discovery / 拒绝 / 撤销 | `-p agent-model-gateway --test mcp_oauth_discovery` | **16 passed，0 failed，0 ignored** |
| 全工作区 | `cargo test --workspace -- --test-threads=4` | **121 个测试二进制，766 passed，0 failed，6 ignored（共 772 项），`CARGO_EXIT=0`** |
| Clippy | `--workspace --all-targets --all-features -- -D warnings` | `CLIPPY_EXIT=0`，0 条诊断 |

> 计数方法：把全部 `test result:` 行相加。管道到 `tail` 会掩盖 cargo 退出码，必须单独记录 `$?`；本轮曾因此
> 把一次含失败的运行误读为成功，也曾手工数漏一个二进制（91 数成 89）。

## 管理面的可核验断言

| 场景 | 断言 |
| --- | --- |
| 无 token | `Unauthenticated` |
| 只有 `mcp.federate` 的 token 调管理面 | 被拒（federate 不蕴含 administer） |
| token 属于 A 租户、请求体写 B 租户 | `PermissionDenied`，且下游使用的是 claims 的租户 |
| 授权调用读取未授权凭证 | `absent`，且整条 wire message 渲染后不含 `access_token`/`refresh_token`/`verifier`/`code_verifier` |

最后一条断言的是**整条消息的渲染结果**而不是几个具名字段：将来若有人新增一个携带 token 的字段，这个断言仍能抓到。

## 远端撤销的可核验断言

| 场景 | 断言 |
| --- | --- |
| provider 返回 500 | `remote_confirmed == false`；状态仍为 `Revoked`；`resolve_access_token` 失败；token 与 revoke 两次请求都真实发生（`hits() >= 2`） |
| provider 返回 200 | `remote_confirmed == true`；状态 `Revoked` |

## 途中定位并修复的两个 tool-runtime 缺陷

两者都**与 OAuth 改动无关**（`agent-tool-runtime` 是 crate，`agent-model-gateway` 是 app，app 依赖 crate 而非
相反），且都**只在全工作区并行下复现**，单独跑与文件级并发都是绿的。

### 缺陷一：定点采样竞态

```text
execution_deadline_terminates_the_process_group_without_a_poll
crates/tool-runtime/tests/process_session_governance.rs:185
panicked: the deadline supervisor left pid <N> alive
```

证据：断言失败后再查该 pid，进程**已经不在**；单独跑 passed；文件级 `--test-threads=4` 连跑 5 次每次 10 passed。

根因不是超时不足，而是**定点采样**：用例设 `max_runtime: 150ms`，随后固定 `sleep(500ms)` 并在那一瞬间断言。
而 `process_alive` 用 `kill(pid, 0)`，该调用对**已终止但尚未回收的僵尸进程同样返回成功**，所以它 race 的既有
supervisor 的调度，也有回收窗口。修法是在有界期限内轮询而非放大 sleep；supervisor 若根本不终止进程组，断言
照样失败。

### 缺陷二：终止原因分类不一致（真实产品缺陷）

```text
supervised_pty_output_budget_stays_bounded_after_host_independence
crates/tool-runtime/tests/process_session_governance.rs:653
left: Some(RecoveredMissing)   right: Some(OutputLimit)
```

`src/process_session.rs` 中两条路径对同一问题给出不同答案：

- 正常终止路径**先**检查 `session_output_lengths`，因此 `OutputLimit` 优先于 `RecoveredMissing`；
- 恢复路径 `(identity_held, resource_alive) == (false, false)`（`terminated_before_recovery`）**无条件**写
  `RecoveredMissing`，从不查看输出长度。

于是**哪条路径先观察到进程退出，决定了调用方被告知什么**：一个因刷爆日志被终止的会话，可能被报告为"就这么
消失了"。真实原因一直可恢复——持久日志比进程活得久。修法是让恢复路径做同样的检查。

安全性核查：整个测试目录**没有任何测试断言 `RecoveredMissing`**，说明该原因本身缺乏覆盖，这也解释了缺陷为何
长期存活；修复不与任何既有预期冲突。

## Provider 偏差矩阵（脚本化，非真实外部证据）

`-p agent-model-gateway --test mcp_oauth_provider_deviations` → **11 passed，0 failed，0 ignored**。

用脚本化回环服务器模拟真实 provider 的已知偏差，逐条判定我们是**正确**、**过严**还是**过松**。
**必须说明：脚本化模拟不构成真实外部兼容证据**，它只固定了"provider 这样做时我们怎么办"。

| # | 偏差 | 判定 | 处置 |
| --- | --- | --- | --- |
| 1 | issuer 尾斜杠与 metadata 不一致 | 已正确 | 比较的是解析后的 URL 而非原始字符串，两种写法相等，但不同 origin 仍不相等 |
| 2 | PRM 缺 `scopes_supported` | 已正确 | 回退到授权服务器的列表 |
| 3 | AS metadata 无 `revocation_endpoint` | 已正确 | 撤销降级为仅本地，`remote_confirmed=false`，不报错 |
| 4 | `expires_in` 返回字符串 | **过严，已放宽** | 见下 |
| 5 | `scope` 返回数组 | **过严，已放宽** | 见下 |
| 6 | `authorization_servers` 多项 | 已正确（有意） | 只取第一项：逐个尝试会让"信任哪台服务器"取决于哪台先应答 |
| 7 | metadata 含未知字段 | 已正确 | 忽略而非拒绝 |
| 8 | `WWW-Authenticate` 含多参数 | 已正确 | `realm`/`error`/`error_description` 共存仍能取出 `resource_metadata` |
| 9 | token endpoint 200 但非 JSON | 已正确 | 拒绝，且不产生凭证 |
| 10 | metadata `Content-Type` 非 JSON | 已正确（有意宽松） | 能否解析才是真正的门；头部是建议性的，解析失败仍然拒绝 |
| 11 | 授权服务器与资源**不同 origin** | 已正确 | 这是标准生产形态（API 主机由独立 auth 主机保护）。只有 challenge 指定的 metadata URL 受同源约束，因为那是攻击者可控输入；已验证文档指向的 issuer 不受此限 |
| 12 | token endpoint HTTP 200 却带 `error` | 已正确 | 拒绝，不产生凭证 |
| 13 | `access_token` 为空字符串 | 已正确 | 拒绝 |
| 14 | metadata body 短于声明的 `Content-Length` 后断流 | 已正确 | 拒绝，不按已收到的内容解析——半途断流不形成伪成功 |
| 15 | refresh **轮换**（返回新 refresh token） | 已正确 | 下一次刷新presenting 新 token |
| 16 | refresh **非轮换**（省略该字段） | 已正确 | 沿用原 token，不会一次刷新后卡死 |
| 17 | provider 授予**更窄**的 scope | 已正确 | 接受而非当失败（RFC 6749 允许）。**但见下方观测性缺口** |
| 18 | `token_type` 非 Bearer（如 `mac`） | 已正确 | 拒绝。存下一个我们不会呈示的方案，会得到一个每次请求都静默失败的凭证 |
| 19 | 同一 credential 并发两次 begin | 已正确 | 第二次作废第一次；旧 flow 的 id 与 state 都不再可兑换 |
| 20 | `expires_in` 落在 `REFRESH_SKEW_MS`(30s) 窗口内 | 已正确 | resolve 时刷新而非呈示——呈示等于发一个我们已预期会失败的请求 |

第 15、16、20 条的断言落在 **token endpoint 实际收到了哪个 refresh token**，而不是"凭证还能解析"：后者在实现
错误时同样会通过。

| 21 | authorization endpoint 本身已带 query 参数 | 已正确 | `query_pairs_mut().append_pair` 扩展既有 query 而非另起一个。断言 provider 自己的 `tenant=`/`audience=` 存活且全串只有一个 `?`——静默丢掉租户判别参数会把用户送进错误的授权上下文，而其余参数看起来都对 |
| 22 | provider 要求 RFC 8707 `resource` 参数 | **已知不兼容（有意）** | 我们不发送 `resource`。要求它的 provider 会拒绝兑换。不臆测补上：向不预期该参数的授权服务器发送 resource indicator，本身可能改变签发的 audience。以测试形式记录当前行为 |

### 观测性缺口（本轮发现并已闭合）

第 17 条最初只能断言"兑换成功"：授予的 scope 虽被持久化，却没有任何 API 暴露它，运维**无法得知 provider 是否
静默收窄了权限**，而第一个症状会是某次工具调用失败、状态页却给不出解释。

已修复：`McpOAuthCredentialStatus::Active` 与 `McpOauthStatusResponse` 现在携带 granted scopes。**scope 名不是
凭证材料**，因此这拓宽的是可见性而非暴露面——管理面响应仍不含 token、code、verifier 或 state，扫描整条渲染
消息的身份测试继续通过。第 17 条的断言随之升级为"上报的是实际授予的，而非所请求的"。

### 为什么放宽 4 和 5 不削弱安全边界

两者都只是**解析**层面的宽松，解析之后的校验完全不变：

- `expires_in`：数字与数字字符串表达同一事实，之后仍走同一个上界检查（≤366 天）。无法解析的值变为 `None`，
  这与"省略该可选字段"含义相同，即生命周期未知——未知不会被当作很长，且被 provider 拒绝的 token 仍会经 401
  路径收敛。
- `scope`：数组与空格分隔字符串归一化为同一个字符串后，仍走同一套校验（≤32 项、非空、≤256 字符、无控制字符）。

配套断言 `missing_s256_is_still_refused_after_leniency`：放宽之后，只声明 `plain` 的 provider **仍然被拒绝**，
证明宽松没有蔓延到 PKCE 强度这类真正的安全边界。

## 尚未证明与风险

- **workload token 没有运维态身份形状**：`verify` 拒绝 run/attempt/worker 为 nil 的 claims，因此管理 token
  实质是绑定 Run 的 token 多带一个 scope。federation 与 administration 的隔离建立在 **scope** 上，而非独立
  身份形状。不应声称更强的性质。
- 远端撤销只在受控回环 server 上验证；真实厂商授权服务器的 revocation 行为、错误与限流**完全未验证**。
- 浏览器承载、本地 callback listener、CLI 登录旅程未实现，管理面只提供可嵌入的 begin/complete。
- Dynamic Client Registration、Client ID Metadata Document、private client secret 仍未覆盖。
- `process_session_process_crash` 曾在一次并发编译期间失败一次，之后 5 次复跑均通过；未取得该次失败的完整
  证据，不当作已澄清。
- **总体进度维持 70–75%**：本阶段全部证据仍来自受控回环 server，没有任何真实外部 OAuth MCP Server 兼容证据。

## 缓存与清理

- `runtime/target` 在本轮曾被 macOS 因暂存系统更新（Tahoe 26.6.1，3.8 GB，需重启）清除：Cargo 写入的
  `CACHEDIR.TAG` 使该目录被视为可清除空间。本会话未执行 `cargo clean`。源码与 Git 状态全程未受影响。
- 测试创建的 `agent-mcp-oauth-*`、`agent-mcp-oauth-disc-*`、`agent-mcp-oauth-admin-*` 临时目录由 `Drop` 清除。
