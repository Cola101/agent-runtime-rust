# 公开显式 Resume 网络闭环证据（2026-08-17）

## RED：Resume 不得扩大模型预算

新的网络消费者让第一 Runtime 在模型请求已发出、响应未持久化时整体消失，再由第二 Runtime 提交
`{"type":"resume"}`。使用默认 `max_same_provider_attempts=1` 时得到：

```text
["run.started", "run.restored", "model.provider.failed", "run.failed"]
expected succeeded, got failed
```

这不是实现错误。第一次模型请求已经占用一次冻结尝试；Resume 无权把“一次”改成“两次”。该 RED 防止
为了让恢复测试变绿而绕过模型重试政策。

## GREEN：策略允许时继续同一 Run

- 前后 Runtime 使用完全相同的模型路由快照，并显式冻结 `max_same_provider_attempts=2`。
- 第一 Runtime 在真实 HTTP/SSE 请求已到达 Provider 后整体销毁，事件公开边界为 `Running`。
- 替代 Runtime 通过真实 gRPC `Control` 接受原 `run_id + owner_epoch`，返回 `applied_owner_epoch = old + 1`。
- route journal 把中断请求计作第一次尝试，只进行第二次调用；Provider 总调用严格为 2。
- 事件最终为 `succeeded`，包含恢复后的模型增量；`run.started` 始终只有一次。
- 同一 `command_id` 在终态后重放，返回相同 command digest 和 `succeeded` 收据，没有第三次 Provider 调用。

## 已执行门禁

- `cargo test -p agent-runtime-host --test grpc_invocation_recovery`：2/2。
- 测试包含原有“等待审批后 Runtime replacement”与新增“运行中显式 Resume”两条网络恢复路径。

## 结论边界

本轮证明公开 Resume 成功路径已经存在，并证明它服从冻结的模型尝试预算；没有修改 Runtime 产品逻辑。
未使用 Java、GUI、Edge、Docker、PostgreSQL、NATS、外部服务或凭据。跨机器、真实厂商、自动 orphan 扫描
和分布式 command ledger 仍未验证，因此总体 Rust Runtime 进度保持 70–75%。
