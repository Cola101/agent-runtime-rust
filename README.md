# Agent Runtime Rust

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-technical%20alpha-orange.svg)](docs/implementation-status.md)

A protocol-neutral, multi-tenant Agent Runtime written primarily in Rust. It is designed to let
SaaS teams embed durable agent execution without coupling their applications to one model provider
or requiring a heavyweight control plane. The Rust runtime owns the agent loop, model routing,
Tool/MCP execution, approvals, checkpoints, recovery, subagents, budgets, and tenant-aware admission.

The same headless runtime can be embedded behind Java applications or used by standalone CLI,
desktop, cloud, and edge clients. The included Java control plane is a reference integration, not a
runtime dependency. Model adapters currently cover OpenAI Responses, Anthropic Messages, and
OpenAI-compatible Chat Completions through a shared model IR.

> **Status:** technical alpha. The repository currently lists 721 Rust tests: 715 pass and 6
> external live tests are explicitly ignored. It contains executable runtime paths,
> tests, architecture decisions, threat modeling, and evidence records, but it is not yet production
> ready and does not claim 1,000 active Runs, 99.9% availability, or SOC 2 certification. See the
> [implementation status](docs/implementation-status.md) for verified and unverified boundaries.

## Why this project

- **Multi-tenant by design:** tenant, application, workspace, workload identity, budget, and
  admission boundaries are part of runtime semantics rather than UI conventions.
- **Provider-neutral execution:** one typed model IR and explicit safe-fallback rules avoid binding
  the agent loop to a single vendor protocol.
- **Durable and auditable:** event sequencing, checkpoints, approvals, fencing, side-effect
  classification, and `indeterminate` outcomes make failure handling explicit.
- **Embeddable and standalone:** the Rust host completes a Run without Java, Docker, Kubernetes,
  PostgreSQL, or NATS; external control planes integrate through versioned contracts.
- **Evidence over claims:** implemented milestones link to tests, ADRs, and evidence, while known
  gaps remain public.

## Runtime quick check

```bash
cd runtime
cargo test --workspace
```

Local development does not require Docker. Cargo's `target/` is a reusable cache of compiled
dependencies and artifacts, retained by default (while remaining ignored by Git); clean it only after
measuring when the cache is corrupt, the toolchain changes materially, or disk pressure requires it.
Note that *incremental* compilation is deliberately off (`incremental = false` on the `dev` and
`test` profiles in `runtime/Cargo.toml`), traded against smaller, more reproducible artifacts; a cold
`target/` therefore rebuilds the workspace in full. All project-local runtime state is confined to
the ignored `.local/` directory.

Governance: [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [License](LICENSE) ·
[Third-party sources](docs/third-party-sources.md)

---

## 中文说明

面向外部客户的多租户 Agent Runtime PaaS。仓库当前交付第一条可运行纵向主链：

项目唯一目标、固定实施顺序和本地运行边界见 [`docs/project-goal.md`](docs/project-goal.md)。

- Java 21 / Spring Boot 控制面；
- Rust Agent Kernel、Worker、Model Gateway、Checkpoint Gateway 与 Edge Node；
- Vue 3 / TypeScript 管理控制台；
- PostgreSQL、NATS JetStream 原生本地依赖与文件系统 Checkpoint 开发后端；
- REST + SSE 公共契约和 gRPC 内部节点契约。

## 仓库结构

| 目录 | 责任 |
|---|---|
| `control-plane/` | IAM 声明校验、资源 API、Run/Approval、Outbox 和 SSE |
| `runtime/` | Rust Kernel、统一事件 IR、Worker、模型网关和边缘节点 |
| `console/` | 管理控制台与 Console BFF 消费端 |
| `contracts/` | OpenAPI 与 Protobuf 真相文件 |
| `deploy/` | macOS 原生开发生命周期与生产 Kubernetes 基线 |
| `docs/` | 架构、ADR、威胁模型和第三方来源 |

## 本地验证

```bash
make test
make check
make check-native-recovery-live # 强杀 Worker 后恢复，并由真实 Chrome 完成审批/Tool
```

本地开发完全不依赖 Docker、虚拟机或 Kubernetes。Java、Rust、Vue、PostgreSQL 和 NATS 均以
macOS ARM64 原生进程运行；所有 PID、日志、数据和临时密钥只能写入仓库内 `.local/`。

首次启动需要提供模型协议、Endpoint、模型名和凭证；协议支持 `openai_compatible`（默认）、
`openai_responses` 与 `anthropic_messages`。凭证只保存于 `.local/secrets`，不会写入日志或
仓库。完整原生运行时已通过确定性回环模型完成模型→可信 Tool→持久审批→文件/内联 Checkpoint→
强杀 Worker→新实例恢复→真实 Chrome 审批→Tool 结果回灌的真实进程主链；外部真实模型验收尚未完成：

```bash
export AGENT_RUNTIME_PROVIDER_ENDPOINT='https://provider.example/v1/chat/completions'
export AGENT_RUNTIME_PROVIDER_MODEL='model-name'
export AGENT_RUNTIME_PROVIDER_API_KEY='secret'
AGENT_RUNTIME_RUN_INPUT='解释 Workspace fencing' make dev-run
                           # 自动引导依赖、构建全部原生进程、创建 Run 并持续输出事件直到终态
make dev-native-bootstrap  # 可选：只预热项目级原生依赖
make dev                   # 只构建并启动 Java、Rust、Vue、PostgreSQL 与 NATS
deploy/native/run-local '继续提交一个 Run' # 复用已经运行的服务
make dev-approve APPROVAL_ID='<事件中的 uuid>' # 默认仅批准一次；也可 DECISION=deny
make dev-status            # 查看全部项目进程状态
make dev-down              # 停止全部项目原生进程
make dev-clean             # 停止并删除 .local、日志、密钥、构建及测试产物
```

脚本会自动读取 macOS 系统代理（包括本机 `127.0.0.1:10808`）；`AGENT_RUNTIME_DOWNLOAD_PROXY`
可显式覆盖。后续启动会复用项目内已保存的模型配置，无需再次导出；`dev-clean` 会删除它。
`dev-run` 只连接回环控制面，开发 JWT 不进入命令参数、日志或浏览器；SSE 在明确终态前中断会返回失败，
不会把 HTTP 202 当作 Agent 已完成。`make test-java` 会启动临时原生 PostgreSQL/NATS，最近一次执行 137 个测试后
自动删除测试进程、端口和运行态；
测试 JVM 的回环连接不会经过 10808。本地 JetStream 最多使用 256 MiB 内存 Store 和 1 GiB 文件 Store。

启动任一控制面进程前必须注入 Prometheus 专用抓取密码：

```bash
export MANAGEMENT_SCRAPE_PASSWORD='<本地或 Vault 注入的强密码>'
```

生产控制面 API 默认使用 8080；macOS 原生开发入口默认使用 18080，避免与常见本机服务冲突；健康检查与
Prometheus 使用独立的 9090 管理端口。健康探针允许匿名访问，
`/actuator/prometheus` 只接受 `MANAGEMENT_SCRAPE_USERNAME`（默认 `metrics-scraper`）及上述密码，
不复用外部用户 OIDC/JWT。生产环境还必须在网络层隔离管理端口并启用受保护传输。

启用控制面的 Outbox 发布器：

```bash
cd control-plane
mvn spring-boot:run \
  -Dspring-boot.run.arguments="--agent.runtime.outbox.enabled=true --agent.runtime.outbox.nats-url=nats://127.0.0.1:4222"
```

发布器把 `run.queued` 写入 JetStream 的 `RUNTIME_CONTROL` Stream，Subject 为
`runtime.control.run.queued.v1`。数据库消息使用短期 claim lease 支持多实例并发；只有收到
JetStream PubAck 后才标记已发布，重试使用 Outbox ID 作为 NATS 消息 ID 去重。

启用独立 Scheduler 进程：

```bash
cd control-plane
AGENT_RUNTIME_WORKLOAD_IDENTITY_PRIVATE_KEY_PKCS8='<Vault 注入的 Ed25519 PKCS#8 DER Base64>' \
mvn spring-boot:run \
  -Dspring-boot.run.arguments="--agent.runtime.scheduler.enabled=true --agent.runtime.scheduler.nats-url=nats://127.0.0.1:4222"
```

Scheduler 只持有签名私钥。Model Gateway 与 Checkpoint Gateway 使用对应 Ed25519 公钥的 32 字节原始编码
Base64，不能配置签名私钥。开发和生产密钥都应由 Vault/KMS 或 Secret Provider 注入，不提交到仓库。

启动 Model Gateway（单实例当前选择一个协议和 Endpoint）：

```bash
cd runtime
AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY='<Ed25519 32 字节公钥 Base64>' \
AGENT_RUNTIME_PROVIDER_PROTOCOL='openai_responses' \
AGENT_RUNTIME_PROVIDER_ENDPOINT='https://api.openai.com/v1/responses' \
AGENT_RUNTIME_PROVIDER_MODEL='model-name' \
AGENT_RUNTIME_PROVIDER_API_KEY='<Vault 注入>' \
AGENT_RUNTIME_GRPC_SERVER_CERT=/var/run/secrets/runtime/tls.crt \
AGENT_RUNTIME_GRPC_SERVER_KEY=/var/run/secrets/runtime/tls.key \
AGENT_RUNTIME_GRPC_CLIENT_CA_CERT=/var/run/secrets/runtime/client-ca.crt \
cargo run -p agent-model-gateway
```

第三方兼容服务使用 `openai_compatible` 和 `/v1/chat/completions`；Anthropic 使用：

```bash
cd runtime
AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY='<Ed25519 32 字节公钥 Base64>' \
AGENT_RUNTIME_PROVIDER_PROTOCOL='anthropic_messages' \
AGENT_RUNTIME_PROVIDER_ANTHROPIC_VERSION='2023-06-01' \
AGENT_RUNTIME_PROVIDER_ENDPOINT='https://api.anthropic.com/v1/messages' \
AGENT_RUNTIME_PROVIDER_MODEL='model-name' \
AGENT_RUNTIME_PROVIDER_API_KEY='<Vault 注入>' \
AGENT_RUNTIME_GRPC_SERVER_CERT=/var/run/secrets/runtime/tls.crt \
AGENT_RUNTIME_GRPC_SERVER_KEY=/var/run/secrets/runtime/tls.key \
AGENT_RUNTIME_GRPC_CLIENT_CA_CERT=/var/run/secrets/runtime/client-ca.crt \
cargo run -p agent-model-gateway
```

启动 Checkpoint Gateway（本地开发使用项目目录内的内容寻址文件系统）：

```bash
cd runtime
AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY='<Ed25519 32 字节公钥 Base64>' \
AGENT_RUNTIME_CHECKPOINT_LOCAL_DIR="$PWD/../.local/state/checkpoints" \
AGENT_RUNTIME_GRPC_SERVER_CERT=/var/run/secrets/runtime/tls.crt \
AGENT_RUNTIME_GRPC_SERVER_KEY=/var/run/secrets/runtime/tls.key \
AGENT_RUNTIME_GRPC_CLIENT_CA_CERT=/var/run/secrets/runtime/client-ca.crt \
cargo run -p agent-checkpoint-gateway
```

本地文件系统模式不需要 S3/MinIO 凭证；生产长期对象存储凭证仍只进入 Checkpoint Gateway，不得下发给
Worker、Edge Node 或 Tool/Skill 沙箱。

启动 Rust Cloud Worker（Worker ID 必须由控制面稳定分配，不能每次启动随机生成）：

```bash
cd runtime
AGENT_RUNTIME_WORKER_ID=0198a5a6-a7a8-7def-8abc-0123456789b2 \
AGENT_RUNTIME_NATS_URL=tls://127.0.0.1:4222 \
AGENT_RUNTIME_NATS_USERNAME=runtime-worker \
AGENT_RUNTIME_NATS_PASSWORD='<Vault 注入的客户端明文密码>' \
AGENT_RUNTIME_NATS_CA_CERT=/var/run/secrets/runtime/nats-ca.pem \
AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY='<Ed25519 32 字节公钥 Base64>' \
AGENT_RUNTIME_MODEL_GATEWAY_ENDPOINT=https://127.0.0.1:50051 \
AGENT_RUNTIME_CHECKPOINT_GATEWAY_ENDPOINT=https://127.0.0.1:50052 \
AGENT_RUNTIME_GRPC_CLIENT_CERT=/var/run/secrets/runtime/worker.crt \
AGENT_RUNTIME_GRPC_CLIENT_KEY=/var/run/secrets/runtime/worker.key \
AGENT_RUNTIME_GRPC_SERVER_CA_CERT=/var/run/secrets/runtime/server-ca.crt \
AGENT_RUNTIME_MODEL_GATEWAY_TLS_DOMAIN=model-gateway.agent-runtime.svc \
AGENT_RUNTIME_CHECKPOINT_GATEWAY_TLS_DOMAIN=checkpoint-gateway.agent-runtime.svc \
cargo run -p agent-runtime-worker
```

Scheduler 只在健康 Worker 当前启动实例有容量且 Workspace 写租约可取得时生成执行命令。稳定
Worker ID 由控制面分配，进程每次启动生成独立 incarnation ID；命令按二者组成的唯一 Subject
投递，并携带 `attempt_id`、`owner_epoch` 与 `fencing_token`。Worker 在持久化
accepted 和 Kernel `run.started` 事件后才确认执行命令。Worker 后续心跳携带活动 assignment，
控制面同时核验启动实例与完整 fencing 身份后续租。Reconciler 默认每秒收敛过期 dispatch：未 accepted 的
任务重新排队；已 accepted 的任务若存在与最新事件、状态和旧租约一致的安全 Checkpoint，则派发到
其他健康 Worker，或同一稳定 Worker 的新启动实例，并签发新 attempt、递增 owner epoch、轮换 fencing
token 和工作负载身份。旧实例因 Subject、accepted 与续租均绑定 incarnation，不能争抢新命令。无安全
Checkpoint 或存在模糊非幂等副作用时进入 `indeterminate`，不会从原输入重跑。

Worker 确认 dispatch 后由独立 Execution Supervisor 异步执行模型 RPC；主循环继续接收取消，并把
模型事件串行交给 Kernel 分配 sequence。终态事件获得 JetStream PubAck 后才释放 Worker 容量。
AgentVersion 的 `delegated_scopes` 与有序 SkillVersion 快照随 dispatch 固化。SkillVersion 由控制面
Ed25519 签名；Worker 在接单前验证摘要、签名、平台和最低 Runtime 版本，只向模型公开 Skill 声明、
预装可信目录与 delegated scope 的交集。Skill 不能扩权。内部已能
保持 Tool Call ID、执行 allow/deny/ask、校验审批绑定并把 Tool Result 组装成下一轮 ModelInvocation；
控制面持久审批与定向 Worker 恢复已接入。Worker 现已具备受限 OCI 容器执行边界，并在 Tool Result
获得 JetStream PubAck 后自动发起下一模型回合。本地另有显式启用的 `workspace.read_text` 可信原生制品：
固定二进制摘要、无 Shell、清空环境、只读 Workspace、64 KiB 上限且默认逐次审批。它不是强沙箱，不能
执行租户上传代码。当前仍缺 OCI/SBOM/恶意扫描驱动的任意 Skill 制品装载、Kubernetes/Kata Provider，
以及 Shell、写文件、受控 HTTP、MCP 的正式制品。

Tool 执行另有 PostgreSQL 权威账本记录 `planned → started → completed`。Worker 只有在
`tool.execution.started` 获得 JetStream PubAck 后才启动外部进程；若非幂等调用启动后 Worker
丢失，Reconciler 会把具体调用摘要写入 `indeterminate`，不会从原输入盲目重放。

Worker 现可生成 v2 Checkpoint，保存 Kernel sequence、Protobuf transcript、待处理 Tool、待审批状态和
Tool Catalog 摘要，并可在新 attempt、更高 owner epoch 和新 fencing token 下恢复。PostgreSQL V10
只把与最新持久事件及原租约完全一致、且不存在模糊非幂等副作用的 Checkpoint 判为 `SAFE`。Worker
以 JetStream PubAck 作为 Checkpoint 持久化屏障；控制面消费后由 Reconciler 定向派发恢复命令，新
Worker 发布 `run.restored`，对 replay-safe Tool 重新规划，对待审批调用发布 `approval.rebound` 后才
确认命令。快照经 Zstd 压缩和双摘要校验：压缩后不超过 512 KiB 时内联，更大对象先通过独立
Checkpoint Gateway 写入 MinIO/S3，再在 JetStream 与 PostgreSQL 中保存内容寻址引用。工作负载令牌 V2
绑定 tenant/run/attempt/worker/incarnation、Gateway audience 和操作 scope；对象缺失会延迟重试，内容损坏会
永久终止该恢复消息。控制面现按持久化 generation 主动续期，401 等待更高代际且每代最多恢复一次；
Model/Checkpoint gRPC 强制双向 TLS。NATS 生产入口强制 TLS、bcrypt 服务端认证和控制面/Worker Subject ACL；
Kubernetes Base 已提供双副本网关、三副本稳定身份 Worker、健康探针、HPA/PDB 和 CSI 密钥注入。当前仍缺
主动吊销、证书自动轮换、对象生命周期、真实云集群滚动维护与生产故障演练。

生产 Kubernetes 清单与真实 NATS 安全验证不属于本地开发命令图：

```bash
make check-deploy
make check-nats-live
```

生产环境必须给发布进程和 Scheduler/Reconciler 使用彼此独立的数据库身份。Outbox 身份只
允许操作 Outbox；Reconciler 身份只允许扫描和收敛过期 dispatch。两者需要受控的跨租户
能力，普通 API 数据库身份不得获得 `BYPASSRLS`。

## 当前边界

当前版本是技术 Alpha 骨架，不宣称已经达到 1000 活跃 Run、99.9% 可用性或 SOC 2 认证。上述目标必须通过 `docs/architecture/acceptance.md` 中的证据门禁后才能对外声明。

逐模块完成度与未实现边界见 `docs/implementation-status.md`。
每阶段 Codex CLI / OpenClaw 对标见 `docs/reference-comparison.md`。
