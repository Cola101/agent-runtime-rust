# ADR-0019：主动工作负载身份续期与 gRPC 双向 TLS

## Status

Accepted

## Context

ADR-0018 将模型和 Checkpoint 访问收敛到最长五分钟的 Ed25519 工作负载身份，但没有续期协议，因而无法支撑
长 Run；数据面 gRPC 也只有应用层 Bearer Token，没有服务进程身份。直接延长 Token 会扩大泄漏窗口，Worker
自行刷新又需要持有控制面签名能力，均不接受。

Codex 的认证管理器会在到期窗口前主动刷新，并对 401 采用有限状态恢复而不是无限重试。OpenClaw 的 Node
连接和待处理调用以配对代际隔离，旧连接或旧代际不能清理、恢复新调用。多租户 Runtime 还必须保留 PostgreSQL
权威状态、Outbox、Worker incarnation 与 Workspace fencing，不能只在进程内换 Token。

## Decision

1. `run_dispatches` 持久化 `workload_identity_generation` 与 `workload_identity_expires_at`。初始 dispatch 为
   generation 1；只有 accepted 且完整 tenant/run/attempt/worker/incarnation/owner epoch/fencing 仍匹配的
   assignment 才能续期。
2. Worker 心跳始终续租 dispatch 与 Workspace；仅当身份剩余时间进入租约时长的一半时，控制面原子递增代际、
   签发不超过五分钟的新 V2 Token，并在同一事务写入 `workload.identity.renewed` Outbox。消息按目标 Worker
   incarnation 的 `identity.v1` Subject 投递。
3. 续期命令必须携带 Token 全部执行绑定、generation、签发时间和新租约到期时间。命令与签名 Token 必须共享
   同一个显式 `issued_at`；签发器不得在内部再次读取时钟，否则毫秒级差异也会破坏绑定。Worker 使用控制面公钥重新
   验签，并要求 `model.execute`、`checkpoint.read`、`checkpoint.write` 三项现有能力、ModelPolicy 和命令时限
   精确一致后，才原子替换活动 attempt 的 Token 与本地租约。
4. generation 只能前进；精确重复投递返回幂等 Duplicate，不触发第二次恢复；旧代际、不同 incarnation、旧
   owner epoch/fencing、错误签名或能力缺失均永久拒绝。
5. Model Gateway 返回 `Unauthenticated` 时，Execution Supervisor 不产生 Run 终态，而是等待更高 generation。
   新身份应用后只恢复一次模型调用；同代际重复消息不能再次触发。`PermissionDenied` 仍视为不可恢复的授权错误。
6. Model Gateway 与 Checkpoint Gateway 启动时必须加载服务端证书、私钥和客户端 CA；Worker 必须加载客户端
   证书、私钥、服务端 CA 及各 Gateway DNS 名。服务端要求可信客户端证书，Worker 验证服务端 CA 和名称。
   mTLS 证明服务进程身份，短期 Token 继续承担每个 Run 的细粒度授权，两者不能互相替代。
7. 证书和 Token 签名私钥只通过 Vault/KMS/Secret Provider 注入文件或受控环境，不写日志和仓库。当前阶段不把
   证书自动轮换或 Token 主动吊销误报为已完成。
8. 原生恢复门禁必须观察到至少一次有效身份续期，并在日志出现命令与 Token 时间绑定不一致时失败，避免把偶发
   同毫秒成功误判为协议正确。

## Consequences

### Positive

- 24 小时 Run 不再依赖长寿命 Bearer Token；泄漏窗口仍限制在五分钟以内。
- PostgreSQL 代际、Outbox 去重、Worker incarnation 与 Workspace fencing 形成跨重启的续期证据链。
- 401 竞态不会直接终止 Run，也不会因旧/重复续期形成无限认证重试。
- 命令与签名声明使用同一权威时间，身份续期不再依赖两个时钟读取恰好落在同一毫秒。
- gRPC 同时具备服务身份、链路加密和每 Run 授权；无客户端证书及错误 CA 在业务处理前被拒绝。

### Negative

- Outbox/JetStream 载荷含短期 Bearer Token；NATS TLS 与角色权限已经验证，但生产签名密钥自动轮换和主动撤销仍是缺口。
- 当前用心跳驱动续期，极端心跳阻塞可能等到 401 后才恢复；尚无独立续期定时器和抖动控制。
- 证书仍由外部系统发放，尚未实现自动轮换、吊销列表、SPIFFE/SPIRE 或节点证明。
- 同一 dispatch Token 仍包含两个 Gateway 的三项能力，尚未细分到单次操作令牌。

## Alternatives Considered

- **把 Token 有效期延长到 Run 上限**：实现简单但泄漏窗口不可接受，拒绝。
- **Worker 持签名私钥自行续期**：破坏控制面授权边界，任一 Worker 泄漏可伪造其他任务身份，拒绝。
- **401 后无条件立即重试**：可能用同一过期 Token 形成重试风暴，改为等待更高 generation。
- **只做 mTLS，不保留 Token**：只能证明 Worker 服务身份，不能表达租户、Run、attempt、ModelPolicy 和 scope，拒绝。
- **只做 Token，不做 mTLS**：无法验证数据面服务进程并缺少链路级双向认证，拒绝。

## References

- Codex：`codex-rs/login/src/auth/manager.rs`、`codex-rs/core/src/client.rs`
- OpenClaw：`src/gateway/server/ws-connection/connect-session.ts`、`src/gateway/node-runtime-state.ts`
- 本平台：`runtime/apps/worker/tests/assignment.rs`、`model_gateway_transport.rs`、`transport.rs`
- 本平台：`runtime/apps/checkpoint-gateway/tests/grpc_contract.rs`
- 本平台：`control-plane/.../JdbcSchedulerRepositoryIntegrationTest.java`
