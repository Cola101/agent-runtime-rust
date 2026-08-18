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

### 但今天这条拓扑是空转的，不是活着的缺陷

追完之后要把话说准：本地 host **从不依赖** processor 的绑定 token 去停任何东西。

- 时长看门狗取消的是 host 自己的根 token（`lib.rs:7314`），模型调用是它的子孙，所以停得住；
  之后才调 `processor.timeout_duration(attempt_id)` **记录**事件。
- `terminate_interrupted` 里的 `processor.cancel(attempt_id)` 同理——循环是先自己发现
  `self.cancellation.is_cancelled()` 才走到那里的，那次调用只负责产出终止事件。

processor 侧有五处会取消绑定 token（`record_required_mcp_unavailable`、`apply_model_event`、
`terminate_uncertain_tool_with_interruption`、`timeout_duration`、`cancel`），
在本地它们全部只用于记录。**所以兄弟拓扑现在无害，它只在 steering 开始依赖它时才变成承重件。**

## 另外两条改变设计的发现

### 一、`apply_steering` 装的是**无父** token —— 不重绑会让取消静默失效

```rust
execution.cancellation.cancel();
execution.cancellation = CancellationToken::new();   // 没有父级
```

本地的 Run 级取消靠父子链向下传播。一次 steer 之后，若 host 不立刻用
`self.cancellation.child_token()` 重绑，这个 attempt 就脱离了那棵树——
**"取消"会从此对这个 Run 失效**，而且只有在有人真去取消一个被 steer 过的 Run 时才会发现。

比原先记的「重绑是为了第二次 steer 打得断」严重得多。`bind_cancellation_token` 允许覆盖
（终止后除外），所以重绑本身是可行的。

### 二、被 steer 打断的模型调用现在会被记成**超时失败**

`ProviderExecutionError::Cancelled` 映射为 `ModelErrorKind::Timeout` +「provider call cancelled」
（`lib.rs:3839`）。不分叉这条映射，一次成功的改向会在事件日志里留下一次模型超时。

## 要做对，需要的四件事（按上面两条修正过）

1. **取消拓扑**：host 自己持有 per-attempt token，绑一份进 processor，
   模型调用从**同一个** token 派生。这样 `apply_steering` 取消它就能打断在途调用，
   而 Run 级取消（父 token）仍然照常向下传播。
2. **区分中断原因**：调用被取消后，若父 token 未取消，则是 steer——
   循环应带着更新后的 transcript 继续，而不是走 `terminate_interrupted`。
   `ProviderExecutionError::Cancelled` 当前的映射（`lib.rs:3839`）必须相应分叉。
3. **重新绑定**：`apply_steering` 换上的是**无父** token。host 必须立刻用
   `self.cancellation.child_token()` 重绑，否则这个 attempt 脱离取消树，
   **Run 级取消会对它静默失效**——这比「第二次 steer 打不断」严重。
4. **控制面**：`RuntimeControlAction::Steer { steering_id, input }`、
   owner/local socket 变体、桌面 composer 在跑动时可发。

## 验证需要什么

fixture **已经有了**：`tests/embedded_control.rs::spawn_cancellable_provider` 接下请求、
发出「已收到」信号，然后**永不回应**——正好把一次调用按在途状态上。
（桌面用的 stub provider 整段回复只要 120ms，打不出这个场景。）

要测到：

- 在途调用被 steer 打断，下一轮 transcript 里有那句话
- 有未决工具/审批/子代理时被 `SteeringUnsafe` 拒绝
- 同一 `steering_id` 重发是幂等的；同 id 不同输入是冲突
- Run 级取消仍然是取消，不会被当成 steer
- **被 steer 过的 Run 仍然取消得掉**（第一条发现直接对应的回归）
- 被 steer 打断的调用**不留下一次模型超时**

## 第四步不是接线，是控制面够不到 host

动手写到第四步才看清：**没有任何外部路径能到达正在跑的 host**。

`ActiveExecution` 持有的是一个 cancellation token，不是 host 的句柄；host 本身活在
`drive_recorded` spawn 出去的那个 task 里，`processor` 是它的 `&mut self`。
Cancel 之所以能工作，是因为它只需要取消一个 token；steering 需要**调用 processor 的方法**。

写完 `steer()` 之后编译器直接说了：`method \`steer\` is never used`。这条警告就是结论。

### 但同时看清了一件让它变简单的事

`apply_steering` 的安全门要求：没有未决审批、没有未决子代理、没有任何在途工具调用。

**循环正在等模型返回的那一刻，这三条按构造全部成立** —— 工具调用是在模型返回之后才发起的。

所以不需要把安全判断搬到控制面（那需要把 processor 的内部状态暴露出去）。正确的形状是：

- `ActiveExecution` 增加一个 steering 信箱（`mpsc` 或 `Mutex<Option<..>>`）
- 模型调用外面套一个 `select!`，**同时**等模型返回和等信箱
- 信箱先到 ⇒ 此刻必然安全 ⇒ 取消这次调用、由循环在 `&mut self` 处调 `apply_steering`
- 循环带着更新后的 transcript 再问一次

安全门仍然由 `apply_steering` 判定（它还要管幂等、冲突、终止态），
`select!` 只是保证调用它的时机是安全的那个时机。

### 本轮落地了什么，没落地什么

**落地**：取消拓扑。host 现在持有 per-attempt token，绑一份进 processor，
模型调用从同一个 token 派生。69 个测试无回归。

这一步独立成立——它让 processor 侧的取消真的能够到在途模型调用，而此前够不到。
但要说准：**现在还没有任何东西依赖它**，所以它是被回归测试保护的，不是被证明有效的。
证明要等 steering 真的用上它。

**没落地**：`steer()`、`Steered` 信号、以及把它接到控制面。写过又撤了——
够不到的代码不提交。

## 结论

这是一次对 Run 循环取消语义的改动——本地 host 最容易出错的地方。
第一步已落地并回归验证；第四步的形状现在是确定的（信箱 + `select!` + 安全时机由构造保证）。
剩下三步一起做，因为它们分开都够不到、也测不了。
