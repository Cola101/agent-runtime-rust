# ADR-0012：受限 Tool 执行与多轮模型闭环

- 状态：Accepted
- 日期：2026-08-01

## 背景

Worker 已能持久接收 Tool Call、等待审批并生成绑定执行请求，但此前没有真实进程生命周期，也不会在 Tool Result 持久化后自动发起下一轮模型请求。直接把模型参数拼成 Shell 命令会破坏租户隔离、审批绑定和至少一次消息投递下的副作用安全。

## 决策

1. 新增独立 `agent-tool-runtime`，以对象安全的 `ToolExecutor` 接口隔离 Worker 编排与具体沙箱 Provider。
2. 首个 Provider 为只读、断网的受限 OCI 容器执行器。镜像必须固定到 `sha256` digest；容器引擎必须使用绝对路径；入口使用参数数组，不经过 Shell。
3. Workspace 只由 Worker 根据 `tenant_id/workspace_id` 推导并以只读方式挂载。Tool 参数和调用身份只通过 JSON stdin 传入，清空继承环境，不进入命令行参数。
4. 容器结果必须同时匹配 `tool_call_id` 和 `binding_digest`；Worker 在写入 Kernel 前再次校验绑定摘要。
5. stdout/stderr 有界读取，执行受租约上限、超时和 Run 取消令牌约束。失败只向模型暴露分类错误，不回显容器 stderr。
6. 模型完成 Tool Call 回合后，Worker 先持久化模型事件，再持久化 Tool 计划，随后启动执行。Tool Result 获得 JetStream PubAck 后才允许下一轮模型请求。
7. 同一 `(attempt_id, tool_call_id)` 在单 Worker 进程内最多启动一次；非幂等 Tool 的跨进程恢复仍由后续持久执行回执解决。

## 结果

- 已形成 `model → tool plan → executor → durable tool.result → model` 的自动闭环，并保留审批暂停/恢复。
- 相比把 Tool 直接嵌进 Worker，更容易替换为 Kubernetes/Kata/Edge Provider。
- 当前受限 Provider 只支持只读 Workspace 和无网络容器，不等同于 Shell、HTTP、MCP 或 Kata 已交付。
- 执行启动与完成回执后来已由 ADR-0013 纳入 PostgreSQL 权威账本；完整 transcript/checkpoint 的跨 Worker 恢复仍未实现。

## 未选择方案

- 直接 fork Codex Tool Runtime：其会话和本机沙箱语义成熟，但不提供本平台所需的租户身份、Workspace fencing 和 JetStream 持久化边界。
- 复用 OpenClaw Node `system.run`：其设备侧命令校验值得参考，但协议面向 Gateway/Node，不是多租户云端 Worker 的权威执行状态机。
- 将 Tool 参数放入 argv 或环境变量：更容易被进程列表、诊断信息和子进程继承泄露。
