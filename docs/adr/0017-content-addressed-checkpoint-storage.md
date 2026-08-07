# ADR-0017：内容寻址 Checkpoint 与存储凭证隔离

## Status

Accepted

## Context

Checkpoint v1 把完整 JSON 快照作为 Base64 放进 JetStream，并以 512 KiB 作为硬上限。Worker 的内部状态还曾以
JSON 数字数组表达二进制，产生多倍膨胀。继续扩大 NATS、PostgreSQL 和 Consumer 的消息上限会同时放大内存、
重投和数据库 WAL 成本，也无法支持长会话。

Codex 通过 compaction checkpoint 替换会话历史，并保证 live/persisted history 一致，rollout 重建和版本兼容成熟；
它解决的是本地 Thread 历史生命周期，不提供跨 Worker 对象交接。OpenClaw 对 overflow compaction、transcript
replay repair 和重启恢复有丰富保护，但主要依赖本机 transcript/SQLite 与 Gateway/Node 生命周期，也没有多租户
对象凭证隔离边界。

## Decision

1. Checkpoint 消息升级为 schema v2。原始 `CheckpointSnapshot` JSON 的 SHA-256 作为语义摘要；Zstd 压缩后字节另有
   stored digest、压缩前后长度和编码元数据。
2. 快照内部二进制状态使用 Base64 JSON 字符串，反序列化继续接受 v1 的数字数组，避免旧 Checkpoint 无法恢复。
3. 压缩后不超过 512 KiB 时继续内联；超过上限时消息只携带
   `checkpoint://sha256/{stored_digest}` 内容寻址引用，最大未压缩大小固定为 16 MiB。Gateway 必须把 token 中的
   tenant 作为对象命名空间的一部分，同一摘要不能绕过租户授权形成跨租户读取。
4. 外部对象必须在 Run Event 与 Checkpoint 发布前写入成功。对象 PUT 必须按摘要幂等；发布失败可以留下无引用对象，
   由生命周期回收，但绝不允许先发布一个尚不存在的引用。
5. 恢复时先校验引用与 stored digest，再做有界解压，然后校验未压缩长度、payload digest、Kernel digest、Run 身份、
   sequence、attempt 与状态。对象暂时不可见或存储不可用时 NAK 重试；内容损坏时终止该恢复消息，不进入 Kernel。
6. PostgreSQL V10 只保存小对象字节或大对象引用，两者必须且只能存在一个；恢复 Outbox 保持引用，不把对象重新
   Base64 展开。
7. Worker 只依赖 `CheckpointPayloadStore` 窄接口。生产实现固定为独立 Rust Checkpoint Gateway，Gateway 独占
   S3/MinIO 凭证并以短期、绑定 tenant/run/attempt/worker incarnation 的工作负载身份授权 PUT/GET。禁止给通用
   Worker、Edge Node 或 Skill 沙箱下发跨租户对象存储长期密钥。

## Consequences

### Positive

- NATS 与 PostgreSQL 的消息/行大小保持有界，Checkpoint 可扩展到长 transcript。
- 内容寻址、双摘要和有界解压能在进入 Kernel 前识别丢失、串租户、损坏和解压炸弹。
- 控制面仍持有权威索引，但不承担大对象数据转发；恢复命令大小与 Checkpoint 大小解耦。
- Worker、Edge Node 和 Tool 沙箱不获得对象存储管理权限。

### Negative

- v1/v2 兼容、对象垃圾回收、保留策略和 Gateway 可用性成为新的运维面。
- 当前仓库已完成协议、Worker 存储接口、V10 索引、独立 Checkpoint Gateway、真实 MinIO 适配、短期令牌
  scope/incarnation 绑定，以及对象缺失/损坏的消息链路故障测试。生产仍缺少 mTLS、令牌刷新、对象生命周期、
  Gateway 高可用和 Kubernetes 节点故障恢复证明，因此不能声称大对象生产闭环已经交付。
- 16 MiB 是 Beta 保护阈值，不等于无限历史；仍需要像 Codex/OpenClaw 一样实现上下文压缩和长期 transcript 分层。

## Alternatives Considered

- **提高 JetStream 最大消息和 PostgreSQL `bytea` 上限**：实现简单，但把长会话成本复制到 Broker、Consumer、WAL
  和每次恢复重投，拒绝采用。
- **Worker 直接持有 S3/MinIO Access Key**：减少一个服务，但云 Worker、边缘设备和租户间的凭证爆炸半径不可接受。
- **把预签名 URL 长期写进 Checkpoint**：URL 会过期且可能进入审计/日志，无法成为稳定恢复引用。
- **按 NATS 分块传输大 Checkpoint**：需要分块重组、过期清理和乱序协议，且重复建设对象存储能力。

## References

- Codex：`codex-rs/core/src/compact.rs`、`compact_remote.rs` 的 compaction checkpoint 与历史替换
- OpenClaw：`src/agents/embedded-agent-runner/replay-history.ts`、`run.overflow-compaction.test.ts`
- 本平台：`runtime/crates/protocol/tests/checkpoint_contract.rs`、`runtime/apps/worker/tests/assignment.rs`
- 本平台：`V10__external_checkpoint_payloads.sql`、`RunCheckpointMessageTest.java`
- 本平台：`runtime/apps/checkpoint-gateway/tests/grpc_contract.rs`、`minio_transport.rs`
- 本平台：`docs/adr/0018-workload-identity-v2-and-checkpoint-gateway.md`
