# Rust Runtime 外部兼容矩阵

更新时间：2026-08-17

“回环”证明执行语义，“外部”才证明协议兼容。表中的未验证项不能由单元测试或另一个本项目 fixture 替代。

## Provider

| 协议 | 外部样本 | 当前证据 | 状态 |
| --- | --- | --- | --- |
| OpenAI-compatible Chat Completions | `deepseek-v4-flash` / `ai.ctaigw.cn` | 真实流式文本、Tool Call、审批、Tool Result 回灌、两轮 usage；2026-08-07 单次运行 | **1 个样本，部分验证** |
| OpenAI Responses | 无 | 真实 HTTP/SSE 回环、reasoning/refusal/Tool/usage 契约 | **外部未验证** |
| Anthropic Messages | 无 | 真实 HTTP/SSE 回环、thinking/signature/redacted thinking/Tool/usage 契约 | **外部未验证** |
| 真实错误与容灾 | 无完整样本 | 回环覆盖 401/403/429/5xx、Retry-After、partial stream 与 cooldown | **外部未验证** |

## MCP

| 协议/实现 | 外部样本 | 当前证据 | 状态 |
| --- | --- | --- | --- |
| MCP 2026 stdio | Codex `ff352fab6209` strict fixture | 原始 hash-pinned source；真实 Agent Loop、input-required、Host replacement | **已验证 1 个样本** |
| MCP 2025/2026 Streamable HTTP | `@modelcontextprotocol/server-everything@2026.7.4` / SDK `1.30.0` | 完整 npm lock + SHA；官方 discovery、echo call、stale digest 拒绝及完整 Agent Loop | **已验证 1 个官方样本** |
| 多个第三方 MCP discovery | Context7、Microsoft Learn | 两个无需凭据的公开生产端点；只读 initialize/`tools/list`，分别观测 2/3 个 Tool；schema、namespace、digest 通过 | **已验证 2 个运营方部署** |
| 认证 MCP / sealed credential | 无当前端点 | ignored public authenticated test 已存在 | **未验证** |
| OAuth discovery/login/refresh/revoke | 无真实 provider | 受控回环覆盖协议和偏差边界 | **外部未验证** |
| Resources/Templates/Prompts 长稳分页 | 无外部 server | 本地 HTTP/stdio/Gateway 契约与 Agent Loop 已验证 | **外部未验证** |

## 兼容完成标准

- 每种 Provider 协议至少两个独立外部实现或一个官方实现加一个兼容实现；覆盖成功、限流、认证失败、服务器
  错误、半途断流和能力不兼容。
- MCP 已覆盖 Codex strict stdio、官方 Streamable HTTP，以及 Context7/Microsoft Learn 两个公开生产部署；
  公开部署的实现栈独立性未证明，完成仍要求一个**已知**非官方 SDK/手写实现和一个真实 OAuth Server；分页、
  elicitation、取消、progress、Resources/Templates/Prompts 与断线恢复按 capability 分项记录。
- 每条外部证据必须固定版本/摘要、明确是否消耗凭据或产生副作用，并使用脚本区分“未运行”和“通过”。
