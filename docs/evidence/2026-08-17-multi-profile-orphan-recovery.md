# 多 Profile 孤儿 Run 恢复证据（2026-08-17）

## RED

1. 多租户消费者首先无法编译：`EmbeddedRuntime` 没有聚合恢复入口，只能逐 Profile 调用内部恢复方法。
2. 相关回归首次暴露：事件已为 `succeeded` 时，网络重放仍可能读到 `Accepted` 收据，`run_status` 为空。
3. 聚合恢复后立即重复扫描曾间歇失败；根因是本进程的执行收尾仍持有 Run，而扫描器将它误判为 orphan。
4. 并发执行两次全量扫描用于证明恢复计划不会重复接纳，而不是用测试等待掩盖竞态。

## GREEN

- 聚合入口内部枚举所有 immutable Profile；坏 Profile 返回 typed failure，健康租户继续恢复。
- Run 级计划按 Profile round-robin 派发，共用原有全局、tenant、Workspace 准入。
- 单 Profile 与全量恢复共用异步门；当前进程仍 active 的 Run 被跳过，replacement Runtime 的孤儿 Run 正常恢复。
- Kernel 终态事件会收敛 Run record 与 `Accepted` control receipt，终态重放直接得到 `Completed`。
- 两个租户的第一 Runtime 均在模型请求出网后消失；replacement Runtime 总计只各执行允许的第二次 Provider
  尝试，两个 Run 都成功。并发双扫描接纳总数严格为 2，后续扫描为 0。

## 已执行门禁

- `embedded_recovery_all`：2/2。
- `daemon_recovery`：9/9。
- `embedded_control`：9/9。
- `grpc_invocation_recovery`：2/2。

共 22 个相关测试通过。测试不依赖 Docker、Java、数据库、NATS、外部凭据或 Edge。

## 结论边界

本轮补齐的是单进程、多 Profile Runtime 的启动恢复原语，并非完整产品自动恢复。生产宿主仍需在取得本地
state-root lease 后调用聚合入口；跨机器 owner 选举、分布式 command ledger、控制面告警和真实故障注入未完成。
因此 Rust Runtime 总体进度仍保持 70–75%。
