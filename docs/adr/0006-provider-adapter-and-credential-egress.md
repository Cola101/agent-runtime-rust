# ADR-0006：Provider Adapter 与凭证出口边界

## 状态

Accepted

## 决策

Model Gateway 提供 OpenAI-compatible Chat Completions、OpenAI Responses 与 Anthropic Messages 三类
真实 Provider Adapter。协议在 Gateway 启动时显式选择，不能根据 URL 猜测。
Agent Kernel 只依赖供应商无关的 Model IR；Worker 不持有 Provider 凭证，也不直接访问模型服务。

Provider 凭证使用不可打印的 `ProviderCredential` 封装，只能在 Model Gateway 发起 HTTP 请求时转换为
OpenAI `Authorization` 或 Anthropic `x-api-key` Header。配置中的 endpoint 禁止内嵌用户名或密码，
错误正文在离开 Adapter 前必须脱敏。

模型流必须分别设置“等待响应头超时”和“流空闲超时”，并接受 attempt 级
`CancellationToken`。Tool Call 的 `finish_reason=tool_calls` 只结束当前模型回合，不结束整个 Run。
只有 `stop` 才产生 `run.succeeded`。

对象存储 URI（例如 `s3://`）不得直接发送给外部 Provider。Model Gateway 后续必须先按租户权限读取，
再转换为受控的短期 HTTP(S) URL 或 data URL；当前 Adapter 对未解析的私有 URI 直接拒绝。

## 理由

- Chat Completions 兼容面覆盖第三方和本地 Endpoint，适合先验证通用 Model IR，而不是先绑定 OpenAI
  Responses 的专有事件。
- Codex 对 SSE 完成事件、流提前关闭和空闲超时的严格处理值得保留，但其 Provider 路径不应成为
  通用 Runtime 的内核依赖。
- OpenClaw 将 Provider 实现注册、传输选择和凭证出口解封分开，证明凭证应尽可能晚地进入请求。
- 多租户 PaaS 还需要把 Provider 凭证与 Worker、Workspace、Tool 沙箱彻底隔离。

## 已落地

- 真实 HTTP POST 和 SSE 解析：文本增量、用量、分片 Tool Call、完成原因；缺失完成原因不得默认成功。
- 401/403、402、429、超时、5xx、上下文溢出、协议错误和连接错误分类。
- 响应头超时、SSE 空闲超时和显式取消；取消不等待 Provider 自行结束。
- 凭证 Debug 脱敏、错误正文脱敏、endpoint 凭证拒绝和私有对象 URI 出口拒绝。
- Worker 为每个 attempt 创建独立取消令牌，定向取消会触发该令牌。
- Responses 使用 typed Items、`text.format` 与 `response.completed`，不复用 Chat Completions chunk。
- Anthropic 使用顶层 system、`tool_use/tool_result`、分片 `input_json_delta` 和强制 `message_stop`。
- 原生 Supervisor 持久化协议名；旧配置缺少该字段时只兼容默认 `openai_compatible`。

## 边界

- Worker 到 Gateway 已使用短期工作负载身份与 mTLS，但 Provider 凭证仍来自 Gateway 启动配置，尚未连接
  Vault/KMS、租户 BYOK 和按请求解封。
- 当前没有 Provider Registry、能力配置档、重试、候选故障转移或熔断；结构化输出和推理参数在不同
  “兼容”服务上的差异仍需显式能力协商。
- 当前只用本地真实 HTTP 服务验证协议，没有调用外部付费模型。
- Anthropic 首版不支持结构化输出、图片以及模型特定 thinking/cache/refusal/OAuth 兼容；遇到无法忠实
  表达的能力必须 fail-closed。

## 参考

- [OpenAI Chat Completions streaming events](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events)
- [OpenAI Responses API](https://developers.openai.com/api/reference/resources/responses)
- [Anthropic streaming Messages](https://docs.anthropic.com/en/api/messages-streaming)
- Codex：`codex-rs/codex-client/src/sse.rs`
- Codex：`codex-rs/codex-api/src/sse/responses.rs`
- OpenClaw：`packages/ai/src/providers/anthropic.ts`
- OpenClaw：`packages/ai/src/transports/anthropic-transport-stream.ts`
