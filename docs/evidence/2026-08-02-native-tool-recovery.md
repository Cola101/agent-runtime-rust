# 2026-08-02 macOS 原生 Tool、审批与 Worker 恢复验收

## Scope

- 主机：Apple Silicon M1 Pro，16GB，macOS ARM64。
- 常驻组件：Java 21 控制面、Rust Model Gateway、Checkpoint Gateway、Runtime Worker、Vue/Vite、
  PostgreSQL 14、NATS JetStream 2.10.20。
- 模型端：只监听 `127.0.0.1` 的确定性 OpenAI-compatible SSE Provider；两轮请求均验证完整请求体。
- Tool：`workspace.read_text`，`pure`、`ask`、`trusted_native`、`tool:workspace.read`。
- 参考源码快照：Codex `ff352fab6209`；OpenClaw `58b4b9430457`。

## 正常主链

Run `db035dbb-beaa-46f7-a630-b507e0c6e0f4` 实际产生以下持久事件：

```text
run.started
model.usage
model.tool_call
model.turn.completed
approval.required
run.resumed
tool.execution.started
tool.result
model.output.delta
model.usage
run.succeeded
```

审批前数据库为 `Run=waiting_approval`、`Approval=pending/version 1`，并持久化 sequence 5 的
`waiting_approval` Checkpoint。使用本地审批命令 `allow_once` 后：

- Approval 变为 `approved/version 2`；
- Tool Ledger 为 `call_native_read_1 | pure | trusted_native | completed`；
- Tool 返回 `README.txt` 的 64 字节 UTF-8 内容；
- 第二轮 Provider 请求包含相同 Tool Call ID 与真实 Tool Result；
- Run 以 sequence 11、`succeeded` 终止。

## Worker 崩溃恢复

Run `1fae80e6-f9ed-4f81-a438-3e4ab1efc07d` 在 `approval.required` 后对 Worker 进程组发送 `SIGKILL`，
未执行优雅 drain。控制面、PostgreSQL、NATS、Gateway 和 SSE 客户端保持运行；随后用同一个稳定
Worker ID 启动新 incarnation。

持久结果：

| 项目 | 故障前 | 恢复后 |
|---|---|---|
| attempt | `da6edee7-c31e-4016-8b9d-c412ce8797b8` | `bf5849b6-0182-449a-98fa-e40edae6e76d` |
| owner epoch | 2 | 3 |
| dispatch | `lost` | `finished` |
| recovery incident | — | `recovered` |
| Tool Ledger | `planned` | `trusted_native/completed` |

SSE 在原连接上继续收到：

```text
run.restored
approval.rebound
run.resumed
tool.execution.started
tool.result
model.output.delta
model.usage
run.succeeded
```

旧审批 ID 在重绑后仍以 version 1 决定，执行结果只出现在新 attempt；最终 Run sequence 为 13。
这证明恢复来自 SAFE Checkpoint 和新 fencing 所有权，不是从原输入盲目重跑。

### 可重复强杀门禁

`make check-native-recovery-live` 现使用随机回环端口和独立临时项目根目录完成同一故障链。最近一次通过结果：

| 项目 | 值 |
|---|---|
| Run | `554d1253-f705-4cb1-9634-e0695355224b` |
| Workspace | `cb8eb050-5333-4fe9-917b-47fd860ab16b`（API 动态创建） |
| AgentVersion | `72ab0e44-11e7-4623-b045-ded01000632d`（API 动态创建） |
| Approval | `019fbfac-1e84-76d1-9c01-369c813774d0` |
| 故障 attempt | `179b74b9-6e5d-49fc-b38d-347a788aca8f` |
| 恢复 attempt | `92f28976-d55c-451a-8c33-3aba9c6e464b` |
| owner epoch | `1 → 2` |
| RSS | `289024 KiB = 282.3 MiB` |
| 审批入口 | 真实 Chrome → Vite 回环代理 → Java 控制面 |

门禁由真实 Chrome 页面决定恢复后的同一个审批，核验精确的 `POST /v1/approvals/{id}:decide` 返回 200、
页面无 Console/API 错误，旧/新 dispatch 分别为 `lost/finished`、恢复事故为
`recovered`、`trusted_native/completed` Tool 恰好一个、Provider 精确收到动态 AgentVersion 的 system
指令且第二轮收到真实 Tool Result，并从 SSE
重放出从 `run.started` 到 `run.succeeded` 的全部 13 个事件。退出后随机端口和临时项目根目录均不存在。

视觉证据：

- [恢复后待审批页面](2026-08-02-native-console-approval-pending.png)
- [浏览器批准后 Run 完成页面](2026-08-02-native-console-run-succeeded.png)

## 资源与边界

- 最新自动门禁将 PostgreSQL 主/子进程、NATS、Java、三个 Rust 服务、pnpm/Vite/esbuild 的 RSS
  相加为 `289024 KiB = 282.3 MiB`，低于 4GB 上限约 14.5 倍；该数值包含真实 Chrome 验收时刻之前的
  原生 Runtime 进程，不把浏览器本身计入常驻 Runtime。
- 小 Checkpoint 经过 Zstd 后按协议内联 PostgreSQL；大于 512 KiB 的载荷才进入本地内容寻址文件后端。
  文件后端的路径隔离、写入、读取和摘要复验由 `filesystem_store.rs` 覆盖。
- `trusted_native` 不是强沙箱；本轮未执行 Shell、网络访问、写文件或租户上传代码。
- Chrome 三视口路由测试已验证 allow-once/deny、并发 409 刷新与可点击尺寸；独立 live 门禁进一步
  直连真实 Vite/Java/PostgreSQL/NATS/Rust 主链，完成恢复后的 allow-once、Tool 执行和 Run 终态。
  外部真实模型仍不在本证据范围内。
- 修正本地 API 默认端口后，未设置任何端口覆盖的 `make dev` 实际启动七类组件并全部健康；项目使用
  18080，未影响机器上已有的 8080 服务。最终 `make dev-clean` 后 `.local`、Rust/Java 构建目录、Vue
  build/coverage/E2E 产物、项目进程、11 个本地监听端口和本轮测试临时目录均复核为无残留。

## 对标结论

- Codex 的 Tool 数量、Shell 成熟度、macOS sandbox 和 rollout/compaction 仍领先。
- 本平台新增 Codex 本地单用户模型没有的 tenant/RLS、delegated scope、Tool Ledger、owner epoch、
  Worker incarnation 和跨进程 SAFE Checkpoint 恢复。
- OpenClaw 的 `system.run`、Node 连接和跨平台进程诊断仍领先；本平台吸收了 approved cwd/脚本漂移
  fail-closed 原则，并进一步把可执行文件 SHA-256 放进模型目录、审批绑定和执行器一致性校验。
