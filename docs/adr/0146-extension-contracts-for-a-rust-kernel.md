# ADR-0146：Rust 内核的扩展方案（插件）

状态：提案
日期：2026-08-19

## 问题

内核是 Rust。openclaw 那样的插件生态怎么办？

## 先把它们的插件读清楚

`~/Documents/Code/agent-source-research/openclaw`：

- 插件入口是 `register: (api: OpenClawPluginApi) => void`
  （`src/plugin-sdk/plugin-entry.ts:332`）。
- `api` 上有约 25 个注册点（`src/plugins/plugin-api.types.ts:96-252`）。
- **加载方式是普通的动态 `import()`**（`src/plugins/*.ts` 全是，没有 vm、没有
  worker_threads、没有沙箱）。插件与宿主同进程、同权限。
- 它文档里的 "capability" 指「注册的是哪一类东西」（channel capability、text
  inference capability，`plugin-api.types.ts:212,259,265`），**不是安全能力**。

**结论一：不能照抄。** 一个能 `import()` 进内核、拿到全部宿主权限的扩展机制，
会一次性作废我们仅有的三项领先（多租户隔离、崩溃恢复、副作用围栏）。
Rust 没有制造这个约束，它只是让这个约束无法被偷偷绕过。

## 25 个注册点其实是 4 份契约

把「注册的是什么」和「怎么送达」分开之后，那 25 个点塌缩成 4 类：

| 契约 | openclaw 的对应注册点 | 我们的现状 |
|---|---|---|
| **Tool** | `registerTool`(:197) | **已有**：MCP |
| **Provider** | `provider-entry` / `provider-auth` / `provider-stream` / `provider-web-search`（`packages/plugin-sdk/src/` 半数文件） | 闭合枚举，要重编内核 |
| **Hook** | `registerHook`(:201)、`registerTextTransforms`(:252)、`registerRuntimeLifecycle`(:164)、`set/getRunContext`(:155,157) | 无 |
| **Surface** | `registerControlUiDescriptor`(:133)、`registerSessionAction`(:131)、`registerCli`(:228)、`registerGatewayMethod`(:221)、`registerHttpRoute`(:206)、`registerChannel`(:213)、`registerSessionCatalog`(:227) | 部分（面自注册，但只对内） |

## 方案

**一个插件永远不在内核里跑。它是一个被内核监督的进程，通过上面 4 份契约之一说话，
它能造成的每一种副作用都已经被现有的 scope + ToolEffect + 审批机器覆盖。**

### 传输：不发明新东西

stdio 上的 JSON-lines，和 MCP、和 `agent-trusted-workspace-tool` 用的是同一套。
理由不是省事：这条路已经被真机验证过（受信任工作区工具就是独立二进制，
`childEnv.cjs` 里指过去的那个），而且它天然给出进程隔离和崩溃隔离。

### 能力：复用既有词汇，不发明第二套

现有 scope 词汇一共 5 个：
`tool:workspace.read`、`tool:workspace.write`、`tool:shell.exec`、
`tool:process.session`、`agent:spawn`。

插件清单**声明**它需要哪些，操作者**授予**其中一个子集——这正是子代理
`delegated_scopes` 已经在做的事（`runtime-host/src/lib.rs`，`SubagentRole`）。
不给插件新的权限概念，只是多一个持有者。

### 副作用：插件提议，操作者裁决

这条已经有先例，直接推广。`LocalMcpServerConfig.tool_effect_overrides`
（`runtime-host/src/lib.rs:1026-1029`）的注释写得很清楚：

> Operator-owned effect declarations keyed by server-local Tool name.
> Missing entries remain `Unknown`; remote MCP annotations are ignored.

**远端声明的 effect 一律不信。** 插件清单里的 effect 是给操作者看的提议，
落进配置的才算数；没落的就是 `Unknown`，而 `Unknown` 走审批。

### 身份与冻结：这是我们能做、openclaw 做不到的一条

每个插件有 id、version、**内容摘要**。摘要进 Run 的冻结策略快照。

后果：一个 Run 是可复现的；插件在飞行中被换掉，也不会改变一个 checkpoint
重放出来的东西。openclaw 的同进程 `import()` 模型拿不到这个性质——这是我们
应该主动拉开的差距，不是要追平的差距。

### 四份契约各自的形状

**1. Tool —— 已经做完了，不要重做**

MCP 就是这一类。工具是插件生态里最大、最多人想写的一类，行业已经在 MCP 上
标准化，而 openclaw 的 `registerTool` 还在进程内——这一条上是它落后。

**2. Provider —— 先动这条，因为它已经在流血**

`ProviderProtocol` 现在是闭合枚举三个变体
（`model-gateway/src/lib.rs:46-52`），加第四种协议要重编内核。
这比插件市场早得多、痛得多：本轮在 `openai_responses.rs` 上刚踩过一次
（`incomplete_details.reason` 没分流）。

改造方向：适配器进程实现一个 `provider.stream` 契约，
逐字段对齐已有的 `ModelStreamEvent`（`TextDelta` / `ReasoningDelta` /
`ToolCall` / `Refusal` / `Usage` / `Completed` / `Failed`）。
内置三种协议保持内建（热路径、且我们要为它们的正确性负责），
第四种起走契约。

**3. Hook —— 唯一真正需要设计的一类**

难点是热路径延迟和确定性。三条规则解决：

- **静态声明，不动态注册。** 挂哪些点、能做什么，写在清单里。
  内核在 Run 开始前就知道完整的 hook 图，能把它冻进 checkpoint。
  动态注册会让重放不可能——而重放是我们的卖点之一。
- **返回决定，不是任意改写。** 枚举：`Continue` / `Replace(payload)` /
  `Reject(reason)` / `Defer`。这保住了确定性，也让「这一步是谁改的」可审计。
- **声明延迟预算，超了就当 `Continue` 并记一条 `hook.timed_out`。**
  这是对「跨进程慢」的正面回答：慢不会变成挂，只会变成一条可见的记录。

**4. Surface —— 只给数据，不给代码**

插件贡献的是**描述符**（命令、面板、状态行条目），桌面端自己画。
永远不接受插件送来的可执行 UI。理由和 markdown 渲染器不用
`dangerouslySetInnerHTML` 是同一条：来路不明的东西不进渲染路径。

## openclaw 现有插件怎么办

**写一个 Node 适配进程。** 它宿主 openclaw 形状的插件，对内核说我们的契约。

这条能成立，是因为上面那张表里三分之二的注册点本来就是 RPC 形状
（`registerTool`、`registerGatewayMethod`、`registerHttpRoute`、`registerChannel`、
`registerSessionCatalog`、`registerAgentEventSubscription`……），跨进程零损失。
真正跨不过去的是 `set/getRunContext` 那种共享可变状态——那部分适配器
按 Hook 契约的 `Replace` 语义映射，映射不了的明确拒绝，不做半吊子兼容。

## 明确不选的

**cdylib / 动态库：排除。** Rust 没有稳定 ABI，得走 C ABI；插件崩溃带走整个
宿主；没有沙箱。一个卖点是「副作用围栏」的东西去装这种扩展机制，是自相矛盾。

**内嵌 JS 运行时：排除。** 那是在 Rust 里重建 Node，把选 Rust 的理由也一起消掉。

**WASM component：不作为地基，留作 Hook 的优化。** 它的 capability 模型和我们的
`delegated_scopes` 是同构的，将来把热路径 hook 搬进去是自然的一步。
但它是同一份契约的另一种传输，不是另一套设计。

## 没有验证的部分（不要当成已知）

- WASM 在本项目的实际开销**一次都没测过**。真要走那一步，第一件事是量
  hook 跨边界的延迟，再决定值不值。
- Hook 的延迟预算该定多少，**没有数据**。要先量一轮真实 turn 里各挂点的耗时分布。
- Node 适配进程能覆盖多少比例的现有 openclaw 插件，**是我按注册点分类估的，
  没有真跑过任何一个插件**。

## 落地顺序

1. Provider 契约（唯一已经在流血的）。
2. 插件身份 + 摘要冻进策略快照（先把可复现性钉住，再谈生态）。
3. Hook 契约（先量延迟）。
4. Surface 描述符。
5. Node 适配进程。
