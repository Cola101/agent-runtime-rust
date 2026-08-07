# 实施状态

更新时间：2026-08-07

本文件区分“已实现并有证据”“仅有契约或骨架”“尚未实现”，避免把六个月目标误报为当前能力。

## 进度（2026-08-07 重算）

此前这里写着「私有 Beta 约 **96%**」，更新时间停在 08-02。那个数字**不可信**，已作废：
它是按「已列出的验收项覆盖率」算的，而那份验收项清单本身没有覆盖工具面、MCP、桌面客户端和
规模验证。分母定小了，分子自然好看。

重算，三个不同坐标系，**不要混用**：

| 坐标系 | 估计 | 依据 |
| --- | --- | --- |
| 十一项路线（`docs/project-goal.md` 后续顺序） | **约 40%** | runtime-host 与本地 IPC/恢复完成（1、2）；第 9 项落了「写文件 + 容器边界 + Shell」 |
| Codex 已发布能力面 | **约 25–30%** | 工具面 3 : 约 18；MCP 0；并行调度 0；会话历史重建、Compaction/Fork/Rollback 均无 |
| 私有 Beta 可交付 | **约 70%** | 缺桌面客户端、命令白名单、Linux 容器化、真实厂商验收、调用方认证与配额 |

三个数字差距大是正常的，因为分母不同。**引用时必须带上坐标系**，单说一个百分比没有意义。

拉低最多的两项是**工具面**和 **MCP**——它们决定「Agent 能干多少事」，而此前的投入几乎都在
「Agent 跑得稳不稳」上。稳定性与安全性上本项目局部已超过 Codex（多租户、fencing、签名 Skill、
更严的命令环境隔离），能力广度上差一个数量级。

### 实测计数（本次重算时执行，非引用历史）

```
cargo test --workspace              274 通过 / 0 失败
deploy/native/run-java-tests        143 通过 / 0 失败 / 1 跳过
ADR                                 38 份
证据文件                            16 份
常驻 live 门禁                      5 条
模型可见工具                        3 个（workspace.read_text / workspace.write_text / shell.exec）
```

Console 与完整 Console E2E 本次**未**重新执行，因此不在上表中，也不得据此声称其状态。

## 已实现并验证

| 模块 | 当前能力 | 证据 |
|---|---|---|
| Rust Kernel | Run 状态机、终态保护、审批暂停/恢复、非幂等模糊结果、事件序号；累计 Token/费用预算越界形成不可重试的分类终态 | `runtime/crates/kernel/tests/run_state_machine.rs` |
| Runtime IR | 供应商无关模型请求/流事件、错误分类、Tool 描述、assistant Tool Call / tool result 消息、事件信封 | `runtime/crates/protocol/tests/model_ir.rs`、`runtime/apps/model-gateway/tests/openai_compatible.rs` |
| Tool 策略 / Checkpoint | AgentVersion delegated scope 快照、allow/deny/ask、隔离等级、调用与实现摘要联合绑定；审批携带完整策略快照、策略摘要和不含 call ID 的 Session scope 摘要；带摘要的检查点恢复 | `runtime/crates/kernel/tests/tool_registry.rs`、`runtime/apps/worker/tests/assignment.rs`、`checkpoint.rs` |
| Model Routing / Provider Registry | 地域、数据等级、能力、健康和预算过滤；租户级三协议 Provider、写入即封装的 BYOK、最多 8 个有序候选；只在尚无输出且 429/超时/不可用时切换，认证/账单/协议错误或部分输出后禁止重放 | `ADR-0028`、`provider_registry.rs`、`failover.rs`、对应 Rust/Java 集成测试 |
| OpenAI-compatible Adapter | 真实 HTTP/SSE、文本与 Tool Call 流、Tool Call/Result 多轮历史、用量计费、完成原因、取消、双层超时和错误分类；凭证及私有对象 URI 出口受控 | `runtime/apps/model-gateway/tests/openai_compatible.rs` |
| OpenAI Responses Adapter | typed input items、Function Call/Output 历史、`text.format` 结构化输出、reasoning effort、typed SSE、Tool Call、用量计费和严格终止事件 | `runtime/apps/model-gateway/tests/openai_responses.rs` |
| Anthropic Messages Adapter | `system`/Messages 转换、`x-api-key` 与版本头、Tool Use/Result 历史、分片 Tool JSON、用量计费、stop reason 和严格 `message_stop` | `runtime/apps/model-gateway/tests/anthropic_messages.rs` |
| Provider 协议选择 | Gateway 和原生 Supervisor 通过稳定协议名选择三类 Adapter；旧本地配置缺少协议时兼容默认 Chat Completions | `runtime/apps/model-gateway/tests/provider_protocol.rs`、`deploy/tests/native_supervisor_lifecycle_test.rb` |
| Worker→Model Gateway | 版本化 gRPC 流、Ed25519 短期身份、tenant/run/attempt/worker/incarnation 精确绑定；v3 身份把不可变 ModelPolicy 快照摘要绑定到 RunExecution v4，Worker 只能转发密文，不能替换端点或解密 BYOK；真实双向 TLS、流式事件和取消；401 每代最多恢复一次 | `contracts/proto/model_gateway.proto`、`runtime/crates/workload-identity/tests/token_contract.rs`、`provider_registry.rs`、`model_gateway_transport.rs` |
| Checkpoint Gateway | 独立 Rust gRPC 数据面；Worker 只持短期工作负载令牌，Gateway 独占 S3/MinIO 凭证；内容地址、大小、tenant/run/attempt/worker/incarnation 和 read/write scope 均验证；真实 mTLS、MinIO PUT/GET、跨租户/旧实例拒绝、对象缺失与损坏恢复分流已测试 | `contracts/proto/checkpoint_gateway.proto`、`runtime/apps/checkpoint-gateway/tests/grpc_contract.rs`、`minio_transport.rs`、`runtime/apps/worker/tests/checkpoint_gateway_transport.rs`、真实 NATS `transport.rs` |
| Java Run API | OIDC Scope、tenant/application 声明、显式模型策略、幂等 Run 创建、Run 列表 | `RunControllerTest`、`RunServiceTest` |
| Runtime 资源配置 | JWT 固定 Tenant/Application；Workspace→Agent→不可变 AgentVersion→Provider→ModelPolicy→Session API 由服务端生成 ID并验证同一 Application；Provider API Key 是 write-only，API/日志/数据库均不保存明文；Agent 指令、子代理角色目录和 Provider 候选均进入不可变配置 | `ADR-0027`、`ADR-0028`、`ADR-0030`、`RuntimeResourceControllerTest`、`JdbcRuntimeResourceRepositoryIntegrationTest`、`execution_contract.rs` |
| Skill Registry / 动态激活 | Tenant/Application 下发布不可变 SkillVersion；控制面用独立 Ed25519 密钥签名 canonical artifact，AgentVersion 固定有序绑定；Scheduler 下发 RunExecution v5，Worker 验签、校验平台/最低版本，再把 Skill Tool 声明与预装可信目录及 delegated scope 求交集；Skill 不能扩权，Checkpoint 绑定有效指令和有效目录摘要 | `ADR-0029`、`V20__signed_skill_registry.sql`、`Ed25519SkillArtifactSignerTest`、`execution_contract.rs`、`assignment.rs`、原生真实恢复主链 |
| PostgreSQL | 租户复合外键、RLS、ModelPolicy 同 Workspace 约束、Run + Outbox 原子提交、事件续传查询 | 独立数据库的原生 PostgreSQL 集成测试 |
| Outbox / JetStream | 完整 RunQueued v1 快照、claim lease、失败释放、PubAck 后确认、NATS 消息 ID 去重 | 原生 PostgreSQL/NATS 集成测试与真实 NATS 暂停恢复 |
| Workspace Lease | owner epoch、fencing token、过期接管、旧所有者续租失败 | `JdbcRunRepositoryIntegrationTest` |
| Scheduler | 健康容量过滤、Workspace 单写租约、幂等 dispatch、定向执行命令；生成含 Provider、Skill、数据库谱系和权限内子代理角色目录的 RunExecution v7；根任务取得 AgentVersion 权限，子任务只取得所选角色的职能指令和权限子集；恢复保持相同不可变绑定 | `JdbcSchedulerRepositoryIntegrationTest`、`Ed25519WorkloadTokenIssuerTest`、`execution_contract.rs` |
| 子代理身份基础 | V21 用租户复合外键、唯一 delegation、深度 0–3 和角色约束保存 Run 谱系；v7 命令携带 root/parent/delegation/depth/role 及权限内角色目录；Worker Checkpoint v3 绑定谱系和角色目录并拒绝恢复漂移；AgentVersion 可配置最多 16 个不可变角色及权限子集 | `ADR-0030`、`V21__agent_run_lineage.sql`、`RuntimeResourceServiceTest`、`JdbcSchedulerRepositoryIntegrationTest`、`execution_contract.rs`、`assignment.rs` |
| 子代理原子准入 | 父 Run 行锁内校验终态、深度、角色、权限子集、每父最多 8 个活动子任务和 Token/费用/时长保守预留；同一 delegation 精确重放幂等、不同意图冲突；子 Run 与 RunQueued Outbox 同事务提交，并发请求不会超卖预算 | `ADR-0030`、`JdbcSubagentAdmissionRepository`、`JdbcSubagentAdmissionRepositoryIntegrationTest` |
| 子代理挂起交接 | Worker 将权限内角色暴露为内建 `agent.spawn`，生成确定性 delegation 和摘要绑定请求，先把父 Run 挂起并写入 Checkpoint v3；V22 在检查点事务内原子完成请求登记、子 Run/Outbox 创建、父 dispatch 挂起、容量归还及 Workspace 租约释放；挂起父状态留驻 Worker 但不计容量、不续租，避免把 Broker PubAck 误当控制面提交确认 | `ADR-0032`、`V22__durable_subagent_handoff.sql`、`assignment.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| 子代理结果与取消 | V23 将子终态事件、受限结果、摘要、恢复 attempt 和回执保存为权威账本；Reconciler 为父 Run 获取新 owner epoch/fencing 并发送 Recovery v2；Worker 验证结果与挂起 Tool Call 后以原 call ID 回灌模型历史，持久化恢复/结果事件与运行中检查点；父取消会递归定向活动子树，未派发后代原子终止，挂起父的当前 attempt 可精确确认取消终态，迟到结果不能复活父 Run | `ADR-0033`、`V23__durable_subagent_results.sql`、`subagent_recovery_contract.rs`、`assignment.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| 持久 Run steering | `POST /v1/runs/{id}:steer` 与 Console 支持同一公开 Run 内重定向；PostgreSQL 以 Tenant/Application 与幂等键保存命令，定向当前 attempt/incarnation；Worker 取消旧模型流，先把输入和回执写入 Checkpoint 再继续，Recovery v3 会把未完成命令重绑新围栏且不重复输入；过期/永久拒绝先发布精确绑定负回执，控制面只让合法回执把账本收敛为 rejected | `ADR-0034`、`V24__durable_run_steering.sql`、`V25__run_steering_outcomes.sql`、`RunControllerTest`、`JdbcSchedulerRepositoryIntegrationTest`、真实 NATS `transport.rs`、Console Vitest 与真实 Chrome、`docs/evidence/2026-08-02-native-steering-outcomes-and-one-command-run.md` |
| 工作负载身份续期 | PostgreSQL 保存 generation/expiry；心跳进入半租期阈值才原子轮换并写 Outbox；命令与签名 Token 共用一个显式 `issued_at`，Worker 验签全部绑定和三项能力后原子替换，旧代际拒绝、精确重投幂等；真实 JetStream 目标实例消费与原生恢复主链均验证 | `ADR-0019`、`V11__workload_identity_renewal.sql`、`Ed25519WorkloadTokenIssuerTest`、`JdbcSchedulerRepositoryIntegrationTest`、`assignment.rs`、真实 NATS `transport.rs` |
| Rust Worker Transport | 定向 JetStream 消费、过期租约拒绝、重复 attempt 幂等、心跳与 accepted 事件；生产连接强制 NATS TLS、CA、角色凭证，Worker 只读预建 Stream 且连接状态进入 readiness | `runtime/apps/worker/tests/assignment.rs`、真实 NATS `transport.rs`、`nats-security/tests/live_tls.rs` |
| Worker 启动实例隔离 | 稳定 Worker 与每次启动 incarnation 分离；V9 保存实例历史和单向前进的 current 指针；执行、恢复、取消、审批、accepted 与续租均精确绑定实例；Kubernetes StatefulSet 用独立 PVC 保存稳定 UUID，损坏身份 fail-closed | `V9__worker_incarnations.sql`、`execution_contract.rs`、`assignment.rs`、`worker_identity.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| Worker 安全下线 | SIGTERM/SIGINT 先原子撤销 readiness 与新任务准入；draining heartbeat 携带 deadline，控制面持久化单向状态并排除新调度/恢复；活动 assignment 继续续租和完成，90 秒到期发布最新安全 Checkpoint 后退出；Kubernetes 保留 30 秒 teardown 余量 | `ADR-0021`、`V12__worker_draining.sql`、`assignment.rs`、真实 SIGTERM/JetStream 测试、`validate_kubernetes.rb` |
| 恢复事故与故障证据 | V13 持久化 waiting capacity、recovery requested、recovered、indeterminate 及原始故障时钟；按租户输出 15 分钟 SLO 快照；健康与 SLO 使用控制面心跳接收时间；750ms 缩放租约、真实 NATS pause/resume、Checkpoint Store 短暂不可用后的 JetStream 重投递均已验证 | `ADR-0022`、`V13__recovery_incidents.sql`、`JdbcSchedulerRepositoryIntegrationTest`、`NatsJetStreamMessageBusIntegrationTest`、真实 NATS `transport.rs` |
| 恢复指标与告警 | V14 以不含租户标识的事务汇总桶提供全局恢复快照；Prometheus 低基数 Gauge、采集陈旧与错误指标；9090 独立管理端口、健康匿名、指标专用凭证；可选三类 PrometheusRule | `ADR-0023`、`V14__recovery_metric_rollup.sql`、`RecoveryMetricsCollectorTest`、`ManagementEndpointIntegrationTest`、`deploy/kubernetes/observability/` |
| macOS 原生开发基线 | 零 Compose/Docker/Kubernetes 本地命令图；项目级 PostgreSQL/NATS、外网统一走系统 `127.0.0.1:10808`、回环直连；项目 CA、mTLS、独立 Ed25519 工作负载/Skill 身份、RSA 本地 JWT、RSA-3072 Provider 凭证密钥和 bcrypt 角色凭证；Provider 通过正式控制面 API 封装并原子绑定开发策略；`make dev-run` 单命令真实主链已验收；最新 steer 主链 RSS 399504 KiB（390.1 MiB），完整故障恢复主链 677040 KiB（661.2 MiB）；`dev-clean` 清除 PID、端口、日志、全部本地密钥、状态和构建/测试产物 | `ADR-0024`、`ADR-0029`、`deploy/native/{devctl,with-download-proxy,configure-local-model-provider,service-runner}`、native Ruby/live Chrome 门禁 |
| Worker Execution Supervisor | 每 attempt 异步模型 RPC、主循环串行 Kernel 事件、并发取消、流错误分类、终态 PubAck 后释放容量 | `runtime/apps/worker/tests/model_gateway_transport.rs`、真实 NATS `transport.rs` |
| Worker Tool Turn 编排边界 | Tool 定义按 delegated scope 暴露；完整 Tool Call 后规划策略；审批绑定调用摘要；Tool Result 保持 call ID 进入下一轮 ModelInvocation | `runtime/apps/worker/tests/assignment.rs`、`runtime/crates/kernel/tests/tool_registry.rs` |
| 受限容器 Tool Runtime | OCI digest 固定、绝对引擎路径、无 Shell argv、只读 Workspace、断网/只读根文件系统/去 capability、stdin 参数传递、超时/取消、有界输出及结果双重绑定 | `runtime/crates/tool-runtime/tests/restricted_container.rs` |
| 可信原生 Tool Runtime | 本地显式 opt-in；可信根目录、固定可执行文件 SHA-256、执行前重验、无 Shell、清空环境、JSON stdin、只读 Workspace、超时/取消和有界输出；`workspace.read_text` 拒绝绝对路径、遍历、符号链接、非 UTF-8 与大文件，默认 ask；执行器摘要必须与模型目录及审批摘要一致 | `ADR-0025`、`runtime/crates/tool-runtime/tests/trusted_native.rs`、`runtime/apps/trusted-workspace-tool/tests/read_text.rs`、`runtime/apps/worker/tests/assignment.rs` |
| 原生 Skill/Provider/Tool/审批/恢复实跑 | API 动态发布签名 SkillVersion 并绑定 AgentVersion；模型精确收到合并后的 Agent + Skill 指令，只看到 Skill 声明的可信 Tool；两个 Provider 完成两次 429 安全切换；强杀 Worker 后 owner epoch 1→2，真实 Chrome 审批后 Tool 只执行一次，13 个 SSE 事件完整重放；独立临时根目录最终自动清理 | `docs/evidence/2026-08-02-native-signed-skill-activation.md`、`native_trusted_tool_recovery_live_test.rb`、Console 单元/三视口 E2E |
| 自动 Tool 闭环 | 模型 Tool Call 完成后自动规划并持久化执行请求；Tool Result PubAck 后自动携带 tool-role 历史发起下一模型回合；策略/人工拒绝均生成模型可见错误 | `runtime/apps/worker/tests/transport.rs`、`assignment.rs`、`tool_execution_supervisor.rs` |
| Tool Execution Ledger | V7 权威账本记录 planned/started/completed、调用摘要、副作用和沙箱；started PubAck 后才启动进程；结果摘要错配不入库；Worker 丢失会记录具体模糊副作用证据 | `JdbcSchedulerRepositoryIntegrationTest`、真实 NATS `transport.rs` |
| Worker Checkpoint | v3 快照保存 Kernel sequence、Protobuf transcript、待处理/执行中 Tool、待审批/子代理请求、角色目录、Tool Catalog 摘要、累计 Token/费用和待发布预算终态，并兼容 v1/v2；用量事件先持久化，崩溃恢复后不会再次调用模型；Zstd 后小对象内联、大对象生成内容寻址引用；新 attempt 以更高 owner epoch 和新 fencing 恢复，目录漂移与非幂等模糊执行 fail-closed | `ADR-0031`、`ADR-0032`、`runtime/apps/worker/tests/assignment.rs`、`runtime/apps/worker/tests/transport.rs`、`runtime/crates/protocol/tests/checkpoint_contract.rs` |
| Checkpoint 权威索引 | V10 `run_checkpoints` 使用 RLS、复合 dispatch 外键、双摘要、编码与压缩前后长度；只允许 payload/ref 二选一，恢复 Outbox 保持对象引用；仅最新 sequence、相同状态/租约且无模糊副作用时判定 SAFE | `JdbcSchedulerRepositoryIntegrationTest`、`checkpoint_contract.rs`、`RunCheckpointMessageTest` |
| 自动围栏恢复 | Checkpoint 获得 PubAck 后由控制面持久化；过期 accepted dispatch 可在其他健康 Worker 或同一稳定 Worker 的新实例上创建新 attempt、递增 owner epoch、轮换 fencing 与工作负载身份；新实例发布 `run.restored` | `runtime/apps/worker/tests/transport.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| 恢复中的 Tool / 审批 | pure/idempotent Tool 在新 attempt 重新产生 requested/started 屏障；pending approval 原子重绑新 Worker，并以 `approval.rebound` 重建新 Tool Ledger；若决定已持久化但旧 Worker 尚未接收，最新 approved/denied 审批和同版本决定会重绑、重发给 replacement attempt | `ADR-0011`、`runtime/apps/worker/tests/assignment.rs`、`transport.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| Kernel 执行入口 | Worker 为 accepted attempt 创建 Kernel 状态机，幂等产生 `run.started`，并从 dispatch 构造绑定身份、租约和预算的首轮 ModelInvocation | `runtime/apps/worker/tests/assignment.rs`、`model_gateway_transport.rs` |
| 租约与恢复协调 | fenced assignment 心跳续租、dispatch 多 attempt 历史、requested 重排、SAFE accepted 自动接管、模糊非幂等失败终止 | `JdbcSchedulerRepositoryIntegrationTest` |
| Worker Event 入库 | 校验 event/attempt/sequence/digest，JetStream 重投不会重复写入 | `JdbcSchedulerRepositoryIntegrationTest`、`NatsRunQueuedConsumerIntegrationTest` |
| 取消控制 | 未调度 Run 原子取消；运行中、等待审批或挂起的 Run 通过 Outbox 定向当前 Worker/attempt，命令有有效期且可重复投递；父取消递归覆盖全部未终态子孙并封闭子代理结果账本 | `RunControllerTest`、`JdbcRunRepositoryIntegrationTest`、`JdbcSchedulerRepositoryIntegrationTest`、真实 NATS `transport.rs` |
| 持久审批与恢复命令 | `approval.required` 原子入库并进入 waiting；待审批按 tenant/application 与 Scope 隔离；决定 API 使用版本锁。V17 将 `allow_session` 限于 pure/idempotent，并精确绑定 Session、Workspace、AgentVersion、参数与 Tool 策略；重复调用由控制面生成当前调用的 allow-once，参数/策略漂移重新审批；RLS 隔离 Grant | `ADR-0026`、`ApprovalControllerTest`、`ToolApprovalScopeTest`、`JdbcSchedulerRepositoryIntegrationTest`、`TenantIsolationIntegrationTest`、`openapi_approval_contract_test.rb` |
| 模型事件与唯一终态 | 模型增量/Tool Call/用量/完成/分类失败生成单调事件；Tool Call 只结束模型回合；首终态胜出，终态 PubAck 后才释放 Worker 槽位 | `runtime/crates/kernel/tests/run_state_machine.rs`、`runtime/apps/worker/tests/assignment.rs` |
| 终态资源释放 | PostgreSQL 同一事务持久化终态事件、完成 dispatch、释放 Workspace 租约并归还容量 | `JdbcSchedulerRepositoryIntegrationTest` |
| SSE | `Last-Event-ID` 转换为持久化序号、轮询补传、终态关闭 | `RunEventControllerTest`、数据库集成测试 |
| Console | Run 状态、Workspace、预算列表；可从授权 Project 完成 Workspace→Agent→签名 SkillVersion→AgentVersion→1–8 个有序 Provider→ModelPolicy→Session；Skill 表单不暴露摘要、签名或租户 ID，只能显式选择预装可信 Tool；运行中 Run 可提交有 32 KiB UTF-8 门限、幂等键及明确反馈的 steer 输入；完成后立即成为 Run target | 24 个 Vitest、TypeScript、ESLint、Vite build、Chrome 390/768/1440 三视口配置+运行 E2E、原生 live Chrome 门禁与截图 |
| NATS 安全 | Rust/Java 客户端分别使用 PEM CA/PKCS12 TrustStore；真实 TLS、bcrypt 正确/错误密码、Worker Subject 越权和 JetStream 管理越权均验证；三节点路由配置要求 mTLS | `deploy/nats/`、`NatsConnectionSettingsTest`、`deploy/tests/verify_nats_tls.sh` |
| 语义健康检查 | `/live` 与 `/ready` 分离；Gateway 绑定 gRPC Socket 后才 ready；Worker 将 NATS 连接状态映射为 readiness | `runtime/crates/runtime-health/tests/http_health.rs`、三个 Runtime 进程入口 |
| Kubernetes 运行基线 | 双副本 Gateway Deployment/Service/HPA/PDB、三副本 Worker StatefulSet/PVC/PDB、SecretProviderClass、非 root/只读根、资源限额和必要流量网络策略；控制面 Skill 签名私钥与 Worker 验签公钥分别从 Vault 物化，部署契约校验环境变量、Secret 键和 Vault 对象三层一致 | `deploy/kubernetes/base/`、`deploy/tests/validate_kubernetes.rb`（29 个渲染资源） |
| 生产容器历史基线 | Dockerfile 与 Kubernetes 清单仅保留为未来生产交付材料；本地 Makefile 不暴露镜像构建目标，`dev`、`test`、`check` 和 live 恢复门禁均不得调用 Docker | `runtime/Dockerfile`、`control-plane/Dockerfile`、`native_command_contract_test.rb` |
| 契约与部署 | OpenAPI、Protobuf、原生 macOS 生命周期、Kata RuntimeClass、默认拒绝网络策略 | 本地原生门禁与生产清单 CI |

## 只有契约或进程骨架

- Scheduler 已下发 Provider 与签名 SkillVersion 快照；Worker 已动态激活 Skill 指令和预装可信 Tool 子集。当前仍不是任意制品装载器：不执行租户上传脚本，Edge Node 仍只有进程骨架。
- Worker 已通过 `CheckpointPayloadStore` 接入独立 Checkpoint Gateway，并保证外部对象先于 Event/Checkpoint 发布；本地 MinIO、身份续期、双向 TLS 和 Kubernetes 双副本基线已验证。尚未在真实集群验证 Gateway 滚动维护、证书轮换和对象生命周期，因此仍不是生产闭环。
- OpenAPI 与 Java 已实现 Workspace、Agent、AgentVersion、Provider、ModelPolicy 和 Session 创建 API；当前仍缺资源列表、详情、重命名/归档、失败后续建和删除治理，Console 动态步骤失败只会明确保留已成功资源，不会自动补偿或恢复到下一步。
- Kubernetes Base 已有 Gateway Deployment/HPA/PDB、Worker StatefulSet/PVC、SecretProviderClass、网络策略和有界 draining 配置，并通过渲染契约；尚无云集群 apply、CSI/Vault 联调、镜像 digest Overlay、真实 eviction 与节点级故障注入证据。
- 三个 ARM64 Runtime 镜像已在本地干净 Rust 1.88 Builder 构建并验证非 root/fail-closed；尚未构建或签名 x86_64 多架构清单，也未推送到 OCI Registry 或按 digest 更新生产 Overlay。
- 原生 PostgreSQL/NATS、Java、三个常驻 Rust 服务、可信 Tool 二进制和 Vue 已真实同时运行；两个动态确定性
  Provider 已完成有序 429 安全切换→Tool→审批→Checkpoint→强杀 Worker→新 attempt/owner epoch→结果回灌→终态。
  小 Checkpoint 按协议内联 PostgreSQL，大于 512 KiB 的载荷使用内容寻址文件后端；真实浏览器已完成
  浏览器→真实控制面→恢复后 Worker 的审批闭环。外部真实模型尚未验收；确定性回环 Provider 只证明协议和执行语义，不证明第三方模型质量或稳定性。
- Java 本地默认门禁最近一次执行 137 个测试（其中 1 个可选 live 测试显式跳过）；7 个原容器集成测试类使用临时原生 PostgreSQL/NATS；控制面与
  Worker 使用不同 TLS 角色，Worker 越权主题和 JetStream 管理操作被拒绝。每类数据库相互隔离，NATS
  暂停恢复使用经 PID 身份校验的原生信号，测试结束后进程、端口和运行态均已清理。

## 尚未实现，不得对外声明

- Provider 的能力档案、地域/数据等级与价格元数据尚未接入动态资源；健康探测、冷却/半开探针、熔断和按 Provider 精确出站策略仍未实现。当前安全候选切换只处理请求提交前的 429、超时与不可用。
- Anthropic 首版尚未覆盖 OpenClaw 的模型特定 thinking/cache/refusal、OAuth/Foundry 与图片预算兼容层；结构化输出和图片输入会 fail-closed。
- 工作负载令牌已完成 incarnation、audience、scope、ModelPolicy 摘要绑定、持久化代际续期与 401 有界恢复；gRPC mTLS、NATS TLS/角色 ACL 和租户 BYOK 密文边界已验证。尚缺主动撤销、签名密钥与证书自动轮换、逐操作最小授权、Vault/KMS 真实联调和 Provider 级精确出站策略。
- Reconciler 已接通 SAFE Checkpoint 自动接管、持久恢复事故、按租户 SLO 快照及不含租户标签的全局 Prometheus 告警指标，Worker 也已实现计划内 draining；当前只有缩放租约、传输层故障和离线告警规则证据，真实 Prometheus/Alertmanager、Kubernetes 节点级故障注入、生产 15 分钟恢复时限证明、加权公平调度和多租户配额抢占尚未完成。
- 受控出网 HTTP 与 MCP 的 Tool 制品；Kubernetes Job/Kata 沙箱生命周期及 OCI 签名验证。
  **写文件与 Shell 已实现**（ADR-0036、0038），但都只在 macOS 上受容器约束，
  Linux 上没有 `landlock` 等价物，因此 Worker 的 Linux 路径不得注册它们。
- **Shell 恢复为全部逐次审批。** ADR-0039 的只读白名单已于 2026-08-07 撤回：
  复审发现名单把 `git branch -D`、`git tag -d`、`git diff --output=`、`uniq in out`、
  `file -C` 判为只读，构成**审批绕过**。豁免机制保留但已关闭（`AutoApproval::Never`），
  重新启用的前提是策略成为**租户决定并签入执行快照**，而不是 Worker 里的常量。
- **真实厂商云端主链已验证**（2026-08-07）：PostgreSQL + NATS + Java 控制面 + Worker +
  Model Gateway，模型自主决定工具调用。仅一个厂商一个模型；Anthropic Messages 与
  OpenAI Responses 仍只有契约测试。
  容器只对 `~/.ssh`、`~/.aws`、`~/.gnupg`、`~/.config/gh` 拒读，其余用户可读文件仍可读，
  所以「Shell 已容器化」不得表述为「Shell 已隔离」。
- Shell 没有交互式或长驻会话（Codex 的 `unified_exec` 对应物）。
- **MCP 客户端未实现**，形态已定案（ADR-0040）：v1 只联邦 HTTP 传输的 server，不派生本地进程。
  这是与 Codex 差距最大的单项。**代价要说清**：今天多数已发布的 MCP server 是本地 npm/Python 进程，
  v1 支持不了它们——而支持它们需要一套「允许出网」的容器化方案，与 ADR-0036 现有 profile 不同。
- 真实模型厂商端到端：**`openai_compatible` 已验证一次**（2026-08-07，DeepSeek 经网关，
  `runtime-host` 路径，与云端共用协议转换代码）。**仍未验证**：Java 控制面 + PostgreSQL +
  NATS 的完整云端链路对真实厂商（被 nats-server 构建的网络问题阻塞）、
  `openai_responses` 与 `anthropic_messages` 两条协议、多 Provider 故障转移、
  限流退避、真实错误响应分类。
- 调用方认证与配额（谁能调用这个 Runtime、能用多少）尚未实现。
- Checkpoint 对象缺失、内容损坏及一次短暂 `Unavailable` 后 JetStream 重投递已做故障分流；尚缺对象保留/垃圾回收、真实 Gateway 进程或存储实例丢失和跨可用区故障注入。
- Skill 的生产 OCI 制品上传、SBOM、恶意扫描、平台审核、租户签名链与公共/私有 ACL；当前已完成控制面签名的结构化 SkillVersion 和可信 Tool 激活，不接受上传脚本。
- 子代理的 Worker 发起、父挂起检查点、原子子 Run 入队、Workspace 正反向租约交接、子结果回送、
  父 Run 新围栏恢复、父取消向子树传播和同 Run 持久 steer 已经实现；wait/message 交互 API、独立 close 操作、完成附件、超时竞态、
  专用投递退避与只读 Workspace 快照尚未实现，因此仍不得声明与 Codex/OpenClaw 等量的完整协作生命周期。
- Steer 的 API、控制面账本、Worker Checkpoint-first 应用、恢复重绑、过期/永久拒绝负回执和真实浏览器中途 steer 已实现；尚缺队列压缩、限速、富输入/附件和面向 generation 的运维治理。
- Worker 已按模型 Usage 累计并 Checkpoint Token/费用，越界后不会再次调用模型；时长预算尚未由单调时钟强制执行，
  控制面的子代理可委派预算也尚未扣除父 Run 自身已经实际消耗的用量，因此当前准入仍是保守分配账本，不是全局精确余额。
- Edge Node mTLS 注册、离线任务、Workspace 三方合并。
- 1000 活跃 Run 压测、故障注入、Multi-AZ、备份恢复和 SOC 2 Ready 证据。

达到私有 Beta 仍必须按 `docs/architecture/acceptance.md` 完成容量、安全、恢复和真实产品闭环验收。

每阶段与 Codex CLI、OpenClaw 的差异复核见 `docs/reference-comparison.md`。
