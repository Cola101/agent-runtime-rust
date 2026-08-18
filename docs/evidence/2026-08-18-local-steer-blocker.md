# 本地 steer 缺口的定位（2026-08-18）

桌面对标清单上唯一一条「被 Runtime 阻塞」的能力。本轮只做定位与设计，**没有落地**，
理由写在最后。

## Steering 已经存在，缺的不是实现

| 部件 | 状态 |
| --- | --- |
| `RunSteeringCommand` / 校验 / 摘要 / 有效窗口 | `crates/protocol` 已有 |
| `record_steering_applied` → `run.steer.applied` | `crates/kernel` 已有 |
| `apply_steering`（幂等、冲突、过期、租约、安全门） | `apps/worker` 已有，**本地 host 持有的正是这个 `WorkerProcessor`** |
| NATS steering subject / consumer / 拒绝原因 | `apps/worker` 已有 |
| `RuntimeControlAction::Steer` | **没有** |
| 本地 host 调用 `apply_steering` | **没有** |

`apply_steering` 的安全门是真的门槛，不是形式：

```rust
if execution.machine.status() != RunStatus::Running
    || execution.pending_approval.is_some()
    || !execution.pending_subagents.is_empty()
    || !execution.pending_tool_calls.is_empty()
    || !execution.outstanding_tool_calls.is_empty()
    || !execution.started_tool_calls.is_empty()
{ return Err(SteeringUnsafe); }
```

本地 host 已经 `processor.accept(command, now)`，`worker_id` 与 `worker_incarnation_id`
都是它自己生成的同一个值，租约在本地恒为有效。所以构造一条本地 `RunSteeringCommand`
并交给它，前七项检查全部成立。

## 为什么不是接一根线就完

`apply_steering` 靠**取消在途模型调用**让 steer 立刻生效：

```rust
execution.cancellation.cancel();
execution.cancellation = CancellationToken::new();
execution.transcript.push(/* 用户消息 */);
```

本地 host 的取消拓扑对不上：

- 绑进 processor 的是 `self.cancellation.child_token()`（`lib.rs:7785`）
- 每次模型调用**在调用点另建一个** `self.cancellation.child_token()`（`lib.rs:4218`）

两者是**兄弟**，不是父子。取消前者不会打断后者。

于是直接接线的结果是：steer 的文本进了 transcript、`run.steer.applied` 也记了，
但当前这次模型调用**跑完为止都不受影响**——一次长回复期间，改向完全不生效。

**这不是可以先上再说的差别。** 把它叫 steer 而它其实是「下一轮才生效」，
正是这一路一直在避免的那种半真话。

## 要做对，需要的四件事

1. **取消拓扑**：host 自己持有 per-attempt token，绑一份进 processor，
   模型调用从**同一个** token 派生。这样 `apply_steering` 取消它就能打断在途调用，
   而 Run 级取消（父 token）仍然照常向下传播。
2. **区分中断原因**：调用被取消后，若父 token 未取消，则是 steer——
   循环应带着更新后的 transcript 继续，而不是走 `terminate_interrupted`。
   `ProviderExecutionError::Cancelled` 当前的映射（`lib.rs:3839`）必须相应分叉。
3. **重新绑定**：`apply_steering` 会把 `execution.cancellation` 换成新 token，
   host 下一次调用必须用新的那个，否则第二次 steer 打不断。
4. **控制面**：`RuntimeControlAction::Steer { steering_id, input }`、
   owner/local socket 变体、桌面 composer 在跑动时可发。

## 验证需要什么

现有 stub provider 整段回复只要 120ms，**打不出「中途改向」这个场景**。
需要一个可延迟的 provider fixture，才能测到：

- 在途调用被 steer 打断，下一轮 transcript 里有那句话
- 有未决工具/审批/子代理时被 `SteeringUnsafe` 拒绝
- 同一 `steering_id` 重发是幂等的；同 id 不同输入是冲突
- Run 级取消仍然是取消，不会被当成 steer

## 结论

这是一次对 Run 循环取消语义的改动——本地 host 最容易出错的地方。
**本轮不落地。** 定位、设计与验证清单在此，下一批按这四步做，
每一步都要能先看到它红。
