# 原生端到端复验：容器化写工具主链

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 为什么要再跑一次

`2026-08-06-native-signed-skill-e2e-reverification.md` 通过时，以下四项尚不存在或未生效：

- 工作区租约拒服务缺陷的修复
- ADR-0036 的 Seatbelt 容器化
- `workspace.write_text`
- 控制面 `BETA_DELEGATED_SCOPES` 加入 `tool:workspace.write`

按「证据不能早于它要证明的代码」这条标准，08-06 的证据已是历史基线。本轮用**当前代码**重跑，
并把只读工具换成**受容器约束的写工具**，其余覆盖面不变。

Docker、虚拟机、Kubernetes：未使用。未注册任何 Homebrew Service。

## 环境与依赖（第 1 项）

| 项 | 实测 |
| --- | --- |
| 工具链 | java 21.0.11 · mvn 3.9.14 · cargo 1.97.1 · node 22.23.1 · pnpm 10.13.1 · postgres 14.22 · ruby 2.6.10 |
| 代理 | 系统代理 `HTTPEnable:0`，`127.0.0.1:10808` **不可达**；直连外网 200 |
| 端口 | 起栈前 10 个约定端口（含 8080）全空 |

移交声称代理「预期为 127.0.0.1:10808」与实测不符。`with-download-proxy` 在系统代理关闭时回落直连，
直连可用，故不构成阻塞。

## 启动的原生进程（第 2、3 项）

PostgreSQL 54329 · nats-server 4222 · control-plane 18080/9090 · model-gateway 18081 ·
checkpoint-gateway 18082 · runtime-worker 18083 · console 5173。

## 签名 SkillVersion 与签名域（第 4、7 项）

经真实 API `POST /v1/skills:publish`：

```
skill_version_id : edd82538-9091-4f02-be79-bba44076a093
artifact_digest  : 113c851b4025ef64a64db10dcc8623cd2558d8e661cb22d6f906845162f56081
signing_key_id   : local-skill-key-v1
signature        : 86 字符（64 字节 base64url）
```

用 `.local/env/native.env` 中的 Ed25519 **公钥**经 `openssl pkeyutl -verify -rawin` 独立验签：

```
域绑定 agent-runtime-skill-v1.<digest> : Signature Verified Successfully
负对照 裸 digest                        : Signature Verification Failure
```

负对照失败证明域绑定真实生效，而非碰巧通过。

## Tool 三方交集（第 8、9、10 项）

AgentVersion `2e987564-ace1-4600-b082-544b3e8ea91d` **只授予 `tool:workspace.write`**。
Worker 两个工具都已安装，Skill 只声明写工具。因此读工具**既未被声明、也无 scope**。

Provider 记录的是它**实际收到的请求**：

```json
{ "advertised_tools": ["workspace.write_text"],
  "write_tool_offered": true,
  "read_tool_leaked": false,
  "system_message_count": 1,
  "system_matches_agent_plus_skill": true }
```

`system_matches_agent_plus_skill` 为真表示系统消息与下式**逐字节相等且只有一条**：

```
Review the workspace and explain evidence before conclusions.

[Skill e2e-0807-author@1.0.0]
Record your findings in a file.
```

## 强杀 Worker 与恢复（第 11、12 项）

在审批挂起时对 Worker 发 `SIGKILL`（pid 48068），确认进程消失后启动替代 Worker。

| | attempt_id | owner_epoch | fencing_token | state |
| --- | --- | --- | --- | --- |
| 杀前 | `254ce465-d12d-48fc-80dc-d6dafcef34d5` | 1 | `ac549760…` | accepted → **lost** |
| 恢复后 | `96fe0141-dbf4-4bec-8af2-0e575da4eecd` | **2** | `9193eee3…` | accepted |

三项身份全部变更。事件中的 `run.restored` 与 `approval.rebound` 表明审批是跨 attempt 真实重绑。

## 权威事件序列与终态（第 14 项）

取自 PostgreSQL `run_events`（**不是** SSE 日志——我的 SSE 捕获在恢复等待期间因
`--max-time 300` 超时截断，只收到前 6 条；事实源以数据库为准）：

```
1. run.started        5. run.restored          9. tool.result
2. model.tool_call    6. approval.rebound     10. model.output.delta
3. model.turn.completed 7. run.resumed        11. run.succeeded
4. approval.required  8. tool.execution.started
```

Run `38eb1175-894c-4235-b194-77f215e1fe59`：`status=succeeded`，`last_sequence=11`，`finished_at` 非空。

## Tool 恰好执行一次（第 13 项）

`tool_executions` 账本两行，同一 `tool_call_id`：

```
call_e2e_write_1  state=planned    attempt=254ce465   ← 被杀的 attempt，从未执行
call_e2e_write_1  state=completed  attempt=96fe0141   ← 恢复后的 attempt，执行一次
```

Provider 侧 `tool_result_seen_times: 1`，绑定结果只回灌一次。

## 容器内写入真实落盘

```
-rw-------  44  agent-findings.md
内容：contained write proves the seatbelt boundary
```

Tool 报告 `{"bytes":44,"path":"agent-findings.md"}`，与文件系统一致。写入发生在
Seatbelt 容器内（ADR-0036），profile 只允许写工作区子树、不含任何 network 规则。

## 资源占用（第 15 项）

| 进程 | RSS |
| --- | --- |
| control-plane | 141.6 MiB |
| console (vite) | 49.0 MiB |
| nats-server | 23.8 MiB |
| runtime-worker | 18.2 MiB |
| model-gateway | 13.7 MiB |
| checkpoint-gateway | 9.9 MiB |
| postgres | 9.5 MiB |
| **合计** | **0.25 GiB** |

远低于 4GB 上限。

## 限制（明确不声称）

- **Provider 为回环假 Provider**，未接触任何真实模型厂商。
- **容器化仅 macOS**；Linux Worker 无 landlock 等价物。
- **容器不保护机密性**：ADR-0036 已写明读不受限，被容器化的 Tool 仍能读用户可读的一切。
- 本轮未跑 Console 与完整 Rust Workspace 全量测试，不得据此声称其状态。
- 我的 SSE 捕获超时截断（见上），事件证据取自数据库而非 SSE；SSE 续传能力本轮**未**独立验证。

## 复现

```
deploy/native/devctl bootstrap          # 先预热，规避 go install 偶发卡死
AGENT_RUNTIME_PROVIDER_ENDPOINT=http://127.0.0.1:PORT/v1/chat/completions \
AGENT_RUNTIME_PROVIDER_MODEL=... AGENT_RUNTIME_PROVIDER_API_KEY=<本地生成> \
AGENT_RUNTIME_PROVIDER_PROTOCOL=openai_compatible make dev
# 发布 Skill → 绑定 AgentVersion(仅 tool:workspace.write) → 建 Session/Run
# 审批挂起时 kill -9 worker → 重启 worker → 批准 → 验证落盘
make dev-clean
```

密钥仅存在于 `.local/secrets` 与会话临时文件，未进入命令行、日志、本文件或仓库。
