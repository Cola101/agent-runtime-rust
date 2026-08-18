# ADR-0141：Runtime 控制边界必须在 owner 释放后才可见

- 状态：Accepted
- 日期：2026-08-18
- 范围：Embedded Runtime、Event Cursor、流式事件、审批、MCP 输入、owner epoch

## 背景

网络审批门禁在全工作区负载下曾返回 `Internal`。隔离重跑可通过，但源码顺序存在真实窗口：

1. Agent Loop 将审批或 MCP 输入写入 Checkpoint/Event；
2. Embedded Runtime 把 Run 投影为 `AwaitingApproval`/`AwaitingMcpInput`；
3. 分页或流式游标立即公开 `WaitingApproval`/`Suspended`；
4. 旧 `ActiveExecutionGuard` 随后才释放；
5. 调用方依据边界立即提交决定时，新 owner 无法取得 Run。

这不是超时问题，而是外部边界承诺早于内部所有权事实。增加 sleep 或客户端重试会把竞态留给 Java、CLI
和未来 GUI，并使不同适配器产生不同语义。

## 决策

1. `WaitingApproval` 与 `Suspended` 定义为**可执行控制边界**，不仅是持久记录的展示状态。
2. `event_cursor` 与 `subscribe_events` 统一检查同一 invocation/Run 是否仍有 active owner；有则继续公开
   `Running`，owner 释放后才公开等待边界。
3. 不修改已经提交的 Event、Checkpoint 或 Run 记录，不延迟终态，也不加入定时猜测、自动重试或第二套状态机。
4. 进程崩溃后 active map 天然为空，因此替代 Runtime 仍能从持久等待状态立即恢复控制。
5. 确定性门禁直接构造“持久等待状态 + 活跃 owner”，同时验证分页、流式订阅、审批和 MCP 输入两类边界。

## 后果

- 正面：外部调用方看到等待边界时，下一代 owner 已可取得，审批不再因投影/guard 微小窗口返回 Internal。
- 正面：分页、SSE/gRPC 流式适配器和未来 Java/GUI 消费者共享同一边界语义。
- 代价：等待边界最多推迟到旧 owner 完成最后一次持久投影并释放，通常仅为一个本地收尾窗口。
- 中性：两个调用方仍可能同时竞争同一决定；既有 command id、owner epoch 和持久 receipt 继续负责幂等与围栏。

## 对标

- **Codex**：`request_command_approval` 先把 oneshot sender 注册进 active turn，再发送审批事件，并由同一 turn
  等待决定；它没有“发布等待边界后换 owner”的窗口。本项目保留“先建立可接收决定的事实，再公开请求”原则，
  但因支持释放计算资源和替代 Host，使用 owner-release 门禁而不是持有原 turn。
- **OpenClaw**：approval channel runtime 先写 pending entry；若 resolution 在请求投递完成前到达，则保存
  `pendingResolution`，投递完成后再统一 finalize。本项目吸收“投递与决定乱序不得丢失”的原则，但把保证放在
  Runtime lifecycle boundary，而不是 Gateway/channel 的内存 pending map。

## 证据

- `runtime/apps/runtime-host/src/embedded.rs`
- `runtime/apps/runtime-host/tests/grpc_invocation_approval.rs`
- `runtime/apps/runtime-host/tests/grpc_invocation_mcp_input.rs`
- `runtime/apps/runtime-host/tests/grpc_invocation_watch.rs`
- `docs/evidence/2026-08-18-actionable-runtime-control-boundaries.md`
