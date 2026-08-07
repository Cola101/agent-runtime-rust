# ADR-0008：模型策略归属与调度身份签发

## 状态

Accepted

## 决策

Run 必须显式引用不可为空的 `model_policy_id`。数据库使用
`(tenant_id, workspace_id, model_policy_id)` 外键，确保模型策略不仅属于同一租户，也属于 Run 的同一
Workspace。`model_policy_id` 必须进入 `run.queued` 和 `run.execution.requested` 的不可变执行快照。

Scheduler 取得 Worker、attempt 和 Workspace 租约后，才使用控制面 Ed25519 私钥签发工作负载令牌。
令牌绑定 tenant、Run、attempt、Worker 和模型策略，有效期取 Workspace 租约到期时间与五分钟上限的
较早者。可靠投递需要把短期令牌保存在 Outbox；应用日志和 Java/Rust 诊断输出必须脱敏。

## 理由与取舍

- 在调度前签发会缺少最终 Worker、attempt 和租约，授权范围过宽。
- 只使用 `(tenant_id, model_policy_id)` 外键仍允许同租户跨 Workspace 误绑定，不满足项目资产边界。
- Outbox 中的 bearer capability 是受控风险；短有效期、数据库访问控制、日志脱敏和后续密钥轮换共同
  降低泄漏影响，但不能替代数据库加密与审计。
- 当前令牌覆盖一次租约窗口。长 Run 和审批恢复需要令牌刷新协议，不能复用过期令牌。

## 已验证

- PostgreSQL V5 迁移启用 ModelPolicy RLS，并用否定测试拒绝跨 Workspace 策略绑定。
- REST、Run、`run.queued`、Scheduler 和执行命令保持同一个 `model_policy_id`。
- Scheduler 集成测试解码令牌并核对五元身份；签名单元测试验证 Ed25519 签名和最长有效期。
- Rust 执行契约拒绝缺失策略或畸形令牌，`WorkloadToken` 的 Debug 输出固定为脱敏文本。
- 真实网络测试从 dispatch 形态命令构造 ModelInvocation，经 gRPC Gateway 调用 HTTP/SSE Provider。

## 尚未完成

- 私钥仍通过启动配置注入，尚未接入 Vault/KMS、轮换和公钥版本 `kid`。
- Worker 首轮模型生产循环已由 ADR-0009 补齐；Tool 执行和后续模型回合仍未实现，跨 Java/Rust 的
  全产品链仍需要部署级验收。
- 令牌刷新与 gRPC mTLS 后续已由 ADR-0019 补齐；Provider Registry、租户 BYOK 和策略内容解析仍未实现。
