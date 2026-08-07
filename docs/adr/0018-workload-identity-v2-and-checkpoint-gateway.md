# ADR-0018：工作负载身份 V2 与独立 Checkpoint Gateway

## Status

Accepted

## Context

ADR-0017 已规定大 Checkpoint 通过内容寻址对象存储交接，并禁止通用 Worker 持有 S3/MinIO 长期凭证。
原工作负载令牌只服务 Model Gateway，未包含 Worker 进程启动实例，也没有 audience/scope，无法安全复用于
Checkpoint 读写。若分别在两个 Gateway 实现令牌解析，签名、时限和身份校验容易漂移。

Codex 的主要信任边界是单机进程、Workspace 与本地 sandbox；OpenClaw 的 Gateway/Node 协议有成熟的设备连接和
命令复核，但两者都没有直接解决多租户 PaaS 中“无状态 Worker 访问租户对象存储、又不获得存储密钥”的问题。

## Decision

1. 控制面签发 Ed25519 `v2` 工作负载令牌。Claims 固定包含 tenant、Run、attempt、稳定 Worker、
   `worker_incarnation_id`、ModelPolicy、audiences、scopes、签发时间和过期时间；最长有效期为五分钟。
2. Rust `agent-workload-identity` 作为 Model Gateway 与 Checkpoint Gateway 的唯一验签实现。Gateway 只有公钥，
   同时校验签名、版本、时限、目标 audience、操作 scope 和非空 incarnation。
3. 每个数据面请求除 Bearer Token 外，还必须携带 tenant/run/attempt/worker/incarnation 绑定；Claims 与请求必须
   精确相等。仅知道另一个租户、Run 或旧 Worker 实例的对象引用不能获得授权。
4. 新建独立 Rust Checkpoint Gateway，提供版本化 gRPC PUT/GET。Gateway 验证
   `checkpoint://sha256/{digest}`、大小和内容摘要后，才访问 S3 兼容对象存储；对象路径固定在
   `tenants/{tenant}/runs/{run}/checkpoints/{digest}.zst`。
5. 只有 Checkpoint Gateway 进程接收 S3 endpoint、bucket、access key 与 secret。Worker 只接收 Gateway 地址和
   当前 dispatch 的短期工作负载令牌；Tool/Skill 沙箱不获得令牌或对象存储凭证。
6. 对象不存在映射为 gRPC `NotFound`，存储不可用映射为 `Unavailable`，摘要损坏映射为 `DataLoss`。Worker 对前
   两类延迟 NAK，对损坏永久终止恢复消息，禁止把不可信快照交给 Kernel。
7. 本地环境用 MinIO 验证真实 S3 适配。工作负载身份续期与 gRPC mTLS 后续由 ADR-0019 补齐；生产仍必须补
   密钥/证书轮换、Gateway 高可用、对象保留/垃圾回收和存储中断演练。

## Consequences

### Positive

- Worker、Edge Node 与沙箱不持有跨租户对象存储长期密钥，凭证爆炸半径收敛在专用 Gateway。
- Model 与 Checkpoint 两条数据面共用相同的身份、时限和能力校验，不会出现两套安全语义。
- Worker 重启后旧 incarnation 的令牌和请求绑定不能操作新实例任务，补强 Broker subject 与数据库 fencing。
- 内容摘要在 Worker、Gateway 和对象存储读取后三次验证，损坏不会静默进入恢复状态机。

### Negative

- 当前控制面对一个 Worker dispatch 同时授予 model.execute、checkpoint.read 和 checkpoint.write；已按 Gateway
  audience 隔离，但还不是逐操作临时授权。
- 最长五分钟令牌的刷新与 gRPC mTLS 已由 ADR-0019 接续；主动吊销、签名密钥轮换和逐操作授权仍未实现。
- Checkpoint Gateway 成为新的可用性依赖，需要 HPA/PDB、健康检查、限流、审计和对象生命周期治理。

## Alternatives Considered

- **Worker 直接使用 S3 Access Key**：组件更少，但任何 Worker/边缘设备泄漏都会扩大到 Bucket 或多个租户，拒绝。
- **把预签名 URL 放进恢复消息**：URL 会过期并可能进入日志，无法作为长期、可重放的权威引用，拒绝。
- **Model Gateway 兼任对象存储代理**：凭证仍可隔离，但把模型出口与大对象吞吐、故障域和扩缩容耦合，拒绝。
- **每个 Gateway 独立解析自定义令牌**：初期改动小，但版本和安全规则必然漂移，改用共享 Rust crate。

## References

- 本平台：`runtime/crates/workload-identity/tests/token_contract.rs`
- 本平台：`runtime/apps/checkpoint-gateway/tests/grpc_contract.rs`、`minio_transport.rs`
- 本平台：`runtime/apps/worker/tests/checkpoint_gateway_transport.rs`、`transport.rs`
- 本平台：`control-plane/.../Ed25519WorkloadTokenIssuerTest.java`
