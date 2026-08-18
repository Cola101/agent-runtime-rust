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

### 让它出声之后，机制更清楚了一层

`execute_detached` 在 **durable 接受时就返回**——那是契约，也是客户端可以挂断而 Run
继续跑的原因。代价是后台任务的结果被送进一个接收端通常已经走掉的 channel，
`let _ = send(..)` 把它丢掉。成功时这样是对的（Run 自己的终局才是记录），
失败时不是：Run 停在最后一次持久化的状态上，没有终局事件，而且**没有任何地方说为什么**。

加了一行日志之后（`note_detached_failure`），同一次复现给出了：

```
ERROR a detached execution failed after durable acceptance and left no terminal
      error="local execution was refused: execution attempt is already terminal"
      operation="execute" run_id=01a0160d-…
```

**这句话是猜不出来的。** 它说的是：子代理失败之后，父的**内存中的 attempt 已经是终局**，
而事件日志里一个终局事件都没有。两边脱节了——不是"父在等一个永远不来的结果"，
而是"父这边已经结束了，只是没人把它写下来"。

这也把修法指向了同一处已有的原则。`terminate_provider_failure` 的注释写着：

> 在这里返回传输错误，会让记录终局而事件日志非终局，使每个外部游标都正确地认定
> 这个 Run 已损坏。

Provider 在 `run.started` 之后失败时，这条原则已经被执行了；子代理失败这条路径没有。

### 再追一层：错误是**子 Run** 的，而且子 Run 本身没问题

`execution attempt is already terminal` 来自 `WorkerAssignmentError::AttemptAlreadyTerminal`，
由 `accept`、`record_required_mcp_unavailable`、`apply_model_event`、`apply_steering`、
`bind_cancellation_token` 五处返回。

顺序是这样的：

1. 子 Run 预算耗尽，**正确地**写下 `run.failed{budget_exhausted}` ——
   它的事件日志里就是这条，而且是最后一条。**子 Run 的终局是对的。**
2. 子的驱动循环**没有停**，又调了一次 `apply_model_event`，被拒。
3. 这个 `Err` 逃出 `execute_subagent`（`return Err(error)`）→ 穿过
   `run_subagent_batch` 的 `outcome?` → 穿过父的 `.await?` → 从父的 `execute` 出来。
4. 父既没拿到结果，也没拿到自己的终局。

**坏的不是"子代理失败没有被投递"，而是"子代理在写完自己的终局之后多走了一步，
那一步的错误被当成整条委托链的失败往上抛"。**

这把问题从「一个契约决定」缩小成了一件具体得多的事：驱动循环在自己已经发出终局事件
之后应当返回，而不是再问一次模型。父那边"错误应当成为durable 终局"仍然成立，
但它现在是第二道防线，不是第一处该改的地方。

### 决定性的一次：**这个缺陷和委托无关**

把子代理拿掉，用这一轮加的 `AGENT_RUNTIME_LOCAL_BUDGET_MAX_TOKENS=1` 让一个
**顶层 Run**（没有父）预算耗尽：

```
事件日志: run.started … model.usage  run.failed        ← 终局正确写下了
宿主日志: ERROR error="… execution attempt is already terminal" operation="execute"
```

**同一个错误。** 所以驱动循环在自己发出终局之后多走一步，是**每个** Run 都会做的事，
不是委托特有的。

区别只在于**有没有人在等**：

| | 顶层 Run | 子代理 |
| --- | --- | --- |
| 终局写下了吗 | 是 | 是 |
| 那一步的错误去哪了 | 被丢掉（现在会记日志） | 被 `execute_subagent` 抬成整条委托的失败 |
| 后果 | 无（记录和日志都对） | **父既没结果也没终局** |

所以这不是"委托的缺陷"，是"一个普遍的循环缺陷，只在有父可毁的时候才造成损害"。

### 顺带：我那条诊断的措辞是错的

第一版写的是 `left no terminal`——而顶层这个 Run 明明留下了终局。改掉了，
现在只说"detached execution 在 durable 接受之后失败了"，
至于有没有留下终局，交给 Run 自己的日志说。**一条断言了自己不知道的事的诊断，
比没有诊断更坏。**

### 还没确定的

- 父的时长看门狗（默认 600 秒）会不会最终把它收掉。没等满十分钟，所以说不准是
  「永远挂着」还是「挂十分钟」。两者都不可接受，但严重程度不同。
- 工具报错、被取消、MCP 起不来是不是同一条路径。测了 token 预算这一种，
  以及它在顶层和子代理两种位置上的表现。

**没有动手修。**（只加了诊断。）修它要先决定「子代理失败对父意味着什么」，而 `is_error` 的存在
强烈暗示答案是「作为一个错误结果投递」而不是「抬成宿主错误」——但这是一个契约决定，
不是一处补丁，而且我还没测另外几种失败原因。

## 三、顺带修掉的 stub 缺陷

`stub-provider.mjs` 的 read 分支**不看自己是否已经答过**，同一个提示词永远产出同一次
`workspace.read_text` 调用。这不是模型的样子，也不是模型的合格替身：被它驱动的子代理
只可能以 `budget_exhausted` 结束——**恰好是这次要问的那个问题的错误那一支**。

第一次跑失败那支时我差点把它当成产品缺陷。是「同一条路径、只改结局」的对照
把它分开的：修好 stub 之后成功支立刻走通，失败支照旧不回来。

stub 现在还能按提示词把子代理的预算掐到 1 个 token，用来确定性地造出失败的孩子。
