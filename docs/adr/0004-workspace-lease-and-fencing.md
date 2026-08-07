# ADR-0004：Workspace 单写租约与 fencing

## 状态

Accepted

## 决策

Workspace 持久保存，执行环境按需租用。每次所有权变更递增 `owner_epoch` 并签发新的 fencing token；旧 Worker 的写入必须被拒绝。

## 已落地

- Scheduler 在同一数据库事务锁定 Run、选择健康且有容量的 Worker、取得 Workspace 租约、创建 attempt，并写入执行命令 Outbox。
- 同一 Run 的重复 `RunQueued` 只返回已有 dispatch，不创建第二个 attempt。
- 活跃租约阻止第二个写任务；过期接管会递增 owner epoch 并更换 fencing token。
- 执行命令按 Worker ID 定向投递；Rust Worker 验证目标、租约时间和容量后发布 accepted 事件，再确认 JetStream 消息。
- 控制面只接受 tenant、run、attempt、worker 全部匹配的 accepted 事件。
- dispatch 期间 Run 保持 `queued`；只有匹配的 accepted 事件落账后才进入 `running`。
- Worker 心跳携带活动 assignment 的 tenant、run、attempt、Workspace、owner epoch 和
  fencing token；控制面只续期仍有效且全部匹配的 dispatch 与 Workspace 租约。
- dispatch 保留多 attempt 历史，但数据库部分唯一索引保证同一 Run 只有一个活动 attempt。
- Reconciler 将未 accepted 的过期 attempt 标记为 `lost` 并重新排队；已 accepted 的过期
  attempt 进入 `indeterminate`，禁止自动重放。

## 尚未落地

Worker 尚未收到控制面“续租已落账”的双向确认，因此未来的模型网关和 Tool Runtime 仍必须
在外部副作用前验证 fencing token。当前 Reconciler 只完成单批次最多 100 条的收敛逻辑，
尚未完成故障注入、告警、指标和 15 分钟恢复目标验收。
