# 配了的 MCP 服务没起来，谁都不说（2026-08-19）

## 它是什么

`McpServerDiscoveryStatus` 记着每个配了的 MCP 服务这次 Run 的结果——
起没起来、试了几次、报什么错、宣告了哪些能力。它被放进 `LocalRunOutcome.mcp_servers`，
然后**就停在这里了**：owner 面和 workload 面都不返回这个字段。

于是分成两种情况：

| 服务 | 起不来时 | 界面看得见吗 |
| --- | --- | --- |
| **必需** | Run 终止，`run.failed`・`required_mcp_unavailable`，事件带着服务名 | 看得见 |
| **可选** | Run 照常往下跑，那些工具不在目录里 | **完全看不见** |

第二行是这次修的。它在界面上的样子是：agent 从来不用你配的那个服务。
**这和「模型选择不用它」一模一样**，而这两件事该做的处理完全不同。

## 修法：写进 Run 自己的日志

新事件 `mcp.discovery.completed`，内核发，负载是每个服务的完整结果。
不是新加一个 socket 调用——事件日志是客户端**已经在读**的那条流，
既是耐久的，也是重连之后能重新折出来的。

内核那半有一处必须说清楚：这条事件**不改状态**。

第一版写的是 `emit(RunStatus::Running, …)`，跑出来是：

```
Execution("agent kernel rejected execution start: command Start is invalid while run is Suspended")
```

因为**替换宿主恢复一个 attempt 时 discovery 会再跑一次**，而一个停在
`mcp.input.required` 上的 attempt 恢复回来是 `Suspended`。一条名为 `Running` 的事件
会把它**悄悄复活**——偏偏是在服务最可能已经消失的那条路径上。
现在它把当前状态原样带下去，唯一的守卫是「不是终局」。

## 客户端：都好的时候不说话

和 `model.provider.selected` 同一套安排：进 `ROUTINE`，
由 `belongsInConversation` 在**有服务没起来**时把它捞出来。
每个 Run 都列一遍哪些服务是好的，那是把机器日志印在对话里。

有没起来的才画一行，说是哪个、是不是必需、试了几次、报的什么错。

## 验收

真二进制、真配置文件、真事件日志：

```
test an_optional_mcp_server_that_never_started_is_written_into_the_run_log ... ok
```

配置里那个服务是 `/usr/bin/false`——一个真实存在、起来就死的程序。

**这里查明了一件本来不知道的事**：如果配的命令路径**根本不存在**，
宿主在**配置阶段**就拒绝启动：

```
Configuration("stdio MCP command must be an existing absolute file: /nonexistent/mcp-server")
```

也就是说在设置里把 MCP 路径写错，代价不是「那个服务不可用」，而是
**整个 Runtime 起不来**。这一条窗口上已经有话说（启动失败横幅带原因），
但它和本文修的不是同一条路径，别混起来。

## 两条老测试改了断言

`standalone_stdio_mcp_timeout_reaps_the_entire_process_group` 和
`..._initialize_timeout_...` 断言的是**精确的事件序列**，现在多了一条：

```
["run.started", "mcp.discovery.completed", "model.provider.failed", "run.failed"]
```

日志里确实多了这一条，而且这两条测试的场景（一个超时的可选 MCP 服务）
正是这个功能存在的理由。**不是放宽，是这两条测试终于能看见它们造出来的那件事。**

## 没有做的

- 失败那条路径（必需服务起不来）**不发**这条事件：那时 attempt 还没 start，
  Run 不在 `Running` 也不在 `Suspended`，而放宽状态守卫去发一条信息性事件
  不值得。那种情况 `run.failed` 的负载里本来就有服务名——只是**只有必需的那些**，
  同一次里可选服务的结果丢了。记在这里。
- MCP 设置页仍然只说"配了什么"，没有把最近一次的结果显示在那一页上。
  一个 Run 一次 discovery，设置页要显示就得说清楚是哪个 Run 的哪一次。
