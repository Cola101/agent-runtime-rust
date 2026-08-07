# ADR-0024：零容器 macOS 原生开发 Runtime

## Status

Accepted

## Context

目标开发机是 M1 Pro 16GB。此前本地 Compose、容器化 Builder 和 MinIO/Vault/Registry 默认依赖既占用
Docker Desktop 虚拟机资源，也偏离了“本地完全不依赖 Docker、虚拟机、Kubernetes”的明确约束。
开发环境仍必须保留 PostgreSQL 的 RLS/复合外键语义和 NATS JetStream 的至少一次投递语义；用 H2、
内存队列或假 HTTP 200 替代会使本地闭环失真。

Codex 以原生 Rust 进程、rollout 文件和平台沙箱提供低摩擦本地执行；OpenClaw 的 Gateway/Node Host
具有成熟的原生进程启动、重连、drain 和清理生命周期。两者都说明本地运行体验不能依赖生产集群拓扑。

## Decision

1. 删除本地 Compose 定义与 Docker 开发命令。默认 `test`、`check` 和 `dev-*` 命令图禁止出现
   Docker、Compose、kubectl、kind、k3d 或 minikube。
2. Java、Rust、Vue 使用已安装的 macOS ARM64 工具链。PostgreSQL 使用原生二进制和仓库内数据目录，
   NATS 2.10.20 使用项目级 Go module 校验构建；不得注册 Homebrew Service。
3. 所有 PID、日志、配置、数据、下载缓存、临时密钥和项目级工具放在 `.local/`。目录必须带专用标记，
   停止/清理命令拒绝操作未标记目录，禁止全局 prune 或删除用户级缓存。
   本地 JetStream 固定最多 256 MiB 内存 Store 与 1 GiB 文件 Store，禁止按机器总资源自动放大。
4. 下载器自动读取 macOS 当前 HTTP 代理，也允许以 `AGENT_RUNTIME_DOWNLOAD_PROXY` 显式覆盖；代理只进入
   当前下载进程，不修改系统设置。下载必须有连接超时、总时限、有限重试和版本/摘要校验。
5. 本地 Checkpoint Gateway 使用内容寻址文件系统后端；生产 S3 后端和工作负载身份契约保持不变。
   本地文件必须仍按 tenant/run/digest 路径隔离并在读取时复验摘要。
6. 本地只允许 ADR-0025 定义的可信 Tool 原生执行：显式启用、固定二进制摘要、无 Shell、清空环境、
   只读 Workspace 和默认审批。该类别不是强沙箱，不得承载租户上传代码或冒充 Kata 隔离。
7. Java 集成测试由测试监督脚本启动临时原生 PostgreSQL/NATS；每个数据库测试类创建独立数据库，NATS
   故障使用经进程身份校验的 macOS 信号注入。成功或失败后均停止进程并删除测试运行态；失败前先输出日志。
8. macOS 系统代理只用于依赖下载和显式外部请求。Java 本地集成测试 JVM 清空 HTTP/HTTPS/SOCKS 代理，
   防止 `127.0.0.1` 的 PostgreSQL/NATS 流量被错误送入 10808。
9. Maven、Cargo、pnpm、Go 和 curl 统一通过项目包装器继承系统代理，并固定绕过
   `localhost/127.0.0.1/::1/.local`。项目级 CA 生成 NATS、Gateway 和 Worker 证书，NATS 使用 bcrypt
   角色凭证与最小 Subject ACL；NATS 以独立 PID/PGID 原生进程运行，停止必须终止完整进程组。
10. `make dev` 由单一监督器依次启动控制面、Model Gateway、Checkpoint Gateway、Worker 和 Console；每个
    应用通过双 fork 脱离启动终端、由 PID 1 接管，并使用独立 PID/PGID 与日志。健康失败必须回滚，停止
    按反向顺序执行且 Worker 获得有界 drain 时间；`dev-clean` 再委托基础设施层清除项目状态、密钥、
    构建和测试产物。下一次 `make dev-run` 必须自动重新引导已被清理的项目级 NATS。
11. 本地 API 使用项目生成的 RSA JWT 密钥和 24 小时开发令牌。令牌只由 Vite 开发服务器读取，且只允许
    转发给 `127.0.0.1/localhost`；不得打包到浏览器。控制面迁移完成后，以事务、RLS 租户上下文和固定
    UUID 幂等写入最小开发资源。
12. 生产 API 继续使用 8080；macOS 原生开发 API 默认使用 18080，避免与本机常见 Java/Web 服务冲突。
    Model/Checkpoint Gateway 的本地监听端口与 Worker endpoint 必须由同一覆盖值生成，禁止一侧写死。

## Consequences

### Positive

- Docker Desktop 不再是本地开发依赖；可信 Tool 与强杀 Worker 恢复主链最新实测 RSS 约
  最新含真实浏览器审批的强杀恢复门禁测得 Runtime 常驻进程 497.5 MiB，远低于 4GB 上限。
- PostgreSQL、JetStream 和内容寻址 Checkpoint 语义没有被轻量替代品削弱。
- `stop` 与 `clean` 的责任范围可验证，网络代理和工具缓存不会污染全局配置。
- 五个应用共享一个启动入口，但保留独立健康、日志和进程责任；开发 JWT 不暴露给浏览器 JavaScript。
- 默认 Java 门禁最近一次为 90 个测试（其中 1 个可选 live 项显式跳过），其中 7 个原容器集成测试类全部使用真实原生进程、TLS 与角色 ACL。
- 生产 S3/Kubernetes 适配器继续独立演进，不侵入本地快速路径。

### Negative

- 需要维护 macOS 生命周期脚本和生产部署两种运行入口。
- 当前 PostgreSQL 14 与 CI PostgreSQL 16 存在版本差，迁移兼容需要双版本门禁。
- 原生 Tool 只能用于可信开发制品，无法提供 Kata 等价隔离。

## Alternatives Considered

- **保留最小 Docker 基础设施**：仍启动 Linux VM，违反明确约束，拒绝。
- **H2 与内存消息队列**：无法证明 PostgreSQL RLS、JSONB、锁和 JetStream 语义，拒绝。
- **注册 Homebrew Service**：生命周期逸出仓库，清理无法保证，拒绝。
- **使用 launchd 托管项目脚本**：项目位于 Documents 时受 macOS 隐私权限影响，脚本、Java 与 NATS 的
  可执行和目录访问不稳定；开发态改用项目内可验证的独立进程组，拒绝注册 LaunchAgent。
- **本地 MinIO**：为开发文件持久化引入额外常驻进程；内容寻址文件系统已满足本阶段，拒绝。
- **macOS 本地执行不可信 Tool**：无法达到生产强隔离，拒绝。

## References

- 本平台：`deploy/native/devctl`、`deploy/native/run-java-tests`、`deploy/tests/native_*_test.rb`
- 本平台：`control-plane/src/test/java/com/agentplatform/control/testing/NativeIntegrationEnvironment.java`
- 本平台：`runtime/apps/checkpoint-gateway/tests/filesystem_store.rs`
- Codex：`codex-rs/core/src/thread_manager.rs`、`codex-rs/core/src/tools/sandboxing.rs`
- OpenClaw：`src/node-host/runtime.ts`、`src/cli/gateway-cli/run-loop.ts`
