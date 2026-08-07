# 技术 Alpha 与私有 Beta 验收

## 技术 Alpha

- 真实 PostgreSQL 中创建 Workspace、Session 与 Run。
- 重复 `Idempotency-Key` 返回同一个 Run，不产生第二条 Outbox 命令。
- Tenant A 无法读取 Tenant B 的 Run 或 SSE 事件。
- Worker 事件重复提交不会产生重复事件；序号缺口会被拒绝并要求补传。
- Run 在审批期间重启控制面后仍可继续。
- Worker 丢失租约后提交事件会被 fencing token 拒绝。

## 私有 Beta

- 1000 个混合活跃 Run 与 10000 个休眠 Session 的压测报告。
- PostgreSQL、JetStream、Kubernetes 节点和 Kata 沙箱故障演练。
- 每次恢复演练必须导出 `recovery_incidents` 与按租户 SLO 快照；从控制面最后确认健康时间计时，只有持久化 `run.restored` 或明确终态才算恢复结束。
- API p95、事件 p95、暖/冷调度延迟均有原始测量证据。
- OIDC、RLS、对象存储前缀、节点身份和 Skill 制品的跨租户否定测试。
- 完成 SOC 2 Ready 控制项映射，但不得宣称已获得认证。
