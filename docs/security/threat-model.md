# 威胁模型基线

优先保护租户数据、模型密钥、Workspace、节点身份和外部 Tool 副作用。

| 威胁 | 控制 |
|---|---|
| 跨租户 ID 猜测 | OIDC TenantContext、复合外键、RLS、对象前缀和否定测试 |
| Worker 被接管 | 短期工作负载令牌、mTLS、owner epoch、无 BYOK 明文 |
| 恶意 Skill | OCI 摘要、签名、SBOM、扫描、权限清单、Kata 沙箱 |
| Tool 重放 | Tool 调用幂等键、副作用分类、模糊结果 fail closed |
| 节点克隆 | 设备绑定私钥、证书轮换、吊销、任务信封过期 |
| 事件伪造/缺失 | mTLS、摘要、单调序号、事件 ID 去重和缺口检测 |
| JetStream 窃听或伪造命令 | TLS 服务端认证、bcrypt 账号、控制面/Worker Subject ACL、Outbox/事件摘要 |
| Worker 越权修改消息拓扑 | Worker 只允许固定 Stream INFO、Consumer、拉取与 ACK；Stream 管理 Subject 服务端拒绝 |
| Pod 读取长期密钥或提权 | Vault CSI、非 root、只读根、RuntimeDefault seccomp、capability 全丢弃、默认拒绝网络 |
| 不健康实例继续接流量 | 存活/就绪分离；Gateway 初始化门禁；Worker NATS 连接状态进入 readiness |
