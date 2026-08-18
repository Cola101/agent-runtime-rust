# 子代理端到端：通了，但失败的孩子回不来（2026-08-19）

两件事，分开说，因为它们的证据强度不一样。

## 一、成功那一支：**确认可用**

这条一直挂在「没有验证、不声称能用」的名单上——此前观察到的是父 Run 到
`model.turn.completed{reason: tool_calls}` 就停住，子 Run 目录根本没建出来。

现在建出来了，而且整条链路走完。干净 state root + 仓库自己的 stub provider：

| Run | 事件 |
| --- | --- |
| 父 `01a01605` | `subagent.spawn.requested{suspended}` → `subagent.result.received{succeeded}` → `run.succeeded` |
| 子 `05c77cc1` | `run.started` → `approval.required` → `run.resumed` → `tool.execution.started` → `tool.result` → `run.succeeded` |

子 Run 有自己的目录、自己的事件日志、自己的审批与工具执行。父拿到了结果并结束。
**这一支不再是待验证项。**

## 二、失败那一支：**父永远收不到**

同一条代码路径，只改一个变量——子 Run 的结局：

| 子 Run 的结局 | 父 Run |
| --- | --- |
| `run.succeeded` | 收到 `subagent.result.received{succeeded}`，正常结束 |
| `run.failed{budget_exhausted}` | **停在 `subagent.spawn.requested{suspended}`**，没有结果、没有终局事件 |

失败那次等了 35 秒以上，父 Run 的事件日志一个字没加。宿主进程确认活着
（第一次观察时宿主已经死了，那次结论作废，重做过）。

### 代码里的形状

`execute_subagent`（`runtime-host/src/lib.rs`）在子 Run 的驱动返回错误时：

```rust
let child_outcome = match child_outcome {
    Ok(outcome) => outcome,
    Err(error) => {
        child.shutdown().await;
        return Err(error);        // ← 抬成宿主错误
    }
};
```

这个 `Err` 一路 `?` 穿过 `run_subagent_batch` 的 `outcome?`、穿过调用点的 `.await?`，
到达父 Run 的驱动任务。而父 Run 是 `drive_recorded` spawn 出去的——**没有任何东西把这个
错误记成父的终局**，所以父就停在最后一次持久化的状态上。

**这与内核的形状矛盾。** `SubagentResultDelivery` 上有 `is_error: bool`，
`record_subagent_result_received` 会把它写进 `subagent.result.received`，
客户端的 `subagentsOf` 也在读它并渲染成「失败的子代理」。
也就是说：**协议、内核、界面三层都准备好接收一个失败的子代理结果，只有本地 host
把它变成了一个没人接的错误。**

### 还没确定的

- 父的时长看门狗（默认 600 秒）会不会最终把它收掉。没等满十分钟，所以说不准是
  「永远挂着」还是「挂十分钟」。两者都不可接受，但严重程度不同。
- 子 Run 因别的原因失败（工具报错、被取消、MCP 起不来）是不是同一条路径。
  只测了 `budget_exhausted`。

**没有动手修。** 修它要先决定「子代理失败对父意味着什么」，而 `is_error` 的存在
强烈暗示答案是「作为一个错误结果投递」而不是「抬成宿主错误」——但这是一个契约决定，
不是一处补丁，而且我还没测另外几种失败原因。

## 三、顺带修掉的 stub 缺陷

`stub-provider.mjs` 的 read 分支**不看自己是否已经答过**，同一个提示词永远产出同一次
`workspace.read_text` 调用。这不是模型的样子，也不是模型的合格替身：被它驱动的子代理
只可能以 `budget_exhausted` 结束——**恰好是这次要问的那个问题的错误那一支**。

第一次跑失败那支时我差点把它当成产品缺陷。是「同一条路径、只改结局」的对照
把它分开的：修好 stub 之后成功支立刻走通，失败支照旧不回来。

stub 现在还能按提示词把子代理的预算掐到 1 个 token，用来确定性地造出失败的孩子。
