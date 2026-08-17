# ADR-0124：必需 MCP 启动失败是 Kernel 终态

- 状态：Accepted
- 日期：2026-08-17
- 范围：Kernel、Worker MCP discovery coordinator、独立/Embedded Runtime Host

## 背景

必需 MCP Server 在冻结发现预算内持续不可用时，`LocalRuntimeHost` 正确地拒绝模型出站，但过去以
`Err` 结束。Detached adapter 随后只把 `run.json` 写成 failed，事件日志没有终态；Event Cursor
因此按 ADR-0114 返回 `CorruptLog`。调用方知道“提交成功”，却永远学不到 Run 的结局。

不能把该错误伪装成 Provider 失败，也不能伪造 `run.started`：MCP 目录尚未挂载，模型执行从未开始。

## 决策

1. Kernel 提供 `record_required_mcp_unavailable`，允许 `Queued` 或恢复后的 `Running` Run 直接提交
   `run.failed`，payload 固定为 `kind=required_mcp_unavailable`、`retryable=false` 和有界 Server 名称。
2. MCP coordinator 保留完整 discovery status。只有
   `RequiredMcpServersUnavailable` 被转换为 `McpDiscoveryCompletion::Failed`；未知 attempt、目录漂移、
   存储错误等仍返回 Host 错误，不得被降格成业务终态。
3. Worker 是 Kernel 的唯一可变所有者，负责记录 terminal event 并取消 attempt。Host 只持久化事件、
   terminal Checkpoint 和结果，不直接拼装 `run.failed`。
4. 可选 MCP Server 失败仍允许 Run 启动并携带 unavailable status；必需 Server 的安全 discovery 重试
   仍由冻结策略完成。只有预算耗尽才终止，模型调用次数必须为 0。
5. Server 的远端错误正文不进入 terminal payload；只暴露配置中已绑定的 Server 名称。详细诊断仍留在
   有界 discovery status，不进入通用事件面。

## 对标

- **Codex**：显式要求的 MCP/Plugin 依赖无法形成 Turn context 时，Turn 返回错误并走显式 error event；
  不会继续采样一个缺能力的 Turn。本决策保留该 fail-closed 语义，并增加多租户持久终态。
- **OpenClaw**：MCP 启动失败会在 CLI/Node Host 管理面显式失败，但其主要权威是 Gateway/进程生命周期。
  本项目把失败放进每个 Run 的 Kernel/event/Checkpoint 契约，更适合嵌入式与多租户调用方。

## 未覆盖

- optional MCP 降级已有独立证据，本 ADR 不改变它。
- coordinator timeout、Checkpoint 漂移、Tool/子代理编排及存储错误仍属于下一轮分类审计；不得据此
  宣称所有 Host `Err` 都已终态化。

