# ADR-0094：Host 外独立 PTY Session Supervisor

状态：Accepted（2026-08-11）

后续：单一 ownership、协议握手、生命周期与有界 attach 已由 ADR-0095 收敛；本 ADR 保留初始外置
supervisor 的决策与证据。

## 决策

1. Runtime Host 的 PTY 会话由同一 `state_root` 下唯一、最小的本地 supervisor 进程持有 master FD；Host 只通过有界 Unix socket 协议执行 start/status/write/resize。
2. 控制 socket 使用 state-root 摘要映射到短的 `/tmp/agent-pty-<uid>/` owner-only 目录，避免 macOS `SUN_LEN`；随机 capability token 仍保存在 state root，目录与 token 分别为 `0700`、`0600`。
3. supervisor 使用启动锁和随机 generation ID 防止双实例接管。每个 `terminal.json` 绑定 supervisor generation；替代 Host 只有在 generation、session、PID 和资源身份同时匹配时才返回 `Reattached`。
4. supervisor 丢失时，新 Host 不重建不可证明的终端控制：先回收原进程组，再把会话持久化为 `indeterminate`。
5. `process.resize` 是显式 `Idempotent` Tool，尺寸限制为 1—2000；resize intent 先进入持久 Manifest，再执行 `TIOCSWINSZ`。
6. PTY 输出使用固定 8 KiB 读取块同步追加日志；达到冻结 byte budget 时精确截断、TERM→KILL。内核 PTY 缓冲承担反压，不建立无界用户态队列。
7. Process Session Manifest 升级为 schema 7，schema 1—6 保持只读迁移；Runtime Host 的 Tool implementation digest 随语义升级。

## 安全边界

- `process-sessions/`、每个 session 目录为 `0700`；Manifest、终端 marker、日志、锁和 FIFO 为 `0600`。
- socket 目录必须是当前有效 UID 拥有的非符号链接目录；控制 frame 最大 256 KiB，token、schema 和 request ID 全部验证。
- supervisor 不接收用户 JWT、模型凭据或跨租户授权；租户和 Workspace 授权仍由 Host 的持久 Manifest 校验。

## 已验证

- owner Host 进程退出 74 后，supervisor 与原 child 保持；替代 Host 恢复同一 PID并继续 write/read/resize/close。
- Runtime Host 的真实 Agent Loop 完成 start→poll→write→poll→resize→write→poll→close。
- supervisor 被 SIGKILL 后，替代 Host 返回 `Indeterminate` 并回收原进程组，不产生假 reattach。
- 4 KiB PTY 输出预算精确停止在 4096 bytes；长 state-root 的 socket 可启动；本地多用户权限门禁通过。

## 对标与剩余差距

- Codex 已有成熟 PTY driver、resize、进程组和有界 channel；本实现增加了跨 Runtime Host 的持久 owner generation，但尚无其完整 exec 产品集、跨平台 backend 和统一 yield/output API。
- OpenClaw 已有 Node PTY 的 pause/resume、WebSocket 高低水位、scrollback 和 Windows 支持；本实现的持久恢复与 fail-closed 更严格，但 viewer 级流控和跨平台覆盖仍落后。
- 下一阶段先消除库内旧 Host-owned PTY 兼容路径，并补 supervisor capability/version handshake、可观测 shutdown 和有界 attach/scrollback；不进入 GUI、Java、容器或控制面。
