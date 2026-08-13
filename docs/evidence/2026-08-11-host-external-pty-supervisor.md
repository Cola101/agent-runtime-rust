# Host 外 PTY Supervisor 证据（2026-08-11）

## RED / GREEN

1. 真实 owner 测试进程退出后，替代 manager 首次得到 `Indeterminate`；将 master FD 移到独立 supervisor 后恢复为同一 PID 的 `Reattached`，并继续交互与关闭。
2. Runtime Host Agent Loop 首次在 `process.resize` 未激活时于模型 Tool Catalog 前失败；注册 Idempotent resize 并进入 Skill snapshot 后，真实 child 观察到终端从 100×32 改为 132×43。
3. 深层临时 state-root 首次使 supervisor 报 `path must be shorter than SUN_LEN`；socket 改为 UID 私有短目录与 state-root 摘要后，完整 Agent Loop 通过。
4. owner-only 状态测试首次观察到 `process-sessions/` 为 `0755`；目录、日志、Manifest、marker、锁和 FIFO 收紧后全部为 `0700/0600`。

## 故障与资源门禁

- Host 崩溃：supervisor 和 child 存活，替代 Host 使用同一 session、supervisor generation 与 PID 续接。
- Supervisor 崩溃：替代 Host 启动新 generation，旧 generation status 不匹配，回收 child group 并持久化 `indeterminate`。
- 输出压力：noisy PTY 达到 4096-byte 上限后终止，日志长度精确为 4096；没有无界 channel。
- 清理：测试失败期间遗留的 supervisor/child 均按已核实 PID/PGID 回收；最终复核无残留进程，并删除 12GB 临时 Cargo target、Graphify 输出、PTY socket 与测试临时目录。仓库内未留下 `runtime/target`。

## 质量门禁

- Rust 全工作区共 581 项：575 通过、0 失败、6 个外部 live 用例显式忽略。
- 输出上限竞态修复后，监督 PTY 压力测试连续 10 轮全部通过。
- `cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 通过。

## 对标快照

- Codex `ff352fab6209`：`codex-rs/utils/pty` 的 driver、resize、进程组与有界 mpsc 是基础参考；本阶段不宣称 Codex 已支持跨 CLI 进程重新取得原 PTY。
- OpenClaw `58b4b9430457`：`terminal-pty.ts`、`pty-command.ts` 与 `output-flow-control.ts` 的 resize、pause/resume 和高低水位是后续参考。
- 本实现领先点只限于已验证的跨 Runtime Host owner generation 与持久 fail-closed；Windows、viewer scrollback、WebSocket 水位和产品交互仍未完成。
