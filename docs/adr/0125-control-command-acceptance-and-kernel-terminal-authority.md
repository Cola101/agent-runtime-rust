# ADR-0125：控制命令接纳与 Kernel 终态权威

- 状态：Accepted
- 日期：2026-08-17
- 范围：Embedded Runtime、独立 Host、审批/MCP 输入/取消控制面

## 背景

`run.json` 是给适配器快速发现状态的投影，不是恢复权威。旧实现只用该投影校验审批目标，随后先写
`ApprovalDecided` 和 Accepted receipt，再由 Worker Checkpoint 发现目标或 Tool binding 不一致。
异步控制失败后，适配器又会直接把整个 Run 写成 `Finished/failed`，但 Kernel 事件日志没有终态；
Event Cursor 因而只能按 ADR-0114 报告 `CorruptLog`。

等待审批的取消也有同类旁路：旧路径只写 `Cancelled` record，不恢复 Kernel，不产生 `run.cancelled`。
这让一个普通控制命令拥有了 Kernel 之外的第二套终态状态机。

## 决策

1. 审批与 MCP 输入在写 receipt、提升 owner epoch 或改变 Run record **之前**，必须同时通过 Run 投影和
   摘要有效的 Worker Checkpoint 校验。Run record 负责定位；Checkpoint 负责证明待消费的精确 binding。
2. 子代理审批的 `target_run_id` 必须能沿 Checkpoint lineage 回到 root Checkpoint 当前持有的 pending
   子任务；目标 Checkpoint 必须持有精确 pending decision，或已持久应用同一 approval id、binding 和
   decision。另一棵 Run 树、错误 binding 和损坏摘要均在接纳前拒绝。
3. 已接纳控制命令若随后遇到事件/Checkpoint/Host 存储错误，保留 `ApprovalDecided` 或
   `McpInputDecided` 与 Accepted receipt，由替代 Host 重放同一命令；不得把适配器错误转换为 Run 失败。
4. `run.json` 的 terminal 状态只能投影已经提交的 Kernel terminal event。`drive_recorded` 可以用事件
   修复落后一拍的 record，但没有 terminal event 时不得合成 `Finished/failed`、`Cancelled` 或完成 receipt。
5. 对 waiting approval / suspended Run 的取消必须恢复同一 Checkpoint，并在任何模型、MCP 或 Tool 出站
   前通过 Kernel 提交唯一 `run.cancelled`。无 Checkpoint 的已接纳 Run 从原始命令以预取消 token 启动，
   同样先过 Kernel，不直接写终态。

## 对标

- **Codex `ff352fab6209`**：pending approval 以精确 approval id 保存；未知决定只记录
  `No pending approval`，不会把 Turn 改写成失败。本决策保留“错误命令不终结工作”的语义，并把 binding、
  子代理 lineage 和崩溃恢复提升为持久多租户边界。
- **OpenClaw `58b4b9430457`**：canonical resolver 在打开 Gateway client 前校验 owner kind、approval id
  与 decision；not-found/permission 等失败向调用方传播，不伪造 Session 终态。本决策采用同一拒绝边界，
  但接纳权威来自 Run/Checkpoint/receipt，而非当前 Gateway 连接。

## 代价与未覆盖

- 每次恢复决定会多读取 root 与目标 Checkpoint；这是低频控制路径，优先保证权威一致性，不用热路径缓存
  换取错误接纳风险。
- 本 ADR 不把一般 Host I/O 错误伪装成业务失败。无法恢复的损坏会持续 fail-closed，需由后续运维修复或
  显式事故处置；不能用一个没有 Kernel event 的假终态“解决”。
- MCP 输入的真实 Embedded 网络闭环不在本 ADR 的原始证据内；后续 ADR-0126 已补专项，但仍不把单个
  loopback 网络闭环外推成外部生态兼容结论。
