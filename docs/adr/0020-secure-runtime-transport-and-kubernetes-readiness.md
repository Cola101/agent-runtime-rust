# ADR-0020：安全 Runtime 传输与 Kubernetes 就绪语义

## Status

Accepted

## Context

ADR-0019 已让 Model/Checkpoint gRPC 具备双向 TLS，但 JetStream 仍使用裸 `nats://`，Worker 与控制面没有
角色权限边界；Kubernetes 也只有 Namespace、Kata RuntimeClass 和默认拒绝网络策略。这样的进程可以在本地
运行，却不能证明生产流量只进入已初始化实例，也无法阻止被接管的 Worker 管理 Stream 或发布控制面命令。

Codex 的有限认证恢复适合约束客户端故障行为，但它是本地 Runtime，没有可直接移植的 Kubernetes 高可用
模型。OpenClaw 把存活、启动中、draining、通道健康和 stale transport 分开判断；Node Host 只在事件循环就绪
后宣告 ready，并在终态认证/配对错误时暂停重连。这些语义值得保留，但多租户 PaaS 还需要 JetStream ACL、
稳定 Worker 身份、PDB/HPA 和 Secret Provider。

## Decision

1. 所有 Rust 工作负载暴露独立 HTTP 健康端口。`/live` 只表示进程事件循环可响应；`/ready` 表示当前可以
   接受新流量。依赖短暂故障只撤销 readiness，不能用 liveness 制造重启风暴。
2. Model/Checkpoint Gateway 在完成配置、工作负载公钥、mTLS 材料和 gRPC 监听 Socket 初始化后才 ready。
   Worker 在 NATS 连接成立、Model/Checkpoint mTLS Client 与 Checkpoint Store 初始化后才 ready；NATS 连接
   状态持续映射到 readiness。
3. 生产 Worker 强制使用 `tls://`、服务端 CA 和角色账号。Java 控制面使用 PKCS12 TrustStore；Rust 使用 PEM
   CA。两端配置对象必须验证必填项并在诊断输出中隐藏密码。
4. NATS 服务端只加载由 Vault Agent 或 Secret Controller 渲染的 bcrypt 授权文件。控制面账号可发布控制与
   执行命令、消费 Worker 事件；Worker 账号只可发布 Worker 事件、读取两个固定 Stream、管理自己的 Consumer、
   拉取和 ACK 消息。Worker 禁止创建、更新、删除 Stream，也禁止发布 `runtime.control.>`。
5. 安全 Worker 连接只读取预先创建的 `RUNTIME_EXECUTION` 与 `RUNTIME_WORKER` Stream；开发测试连接可以显式
   初始化拓扑。生产拓扑初始化归控制面/独立 Bootstrap Job，不借用 Worker 权限。
6. Gateway 使用两个起始副本、Service、HPA 和 PDB。Worker 使用三副本 StatefulSet；稳定 Worker UUID 首次
   写入独立 PVC，进程重启复用，文件损坏时 fail-closed。Worker 的 lease-aware draining 后续由 ADR-0021 补齐；
   在真实 Kubernetes eviction 与恢复 SLO 验收前仍不启用 HPA。
7. 所有容器使用非 root、只读根文件系统、RuntimeDefault seccomp、禁止提权并丢弃全部 Linux capability。
   Vault Secrets Store CSI 注入 gRPC/NATS 证书与凭证；敏感环境变量只能引用同步生成的 Kubernetes Secret。
8. Namespace 保持默认拒绝网络策略，仅开放 DNS、NATS、Gateway gRPC、健康端口、Vault 和 Provider HTTPS 的
   必要流量。生产 Overlay 必须把宽泛的 443 出站进一步限制到批准的 Provider/Vault 地址。
9. NATS 配置包含路由端口、集群名、路由双向证书验证和可配置 seed routes，供三节点 JetStream 部署使用。
   本 ADR 只证明配置与客户端互通，不把尚未在真实 Multi-AZ 集群执行的演练声明为完成。

## Consequences

### Positive

- NATS 上的短期工作负载令牌、Run 命令和事件不再明文传输，Worker 越权发布与 Stream 管理在服务端被拒绝。
- Kubernetes 不会把“进程活着但尚未初始化”或“NATS 已断连”的 Worker 当作可接流量实例。
- Worker 身份跨容器重启稳定，而 incarnation 仍按进程启动轮换，不会混淆节点身份和执行代际。
- Gateway 可以滚动维护和横向扩展；PDB 明确限制计划内中断。

### Negative

- Gateway readiness 当前是初始化就绪，不主动探测上游 Provider/S3；深探测若直接绑定 readiness 可能在上游
  抖动时放大故障，后续应增加带滞后和熔断状态的依赖健康模型。
- NATS 和 gRPC 证书轮换目前依赖 Pod 重启，没有热加载或 SPIFFE/SPIRE。
- Worker 的 draining 协议、主动 SIGTERM Checkpoint 和部署时间预算已由 ADR-0021 补齐；真实集群 eviction、
  租约感知 HPA 缩容和 15 分钟恢复 SLO 尚未验收。
- SecretProviderClass、Vault Role、NetworkPolicy 的 CIDR/FQDN 约束和镜像 digest 必须由环境 Overlay 提供，
  Base 清单不能独立代表某个生产集群已验收。

## Alternatives Considered

- **用 TCP Socket 作为 readiness**：不能证明身份材料、Gateway Client 或 NATS 已初始化，拒绝。
- **NATS 断连触发 liveness 失败**：会在 Broker 故障时制造全体 Worker 重启风暴，拒绝。
- **Deployment + 每次随机 Worker ID**：重启会形成新节点并破坏稳定节点审计，拒绝。
- **给 Worker 完整 `$JS.API.>` 权限**：实现方便但可删除 Stream 或更改保留策略，拒绝。
- **立即给 Worker 配 HPA**：没有 draining 和活跃 Run 保护时，缩容可能制造模糊副作用，拒绝。
- **把 bcrypt 密码直接写入主 ConfigMap**：扩大静态配置暴露面，改为 Vault 渲染独立授权文件。

## References

- Codex：`codex-rs/core/src/client.rs`、`codex-rs/core/src/responses_retry.rs`
- OpenClaw：`src/gateway/server/readiness.ts`、`src/gateway/server/channel-health-policy.ts`
- OpenClaw：`src/node-host/runner.ts`
- 本平台：`runtime/crates/runtime-health/tests/http_health.rs`
- 本平台：`runtime/crates/nats-security/tests/config.rs`、`live_tls.rs`
- 本平台：`runtime/apps/worker/tests/worker_identity.rs`
- 本平台：`deploy/tests/validate_kubernetes.rb`、`verify_nats_tls.sh`
