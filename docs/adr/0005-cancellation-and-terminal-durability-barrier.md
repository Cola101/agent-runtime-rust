# ADR-0005：取消控制与终态持久化屏障

## 状态

Accepted

## 决策

运行命令与取消命令使用不同的定向 JetStream subject。取消命令必须绑定
`tenant_id + run_id + attempt_id + worker_id`，携带有限有效期；Worker 不允许只按 Run ID
取消当前进程中的任意任务。

模型流统一转换为 Runtime Event IR。`Completed`、`Failed`、`Timeout` 与取消竞争时，Kernel
只允许第一个终态改变状态，后续信号不得覆盖终态。

Worker 在终态事件获得 JetStream PubAck 前继续把 attempt 计入活动容量。控制面必须在同一
PostgreSQL 事务中先插入终态事件，再更新 Run、完成 dispatch、释放 Workspace 租约并归还
Worker 容量。

## 理由

- Codex 的 turn 取消和 rollout flush 证明：取消不仅是内存标记，终态必须先成为可恢复事实。
- OpenClaw 的活动 invoke/AbortController 映射证明：节点需要独立、可定向的取消控制通道。
- 多租户平台还必须防止过期或串租户取消，因此命令身份比单机 Runtime 更完整。

## 已落地

- 控制面 `POST /v1/runs/{runId}:cancel`：未调度 Run 原子写入 `run.cancelled`；已取得
  attempt 的 queued/running/waiting_approval Run 通过 Outbox 向当前 Worker/attempt 发送一次定向取消命令。
- Rust 协议验证取消命令版本、目标、有效期和原因。
- Worker 对重复取消返回同一终态事件，迟到的模型完成事件不能覆盖取消。
- Kernel 将模型增量、Tool Call、用量、完成和失败转换为单调序号事件；超时进入
  `timed_out`，其余模型失败进入 `failed`。
- 控制面校验摘要、attempt 和序号后持久化终态，并原子释放 dispatch、租约和容量。

## 边界

- 当前没有真实模型 Provider Adapter，因此已验证的是供应商无关模型事件驱动与取消语义，
  不是 OpenAI/Anthropic HTTP 流已经可以被中断。
- `waiting_approval` 已沿用当前 fenced attempt 定向取消；`suspended` 尚未定义明确执行所有权，因此
  API 仍拒绝处理。
- Worker 的完成回执缓存仍是进程内缓存；跨进程重复取消依赖 JetStream 去重和控制面终态事实，
  后续应使用检查点或控制面 EventAck 完成闭环。
