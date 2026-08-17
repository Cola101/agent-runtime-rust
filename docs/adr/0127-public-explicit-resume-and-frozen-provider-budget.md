# ADR-0127：公开显式 Resume 与冻结 Provider 重试预算

- 状态：Accepted
- 日期：2026-08-17
- 范围：RuntimeInvocation、Embedded Runtime、持久模型路由、Runtime replacement

## 背景

ADR-0123 已让网络调用方提交、观察、审批和取消 Run，并证明等待审批的 Run 能跨 Runtime replacement。
但其风险清单明确保留了一项：`{"type":"resume"}` 的成功路径只在进程内测试过。Java SDK、CLI 或未来
GUI 因而还不能把公开 `Control` 当作已验证的崩溃恢复入口。

显式 Resume 还有一个不能被适配器掩盖的边界：原 Runtime 可能已经发出模型请求、但没有得到持久响应。
替代 Runtime 只能在冻结的同 Provider 尝试预算仍有余额时重试；Resume 本身不得扩充预算。

## 决策

1. `RuntimeControlAction::Resume` 的公开 JSON 形状保持 `{"type":"resume"}`。调用方使用 Submit 返回的
   `run_id + owner_epoch`、新的 `command_id` 和原 invocation 提交；替代 Runtime 将 owner epoch 单调提升。
2. Resume 只接管仍为 `Running` 且已有持久 Checkpoint 的 Run。它不替代审批、MCP 输入回答或取消命令，
   也不把 `Interrupted`、终态或没有 Checkpoint 的 Run 猜成可恢复。
3. 模型 route journal 的 candidate、attempt cursor、inflight 标记和 `max_same_provider_attempts` 继续作为
   恢复权威。若冻结预算只有 1 次，原请求已越过出网边界后 Resume 必须失败终结；策略明确允许第 2 次时，
   替代 Runtime 才能重试同一候选。
4. `command_id` 仍是持久幂等键。调用方丢失 Resume 响应后重放同一命令，只得到同一 digest 与终态收据，
   不会再提升 epoch、再调用 Provider 或启动第二个逻辑 Run。
5. 验收必须让第一 Runtime、gRPC Server 和执行任务整体消失；第二 Runtime 打开相同 state root。调用方不读
   内部文件，仅凭公开身份、Run、epoch 和事件完成恢复。

## 对标

- **Codex `ff352fab6209`**：`InitialHistory::Resumed`、`ThreadManager::resume_thread_with_history` 与公开
  TypeScript `resumeThread(threadId)` 从持久 rollout 重建 Thread；恢复的子代理还避免重复写
  `ThreadStarted`。它的 SDK 和 Thread 产品面明显更成熟。该路径主要重建已持久历史并继续新 Turn，本 ADR
  不据此声称 Codex 对同一在途模型请求提供 owner epoch 或冻结重试预算。
- **OpenClaw `58b4b9430457`**：main-session restart recovery 使用持久 cycle/revision、最多 3 次 charged
  automatic attempts、当前进程 owner 检查和 transcript tail 分类；模糊 Tool 会强制
  `forceRestartSafeTools`。它的自动扫描、重试退避和用户通知比本项目当前显式 API 更完整。
- 本项目的窄面增强是把 tenant/application/workload/Workspace、Run、command digest、owner epoch 与冻结
  Provider 尝试账本绑定在一起，适合无状态、多租户调用方。它不代表整体恢复产品领先 OpenClaw，也不替代
  Codex 已有的 SDK/Thread 生命周期。

## 代价与未覆盖

- 本轮补的是公开证据；现有 Runtime 实现无需修改。首次测试在默认单次 Provider 预算下得到
  `model.provider.failed → run.failed`，这是正确围栏，不是缺陷；显式配置两次尝试后才得到成功路径。
- 测试使用同机真实 gRPC 与 HTTP/SSE，但不是跨机器、真实厂商或主机掉电验证。
- 当前没有自动发现所有 orphaned Running Run 的公共管理 API；`recover_unfinished_detached` 属于 Runtime
  生命周期管理入口，外部调度与多进程 command ledger 仍未完成。
- `action_json` 仍没有生成式 Java SDK；本 ADR 只冻结 Resume 的最小 JSON 形状。
