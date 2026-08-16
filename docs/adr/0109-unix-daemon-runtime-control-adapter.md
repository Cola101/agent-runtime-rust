# ADR-0109：Unix daemon/CLI 作为统一 Runtime 控制适配器

状态：Accepted（2026-08-15）

## 背景

ADR-0108 建立了持久且协议中立的 `EmbeddedRuntime` 控制命令，但本地 Unix daemon 仍独立维护
Run handle、取消 token、审批/MCP 决定和崩溃恢复。CLI、嵌入式调用与未来 transport 若各走一套状态机，
同一 Run 会出现不同的 owner epoch、幂等和恢复结果；内存 handle 也不能作为 daemon replacement 后的
Attach 权威。

## 决策

1. Unix daemon 只保留 transport、0600 socket、兼容命令映射和展示顺序；Run 执行、控制和恢复全部委托给
   同一个 `Arc<EmbeddedRuntime>`。本地默认限制为 16 个活动 Run、每 Workspace 1 个活动 Run、最多
   1024 个排队请求。
2. `submit` 调用 `execute_detached`；完整 `control` 接受 versioned `RuntimeControlCommand` 并返回
   `RuntimeControlReceipt`。CLI 增加 `resume` 与直接提交 JSON control command 的入口。
3. 旧 `approve`、`deny`、`cancel`、`resume` 与 MCP input 命令只负责生成确定性 command ID 和精确 action，
   然后调用同一 `control_detached`。相同旧命令重试必须命中同一收据，不能再次执行 Tool。
4. daemon 启动恢复调用 `recover_unfinished_detached`。终态事件、Run record、Checkpoint、已接受 control
   receipt 与 owner epoch 的解释只存在于 Runtime core，不在 IPC 层复制。
5. `attach` 只读取已提交事件日志和持久 Run record，并以游标轮询新事件；没有内存 Run handle 的替代
   daemon 仍能继续 Attach。存储错误必须 fail-closed，不能伪装成空事件或成功终态。
6. 为兼容旧本地数据，只允许把 tenant/application/workload/Workspace/AgentVersion/model policy 全部为空的
   legacy Run record 迁移为当前内置 local invocation。任何部分身份或外部身份均不得被本地 daemon 认领。
7. 一旦 control receipt 已持久接受，随后执行任务即使快速失败，transport 仍返回该 accepted receipt；
   后台失败由 Runtime 写入终态并完成收据。客户端断开不能取消已持久接受的执行。

## 验收不变量

- legacy 审批重放只有一份 receipt，且 `tool.execution.started` 只出现一次。
- 错误 owner epoch 在产生 receipt 前拒绝；正确完整 command 原样返回 caller command ID 和 epoch。
- daemon replacement 不依赖旧进程内 handle 即可 Attach 和恢复。
- 完全空身份的旧记录可迁移；带任何身份的记录不会被兼容路径提权。
- accepted receipt 与快速后台错误不存在“已经落账却返回 transport error”的竞态。

## 对标判断

- Codex app-server 的 `thread/resume`、`turn/interrupt` 和审批请求拥有更成熟的客户端协议和产品交互；本项目
  本轮吸收其统一控制入口思想，并额外把完整多租户 invocation、owner epoch 和可重放 durable receipt
  固定在协议中立 Rust 内核中。该窄面更适合被 Java/CLI/未来 GUI 共同嵌入，不代表整体超过 Codex。
- OpenClaw Gateway/Node 已有 invoke input、cancel、progress、timeout 和连接生命周期；inspected 主链围绕
  online pending/active invoke map。本项目本轮的本地 receipt 与 replacement Attach 不依赖当前连接，
  但远端认证、relay、动态能力、跨平台节点和运维仍明显落后。

## 边界与后果

- Unix socket 的 0600 权限只适合本机用户，不是远端认证、租户授权或命令签名。
- 当前没有允许多个进程共享同一 state root 的分布式 command ledger；单 daemon 锁只是本地运行边界。
- Attach 使用持久日志轮询，不是在线 fanout。后续可加通知适配器，但不能改变持久日志和 Run record 的权威。
- control receipt 的临时文件读取失败会 fail-closed；异常退出后的 ledger 维护仍需独立设计，不能静默忽略。
- 本 ADR 不引入 Edge、Java、GUI、Docker、数据库或消息总线。
