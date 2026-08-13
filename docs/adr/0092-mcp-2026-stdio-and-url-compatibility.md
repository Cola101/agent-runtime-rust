# ADR-0092：MCP 2026 stdio 与 URL elicitation 兼容边界

状态：Accepted（2026-08-11）

## 决策

1. 本地配置新增显式 `stdio2026` transport；旧 `stdio` 继续固定为 `2025-06-18`，不做静默协议升级或失败回退。
2. HTTP 与 stdio 共用同一 MCP 2026 `_meta` 生成、`input_required` 校验和 `inputResponses` 编码，不允许 transport 改变客户端能力。
3. `elicitation` 仍是显式、按 Run 冻结的 authority；未授权服务即使返回合法 MRTR 也会 fail-closed。
4. 现代 stdio 使用 `server/discover`，每次 `tools/list` / `tools/call` 都携带 2026 metadata。用户等待状态只保存在 Worker/Host Checkpoint，MCP 子进程不是恢复事实源。
5. URL elicitation 只允许 HTTPS URL；Runtime 只回传 `accept/decline/cancel`，不承载浏览器内的凭据或授权码。
6. 现代 stdio 继续继承原有进程组回收、请求取消、目录冻结和“已接受 Tool 不自动重放”的安全边界。

## 理由

Codex 的严格 MCP 2026 stdio 测试服务同时检查 `server/discover`、每请求 metadata、无状态 `requestState` 与新请求 ID。使用它作为外部兼容门禁，可以避免本项目的客户端和自写 fixture 形成同源盲区。

OpenClaw revision `58b4b9430457` 的 inspected MCP 客户端路径未发现等价 MRTR 闭环，因此本项以 Codex revision `ff352fab6209` 为协议兼容基线；OpenClaw 仍作为 Node、跨平台进程与应用生态参考。

## 边界

- 已验证：内置真实 stdio 进程跨 Host replacement 两轮完成；Codex 严格 stdio 服务跨项目完成同一 Agent Loop；HTTP URL elicitation 跨 Host replacement 完成且不传 secret content。
- 未实现：MCP 2026 服务端 `ttlMs/cacheScope` 目录缓存策略、OAuth onboarding、Resources/Prompts、sampling/roots 与旧 2025 held-open elicitation。
- 本 ADR 不引入 Java、NATS、PostgreSQL、Docker 或 Kubernetes 依赖。
