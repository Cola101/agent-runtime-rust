# 原生签名 Skill 激活与故障恢复证据

日期：2026-08-02

## 验收结论

在完全原生的 macOS ARM64 环境中，已跑通以下真实主链：

```text
API 发布签名 SkillVersion
→ AgentVersion 固定绑定
→ Scheduler 下发 RunExecution v5
→ Worker 重算摘要并验证 Ed25519 签名
→ Skill 指令注入 + 可信 Tool/Scope 求交
→ 主 Provider 429 后安全切换
→ Tool 审批与 Checkpoint
→ 硬杀 Worker
→ 新 attempt / owner epoch 恢复
→ Chrome 审批
→ Tool 单次执行
→ 模型完成与 SSE 终态
```

- Run：`3502dbbc-7ace-4c58-80a2-c961665746db`
- SkillVersion：`9e8df18c-1054-46d2-9bdb-e7e32f1327ca`
- 旧/新 attempt：`ce85337f-f097-4e3d-a154-f4b302ace805` → `2fb8aa7a-016b-4d08-a10c-c50a7d12290f`
- owner epoch：`1` → `2`
- 安全 Provider 切换：2 次
- Tool 执行：1 次
- 浏览器审批：通过
- SSE：13 个关键事件按序完整重放
- 全部原生服务 RSS：`313136 KiB`（约 305.8 MiB）
- Docker、虚拟机、Kubernetes：未使用
- 测试临时进程、监听端口与本地根目录：结束后全部清除

## 安全边界

- 控制面用独立 Ed25519 密钥签名 canonical Skill artifact；本地私钥随 `dev-clean` 删除。
- AgentVersion 只能绑定同 Tenant/Application 下的 SkillVersion，且版本不可覆盖。
- Worker 在接单前校验 artifact digest、签名 key ID、签名、平台与最低 Runtime 版本。
- Skill 不能增加权限；有效 Tool 集是 Skill 声明、Worker 预装可信目录和 delegated scope 的交集。
- 未声明或未预装 Tool fail-closed；本地模式不执行上传脚本。
- Checkpoint 绑定合并后的有效指令摘要和有效 Tool Catalog 摘要，恢复时漂移即拒绝。

## 门禁

- Java：110 个测试通过，1 个可选 live 测试显式跳过；临时 PostgreSQL/NATS 自动停止并清除。
- Rust：全工作区测试通过；`cargo fmt --check`、全 workspace/all-targets/all-features Clippy 零警告。
- Console：20 个 Vitest；Chrome 390/768/1440 三视口 E2E；ESLint、TypeScript、Vite build 通过；生产依赖无已知漏洞。
- OpenAPI：Redocly 与资源契约测试通过。
- 原生生命周期：零容器命令图、系统 `127.0.0.1:10808` 外网代理、回环直连、身份、种子和清理契约通过。
- 生产部署静态契约：29 个资源成功渲染；控制面签名私钥和 Worker 验签公钥均校验到 Secret/Vault 来源。该项不代表真实集群或 Vault 联调完成。

## Codex / OpenClaw 对标

- 参考源码快照：Codex `ff352fa`；OpenClaw `58b4b943`。
- Codex 的 `SkillAuthority`、环境 Provider、有界资源读取、显式/隐式注入和完整 Tool/rollout 生态仍领先。
- OpenClaw 的 Skill 树摘要、符号链接/硬链接拒绝、静态扫描、Session snapshot 与真实 Node 分发仍领先。
- 本平台在 Application/RLS、不可变 AgentVersion 绑定、签名快照、跨 Worker 恢复和“Skill 不扩权”上更适合多租户 PaaS。
- 尚未完成生产 OCI 制品、SBOM/恶意扫描、公共/私有 ACL、节点分发和上传脚本强沙箱；不得把当前结构化 Skill 激活称为完整 Skill 平台。
