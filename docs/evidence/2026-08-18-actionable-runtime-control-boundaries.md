# 可执行 Runtime 控制边界证据（2026-08-18）

## 真实症状

Rust 全工作区回归中，网络调用方已从事件页读到 `WaitingApproval` 和精确 approval binding，随即提交
`DecideApproval`，服务端却返回：

```text
Status { code: Internal, message: "the Runtime could not complete this request" }
```

负载结束后的连续重跑能通过，说明症状依赖一个短时序窗口，但不能证明契约安全。

## 确定性 RED

新增内核测试不使用 sleep 放大竞态，而是直接建立真实冲突条件：持久 Run 已是 `AwaitingApproval`，同一
invocation/Run 的 `ActiveExecutionGuard` 仍存活。修改前结果稳定为：

```text
left: WaitingApproval
right: Running
```

这证明事件游标把“记录已经等待”错误等同于“下一控制命令已经可以取得 owner”。

## 修复

- `event_cursor` 与 `subscribe_events` 共用 `state_after_execution_owner_release`。
- 活跃 owner 存在时，`WaitingApproval`/`Suspended` 对外仍是 `Running`。
- owner 释放后，分页返回 `WaitingApproval`，流式订阅发送同一 typed boundary。
- 持久 Event、Checkpoint、Run record、owner epoch、control receipt 和终态提交顺序没有改变。
- 没有增加延时、重试次数、后台任务或外部服务。

## GREEN

| 门禁 | 结果 |
| --- | --- |
| 确定性分页 + 流式 owner-release 测试 | 1/1 |
| Runtime Host lib | 33/33 |
| 网络审批完整闭环 | 1/1；随后连续 30/30 |
| 网络 MCP 输入替代 Runtime 闭环 | 1/1 |
| gRPC 流式事件 | 3/3 |
| Embedded 多租户/事件/恢复 | 12/12 |
| 替代 Runtime 网络恢复 | 2/2 |
| Embedded 审批/取消/幂等控制 | 9/9 |
| Runtime Host all-targets Clippy `-D warnings` | 通过 |
| Rust 格式与差异门禁 | 通过 |

本轮不重跑会重新产生约 15 GiB 多版本工作区产物的全工作区门禁；确定性状态测试覆盖原竞态条件，
真实网络压力重跑覆盖调用方链路，恢复与 Embedded 控制门禁覆盖新 owner 接管和并发命令。

## 对标边界

- Codex 的 active turn 在发出审批事件前已经注册决定 channel，并由原 turn 原地等待；本项目为了可释放资源和
  Host replacement 不复制该生命周期，只保留“请求可见前，决定路径必须已经可用”的不变量。
- OpenClaw 对投递中提前到达的 resolution 使用 `pendingResolution` 收敛；本项目没有 Gateway/channel 依赖，
  因而在协议中立 Runtime boundary 上阻止过早可见。
- 本轮修复的是网络/嵌入调用契约可靠性，不增加真实 Provider、跨平台隔离或生产持久层证据；总体仍为
  70–75%。
