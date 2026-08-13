# Runtime 执行策略快照闭环

日期：2026-08-09
范围：Rust Protocol、Worker、Model Gateway、Standalone Runtime Host；不依赖 Java、NATS、PostgreSQL、Docker 或 Kubernetes。

## 先红后绿证据

1. v10 命令携带策略时，旧协议因只支持到 v9 而拒绝；伪装成 v9 时策略字段被静默忽略。升级后 v10
   必须携带合法快照，旧 schema 夹带字段明确拒绝。
2. 真实 5-server MCP/Gateway socket 测试把命令并发上限设为 2。旧 Worker 仍同时启动第 3 台，测试失败；
   改为从命令读取后，第 3 台保持排队且目录顺序不变。
3. 真实 HTTP Provider 测试把最大模型尝试次数设为 1。旧网关会在 429 后进入下一 Provider；新路径在
   首次失败后停止，同时仍保留“已有输出绝不切换”和认证错误不切换的不变量。
4. Tool 执行上下文测试把超时设为 1234ms。旧 Worker 得到剩余租约约 299 秒；新 Worker 得到精确
   1234ms，并继续受剩余租约的更严格上限约束。
5. 原 Worker Checkpoint 后只改变 Tool timeout，旧恢复错误返回成功；Checkpoint schema 8 后在模型、
   MCP 或 Tool 工作恢复前返回 `CheckpointIdentityMismatch`。
6. 独立 `runtime-host` 真实完成一次 loopback 模型 Run，其文件系统 Checkpoint 中保存 schema 8 和
   1234ms 策略；审批、守护进程重启、IPC 重连和终态恢复测试继续通过。

## 门禁

- `agent-protocol`：57 通过。
- `agent-model-gateway`：49 通过、4 个需外部公共 MCP 的用例忽略。
- `agent-runtime-host`：19 通过。
- `agent-runtime-worker`：加入 ADR-0043 的异步监督器闭环后，测试框架报告 126 通过；其中 19 个
  NATS 用例因未配置 `TEST_NATS_URL` 在测试体内提前返回，实际执行断言的用例为 107 个，
  不能将其计作 NATS 验收。
- 四个包的 Clippy `-D warnings`、Rustfmt 和 `git diff --check` 通过。

## 对标结论

- **相对 Codex**：本实现首次把 MCP 发现、模型故障转移和 Tool timeout 组成跨 Worker/Checkpoint 的
  同一 Run 身份，这一恢复约束更严格；Codex 在 MCP stdio、生命周期、缓存、逐 Server 配置和 Tool
  广度上明显领先。
- **相对 OpenClaw**：本实现的类型化策略和 Checkpoint 身份比 CLI 配置哈希更适合多租户 Runtime；
  OpenClaw 的模型 fallback 候选构建、auth cooldown、错误分类与观测明显更成熟。

## 下一缺口

共享 backpressure 与租户轮转已在 ADR-0042 落地，实测见
`2026-08-09-mcp-fair-admission.md`。当前 NATS Worker 仍在串行接单方法里等待 MCP 发现完成，所以下一
协议中立异步发现监督器已在 ADR-0043 落地；当前缺口进一步收窄为 NATS adapter 接入：`poll_once` 与
`poll_recovery_once` 尚未通过监督器提交不可变完成结果，因此生产 Worker 的接单循环仍会等待慢 MCP。
