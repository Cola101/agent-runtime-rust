# MCP 接进 Worker 主链

日期：2026-08-08
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 做法：让联邦工具实现 `ToolExecutor`，不在派发路径加特例

`NatsWorker` 执行工具的路径是
`prepare_tool_launch` → 按名字取 `Arc<dyn ToolExecutor>` → `execute()`，
之后的审批、`tool.execution.started`、`tool.result`、检查点、转录全部走同一段代码。

所以联邦工具做成一个 `ToolExecutor`：**上面那一串一行都不用改**。
在派发路径里加 `if name.starts_with("mcp:")` 的分支意味着要把这些逐个重新实现一遍，
并且在其中任何一个改动时开始漂移。

`implementation_digest()` 返回冻结的目录摘要，与描述符里的同一个值——
注册时两者必须相等，Checkpoint 恢复重算时也是它。

## 解掉上一轮留下的张力：执行器注册表是 Worker 级的

上一片把目录冻结做成 Run 级之后，留了一个没解的矛盾：
`tool_executors` 和 `tool_definitions` 都是 Worker 级、按名字为键的。

改成**每个 attempt 一份**，挂在 `ActiveExecution` 上：

- `plan_next_tool_call` 优先用该 Run 的注册表（Worker 原生工具 + 本 Run 的联邦工具），
  没有就退回 Worker 的。用 Worker 的会让每个联邦调用都变成「未知工具」。
- `prepare_tool_launch` **先查该 attempt 的联邦执行器**，再查 Worker 级的。
  顺序是有意的：Worker 级的同名条目不能遮蔽本 Run 自己的。

`ActiveExecution` 有 `#[derive(Debug)]`，而 `dyn ToolExecutor` 没有 `Debug`。
包了一层只打印工具名的 newtype，而不是去掉 derive——
**一个 Run 的诊断输出不该开始取决于某个执行器打印什么**。

## 发现在哪里发生

`accept()` 是同步的，发现要走网络，所以发现放在 `accept()` **之后、问模型之前**：
模型被提供的目录就是这个 Run 冻结的目录。

三条明确的降级行为，都不是致命错误：

| 情况 | 行为 |
| --- | --- |
| Worker 没配联邦客户端，但 Run 带了服务器 | 记 warn，Run 照跑，只是没有那些工具 |
| 某台服务器发现失败 | 逐台记 warn 并跳过，其余照常 |
| 命令里没有 MCP 服务器 | 直接返回，不产生任何网络往返 |

理由是同一条：**一台第三方服务器宕了，不该让一个可能根本用不到它的 Run 失败**；
但也不能悄悄消失，所以逐台记名字和原因。

## 执行器自己的三道判据

| 判据 | 挡住什么 |
| --- | --- |
| `request.sandbox != Federated` → `WrongSandbox` | 请求被路由到错误的执行器 |
| `context` 的 tenant/run 与执行器不符 → `InvalidContext` | 一个 Run 的执行器被另一个 Run 用 |
| 网关任何错误 → `Engine`（**不是重试**） | 联邦工具按定义非幂等；目录变了是**判定**不是抖动 |

第二条今天成立得很平凡（就是同一个 Run），钉住它是为了**它不能悄悄不再成立**。

`exit_code` 用 `i32::from(is_error)`：联邦工具没有进程也就没有退出码，
0 表示调用完成、1 表示服务器自己报告失败，下游区分得开，又没有编造一个没人设置过的状态。

## 故障注入

| 注入 | 结果 |
| --- | --- |
| 执行器忽略冻结摘要（传空串） | `the_worker_executor_path_runs_a_federated_tool` 失败 |
| 去掉 Run 上下文比对 | 同一条失败 |

## 部署接线

联邦客户端复用模型网关的 endpoint 与 mTLS 材料——**是同一个进程**，
再给一个地址就是多一样能配错的东西。连接失败**不致命**：
网关没提供联邦服务的部署仍然应该能跑不用它的 Run。

（`ClientMtlsMaterials` 会被 connect 消费掉，所以单独构造了一份，而不是克隆模型客户端那份。）

## 检查结果

```
Rust（cargo test --workspace）335 通过 / 0 失败
Java（run-java-tests）        167 通过 / 0 失败 / 1 跳过
```

## 写证据时发现主链还断在契约层

这份证据初稿写到「明确不声称」时，我写下「模型还看不到联邦工具」——
写完发现那句话的意思是**主链没接完**，于是继续查。

往下挖了两层：

1. `prepare_model_invocation` 只从 `tool_definitions`（Worker 级）组装工具清单，
   Run 级的联邦定义根本不在候选里。
2. 更深的一层：`effective_skill_state` 会把 Skill 声明的每个工具名对
   `tool_definitions` 查一遍，查不到就**让整个 Run 被拒**。
   联邦工具永远不会安装在 Worker 上，所以一个声明 `mcp:search/web_search` 的 Skill
   会让 Run 直接失败。
3. 最深的一层在**契约里**：`portable_identifier` 禁止 `:` 和 `/`，
   所以 `SkillSnapshot` 压根**无法合法地声明**一个限定名。
   一个 Skill 想用 ADR-0040 提供的能力，快照校验这一关就过不去。

三层都修了：

- 契约新增 `skill_tool_name()`：要么是原生工具的可移植标识符，
  要么是 `mcp:<server>/<tool>` 且**两半各自都是可移植标识符**——
  于是只能有一个 `:` 和一个 `/`，两半都不能空，名字无法解析成另一个服务器。
- `effective_skill_state` 认识联邦名：**命令带了这台服务器**且
  **该服务器的作用域被委派**时才算数，两条都查，所以 Skill 依然只能收窄不能放宽。
- `prepare_model_invocation` 把 Run 级联邦定义并入候选，并对该 Run 的注册表授权。
  三方交集（Skill 声明 ∩ Worker 信任 ∩ 委派作用域）一个字没改。

`attach_federated_tools` 只保留**拿到了执行器**的那些定义：
一个有定义无执行器的工具会被提供给模型然后启动失败，那比不提供更糟。

## 一条负例曾经因为无关原因通过

`a_skill_cannot_declare_a_federated_tool_it_was_not_granted` 最初只断言 `is_err()`。
收紧成「必须是 `ToolConfiguration` 且消息含 federated tool」之后立刻变红——
它当时是因为**快照摘要不匹配**而通过的，和它要检查的东西毫无关系。

一路修下去还撞了三个同类的无关失败：命令发给了别的 worker（`WrongWorker`）、
录制样例的租约早就过期（`LeaseExpired`）、签名密钥 id 对不上（`InvalidSkillArtifact`）。
**每一个都会让只断言 `is_err()` 的负例继续绿着。**

## 明确不声称

- **没有跑过一个真实 Run 的完整回路。** 测试直接驱动 `ToolExecutor::execute`——
  那正是 Worker 循环调用的入口——但**没有**从「模型建议一个联邦工具」开始，
  经审批、执行、结果回到模型上下文、直到终态事件。要证明那条需要一次带真实模型的实跑。
- **没有跑过一次真实模型建议联邦工具的回路。** 模型现在能看到这些工具了（见下），
  但「模型主动选它 → 审批 → 执行 → 结果回上下文 → 终态」这条完整回路
  只有带真实模型的实跑才能证明。
- 没有对真实第三方 MCP 服务器验证过。
- 恢复路径：`restored_from_checkpoint` 分支把联邦状态置空、要求重新发现，
  但**没有用例覆盖恢复后重新发现**。
- SSRF 的 TOCTOU 残余风险仍在（见上一份证据）。
