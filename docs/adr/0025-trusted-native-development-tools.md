# ADR-0025：可信原生开发 Tool 与实现摘要绑定

## Status

Accepted

## Context

M1 Pro 本地开发环境禁止 Docker、虚拟机和 Kubernetes，但真实 Agent 主链必须能完成模型 Tool Call、
持久审批、执行、结果回灌、Checkpoint 和 Worker 故障恢复。macOS 宿主机无法为不可信代码提供 Kata
等价隔离，因此不能把普通 Shell、任意脚本或租户上传 Skill 直接放到本机运行。

Codex 将审批对象与命令、工作目录和沙箱策略绑定，并在执行前选择 macOS sandbox 或其他平台隔离；
OpenClaw 的 `system.run` 会对解析后的 argv、批准时工作目录及可变脚本操作数重新校验，发现漂移就拒绝。
两者都说明“用户批准了一个调用”不等于“之后可执行任意同名实现”。

## Decision

1. 新增 `trusted_native` 执行类别，只允许在 macOS 原生开发入口显式设置
   `AGENT_RUNTIME_TRUSTED_NATIVE_TOOLS=true` 后启用；生产清单和 Worker 默认保持关闭。
2. 每个可信原生 Tool 必须指向明确可信根目录内的普通可执行文件。注册时记录 SHA-256；每次启动前重新
   canonicalize、检查非符号链接、可执行属性和摘要，发生替换或漂移立即拒绝。
3. `ToolDescriptor` 增加 `implementation_digest`。Kernel 的审批绑定摘要同时覆盖 Tool Call、
   副作用类别、执行类别、delegated scope 和实现摘要；Worker 注册的执行器摘要必须与模型可见目录一致。
4. 执行器不调用 Shell，不拼接命令字符串。入口和固定参数由平台配置，模型参数只通过有界 JSON stdin
   传入；子进程清空环境、使用规范化 Workspace 作为工作目录，并限制超时、stdout 和 stderr。
5. 首个制品 `workspace.read_text` 只读一个相对路径 UTF-8 文件，拒绝绝对路径、`..`、符号链接、目录、
   非 UTF-8 和超过 64 KiB 的文件；声明 `pure`、`ask`、`tool:workspace.read`，没有写入或网络能力。
6. 控制面 Tool Ledger 接受 `trusted_native`，但这不提升其安全等级。审批结果仍按版本、调用绑定摘要和
   当前 Worker incarnation 定向；Worker 崩溃后只从 SAFE Checkpoint 以更高 owner epoch 恢复。
7. 本地命令提供 `make dev-approve APPROVAL_ID=...`；它只访问回环 API，从权限为 `0600` 的项目令牌
   文件读取身份，仅允许 `allow_once` 或 `deny`，不提供永久放行。

## Consequences

### Positive

- 零容器环境可以验证真实模型—Tool—审批—Checkpoint—恢复主链，不以 Mock 或 HTTP 200 代替执行。
- 审批绑定到具体实现摘要，吸收 OpenClaw 的执行前漂移检查，并保留 Codex 的调用/策略绑定语义。
- 没有 Shell 解释、继承环境或浮动可执行文件，默认能力明显小于通用本机命令执行。
- delegated scope、RLS Tool Ledger、owner epoch 和 Worker incarnation 继续保持多租户 PaaS 边界。

### Negative

- `trusted_native` 不是强沙箱；开发者账户、仓库或已批准二进制被攻破时，宿主机仍属于信任域。
- 当前工作区路径校验不是内核级 `openat`/`O_NOFOLLOW` 事务，不能承载对抗性租户文件系统竞态。
- 只读文本 Tool 的能力远少于 Codex 的 Shell/macOS sandbox，也少于 OpenClaw 的跨平台 Node Host。
- 每次执行计算摘要会产生少量 I/O，更新二进制后必须重新注册目录并重新审批。

## Alternatives Considered

- **复制 Codex Shell Tool 与 macOS sandbox**：能力成熟，但会扩大本轮本机信任面，也不能解决 Linux
  生产环境的 Kata 边界，暂不移植。
- **复制 OpenClaw `system.run`**：argv/cwd 重验值得吸收，但通用命令执行超出“仅可信 Tool”约束，拒绝。
- **本地继续运行 OCI 容器**：需要 Docker/虚拟机，违反明确目标，拒绝。
- **完全使用假 Tool**：无法证明审批、账本、Checkpoint、恢复和结果回灌，拒绝。
- **按 Tool 名称而非实现摘要审批**：同名二进制可在批准后被替换，拒绝。

## References

- Codex：`codex-rs/core/src/tools/approvals.rs`、`codex-rs/core/src/tools/sandboxing.rs`
- OpenClaw：`src/node-host/invoke-system-run-plan.ts`、`invoke-system-run-allowlist.ts`、
  `invoke-system-run.ts`
- 本平台：`runtime/crates/tool-runtime/src/lib.rs`、`runtime/apps/trusted-workspace-tool/`
- 本平台：`runtime/apps/worker/src/main.rs`、`deploy/native/approve-local`
