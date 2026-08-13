# ADR-0093：原生 PTY 与 Host 替换边界

状态：Superseded by ADR-0094（2026-08-11）

本 ADR 保留第一阶段决策记录；Host 内持有 PTY master 的临时边界已由 ADR-0094 取代。

## 决策

1. `process.start` 新增显式 `tty`、`cols`、`rows`；默认仍为原有 pipe 模式，不把所有长进程静默改成终端。
2. Unix 本地 Runtime 使用 `openpty`，child 在 exec 前执行 `setsid` 与 `TIOCSCTTY`；stdin/stdout/stderr 连接同一 slave，不能用 `TERM` 环境变量冒充 PTY。
3. PTY master 由当前 `PersistentProcessSessionManager` 持有；读取端把统一终端字节追加到既有 `stdout.log`，继续使用有界 byte cursor。写入端由 `process.write` 访问。
4. 原有 tenant/Workspace access、deadline、idle TTL、容量、RLIMIT、identity lease、进程组 interrupt/close 与 orphan sweep 不因 PTY 绕过。
5. 启动 child 前原子写入带摘要的 `terminal.json` marker。替代 Host 若看见 live PTY marker 但没有 master handle，必须先回收资源身份，再持久化 `indeterminate`；不得返回 `Reattached`。
6. 本阶段不伪造跨 Host PTY 恢复。真正续接要求独立 session supervisor 持有 master，并通过有身份约束的本地 IPC 暴露 write/resize/output。

## 理由

Codex 的 `codex-utils-pty` 和 `unified_exec` 证明 PTY/pipe 统一接口、进程组和增量输出是交互 Tool 的基础。OpenClaw 的 `TerminalSessionManager`、`node-pty`、resize 与高低水位 pause/resume 证明终端还需要会话所有权和回压。当前 Runtime 的日志、租户绑定与恢复账本比直接复制进程内 manager 更持久，但 PTY master 是不可按路径重新打开的文件描述符；在没有 supervisor 前声称 replacement reattach 会制造失控进程。

## 已验证边界

- macOS child 真实观察到 stdin/stdout `isatty=true`。
- `process.write` 经 PTY master 输入，终端 CRLF 输出经 byte cursor 返回模型。
- 独立 Runtime Host 的回环模型完成 start、poll、write、poll、close。
- replacement manager 不会假 reattach：它回收进程组并返回持久 `indeterminate`。
- pipe 模式原有跨 Host reattach、治理与进程树测试保持通过。

## 未完成

- `process.resize`、输出高低水位回压、scrollback attach、独立 PTY supervisor、真正跨 Host 续接。
- Windows ConPTY 与 Linux 真实 cgroup/PTY 联合门禁。
- 本 ADR 不引入 Java、NATS、PostgreSQL、Docker、Kubernetes 或 GUI。
