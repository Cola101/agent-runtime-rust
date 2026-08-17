# 必需 MCP 启动失败终态证据（2026-08-17）

## RED

Embedded Runtime 注册一个合法、required、但连接必然拒绝的 Streamable HTTP MCP Server。
`execute_detached` 返回已接纳 Run；旧实现随后形成 `Finished/failed` 记录和空事件日志，
`event_cursor` 返回 `CorruptLog`。这证明问题是外部可观察契约，不是文档推断。

## 实现边界

- Kernel 可从 `Queued` 直接提交 `run.failed`，不会伪造 `run.started`。
- Worker coordinator 只把穷尽 discovery 策略的 required failure 转成 typed completion，并保留
  unavailable Server status；其他内部错误继续 fail-closed。
- Host 持久化 Kernel event 和 terminal Checkpoint；Provider fixture 接受 0 次请求。
- terminal payload 只含绑定 Server 名称，不含远端错误正文。

## 验证

- `agent-kernel --test run_state_machine`：11/11。
- `agent-runtime-host --test embedded_multi_tenant`：9/9，Event Cursor 返回
  `Terminal { failed }`，末事件为 `run.failed`、kind 为 `required_mcp_unavailable`。
- `standalone_run::unavailable_required_stdio_mcp_fails_before_model_egress`：1/1，真实 stdio 启动重试
  耗尽后模型零出站、子进程全部回收。
- `agent-runtime-worker --test mcp_end_to_end`：16/16，公平准入、取消、恢复目录验证未回归。

## 对标结论

与 Codex 一样，显式依赖缺失不会静默进入模型；与 OpenClaw 一样，启动失败对调用方可见。本项目额外
把结局绑定到 tenant/Run/attempt 的持久事件与 Checkpoint。尚未验证的 Host 错误类别继续列为风险，
总体进度仍为 70–75%。

