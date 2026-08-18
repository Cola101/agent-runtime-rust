# Codex app-server 协议对照（读源码，2026-08-18）

- 参考：`codex-app-server` @ `ff352fa`，本地路径 `~/Documents/Code/agent-source-research/codex-app-server`
- 我方：`runtime/apps/runtime-host/src/ipc.rs` 的 `LocalRequest` + `OwnerRequest`

## 为什么单独写这一份

`docs/desktop-ui-gap.md` 是**看截图写的**——拿 Codex 和 Claude Code 的两张桌面截图，
推断它们有什么、我们缺什么。那份清单因此漏掉的不是细节，是整类能力：截图上看不见文件系统 API，
看不见模糊文件搜索，看不见技能与插件，看不见账号与配额。

这一份是读 `codex-rs/app-server-protocol/src/protocol/common.rs:462` 起的
`client_request_definitions!` 宏——**Codex 前端能向它的 Rust 服务发起的全部请求**——
再对照我们 socket 上的全部变体。

两边是同一种形状：GUI ↔ 本机 Rust agent runtime。所以这条对照比任何界面截图都硬。

## 数量

| | 客户端可发起的方法 |
| --- | --- |
| Codex app-server | **132** |
| 我们（workload 11 + owner 12） | **23** |

数量本身不是结论——他们把账号、计费、插件市场、远程配对都放进了同一个协议，
而那些不在我们的范围里。但下面按类别拆开之后，**有几类是我们该有而完全没有的**。

## 我们完全没有的类别

### 一、文件系统 API（8 个方法）

`fs/readFile` `fs/writeFile` `fs/createDirectory` `fs/getMetadata` `fs/readDirectory`
`fs/remove` `fs/copy` `fs/watch` `fs/unwatch`

**客户端**能读写文件，并且能 **watch**。我们的做法完全不同：Electron 宿主自己读磁盘
（`desktop/shell/electron/workspace.cjs`），Runtime 完全不参与。

这是一个真实的架构分歧，不只是缺功能：他们的客户端文件操作**经过 agent runtime**，
因而落在同一套权限与审计里；我们的绕过了它。我们那份 containment 是宿主自己实现的，
`fs/watch` 我们没有等价物——工作区变了，界面不知道。

### 二、模糊文件搜索（4 个方法）

`fuzzyFileSearch` `fuzzyFileSearch/sessionStart` `fuzzyFileSearch/sessionUpdate`
`fuzzyFileSearch/sessionStop`

带**会话**的增量搜索。这是 `@` 引用文件那套交互的底座。我们没有任何等价物，
而 `docs/desktop-ui-gap.md` 里"文件引用"那条只写了"未做"，没说它需要服务端支持。

### 三、会话检索（2 个方法）

`thread/search` `thread/searchOccurrences`

**在服务端搜索历史**。我此刻正让一个子代理做客户端 ⌘F ——那只能搜当前 Run 已加载的事件。
Codex 搜的是整个 thread 存储。这是设计层面的差别，不是实现进度差别。

### 四、模型与配置枚举（6 个方法）

`model/list` `modelProvider/capabilities/read` `permissionProfile/list`
`collaborationMode/list` `experimentalFeature/list` `config/read`

客户端**问服务端**有哪些模型、哪些权限档、哪些协作模式。我们的设置页是一个手写死的三选一
协议下拉（`desktop/shell/src/surfaces/Settings.tsx`），Runtime 从不被问。

### 五、线程管理（10 余个方法）

`thread/name/set` `thread/metadata/update` `thread/archive` `thread/unarchive`
`thread/delete` `thread/goal/set|get|clear` `thread/section/move` `thread/list`
`thread/loaded/list` `thread/turns/list` `thread/items/list` `thread/inject_items`

我们有 `session_list` / `session_read` / `session_history`，**没有命名、没有归档、没有删除、
没有目标（goal）**。会话在我们这里只能创建和继续，攒下来之后没有任何整理手段。

`thread/goal/*` 是一个我们没有的概念：一段对话有一个**持久目标**，与逐轮输入分开。

### 六、终端 / 进程（8 个方法）

`command/exec` `command/exec/write` `command/exec/terminate` `command/exec/resize`
`process/spawn` `process/writeStdin` `process/kill` `process/resizePty`
以及 `thread/backgroundTerminals/list|clean|terminate`

**客户端可以直接开进程并写 stdin**，还有后台终端的清单与回收。我们的 Runtime 有八个
process-session 工具（`runtime/crates/tool-runtime/src/process_session.rs`），但
**只有 agent 能用，客户端够不到**，而且桌面启动器根本没配 `PROCESS_EXECUTABLE`。

### 七、MCP（6 个方法）

`mcpServerStatus/list` `mcpServer/tool/call` `mcpServer/resource/read`
`mcpServer/oauth/login` `config/mcpServer/reload` `mcpServer/startupStatus/updated`（通知）

MCP 在他们那里对客户端**完全打开**：能列状态、能直接调工具、能读资源、能走 OAuth 登录、
能热重载配置。我们：桌面启动器不写 MCP 配置，socket 上也没有任何 MCP 读取面。

### 八、结构化 diff

`gitDiffToRemote`

一个方法就把"改了什么"变成可审阅的东西。我们的工作区面只能从 `model.tool_call` 的参数
推出"代理被要求动过哪些路径"——这是我特意写清楚的诚实措辞，但它确实比 diff 弱得多。

### 九、我们范围之外的（记录但不追）

`account/*`（登录、配额、用量、加信用额度）、`plugin/*` `marketplace/*` `app/*`（插件与市场）、
`remoteControl/*`（配对另一台设备）、`thread/realtime/*`（语音）、`windowsSandbox/*`、
`review/start`、`feedback/upload`、`environment/*`。

这些要么依赖厂商后端，要么是被明确排除的方向（Windows、云）。

## 我们有而他们没有的

不是没有。读下来这几条是我们更强的：

- **两个 scope 的信封**。`OwnerRequest` 与 `LocalRequest` 分开，且**任一方向都没有回退**
  （`ipc.rs` 的 `parse_wire_request`）。Codex 的 132 个方法在同一个平面上。
- **审批绑定摘要**。我们的审批决定绑在一次具体调用的 `binding_digest` 上，
  决定无法被套用到另一次调用。Codex 的 `item/commandExecution/requestApproval` 是否有等价物，
  我还没读到——**这条待证**，不是已证。
- **generation 围栏与 turn ordinal**。`session_continue` 必须带 generation，
  rollback 移动它；一个持有过期 generation 的客户端会被拒绝而不是覆盖历史。
- **事件日志是唯一权威**，生命周期边界只来自 cursor 的类型化状态，客户端从不从事件推断结束。

## 这份对照改变了什么

1. `docs/desktop-ui-gap.md` 需要重写。它按"界面上看得见的东西"分类，
   而真正的差距有一半在**协议层**：客户端能不能问服务端要东西。
2. 我正在并行做的 ⌘F 搜索，做出来也只是客户端过滤，**不等于 `thread/search`**。
   做完要说清楚它是什么。
3. 优先级应该重排。会话命名/归档、模型枚举、MCP 状态读取——这三样都是
   "Runtime 侧要开一个读面"，而不是界面工作。

## 服务端通知：**71 条**，以及一处比数量更深的分歧

`common.rs:1642` 的 `server_notification_definitions!`。读完之后，最重要的发现不是缺哪几条，
而是**两边给客户端的东西根本不是同一类**。

### 他们发的是 item，我们发的是日志

Codex 的通知围绕 **item** 组织：`item/started`、`item/completed`，以及按类型分的增量——
`item/agentMessage/delta`、`item/reasoning/textDelta`、`item/reasoning/summaryTextDelta`、
`item/commandExecution/outputDelta`、`item/fileChange/patchUpdated`、`item/mcpToolCall/progress`、
`item/plan/delta`。客户端拿到的是**已经分好类、带生命周期的渲染单元**。

我们发的是内核事件日志：`model.output.delta`、`model.tool_call`、`approval.required`……
客户端自己去判断哪些该显示、哪些该折叠、哪些是记账。
`desktop/shell/src/surfaces/model.ts` 里的 `EVENT_NOTE`、`belongsInConversation`，
以及 `Chat.tsx` 里连续工具调用的折叠逻辑——**这些全是因为 Runtime 不告诉客户端"这是一个什么"**。

这解释了我这一路踩的坑：转录会静默丢掉不认识的事件类型、生命周期记账和对话内容混在一列里、
提交前后同一段对话用两套渲染器。**这些不是我实现得差，是协议少了一层。**

哪种更好不是显然的。我们的日志是可回放、可校验、带 digest 的durable 事实；
他们的 item 是为渲染服务的投影。但**他们两者都有**——`rawResponseItem/completed` 与
`rawResponse/completed` 说明原始层也在，item 是叠在上面的一层。

### 我们完全没有的通知

| 通知 | 是什么 |
| --- | --- |
| `turn/diff/updated` | **结构化 diff，逐步推送**。我们只能从工具调用参数推路径 |
| `turn/plan/updated` `item/plan/delta` | **计划 / 待办清单**，流式更新。Codex 右栏那块 |
| `item/reasoning/textDelta` `summaryTextDelta` `summaryPartAdded` | **思考过程流**。我们的内核有 `model.reasoning` 事件，但没有摘要分段 |
| `thread/tokenUsage/updated` | 用量作为独立通知，而不是埋在事件里 |
| `command/exec/outputDelta` `process/outputDelta` `process/exited` | 终端输出流 |
| `item/fileChange/patchUpdated` | 文件改动的补丁 |
| `fs/changed` | 文件系统变化 |
| `item/autoApprovalReview/started|completed` | 自动批准的**复核**过程可见 |
| `thread/compacted` | 压缩发生了（我们有 `context.compacted` 事件，但客户端丢掉了） |
| `model/rerouted` `model/verification` `model/safetyBuffering/updated` | 模型被改路由、验证、安全缓冲 |
| `warning` `guardianWarning` `deprecationNotice` `configWarning` | **分级的警告通道**。我们只有错误或没有 |

### 我们有而他们不发的

- 每条事件带 **digest**，且事件日志本身是持久权威；他们的通知是投影，`rawResponse/*` 才是原始层。
- `run.indeterminate`——结果无法判定是一个**一等终止状态**，需要人裁决。
  他们的 `turn/completed` 没有对应物（待证：可能在 turn 的 status 字段里）。

## 一处我们明确更强的：漏了一条，客户端知不知道

自己读过并确认（`common.rs:1749-1764`）：

```rust
pub struct ServerNotificationEnvelope {
    #[serde(flatten)]
    pub notification: ServerNotification,
    pub emitted_at_ms: Option<i64>,   // 全部
}
```

**信封里只有一个时间戳。** 71 条通知，没有序号、没有游标、没有缺口标记。
一个 Codex 客户端如果因为断连或消费太慢漏掉一条通知，**它无从知道自己漏了**。

我们的（`embedded.rs:86-95`）：

```rust
pub struct RuntimeEventCursorPage {
    pub requested_after_sequence: u64,
    pub next_after_sequence: u64,
    pub earliest_available_sequence: Option<u64>,
    pub highest_committed_sequence: u64,
    pub history_gap: bool,
    ...
}
```

每条事件有 `sequence` 和 `digest`；页里带最早可读序号、已提交最高序号、以及**历史是否有缺口**。
界面上就是那句「更早的事件已被回收，这段转录不完整 —— 最早还能读到第 N 条」。

这不是小差别。它决定了「转录是完整的」这句话能不能被证明。

## 服务端反向请求客户端：9 个方法

`common.rs:1487` 起（自己读过）：

```
item/commandExecution/requestApproval   item/fileChange/requestApproval
item/tool/requestUserInput              mcpServer/elicitation/request
item/permissions/requestApproval        item/tool/call
account/chatgptAuthTokens/refresh       attestation/generate
currentTime/read
```

两点值得注意：

**`item/tool/call`** —— 服务端调用**客户端实现的工具**。客户端不只是显示器，它可以是能力提供方。
我们完全没有这个方向；`ResolveMcpInput` 是我们最接近的东西，但那是客户端**回答**一个请求，
不是客户端**提供**一个工具。

**审批是一个阻塞的服务端请求**，等客户端的类型化回答。我们的审批是
「事件 + 控制命令 + 持久收据」。子代理查到他们的 pending 映射是进程内的、
请求 id 由进程全局 `AtomicI64` 生成（`app-server/src/outgoing_message.rs`）——
**这意味着 app-server 进程一死，在途审批无法重建**。我们的控制收据是落盘的，
重启后 `list_control_receipts` 还在。这条我只核实了他们的 id 是进程全局，
**「无法重建」是子代理的推论，我没有亲自验证**，标在这里等复核。

## 下一步该做什么（按这份对照，不按截图）

1. **item 层**。这是最大的一处结构差距。要么在 Runtime 侧加一层 item 投影，
   要么承认客户端要一直做这件事——但要**明确选一个**，现在是默认了后者却没写下来。
2. `turn/plan/updated` 与 `turn/diff/updated` 是两条能立刻提升可用性的通知，
   而且都需要 Runtime 先有对应概念。
3. 分级警告通道：我们现在只有"错误"和"没有"。

## 没读的

`app-server` 本体的实现（只读了协议定义与通知定义）、`server_request_definitions!`
（服务端反向请求客户端的那组）、OpenClaw 的对应面。
