# 原生 Provider 安全切换、审批与恢复证据

日期：2026-08-02

## 验收结果

在 macOS ARM64 本机原生进程中，通过真实 Java API 动态创建 Workspace、Agent、AgentVersion、
两个 Provider、有序故障转移 ModelPolicy 与 Session，再创建 Run。整个过程没有调用 Docker、
虚拟机或 Kubernetes。

- 首选 Provider 在两个模型回合均确定性返回 HTTP 429。
- Model Gateway 在没有产生任何模型输出时安全切到第二个 Provider，共记录 2 次安全切换。
- Provider API 只返回 `credential_status=configured`，不返回 `api_key` 或 `credential_envelope`。
- PostgreSQL 中 ModelPolicy 候选顺序与 API 提交顺序一致；两个测试 API Key 在凭证密文中的明文匹配数为 0。
- Run 进入待审批后强制终止 Worker；新 Worker 将 owner epoch 从 1 提升到 2，并发布
  `run.restored` 与 `approval.rebound`。
- 真实 Chrome 通过 Console 完成审批；`workspace.read_text` 只执行一次，Run 最终成功。
- SSE 完整重放 13 个事件：`run.started`、两轮模型事件、审批、恢复、Tool 执行和 `run.succeeded`。
- 主链五个应用与本地依赖的 RSS 合计为 349408 KiB（341.2 MiB），低于 4 GiB 目标。

## 门禁

- `make check-native-recovery-live`：通过。
- Console：19 个 Vitest、ESLint、`vue-tsc`、Vite build 通过。
- 真实 Chrome：390、768、1440 三个视口的动态 Provider 配置和 Run 链路通过，无水平溢出或运行时错误。
- 确定性 429 Provider 独立协议门禁通过。

## 对标边界

- 相比 Codex CLI，本平台多了租户 Provider 权威资源、Worker 零明文凭证和签名不可变快照；
  Codex 的 OAuth、命令型 Token、Responses 传输恢复和单进程易用性仍更成熟。
- 相比 OpenClaw，本平台的 RLS、租户/应用边界、工作负载身份与 Workspace fencing 更适合 PaaS；
  OpenClaw 的 Auth Profile 轮转、cooldown/半开探针、Provider 兼容矩阵和长期运维经验仍领先。

## 未证明的事项

回环 Provider 只证明协议、凭证边界、故障转移和执行语义，不证明任何第三方模型的质量或稳定性。
生产 Vault/KMS、凭证轮换/吊销、Provider 健康冷却与半开探针、精确出站策略仍未完成。
