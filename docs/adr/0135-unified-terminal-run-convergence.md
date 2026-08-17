# ADR-0135：统一终态 Run 收敛语义

- 状态：Accepted
- 日期：2026-08-17
- 范围：独立 Runtime Host、EmbeddedRuntime、网络调用面、模型路由 WAL、终态 Checkpoint/Event

## 背景

ADR-0132 已使单次模型请求具备有界路由 WAL，ADR-0133/0134 又分别关闭了“Kernel Event 已终态但控制投影
滞后”和“终态 Checkpoint 已提交但 Event 尚未发布”的窗口。剩余窗口位于两者之间：普通 one-shot Run
此前不保存终态 Checkpoint；若进程在 terminal Event 提交后、模型路由 WAL completion 或 Run 投影提交前退出，
直接 `LocalRuntimeHost::resume` 会从旧的非终态 Checkpoint 再次进入 Agent Loop，追加第二组恢复、输出与终态事件。
这既违反 Run 终态不可变，也可能重放 Provider 或 Tool。

## 决策

1. 每个 Run 的所有 Kernel 终态事件都必须先写入 schema 27 终态 Checkpoint；不再只覆盖 Root Session 和子代理。
   Checkpoint 保存并摘要绑定原始 terminal `EventEnvelope`，提交顺序统一为：
   `model response staged → Kernel apply → terminal Checkpoint → terminal Event → route WAL completion → adapter projection`。
2. `resume` 和 `resume_with_imported_history` 遇到终态 Checkpoint 时不得调用普通 restore/drive。Runtime 先用完整
   replacement command 校验 tenant、application、workload、Workspace、AgentVersion、模型策略、输入、历史、
   Tool/Skill/MCP 与运行策略绑定，再返回该 Run 的既有终态。继续业务对话必须创建新的 Run/Session Turn。
3. 若 Checkpoint 已终态而 Event 缺失，复用 ADR-0134 的原始 envelope 发布；若 Event 已存在，则验证完全一致。
   不重新生成 event id、timestamp、trace id 或 payload。
4. 终态收敛同时检查该 Run 的模型路由 WAL。只有唯一、同 attempt、无在途 Provider、无重试截止时间，且成功选择
   已观察或终态失败已观察的未完成 WAL，才能标记 completion。另一个 unfinished 权威 WAL、身份漂移、revision
   损坏或非法目录项全部 fail-closed。合法 64 位摘要 `.json.partial` 仅是未提交 staging，不作为权威记录。
5. 若 WAL 仍记录 in-flight Provider，则不把外部结果不确定性伪装成 completed。终态 Checkpoint 阻止 Run 重放，
   该 WAL 保留给审计与 retention；后续需要专门的模糊 Provider 观察/清理策略。

## 后果

- 正面：Direct Host、EmbeddedRuntime、daemon/gRPC 都以同一终态 Checkpoint/Event 为权威，终态 Run 不再因入口
  不同而重新执行；Event 缺失、WAL 滞后和投影滞后三类已证明窗口可以依次收敛。
- 正面：运行身份或 MCP/Skill/指令漂移在任何 Provider/Tool egress 前拒绝；失败类型明确为不可用 Checkpoint。
- 代价：普通 one-shot Run 也需要一次终态 Checkpoint 强提交，并在恢复时扫描该 Run 的有界 route WAL 目录。
- 中性：这不是任意多文件事务。跨机器共享 state root、硬件掉电、Windows 与介质损坏仍未验证。

## 被否决方案

- **把所有状态放进外部数据库事务**：可解决更宽的事务面，但会使独立本地 Run 依赖 PostgreSQL/SQLite 服务，
  不符合当前嵌入式边界。
- **Event-first 且不写终态 Checkpoint**：真实 RED 已证明 replacement 会从旧 Checkpoint 重放终态 Run。
- **从状态重新生成终态 Event**：会产生新的事件身份，破坏断线续传、审计和上游幂等。
- **Direct Host 保留旧 resume 行为**：会形成第二套生命周期语义，网络/Embedded 安全而本地入口不安全。

## 对标

- **Codex `ff352fab6209`**：rollout recorder 用单 writer task、Flush/Shutdown ack 形成明确提交边界。这里采用同一
  “已提交历史优先、终态只观察不重做”的原则，并将其绑定到协议中立的多租户执行身份；Codex 的 Thread/rollout
  生命周期、迁移与跨平台产品链仍更成熟。
- **OpenClaw `58b4b9430457`**：Session transcript 使用 writer queue、SQLite WAL 与 `BEGIN IMMEDIATE` 后重验快照。
  它在多记录事务、跨进程并发、schema migration 和 quarantine 上仍领先。本 ADR 只证明无数据库文件模式下的
  单 Run 收敛边界，不宣称通用存储领先。

## 参考

- ADR-0132：有界模型路由 WAL
- ADR-0133：终态 Event 与控制收据收敛
- ADR-0134：Checkpoint 绑定的终态事件发布
- `runtime/apps/runtime-host/tests/multi_provider.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
- `docs/evidence/2026-08-17-unified-terminal-run-convergence.md`
