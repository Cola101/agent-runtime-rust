# ADR-0009：Worker 模型执行监督器与终态持久化屏障

## 状态

Accepted

## 背景

Worker 原先只能接收 dispatch、启动 Kernel 并发布 `run.started`。如果在同一个 NATS 轮询方法里等待
完整模型流，长响应会阻塞取消命令、心跳和其他 Run；如果让多个任务直接修改 Kernel，则会破坏单调
事件序号和唯一终态。

## 决策

每个 attempt 的模型 RPC 由独立 `ModelExecutionSupervisor` 异步驱动。监督器只能输出 Provider 无关的
`ModelStreamEvent`，不能直接修改 Run 状态或发布 NATS。`NatsWorker` 主循环串行执行：

1. 持久发布 accepted 和 `run.started`，确认 dispatch 后才启动模型 RPC；
2. 从监督器逐条取事件，并由唯一的 `RunMachine` 分配 Run sequence；
3. 等待 JetStream PubAck 后才处理下一条模型事件；
4. 终态事件获得 PubAck 后，才从 Worker 活动集合释放 attempt；
5. 用户取消使用同一个 `CancellationToken`，并发关闭 gRPC 和 Provider HTTP 流；取消本身已经产生终态，
   监督器不得再合成失败终态。

模型流异常统一转换为分类 `ModelStreamEvent::Failed`。认证错误不可重试，限流、超时和不可用可标记
重试，协议错误不可重试；是否真正重试仍由后续 ModelPolicy 决策，Worker 当前不会自行重放。

## 替代方案

- **在 `poll_once` 中同步等待模型结束**：实现简单，但一个慢模型会阻塞取消、心跳和其他租户任务。
- **把整个 WorkerProcessor 放进共享异步锁**：可并发接收，但模型流期间容易形成长锁和取消死锁。
- **让每个模型任务直接发布 NATS**：吞吐高，但 Kernel 顺序、唯一终态和 PubAck 资源屏障难以统一。

## 已验证

- 真实 Gateway + HTTP/SSE 测试证明同一 dispatch 只启动一次，文本、用量和完成事件顺序不变。
- 取消在 500ms 门禁内穿透监督器、gRPC 和 Gateway，并关闭 Provider TCP；没有附加失败事件。
- 提前关闭的 gRPC 流转换为不可重试 Protocol 失败，不会被误认为成功。
- 真实 JetStream 测试得到 `run.started → text → usage → run.succeeded`，终态 sequence 为 4，PubAck
  后心跳活动 Run 数才从 1 变为 0。

## 影响与未完成

- Worker 主进程现在必须配置 Model Gateway Endpoint；模型凭证仍不进入 Worker。
- 事件提交当前由单个 Worker 循环串行完成，尚未做 1000 Run 吞吐和背压压测。
- Tool Call 只结束当前模型回合；ADR-0010 已补齐策略规划、审批绑定和结果回灌的内部契约，真实
  持久审批命令已接线；Tool Executor 及生产循环自动发起下一轮仍未接线。
- 进程崩溃、优雅停机、监督任务回收、令牌刷新和策略控制的模型重试仍需后续实现。
