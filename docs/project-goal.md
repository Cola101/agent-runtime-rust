# 项目目标与执行边界

## 唯一目标

持续对标 Codex 的 Agent 执行内核与 OpenClaw 的节点协同能力，交付一款**多租户、
协议中立、可嵌入且可独立运行的 Rust Agent Runtime**。同一内核应支持云、边、端一致运行和
受控协作，能被各类 Java 业务系统快速集成，也能形成独立 CLI、桌面 GUI、边缘节点和云端
Runtime 服务。

Runtime 必须保持 Headless（无头）与控制面中立：它自己拥有 Agent Loop、模型 IR 与调度、
Tool/MCP、审批、Checkpoint、故障恢复、子代理、权限、预算和租户运行隔离语义；任何 GUI、
Java 系统或控制面都只能通过稳定契约使用它，不得成为完成一次 Run 的必需依赖。

## 对标与产品边界

- **Codex 对标面**：Agent Loop、模型与 Tool 多轮调度、流式事件、审批与权限、上下文压缩、
  会话生命周期、沙箱语义和子代理执行。
- **OpenClaw 对标面**：Node 协议、设备能力发现、连接生命周期、模型容灾、离线任务、Workspace 协调
  和跨平台运行。
- **本项目必须增强的边界**：租户身份、模型凭证与策略隔离、配额和预算、租户间公平准入、
  可审计恢复，以及不依赖单一控制面的标准适配能力。

## 控制面与 Java 集成边界

- 客户自有控制面与平台提供的 Java 控制面使用同一契约接入 Runtime，两者在运行时地位对等。
- 标准控制面适配契约至少覆盖租户与工作负载身份、Agent/Skill/Workspace 快照、审批、预算、
  调度与审计，不向 Runtime 泄漏某个 Java 系统的数据库模型或内部类型。
- 控制面管理的模型必须转换为协议中立的不可变运行快照，至少包含 Provider/Endpoint 能力、租户级
  凭证引用或 BYOK、地域与数据等级、路由/故障转移策略、价格与预算限制；Runtime 不直接依赖
  控制面的模型表结构。
- 平台自建的 Java 控制面是这套契约的完整参考实现和集成范本，不是 Rust Runtime 的唯一官方入口或
  强制依赖。
- Java 业务系统优先通过 Java SDK + gRPC/HTTP/SSE 或 Sidecar 嵌入 Runtime；进程内 JNI 只是可选形态，
  不作为唯一公共契约。

## 云、边、端与客户端形态

- 云端 Runtime、边缘 Node、端侧/本地 Runtime、CLI、独立桌面 GUI 与 Java 嵌入形态共用同一内核语义和
  版本化契约；GUI 是 Runtime 客户端，不拥有另一套 Agent 状态机。
- “无缝协作”指身份、策略、Skill、事件、Checkpoint、Artifact 和 Workspace 分支协议一致，可以进行
  放置、暂停、恢复和受控迁移。
- 无缝协作不代表任意进程或已开始的外部副作用都能安全迁移；非幂等/Unknown Tool 的模糊结果必须
  进入 `indeterminate` 或人工协调，离线 Workspace 冲突不得静默覆盖。

## 当前实施阶段

当前仍以 Rust 内核完善为唯一实施主线，不因最终产品目标扩大就立即扩展 GUI、Java 控制面、云边端节点或
集群部署实现。当前阶段只完善 Agent Loop、模型 IR 与调度、Tool/MCP、审批、Checkpoint、故障恢复、
子代理、权限、预算、租户运行隔离与容量治理；每项能力必须在 M1 Pro 16GB Mac 上以真实任务闭环验证。

Runtime 不得依赖 Docker、虚拟机、Kubernetes、Java、PostgreSQL 或 NATS 才能启动和完成一次 Run。
GUI、控制面、节点协议和部署编排可以驱动必要的稳定公共契约，但不得掩盖内核缺口或迫使内核依赖
某个上层实现。

## 固定实施顺序

1. **Agent Loop 与模型调度**：多轮模型/Tool 循环、协议中立 IR、流式事件、上下文与安全故障转移。
2. **Tool/MCP 执行内核**：目录、权限、审批、副作用分类、取消、超时、进程树回收与协议生命周期。
3. **持久恢复**：Checkpoint、不可变执行身份、重启恢复、重复/乱序输入和模糊副作用处理。
4. **子代理与治理**：角色、深度/并发、权限与预算子集、父子结果回送、取消和恢复。
5. **多租户容量与可观测性**：租户运行隔离、配额与预算、公平准入、背压、资源上限、健康和故障注入，
   并以 1000 个并发 Run 证明不同租户间不串数据、不串凭证，也不会因单一租户占满资源而饿饿。

前一项没有真实闭环证据时，不用 GUI、控制面或部署编排把它包装成“已完成”。

## 当前里程碑

ADR-0142 已完成 Desktop-Ready 的第一道门禁：未来 Tauri 命令层、Electron sidecar、CLI 和 Java
adapter 均使用同一 `RuntimeClient v1`，并在执行前完成版本/能力协商。ADR-0143 接着收口了 Session
契约的语义：创建、继续、分支、回滚、查询与恢复在进程内与 gRPC 上语义一致，被拒绝的 Turn 不留下
不可继续的分支，终态 head 投影以 Checkpoint 为权威。该里程不代表已有桌面客户端：Profile 动态
生命周期、应用关闭/恢复语义、本地凭证解析与可分发 artifact 仍是内核准备条件，不进入 GUI 开发。

独立 Rust Host 已完成角色子代理串行与最多 8 路有界并发闭环、嵌套审批、父子取消、Checkpoint 恢复、
权限与预算子集约束，以及可恢复的父子树执行时限。持久异步生命周期的**第一阶段**也已落地：显式
`agent.spawn(mode=async)` 在启动子任务前持久化并立即返回稳定句柄；`agent.wait` 可超时而不取消；
`agent.send` 可在前一子回合终态后创建同一句柄下的后续 Run；`agent.close` 持久化不可逆关闭边，父终态
会回收未关闭子任务；新 Host 可从父子 Checkpoint 恢复同一句柄。旧 `inline` 模式继续兼容。

持久消息投递与运行中邮箱阶段也已完成：`agent.send` 要求调用方幂等键；schema 15 在 schema 14 收据上
增加状态与逐句柄有界队列。普通消息在活动子回合后按 FIFO 激活；`interrupt=true` 先持久化收据和 Tool
结果，再取消旧子 Run、结算实际用量并启动重定向消息。替换 Host 会先收敛待处理中断，再恢复普通活动
请求；schema 14 可迁移，损坏的 schema 15 队列 fail-closed。

句柄级对话历史阶段现已完成：RunExecution schema 12 以协议中立结构携带已完成问答轮次，Worker 将其
还原为低权限 user/assistant 消息而非 system prompt；Checkpoint schema 16 按实际激活顺序保存历史及
摘要绑定，interrupt 越过 FIFO 后仍保持真实时间线。`agent.history` 提供最多 50 条的只读游标分页；
真实 HTTP 多轮、损坏状态拒绝和 Host 崩溃恢复均已通过。旧 schema 只能迁移可验证尾部，不伪造历史。

协议中立上下文压缩阶段现已完成：RunExecution schema 13 / runtime-policy schema 3 冻结触发、保留和
摘要预算；Checkpoint schema 17 持久化待处理/已应用压缩及来源、前缀、保留尾部摘要。摘要通过同一模型
IR 请求但不暴露 Tool，以普通 user 消息回灌，原 system 指令和最近完整 Tool Call/Result 保持不变。
真实 HTTP/SSE + MCP 已验证正常四回合、`context.compacted`、503 后替代 Host 使用同一边界恢复且 Tool
不重放。该能力默认关闭，不用主机隐式阈值改变同一 Run 的语义。

完整 transcript 胶囊与终态恢复阶段现已完成：RunExecution schema 14 让稳定子代理句柄继承子 Run 的
assistant narrative、Tool Call、绑定 Tool Result 和终态 Assistant；结果 digest v3 绑定完整协议中立
transcript，缺失、孤立或重复 Tool 对 fail-closed。Worker Checkpoint schema 18 保存同一 typed transcript；
独立 Host 在发布子 Run 终态事件前先写终态 Checkpoint，因此父 Host 在结果收据落盘前崩溃后，替代 Host
仍可从子 Checkpoint 恢复精确历史，且不会重放已完成 Tool。旧无 transcript 收据保持可读，但不会伪装为
schema 14 保证。

显式历史修复阶段现已完成：RunExecution schema 15 新增独立的低权限 `history_import`，只在调用方明确
选择 external/truncated 导入时运行协议中立修复。缺失 Tool Result 会成为模型可见的合成错误，孤立或
重复 Result 被丢弃，可唯一归属的错位 Result 移回对应 Call；System 注入、非法角色内容和重复 Call ID
直接拒绝。修复前后摘要及四类计数写入 Worker Checkpoint schema 19 和本地 Run 结果；替代 Host 必须提交
同一原始导入才能恢复。历史 Tool Call 从不进入执行队列，权威 Checkpoint 也不会隐式调用修复器。

会话分支的 **Fork 阶段现已完成**：`agent.fork` 必须绑定调用方看到的 source generation 和一个已完成
activation ordinal，创建 deterministic 新句柄与 generation 1。只复制协议中立的完成历史前缀，不复制
active child、邮箱、收据、close 或进程状态；角色不可改变，预算 cap 不得超过源 cap 和父剩余预算。
Checkpoint schema 20 保存 generation 与 Fork 收据，替代 Worker 在 Tool 结果写入前恢复同一 handle/event。
真实 HTTP + 原生 `workspace.read_text` 证明源 Tool 只执行一次，Fork 上下文保留完整 Tool 对但不重放；
源历史和分支历史随后独立追加。

会话分支的 **Rollback 阶段现已完成**：`agent.rollback` 在同一稳定句柄上将 completed prefix 提升为
generation N+1，不删除旧 generation。Checkpoint schema 21 以唯一 archived Turn + generation ordinal head
保存旧版本并校验摘要，避免复制每份完整历史；旧 generation 命令和迟到结果被 binding fence 拒绝。
真实 HTTP + 原生 `workspace.read_text` 已证明 generation 1 `[0,1]` 回滚为 generation 2 `[0]`，后续变为
`[0,2]`，旧历史仍可读且 Tool 总计只执行一次。替代 Worker 在 Tool 结果前恢复同一 receipt/event。

树级预算保留账本阶段现已完成：所有 pending/active/queued 子任务进入同一父 Run 预留域；不同 handle
不能重复使用 Token、费用或时长余额。Checkpoint schema 22 独立保存并从权威待处理工作重算账本，缺失、
额外或篡改记录 fail-closed；result、close、cancel 和终态释放精确预留。真实 HTTP Host 已证明两个稳定
handle 的后续子 Run 分别获得 `400` 与剩余 `300` Token，而非各取 `400`，终态账本归零。

root Thread/Session 的不可变分支与回滚现已完成：RunExecution schema 16 以独立的权威
`session_branch` 绑定稳定 Session、分支、generation、完成 Turn 与历史摘要；Checkpoint schema 23 绑定
同一 head。独立 Host 先持久化 active Turn，再执行 Run；Fork 复制完成前缀到独立 branch，Rollback 归档
旧 generation 后只移动当前 head。活动 Turn、过期 generation、漂移 Checkpoint 与迟到终态提交均被围栏。
终态 transcript Checkpoint 先于终态事件持久化，因此 Session head 提交前崩溃也无需重放模型或 Tool。
真实 HTTP/SSE + MCP 已证明 source/Fork/Rollback 共用的历史 Tool 只执行一次，Provider 503 和终态提交窄窗
均可由替代 Host 恢复。

协议中立的独立 Host 多 Provider 调度与安全故障转移里程碑现已完成：Rust Host 不依赖 Java、NATS 或
外部 Model Gateway 进程，使用同一模型 IR 驱动 OpenAI Responses、Anthropic Messages 与
OpenAI-compatible 三类候选；按地域、数据等级、能力、健康和费用过滤，再冻结最多 8 个候选。只有模型
尚未产生任何增量、Tool Call 或其他外部语义且错误分类在 Run 策略允许范围内时才切换；半途断流保留
已产生内容并禁止跨 Provider 重放。普通 Agent 回合和上下文摘要请求共用同一路由路径。

每次调用的候选链、游标、失败摘要、暂存响应和最终选择进入原子持久 route journal；对应失败/选择事件
先进入 Kernel 事件序列和 Worker Checkpoint，再推进 journal。替代 Host 可从已知游标继续，也可直接应用
已暂存的终态响应而不再次请求 Provider。二进制可读取密钥不落盘的多 Provider JSON，旧单 Provider 环境
变量继续兼容。真实回环 HTTP/SSE 已覆盖三协议连续切换、能力/地域/费用过滤、半途断流、两类崩溃恢复和
摘要请求故障转移；未使用 Docker、Java、PostgreSQL、NATS 或外部 API Key。

持久 Provider 健康、重试与冷却治理现已完成：同 Provider 仅在零输出且策略允许时按持久次数预算重试，
退避与 `Retry-After` 截止时间写入 route journal；连续的 rate-limit、timeout、unavailable 会打开原子健康
状态，冷却到期后只租出一个 half-open 探针。替代 Host 保留候选链、尝试次数、inflight 模糊请求与冷却
状态，不会因重启无限重放。认证和账单错误不切换 Provider，也不进入共享健康计数。真实回环 HTTP 已覆盖
同 Provider 恢复、持久冷却、`Retry-After`、并发单探针、认证隔离和进程中断预算；未使用外部服务。

协议中立 Rich Model Item 与推理状态连续性现已完成：OpenAI Responses reasoning summary、加密 continuation
和 refusal，以及 Anthropic thinking/signature，均进入带 Provider route、协议、模型和格式来源绑定的 typed
transcript item。相同来源可穿过 gRPC、Tool 回合、Checkpoint、compaction 保留尾部、Host replacement 及
Session Continue/Fork/Rollback；不匹配来源只审计丢弃 opaque state，且该观察不会误阻断安全 fallback。
private state 不进入公共事件或可见文本，typed refusal 也不会再形成空成功结果。真实回环 HTTP/SSE 与 gRPC
已证明上述边界；真实厂商兼容、字段级 Checkpoint 加密和更多 Provider content item 仍单列为未验证。

有界并行 Tool 执行与确定性结果提交阶段现已完成：RunExecution schema 17 / runtime-policy schema 4
冻结最多 1–16 路、默认 4 路并发；只有相邻 `Pure` Tool 可重叠，规划、权限和审批仍串行。Worker
Checkpoint schema 24 保存原始调用顺序、未完成请求和乱序暂存结果；所有 started 事件先落盘再启动进程，
替代 Host 只重试未完成 Pure 调用。真实 HTTP/SSE 模型与真实子进程已证明执行区间重叠、后完成的首调用
仍先进入 transcript，以及半批崩溃后恢复不乱序、不重放已暂存结果。NATS 适配已共享有序提交状态，但本轮
未启动外部消息服务，不把它计作分布式验收。

模糊副作用的显式 `indeterminate` 收敛与人工 reconciliation 现已完成：`non_idempotent` / `unknown`
Tool 在“started 已持久、结果未知”窗口会形成绑定调用、实现、原 attempt 与 started event 的稳定终态，
且明确标记 `replay_safe=false`。原 Run 保持不可变；“已生效 / 未生效 / 无法判定”使用幂等、连续版本的
协议命令记录，只有前两者会把操作员证据作为低权限 Tool Result 带入一个新的确定性 Run。真实子进程、
Host 中断、替代 Host 和两类人工裁决均已证明旧副作用不会重放。

协议中立的持久 Tool Process Session 第一阶段现已完成：显式可信可执行文件通过
`process.start/write/poll/interrupt/close` 暴露，稳定 UUID、tenant/Workspace/实现摘要绑定、原子 Manifest、
stdout/stderr 字节游标和跨进程 identity lock 共同形成所有权证据。真实 owner 进程直接退出后替代 manager
返回 `reattached` 并继续原 PID；独立 Host 在 start 结果落入 Checkpoint 后替换，也不会重放 start。显式
close、SIGINT 和自然 leader 退出均覆盖整个进程组，TERM 后会升级 KILL；64 槽只统计活跃会话。

持久 Process Session 的资源治理与可恢复孤儿监管现已完成：schema 2 将治理摘要、绝对执行 deadline、
idle TTL、最后活动、CPU/输出及可选内存上限和终止原因写入 digest Manifest；跨进程准入分别约束全局、
tenant 与 `(tenant, canonical Workspace)`，容量满只拒绝新会话，不淘汰其他租户的 live process。原 Host
正常存活时按最近 deadline 动态监督；owner 进程直接退出后，替代 manager/Host 会按原 deadline sweep 同一
PID。真实 Agent Loop 已让模型看到 `execution_deadline` 并完成后续回合。macOS 只验证 CPU 与文件大小硬
限制，显式内存限制会被拒绝；Linux `RLIMIT_AS` 尚未做本机 live 证明，不能声称跨平台资源隔离完成。

Linux 硬资源边界的第一阶段现已完成：Runtime 公开不可变的 process resource capability，区分 hard
output-file、CPU-time、memory、process-count 和 whole-tree accounting；`max_processes` 与整树计量成为
operator governance，进入 Tool 实现摘要。在当前 macOS `UnixRlimit` 后端上，memory、process-count 和
whole-tree 均明确为 false，任何对应要求都会在 state-root、Provider 和 child 创建前 fail-closed。它只
证明契约不说谎，不等于 Linux cgroup 已实现。

Linux cgroup v2 的文件协议边界现已完成：配置显式区分 rlimit/cgroup，五个限制文件只打开既有普通文件、
拒绝最终路径符号链接，membership 使用 `0`，`cpu.stat` 只接受唯一 `usage_usec`。非 Linux 会在状态创建前
拒绝；Linux 也保持 `backend_not_wired`，所以这仍不是可用后端。普通文件测试只证明字节和路径协议，
不证明 cgroupfs、delegation 或真实进程树限制。

子代理崩溃 socket 风险现已闭环：旧测试依赖销毁整个 Tokio Runtime，掩盖了 Host 被 abort/drop 时
JoinHandle 脱离且 CancellationToken 不自动取消的缺陷。Host 现在拥有 caller token 的独立 child domain；
正常 shutdown 先取消再等待，异常 Drop 会取消并 abort 全部登记子任务。真实 loopback Provider 在 Runtime
继续存活时确认 parent/child 两条 TCP 连接均关闭，replacement 恢复同一 handle 且不重放 spawn。

cgroup 生命周期的持久身份与无逃逸启动边界现已完成：Process Session Manifest schema 3 保存摘要保护的
resource identity 与预留 aggregate CPU 计数；schema 2 只能迁移为历史上真实存在的 Unix rlimit 身份。
父进程预先安全打开 `cgroup.procs`，真实 child 在 exec 前通过 `write(2)` 写入 `0`；创建失败撤销空组，
既有组一律拒绝接管。该测试使用普通文件证明 pre-exec 行为，不是 Linux cgroupfs 证明，生产 backend
继续返回 `backend_not_wired`。

cgroup 身份驱动的监管与终止阶段现已完成：每次 sweep 会从 schema 3 identity 对应的 `cpu.stat` 读取单调
`usage_usec` 并持久化，达到预算时形成明确 `cpu_limit`；`cgroup.events: populated` 成为 Linux 身份存活信号，
整组终止写 `cgroup.kill=1` 并等待 populated 清零及 identity lease 释放。Manager 的 start supervisor、
interact、recover 和 sweep 均携带冻结 backend，backend 与 Manifest 身份不一致会拒绝访问。普通文件测试
只证明协议与治理决策，生产 backend 仍返回 `backend_not_wired`。

cgroup 的 fd-relative 生命周期边界现已完成：delegated root 与 session group 都持有目录 descriptor；创建、
失败回滚、五项限制、pre-exec membership、CPU/存活观测和整组终止全部通过 `mkdirat/openat/unlinkat` 相对
访问。根路径或组路径被 rename/replacement 后，原操作仍固定在已打开目录，replacement sentinel 不会被读写；
旧 PathBuf 生命周期入口已删除。该结论仍只限单次已打开操作，Manager 生命周期级 root pinning、终态空组
清理和真实 Linux cgroupfs 仍未证明，生产 backend 继续禁用。

cgroup 的 Manager 生命周期身份现已固定：公开配置与运行期 backend 分离，Manager 创建时只打开一次
delegated-root descriptor，并以 `Arc` 传播给 watcher、supervisor 与所有前台操作。Manager 创建后即使原路径
被 rename 并替换，后续 sweep 仍读取原 root 的 `usage_usec=2000000`，不会读取 replacement 的 `7`。
该结果仍是 macOS 普通文件协议证据；生产 Linux backend 继续 fail-closed。

cgroup 的启动与终态生命周期现已接线：`process.start` 在 child spawn 前通过 Manager root 创建/配置确定性
组并安装 pre-exec membership；准备、membership 或 spawn 失败会相对同一 root 回滚空组。终态先持久化，
随后执行 fd-relative、幂等空组移除；替代 Host 的 terminal sweep 会重试，配置路径被替换也不会逃逸。
本机 GREEN 路径因没有 cgroupfs 在 spawn 前 fail-closed，不能替代真实 Linux 验证。

cgroup 的持久资源阶段与 `Starting` 崩溃恢复现已升级到 Process Session Manifest schema 5：Unix 与 Linux
都必须在 spawn 前独立持久化 `prepared`，再发布 `Running/active`。schema 2/3/4 的旧 `Starting` 全部迁移为
`legacy_unknown`；prepared/legacy 状态即使资源组为空或缺失，也只能收敛为 `Indeterminate`，因为快速 Tool
可能已经执行并退出。只有当前 schema 明确的 `unprepared` 且身份缺失才能成为 `RecoveredMissing`。active
schema 2 真实进程可由替代 Manager 重附着原 PID 并原子升级，不会重启 Tool。本机普通目录证据不等于
真实 cgroupfs，生产 backend 继续 fail-closed。

同步启动失败现已升级到 Process Session Manifest schema 6：`Command::spawn` 返回错误后，Manager 先持久化
`Terminated/start_failed` 和失败 Session ID，再清理资源；Linux 清理失败保留 `cleanup_pending` 供替代 Host
重试。活动 schema-5 Unix 进程经摘要校验后可重附着原 PID 并重写 schema 6。

确定性启动失败的端到端类型传播现已完成：`ProcessSessionToolExecutor` 保留失败 Session ID 与私有 OS
原因，Worker 事件与独立 Host 则只公开稳定的 `process_session_start_failed`、安全消息和 Session ID。只有
能证明未越过外部副作用边界的错误可转换为模型可见 Tool Result；其他非幂等模糊错误继续走恢复或人工
reconciliation。真实同步 OS spawn 失败、Worker 分类，以及 loopback HTTP 模型→Tool→错误结果→模型与
持久事件日志均已验证；模型不可见私有 OS 原因。

在线 Tool 执行失败的 effect-aware 收敛现已完成：Worker 与独立 Host 在持久 `started` 边界后同时读取冻结
的 `ToolEffect`。确定性未执行失败以及 `Pure/Idempotent` 的其他失败可形成脱敏错误 Tool Result；
`NonIdempotent/Unknown` 的未分类失败直接形成绑定的 `run.indeterminate`，不进入下一模型回合。真实
loopback HTTP Agent Loop 已执行一次文件副作用后收到执行器错误，持久化 indeterminate Checkpoint，再由
操作员确认已生效并启动独立 continuation；原 Tool 没有重放，私有错误没有进入事件或模型上下文。

MCP 接受调用后的响应丢失也已完成真实闭环：Streamable HTTP MCP 服务执行一次副作用后截断响应，独立
Host 形成 `run.indeterminate`、不伪造 `tool.result`、不启动下一模型回合。stdio actor 已接收
`tools/call` 后丢失响应时不再建立新进程重发；只有发送失败且请求根本未入队，或 Health/`tools/list`
这类安全操作，才允许重连。

MCP Tool 的操作员权威 effect 已由 RunExecution v18 冻结：本地配置或受信执行命令可按 server-local Tool
名称声明 `Pure/Idempotent/NonIdempotent`，缺省仍是 `Unknown`。覆盖项必须属于已签名 Skill 声明和已委派
server scope；所有 MCP Tool 继续 `Ask + Federated`，第三方 `readOnlyHint/idempotentHint` 不能降低审批或
改变重放语义。effect map 进入 MCP server binding digest，替代 Host 遇到策略漂移会在模型前拒绝恢复。
真实 HTTP Agent Loop 已证明：伪装为只读/幂等的远端 annotation 在无 override 时仍 indeterminate；显式
`Idempotent` override 时产生脱敏错误 Tool Result、继续模型并成功终止，两条路径都只调用 MCP 一次。

取消/超时与已开始副作用的双重终态证据现已完成：Worker 在终止前检查持久
`tool.execution.started` 与冻结 `ToolEffect`。已开始的 `NonIdempotent/Unknown` Tool 不再被
`cancelled/timed_out` 掩盖，而是形成带 `interrupted_by` 和调用方请求状态的 `run.indeterminate`；未开始
或 `Pure/Idempotent` 仍保持原终态。独立 Host 会先持久化 indeterminate Checkpoint，再发布终态事件。
真实原生 Shell 进程树与 Streamable HTTP MCP 连接均已证明资源被关闭、结果不重放且不确定性未丢失。

MCP 请求级生命周期现已完成：独立 Host 的 Streamable HTTP 与 stdio `tools/call` 都携带唯一
`progressToken`，只接受有限、单调递增的匹配进度，并将其写成绑定 Tool call/实现摘要的
`tool.execution.progress` 事件；每次事件后持久化 Checkpoint。Run 取消会向原 JSON-RPC request ID 发送
`notifications/cancelled`，随后才以连接关闭或进程组回收兜底。真实 TCP/SSE 和本地 stdio 进程均已证明
通知送达、进度可重放、无残留进程；Unknown Tool 的终态仍是 `indeterminate`，协议通知不伪装成副作用回滚。

默认并发崩溃门禁的无界等待也已收口：测试必须同时观察父/子 Checkpoint 和 Provider 两条真实连接建立后
才触发 Host abort；signal、socket close、replacement、Provider completion 与 shutdown 均有分阶段 deadline。
并发子代理套件连续 20 轮 400/400；重启期间的空 Provider 连接也由独立竞态守卫覆盖。当前全工作区共 577 项：571 通过、0 失败、6 个外部 live
用例显式忽略。该修复提高证据可信度，不新增生产能力。

当前内核里程碑已改为：**把协议中立 Rust Runtime 的真实恢复闭环补齐，不扩 GUI/Java/云边范围**。
ADR-0090 已完成 Run 内冻结、可持久恢复的 MCP 2026 MRTR 用户输入回路；独立 Host 可跨进程替换继续
原 Tool，unsafe Tool 在续传发出后失联会 `indeterminate`。ADR-0091 又完成无状态
Worker↔Model Gateway gRPC 轮次桥，真实 MCP 输入与回答可穿过 Gateway；这不等于 NATS recovery poll
已经形成云端恢复闭环。ADR-0092 已新增显式 `stdio2026`，内置真实进程与 Codex 严格外部 fixture 均跨
Host replacement 完成 MRTR；HTTP URL elicitation 也已证明 Runtime 不承载外部授权 secret content。

ADR-0094 已取代 ADR-0093 的临时 Host-owned PTY 边界：最小本地 supervisor 持有 PTY master，Runtime
Host 通过 owner-only token 与有界 Unix socket 执行 start/status/write/resize。owner Host 退出后替代 Host
已实测续接同一 PID；supervisor 丢失则回收进程组并持久化 `indeterminate`。输出在冻结 byte budget 精确
截断，Process Session 状态与内容目录收紧为 `0700/0600`，Manifest schema 升级为 7 并兼容迁移 1—6。

ADR-0095 已完成单一 PTY ownership 收敛：无 supervisor 时禁止新建 PTY，v2 `Hello` 冻结精确协议和
start/status/write/resize/lifecycle 能力；owner-only 生命周期持久化 clean/unclean 前任、活跃数与退出原因。
模型可见的 Pure Tool `process.attach` 可按冻结上限读取 stdout/stderr 尾部并返回起止游标和截断标志，
替代 Host 已实跑 attach 后继续交互与关闭。

ADR-0096 复核后没有照搬 OpenClaw 的 WebSocket 高低水位：当前 PTY reader 同步落盘、没有用户态发送
队列，磁盘阻塞会通过 PTY 内核缓冲自然反压。新增的 Pure Tool `process.wait` 可按显式 cursor 在同一 Tool
调用内等待输出或终态，且 `yield_time_ms` 受 Run-frozen Tool timeout 约束；真实 Agent Loop 与 wait 中 Host
replacement 已证明不重启 child、不重新请求模型 Tool Call。

ADR-0097 已把同一 Session 的 wait 收敛为一个共享持久观察器：1000 个并发 wait、250ms 空闲文件观察、
2 秒全量唤醒、取消回收、pipe、外部 PTY 和 Host replacement 均已实跑。实现采用 Codex 的共享通知原则，
但不把原 Host 的内存 handle 当作恢复真相；新 Host 仍从 Manifest 和 durable logs 重建观察。

ADR-0098 已完成 8 tenant / 64 Workspace / 64 个真实 Process Session / 1024 wait 的混合容量门禁：
1024 waiter 只保留 64 observer，最终取证的 250ms 实测 295 次持久观察；每 Session 取消一个 wait 不影响
其余 960 个，并发输入后的 p50 905.30ms、p95 982.02ms、p100 995.13ms，连续 10 轮通过。压力测试同时暴露
旧 PGID 在身份租约释放后仍被终止的竞态；Unix TERM/KILL 现已绑定 identity lease，残留回收不再静默成功。

ADR-0099 已将 `process.start` / `process.write` 与有界 yield 合并为统一交互语义：模型一次 Tool Call 可启动
或写入后等待首批输出/终态；省略 yield 时保持立即返回。真实 Agent Loop 已完成 start-yield→write-yield→close，
没有追加 poll/wait 模型回合；原有 cursor、冻结 timeout、取消、共享 observer、Host replacement 和 64 Session /
1024 wait 容量边界保持通过。

ADR-0100 已为副作用已接受、但 start/write yield 结果尚未返回时的 Host 崩溃建立持久交互收据。真实进程已
证明 start 只启动一次、write 只发送一次；替代 Host 使用原 attempt 身份交付原 Session 的有界结果。缺失、
损坏或不匹配的收据继续进入 `indeterminate`，不会自动重放非幂等操作。

ADR-0101 已收敛 Process Session 的关闭语义：`process.close` 在 TERM/KILL 前持久化精确绑定的 close intent，
Manifest 以 `Terminating/Closed → Terminated/Closed` 表达唯一终态；首个 Host 在终止过程中崩溃后，替代
Host 可继续同一身份围栏关闭并交付原 Tool 结果。真实进程只启动一次、模型也只发一次 close。自然退出
不伪造关闭收据，`interrupt` 仍保持信号型非幂等边界。

ADR-0102 已完成第一段多租户 Runtime Invocation 闭环：RunExecution v20 和显式
`RuntimeInvocationContext` 绑定 tenant/application/workload/Workspace/AgentVersion/model policy；事件与
Worker Checkpoint 26 持久同一身份，换 application 的恢复在模型前拒绝。`EmbeddedRuntime` 只接受预注册
Profile，不接受调用方路径或凭据；全局、tenant、Workspace active limit、全局/tenant queue limit、取消
即时退队和 tenant round-robin 已通过真实 A1→B1→A2 Provider 顺序验证。同 Workspace 单写不会阻塞同
租户另一 Workspace；同一 tenant/application/Workspace 可用稳定根目录注册多个 AgentVersion，而不同
Workspace 身份不能复用持久根目录。默认 CLI 仍通过显式兼容 Profile 工作，执行主链不再直接读取
`LOCAL_TENANT_ID`。默认并行的全 Rust workspace 门禁已经通过。

ADR-0103 已补齐多租户身份的 Rust egress 与恢复授权链；ADR-0104 又建立了签名 Edge Task、真实
`EmbeddedRuntime` 执行、重启去重、终态崩溃恢复和本地持久 outbox 基础层。

ADR-0105/0106 已完成设备身份、Enrollment 与认证出站会话，但该方向当前暂停，不作为 Runtime 内核阶段的
完成条件。ADR-0108 已把 `EmbeddedRuntime` 的 resume、精确审批决定和 cancel 收口为统一、协议中立且有
持久收据的 control contract，并用单 owner、owner epoch、command digest 和双崩溃恢复测试固定语义。

ADR-0109 已让 Unix daemon/CLI adapter 复用同一 Runtime control contract，移除了审批、取消、MCP 输入和
恢复的双状态机；Attach 也改用持久事件与 Run record，不再依赖进程内 handle。ADR-0110 又完成 10 tenant /
100 Profile 的混合 execute/control/cancel/resume 风暴，固定 8 active / 92 queued、tenant/Workspace 高水位、
40 个收据和 130 次 Provider 请求；饱和队列拒绝现在发生在 durable acceptance/epoch 推进之前。

ADR-0111 已完成终态 Runtime 账本的第一段 crash-safe retention/GC：只有完整事件序列、摘要、身份和终态
一致的 Run 才能先提交 digest-bound Run/control tombstone，再删除热 artifacts 并完成 cleaned commit；中途
崩溃由替代 Runtime 幂等 repair。活动、等待审批、`indeterminate` 和未完成 `Accepted` receipt 永不自动
回收。单 Workspace 与同一进程内 tenant 的多 Workspace hard cap 都在模型前执行；没有安全候选时失败关闭。
1000 个真实 HTTP/SSE 顺序 Run 已证明热目录保持 16、墓碑重启后继续拒绝重放、RSS/FD 与 state root 在
当前策略下有界。

ADR-0112 已完成 Session/子代理图感知 retention 与分段 terminal ledger：活动 Session Turn、父 Checkpoint
中的 pending/active/reservation 和未完成 control receipt 是强恢复边；完成 Session/子代理历史内嵌完整
transcript/result，只保留来源 ID，不再钉住热目录。root Session Turn 与新子代理 Run 现在都写统一
`run.json`。旧单文件可崩溃安全迁移为 manifest、256-Run immutable segment 与 bounded active segment；
1000 个真实顺序 Run 为 110.617 秒、16 热目录/984 墓碑，4 tenant×3 Workspace×32 Run 为 36.64 秒且每
Workspace 最终 6 个热目录。恢复调度现在只启动图根，父 Checkpoint 所有的 child 不会与父并行争抢
owner epoch。当时公开 history gap、冷归档读取与外部 tombstone 转储仍未实现；前两项后来由 ADR-0114/0136
完成，外部 tombstone 转储仍缺。

ADR-0113 已完成本地容量目标：**1000 claimed in-flight / 32 admitted Host+Provider**，不是 1000 个真执行。
20 tenant、200 Workspace/Profile 的 peak queue 为 968；500 queued abort、16 active durable cancel、484
成功后无 owner/queue 残留。事件分发已由无界 live sink 改为 durable JSONL + bounded cursor subscription，
同时限制单订阅、进程订阅数、总缓冲槽与事件行大小。M1 Pro exact RSS 14.6→48.5 MB、FD 211→278→211，
38.300 秒完成。日常快速门禁采用 256/16；1000 个真正同时执行只留给专用或分布式环境，不能再用排队数
冒充执行并发。

ADR-0114 已完成版本化、协议中立的 Runtime Event Cursor：有界 page/subscription、显式 terminal/waiting/
suspended/interrupted/retired Boundary、真实 history gap、typed error 与 Legacy Attach 兼容均已进入 Rust/IPC
契约。低层 bulk helper 与 Embedded compatibility shim 只保留给内部恢复和暂停中的既有 Edge consumer，
Runtime Host/IPC 新集成面不再依赖；随机分页的 O(日志长度)扫描保留为 profiling 驱动的优化点。

ADR-0115 已完成 MCP 能力面的第一段安全边界：HTTP 2025/2026、stdio、Model Gateway gRPC 与 Worker 统一
识别 Tools/Resources/Prompts；只提供 Resources/Prompts 的 Server 是合法空 Tool 目录，且不会误收到
`tools/list`。directory schema 2 与摘要绑定受支持表面；schema 1 只兼容推断 Tools，未知或自相矛盾的目录
fail-closed。Roots/Sampling 仍不声明并按原 request ID 拒绝，凭证仍只在 Gateway 域打开。

ADR-0116 已继续完成 Resources list/read 与 Prompts list/get：HTTP 2025/2026、stdio、Gateway gRPC 与 Worker
共用协议中立有界类型、单页 opaque cursor、完整 workload identity、Server snapshot 和冻结目录摘要；超大响应、
未知 wire schema、未知 role、越权 Server 与 capability 漂移均 fail-closed。该能力当前是 Runtime/Worker API，
其模型入口与 Resource Templates 已由 ADR-0117 接续完成。

ADR-0117 已完成五个 Runtime-owned 模型只读 Tool，并复用 ADR-0116 的 tenant/Run/Server/digest、分页、预算
和审计边界。读取必须具有独立 `mcp:read:<server>` scope，不能从远端 Tool 副作用授权推导；Prompt 只作为
低权限 Tool Result，Resource Templates 与其他读取共享有界协议。

ADR-0118 的 OAuth 第一阶段已完成稳定句柄、PKCE、加密文件账本、CAS/租约、并发 refresh 单飞、崩溃提交点、
本地 revoke 与 Gateway 内 Token 解析；独立 Host 明确拒绝凭证域句柄。下一优先目标固定为：**补全
credential-domain-owned MCP OAuth 的协议发现与消费闭环**——Protected Resource / Authorization Server
metadata discovery、管理 API/CLI callback、MCP 401 rejected-token CAS 联动、远端 revoke、真实外部 Server
兼容矩阵，以及可替换的跨平台 CAS/lease store。Roots/Sampling 继续默认关闭，
不把 Java/GUI/Edge 或外部数据库引入一次 Run。跨进程/跨节点 tenant authority、Windows ConPTY、真实 Linux
cgroup、生产远端 command ledger 与外部冷存储适配继续保留为明确缺口。

ADR-0132—0135 已把本地文件模式的模型路由与 Run 终态提交链收敛为统一语义：有界 route WAL 先保存 Provider
出站/响应事实，Kernel terminal Event 先进入摘要有效的终态 Checkpoint，再发布原始 Event，随后封口路由 WAL
并收敛 adapter 投影。Direct Host、Embedded 与网络入口对终态 Run 都只验证和观察，不再把 `resume` 当成新的
模型回合；继续对话必须创建新 Run/Session Turn。该机制不依赖数据库，但也不冒充通用多文件事务；跨机器 owner、
共享文件系统、硬件掉电与 Windows 仍是明确缺口。

ADR-0136 已补上 opt-in 的有界冷 Event 层：只有读回校验通过的内容寻址对象和摘要索引先提交，retention 才会
提交 tombstone 并删除热 Run；公开 Cursor/subscription 可继续读取退休历史。单 Run、Workspace 和 tenant
条数/字节预算共同限制磁盘，跨 Workspace 不会放大 tenant 上限；淘汰形成真实 history gap，损坏承诺则失败
关闭。该层默认关闭，不给本地 1000 Run 路径施加持续 fsync/磁盘成本；压缩、自动 quarantine、外部对象存储、
共享文件系统和 Windows 仍未实现。

ADR-0137 已把 Codex strict MCP 2026 stdio server 从人工 binary path 变成哈希固定的跨项目 release gate：
只接受精确 Codex commit、clean fixture 和已复核 SHA，不复制上游源码；真实 Agent Loop 跨 Host replacement
完成 input-required continuation。普通 workspace 无参考 checkout 时只构建 fail-closed stub，不把 ignored
冒充通过。它只增加 N=1 的外部 MCP 证据，不代表 Streamable HTTP、OAuth、多第三方 Server 或三类 Provider
兼容矩阵完成。

ADR-0138 已把历史上人工启动的官方 Streamable HTTP 验证升级为锁定门禁：npm lock 固定
`server-everything@2026.7.4`、SDK `1.30.0` 和完整传递依赖，空环境临时安装/启动，按精确 PID 回收且不留下
`node_modules` 或 npm cache。Model Gateway 的 discovery/call/stale digest 拒绝与独立 Host 的完整 Agent Loop
共 3/3 通过；每条还验证 test name 与 `1 passed`，不存在零测试假绿。当前 MCP 有两个外部样本，但真实 OAuth、
非官方 SDK/手写 Server、长稳公网流和三类真实 Provider 兼容矩阵仍未完成。

ADR-0139 又把只读 discovery 扩展到 Context7 与 Microsoft Learn 两个无需凭据的公开生产端点：门禁显式清除
本地认证变量，只执行 initialize/initialized/`tools/list`，并要求每个端点都有非空对象 schema 目录、租户
namespace 和稳定格式摘要。两端首轮共同运行 1/1 通过，分别观测 2/3 个 Tool；没有远端 Tool 调用、用户数据或
生产协议修改。该阶段证明运营方与真实部署多样性，不证明实现栈独立；已知非官方 SDK/手写 Server、真实 OAuth、
长稳公网流和三类真实 Provider 兼容矩阵仍未完成。

ADR-0140 用 `mark3labs/mcp-filesystem-server v0.11.1` / `mcp-go v0.32.0` 补上首个实现栈已知独立的外部
Server。真实 RED 暴露 `2025-03-26` 被旧客户端拒绝；RunExecution schema 22、Gateway 与 stdio 现可显式冻结
并验证该修订，绝不把 Server 选择漂移当成兼容。只允许 `list_allowed_directories` 的完整 Agent Loop 1/1
通过，Go 源码/依赖/构建缓存全在受控临时目录并原地删除。真实 OAuth、长稳公网流和三类真实 Provider
兼容矩阵仍未完成。

ADR-0141 收紧网络与嵌入调用方的生命周期承诺：持久 Run 已进入 `AwaitingApproval`/`AwaitingMcpInput`，但旧
execution owner 尚未释放时，分页和流式事件继续公开 `Running`；只有下一代 owner 真正可取得后才公开
`WaitingApproval`/`Suspended`。该规则关闭了负载下“已显示可审批、决定却返回 Internal”的窗口，并保留
Codex 先注册决定通道再发事件、OpenClaw 对投递中提前 resolution 延后 finalize 的共同不变量；没有引入
Gateway、客户端重试或第二套状态机。

## 本地运行边界

- Mac 本地内核开发和验收禁止调用 Docker、虚拟机、Kubernetes、Java、PostgreSQL、NATS、Vault
  或 OCI Registry。
- 本地 Checkpoint 使用内容寻址文件系统；生产控制面、消息总线和对象存储通过可选适配器接入，
  不进入独立 Runtime 的必需依赖链。
- 只允许仓库内声明的可信 Tool 原生执行；macOS 本地限制不得冒充 Kata 级强隔离。
- 测试结束后不得残留进程、端口、临时目录、日志或测试密钥。Rust `target` 是可复用的增量构建缓存，
  默认保留且不提交 Git；只有缓存损坏、工具链大版本切换、磁盘空间紧张或用户明确要求时，先统计再定向清理。
- 生产 Dockerfile、Kubernetes 清单和 Java/Vue 模块不得被内核 `dev`、`test` 或 `check` 路径调用。

## 对标规则

每个阶段完成时必须分别回答：

- 相比 Codex，Agent Loop、模型、Tool/MCP、审批、沙箱、Checkpoint、子代理和会话生命周期还差什么。
- 相比 OpenClaw，模型容灾、连接生命周期、故障恢复、Workspace 协调和跨平台运行还差什么。
- 如果采用不同实现，必须说明为何更适合多租户 PaaS；没有证据时不得宣称领先。
- 每轮固定汇报：已验证能力、Codex 差距、OpenClaw 差距和下一优先缺口。

## 完成定义

单元测试、静态门禁或 HTTP 200 均不等于完成。只有发布形态的本地 Runtime 完成真实模型或可审计
回环模型、真实 Tool/MCP、权限 allow/deny、Checkpoint/故障恢复、最终事件与无残留退出，才可以提升
对应内核能力状态。需要外部厂商凭据或服务的验收必须单列为未验证，不得用模拟结果替代。
