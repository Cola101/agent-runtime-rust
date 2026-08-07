# 项目目标与执行边界

## 唯一目标

持续对标 Codex 与 OpenClaw 完善 Agent 架构与能力模块，并在 M1 Pro 16GB Mac 上交付完全不依赖
Docker、虚拟机或 Kubernetes 的原生 Agent Runtime 开发环境。Java、Rust、Vue、PostgreSQL 和 NATS
必须以 macOS ARM64 原生进程运行，Checkpoint 本地使用文件系统，只允许可信 Tool 在本机执行；
一条命令启动真实 Agent 主链，一条命令停止并清除全部项目进程、端口、日志、临时数据、测试密钥
和构建产物，整套环境常驻内存不得超过 4GB。

## 固定实施顺序

1. **本机开发闭环**：全部组件使用项目管理的宿主机原生进程；PostgreSQL/NATS 不注册系统服务，
   状态集中到 `.local/`；本地 Checkpoint 使用文件系统，开发流程不得调用 Docker。
2. **云端真实主链**：从 UI/API 创建 Run，经模型调用、Tool 审批、Checkpoint、Worker 故障恢复、SSE
   和制品输出完成一次可复查闭环。
3. **多租户与容量**：完成 IAM、RLS、对象与消息隔离、配额、公平调度和 1000 Run/1 万 Session 压测。
4. **Skill 与云边协同**：完成签名 OCI Skill、节点注册、离线任务、Workspace 分支合并和冲突处理。
5. **私有 Beta 门禁**：完成真实 Kubernetes/Kata、Multi-AZ、备份恢复、安全攻防与 SOC 2 Ready 证据。

在第 1、2 项尚未形成真实用户闭环前，不继续扩展 HPA、多地域、复杂微服务拆分或更多生产拓扑。

## 本地运行边界

- Mac 本地开发命令禁止调用 Docker、虚拟机、Kubernetes、Vault 或 OCI Registry。
- PostgreSQL 与 NATS 使用已安装或项目级校验下载的 ARM64 二进制，但不得注册 Homebrew Service。
- 本地 Checkpoint 使用内容寻址文件系统；生产 S3 Gateway 契约保持独立，不进入本地依赖链。
- 只允许仓库内声明的可信 Tool 原生执行；macOS 本地限制不得冒充 Kata 级强隔离。
- 所有可变状态必须位于 `.local/`，`make dev-clean` 后不得残留进程、端口、数据、日志、测试密钥或构建产物。
- 生产 Dockerfile 与 Kubernetes 清单只作为未来交付材料，不得被本地 `dev`、`test` 或 `check` 路径调用。

## 对标规则

每个阶段完成时必须分别回答：

- 相比 Codex，执行语义、工具审批、沙箱、事件与可观测性还差什么；哪些多租户能力是本平台新增。
- 相比 OpenClaw，节点连接、断线恢复、模型容灾、Workspace 协调和跨平台运行还差什么。
- 如果采用不同实现，必须说明为何更适合多租户 PaaS；没有证据时不得宣称领先。

## 完成定义

单元测试、静态门禁、HTTP 200 或 Kubernetes 清单渲染均不等于完成。只有真实 UI/API、持久数据、
租户 allow/deny、模型和 Tool 结果、故障恢复、最终制品及审计证据共同成立，才可以提升对应能力状态。
