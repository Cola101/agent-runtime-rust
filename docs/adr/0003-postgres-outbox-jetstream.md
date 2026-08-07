# ADR-0003：PostgreSQL + Outbox + JetStream

## 状态

Accepted

## 决策

PostgreSQL 是权威状态源。API 在同一事务写业务状态和 Outbox，发布器至少一次投递到 JetStream；消费者以事件 ID 和业务幂等键去重。

## 结果

API 确认后不依赖消息总线即时可用；代价是所有消费者必须正确处理重复和重放。

## 已落地的发布语义

- `run.queued` 使用 `runtime.control.run.queued.v1` Subject，消息体是完整且带版本的 Run 快照。
- 多发布实例通过数据库 claim token、到期时间与 `FOR UPDATE SKIP LOCKED` 竞争任务。
- claim 时递增尝试次数；发布失败释放 claim 并保留有界错误信息。
- 只有 JetStream 返回 PubAck 后才写 `published_at`。
- Outbox ID 同时作为 NATS 消息 ID；Stream 使用 24 小时重复检测窗口。
- 发布器允许受控跨租户读取 Outbox，因此生产部署必须使用独立数据库身份；API 身份继续受 RLS 约束。

当前只完成 API → Outbox → JetStream。Scheduler 消费、Workspace 租约、execution command
和 Worker ACK 属于下一条主链，不得把本 ADR 的发布完成解释为 Run 已经开始执行。
