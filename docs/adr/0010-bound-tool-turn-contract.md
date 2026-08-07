# ADR-0010：Tool Call 身份保持、委派权限与审批绑定

## 状态

Accepted

## 背景

首轮模型流已经能够产生 Tool Call，但原有模型消息契约无法表达 assistant Tool Call 历史，Worker
也没有把 AgentVersion 权限、Tool 策略和下一轮 Tool Result 串起来。若只把 Tool Result 作为普通文本
回灌，Provider 会失去 `tool_call_id` 关联；若审批只绑定 Tool 名称，等待期间参数或工作目录变化可能
复用旧授权。

Codex 的 `ToolRouter`、`ToolCallRuntime` 保留 call ID，按工具能力决定串行或并行执行，并把普通失败
转换为可回灌模型的 FunctionCallOutput。OpenClaw 的 Node 执行会校验审批计划与 argv、cwd、Agent、
Session 以及策略快照一致，并在执行前重新检查工作目录和脚本文件漂移。

## 决策

1. 通用模型 IR 和 gRPC `ContentPart` 同时表达 assistant Tool Call 与 tool-role Tool Result，二者共享
   原始 `tool_call_id`；OpenAI-compatible Adapter 映射为标准 `assistant.tool_calls` 和 `tool` 消息。
2. Scheduler 从不可变 AgentVersion `spec.delegated_scopes` 生成排序、有限的权限快照，并随 dispatch
   下发；Worker 只向模型公开 delegated scope 可以覆盖的 Tool。
3. Tool Registry 对调用执行 allow/deny/ask 策略。执行请求包含 effect、sandbox 和 SHA-256
   `binding_digest`；摘要覆盖完整调用、隔离等级、副作用类别和所需 Scope。
4. ask 生成独立 `approval_id`。Worker 只有同时匹配 attempt、approval ID 和 binding digest 才能恢复；
   不允许审批后替换参数或降级 Sandbox。
5. Worker transcript 保留 user → assistant Tool Call → tool Tool Result 的有序历史。未处理调用、待审批
   或未返回结果时禁止发起下一轮模型调用。
6. 本 ADR 只建立编排边界，不把进程内函数当成生产 Tool Runtime。Shell、文件、代码和不可信 Skill
   仍必须等待受限容器/Kata 执行器；审批权威存储和恢复命令仍由后续控制面阶段实现。

## 后果

### 正面

- Tool Call 跨 Provider、gRPC 和 Worker 后仍可精确关联结果，不依赖文本约定。
- Agent 权限是每次 dispatch 的显式最小权限快照，不从 Worker 全局配置隐式继承。
- 审批绑定比仅绑定命令字符串更适合多租户、长时间暂停和远程 Worker。

### 负面

- Tool 消息契约增加一种 ContentPart，所有 Provider Adapter 都必须实现或明确拒绝。
- 当前 Worker 仍需外部执行器和持久审批恢复后，才能自动完成多轮 Agent Run。
- 暂未实现 Codex 的按工具并行能力，也未实现 OpenClaw 的 cwd/脚本文件实体快照复核。

## 替代方案

- **直接复制 Codex Tool Runtime**：功能完整，但会把单会话进程信任边界和 OpenAI Responses 类型带入
  多租户数据面，因此只参考语义。
- **只参考 OpenClaw Node system.run**：边缘执行和路径复核成熟，但其本地审批文件/Gateway 所有权模型
  不能作为平台权威状态源。
- **Tool Result 转普通 user 文本**：协议简单，但破坏调用身份，多个调用和重试时容易错配。

## 参考

- Codex：`codex-rs/core/src/tools/router.rs`
- Codex：`codex-rs/core/src/tools/parallel.rs`
- OpenClaw：`src/node-host/invoke-system-run.ts`
- OpenClaw：`src/agents/embedded-agent-runner/run/attempt.subscription-cleanup.ts`

