# MCP 单写发现协调器闭环

日期：2026-08-09
范围：Rust Worker library、真实 loopback MCP HTTP、Gateway gRPC、签名 Skill、Kernel Start、
Checkpoint 恢复与 ModelInvocation；未启动 NATS、Java、PostgreSQL、Docker 或 Kubernetes。

## 先红后绿

1. 新 Run 测试先改为调用不存在的 `McpDiscoveryCoordinator`，编译以 `E0432` 失败；加入最小空实现后，
   测试在第一次 `start` 返回 `false` 的真实行为上失败。
2. GREEN 后，慢 Run 停在真实 MCP `tools/list` 时，快 Run 完成发现、挂载签名 Skill Tool 并由同一
   协调器产生 `run.started`；真实 `ModelInvocation` 只包含 `mcp:fast/fast_tool`。
3. 恢复测试先要求不存在的 `Restore`/`Restored` 语义；加入错误的“恢复也调用 Start”分支后，测试按
   “返回 Started 而非 Restored”的行为失败。
4. 最终实现先挂载目录，再调用 `verify_restored_federated_tools`。恢复前的显式校验返回
   `CheckpointToolCatalogMismatch`；协调器完成后恢复动作为 `InvokeModel`，模型只看到 checkpoint
   冻结的 `mcp:search/web_search`。
5. 最后去掉调用方可传的 Start/Restore 选择，测试先以 `E0061` 证明期望接口尚不存在；Coordinator
   改为从 accepted attempt 的 `restored_from_checkpoint` 权威状态推导模式。新建与恢复两条真实测试
   分别约束两侧，调用方无法把恢复 Run 伪装成新 Run，或让新 Run 跳过 Kernel Start。

## 权威与边界

- 调用方只提交 `attempt_id`，协调器从 `WorkerProcessor` 取得已验收命令、取消令牌和基础目录，避免
  另一个命令副本替换身份、服务器或工作负载令牌。
- 后台 Supervisor 只执行网络 I/O；目录挂载、Kernel Start 和恢复校验都由接收方串行调用。
- 返回成功后，传输层仍需完成事件发布、Checkpoint 和消息确认。本轮没有以库测试替代 NATS ack 证明。

## 门禁

- `mcp_end_to_end`：13/13 真实 socket 测试通过。
- `agent-runtime-worker`：框架报告 128 通过；其中 19 个 `transport.rs` 用例因未配置
  `TEST_NATS_URL` 提前返回，实际执行断言 109 项。
- `cargo clippy -p agent-runtime-worker --all-targets -- -D warnings`：通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过。

## 对标结论

- **Codex**：stdio、OAuth、缓存、required/optional server、重连和资源/应用协议明显领先；其目录构建
  在单 session 内以 `join_all` 并发，未发现跨 Run 的 checkpoint-aware 单写结果协调器。
- **OpenClaw**：command lane 已有优先级、active snapshot、progress timeout、abort grace、generation、
  drain 与 restart 迁移，生命周期治理明显领先；任务本身拥有执行权，不提供本实现这种网络结果与
  Kernel mutation 分离的恢复校验边界。
- **下一缺口**：让实际 Host/可选 NATS adapter 驱动 Coordinator，并证明事件与 Checkpoint 成功前绝不
  确认输入；随后补 active discovery snapshot、drain 和强制取消。
