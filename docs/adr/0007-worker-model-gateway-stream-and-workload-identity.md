# ADR-0007：Worker 到 Model Gateway 的流式边界与工作负载身份

## 状态

Accepted

## 决策

Worker 与 Model Gateway 使用服务端流式 gRPC。请求和事件采用独立的版本化 Protobuf 契约，
不直接暴露 OpenAI、Anthropic 或其他 Provider 类型。

每次模型调用必须携带由控制面签发的短期 Ed25519 工作负载令牌。Gateway 只配置公钥并验证签名，
令牌绑定 `tenant_id + run_id + attempt_id + worker_id + model_policy_id`，同时限制 audience、scope、
签发时间、到期时间和最长五分钟有效期。身份与请求任一字段不一致时，在访问 Provider 前拒绝调用。

Provider 凭证继续只存在于 Model Gateway。Worker 只能获得工作负载令牌，不能获得或转发 Provider
API Key。Worker 取消 attempt 时停止读取 gRPC 流；服务端流对象销毁会触发同一个取消信号并关闭
Provider HTTP/SSE 请求。

## 理由与取舍

- 相比让 Worker 直接调用模型，独立 Gateway 能集中实施 BYOK、地域、数据等级、计量、限流和故障转移。
- 相比共享 HMAC，Ed25519 让 Gateway 只有验签公钥，不能自行伪造控制面身份。
- 服务端流式 RPC 足以覆盖单次模型回合；显式双向控制流会增加状态同步复杂度，当前取消通过 HTTP/2
  流关闭传播。
- Protobuf 增加代码生成和版本治理成本，但比 JSON 透传更早发现跨语言契约漂移。

## 已落地证据

- 真实 TCP gRPC + HTTP/SSE 测试证明文本、用量、Tool Call 和完成原因可跨两层流传递。
- 合法签名但租户不匹配的请求以 `PERMISSION_DENIED` 拒绝，且不会访问 Provider。
- Worker 的真实 attempt 取消令牌可关闭 gRPC 流，并在 500ms 门禁内关闭 Provider TCP 连接。
- 测试抓取 Provider 请求证明 API Key 只出现在 Gateway→Provider 请求，工作负载令牌不包含该凭证。
- Worker 要求流中出现 `Completed` 或 `Failed`；网络提前结束不能被误判为成功。

## 尚未完成

- Scheduler/控制面已生成 `model_policy_id` 和工作负载令牌，Worker 已能从执行命令构造
  ModelInvocation；首轮模型生产循环由 ADR-0009 补齐，但 Tool 回合尚未形成完整 Agent Run。
- Model Gateway 生产 `main` 已能从环境配置启动 gRPC 服务；mTLS、健康检查、优雅停机和 Kubernetes
  Service 尚未接线。
- Protobuf 的图片、音频和结构化输出字段尚未映射；当前只完成文本、Tool schema 和 Tool Call。
- 尚未接 Vault/KMS、租户 BYOK、Provider Registry、重试、熔断和候选故障转移。
