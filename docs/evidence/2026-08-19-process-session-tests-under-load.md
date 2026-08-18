# 进程会话测试在负载下的失败（2026-08-19）

**结论先写在前面：还没找到竞态。** 这份记录的是已经量到的东西和已经排除的东西，
不是一个诊断。写下来是因为过程中三次全量门禁的失败**都是我自己造成的**，
而这件事本身值得记住。

## 三次被污染的门禁

| 轮次 | 失败项 | 我当时在同时跑什么 |
| --- | --- | --- |
| ws.log | `interrupt_reaches_the_registered_process_group_and_converges_terminal` | 我正在编辑它所属的源码树，并另开了两个 `cargo test -p agent-runtime-host` |
| ws2.log | `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded` | `vite build` + `vitest run` |
| ws3.log | 上面第一条 + `sixty_four_sessions_keep_one_thousand_waits_bounded_and_tenant_fair` | 两次 `vitest run` |

ws2 那次还额外说明了一件事：`cargo test` 默认 fail-fast，所以那一轮只跑了 411 条就停了，
而我第一次读日志时它**根本还没跑完**——我据此报出的 "742 passed, 0 failed" 是无效的。
后面的门禁一律加 `--no-fail-fast`，并且读之前先确认日志里有 `EXIT=`。

## 刻意复现出来的是什么

隔离跑：`persistent_process_session` 整个二进制连跑 5 次，18/18 全绿，每次 3.7–4.9 秒。

同时跑 8 份同一个二进制（≈100+ 并发 PTY 会话）：**每份挂 5–13 条**。
失败信息高度一致：

```
state: Running, pid: Some(80479),
stdout: "", stdout_cursor: 0, stderr: "", stderr_cursor: 0
```

进程活着，**一个字节都没读到**。不是"慢了一点"，是观测侧在整个窗口里没有推进。
`process_wait_yields_until_delayed_output_is_durable` 期望 `"delayed-ready\n"`，拿到空串。

这个负载比门禁本身重得多：`cargo test --workspace` 一次只跑一个测试二进制，
二进制内部按 CPU 数并行；我那是 8 个二进制各自再并行。所以**这次复现不能直接
用来解释门禁里的那一两条**。

## 已经看清、但还不构成诊断的一点

产品侧**有**事件驱动的观测通道：`ProcessSessionToolOperation::Wait` 走
`subscribe_wait_observation` 拿一个 watch，在「有新字节」或「到达终局」时被唤醒，
并且用 `select!` 同时等取消和 deadline（`process_session.rs:891` 起）。

而这些测试全部绕开它，用 `poll_until` 固定轮询 **500 次 × 10ms**，
`interrupt_...` 那条是 **200 次 × 10ms**。也就是说测试观察的是
"5 秒内观测者有没有把字节落到durable"，用**迭代计数**表达，
而不是用产品自己那条唤醒通道。

这解释了为什么它们对 CPU 超订如此敏感，但**不能证明产品有缺陷**：
一个被饿死的观测者和一个坏掉的观测者，从这些断言里看不出区别。

## 没有做的事，以及为什么

- **没有延长 timeout**。这条线是明令禁止的，而且在这里它也确实答非所问：
  失败时读到的是 0 字节而不是"差一点"。
- **没有把测试串行化**，同理。
- **没有把它归为 flaky 然后放过**。

## 要往下走需要什么

一次**真正干净**的全量（机器上没有别的东西在跑）。如果这两条在干净门禁里也失败，
那说明和我制造的负载无关，值得按竞态查下去——第一步是看 PTY 的排空是 tokio 任务
还是独立 OS 线程，以及它在 `#[tokio::test]` 默认的 current-thread 运行时下由谁驱动。
如果干净门禁全绿，那么已知的事实就只有一条：这些断言用迭代计数表达墙钟，在超订下
会失败——那是测试形状的问题，修法是让它们等产品自己的观测通道，不是放宽界限。
