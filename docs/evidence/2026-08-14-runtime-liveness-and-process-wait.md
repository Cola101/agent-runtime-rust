# 2026-08-14 Runtime 会话探活与 Process Wait 证据

## 已验证

- stdio MCP 5 项生命周期测试通过，包含两条关键反例：会话已经退出；会话进程仍活着但 `ping` 不响应。
  两者都不能授权目录缓存复用，失败 actor 会回收完整进程组。
- 64 个真实 Session / 1024 个 wait 在增加副作用后通知后连续 3 次通过：
  - p50 869.39ms / p95 934.15ms / p100 939.68ms；
  - p50 727.03ms / p95 905.88ms / p100 920.18ms；
  - p50 883.04ms / p95 949.42ms / p100 963.76ms。
- 同一 Session 的取消、1000 waiter 合并、外部 PTY 输出、yield deadline 和 durable delayed output 共 5 项
  通过；PTY supervisor generation-fence 6 项通过，真实 TTY exact 测试连续 10 次通过。
- `cargo test --workspace --no-fail-fast -- --test-threads=8` 最终 659 通过、0 失败；6 个需要外部 MCP/NATS 的 live
  用例显式忽略。期间 PTY exact 测试在一次重叠包级压力中出现过 1 次 `indeterminate`，随后聚焦套件
  连续 5 轮 85/85 与完整工作区复跑通过；因此保留为负载稳定性观察项，不把它描述为绝不再现。
- `cargo check -p agent-runtime-host -p agent-tool-runtime --all-targets`、两包 all-targets Clippy
  `-D warnings` 与 Rust 格式门禁通过。

## 参考源码复核

- Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`：`rmcp-client` 的连接可用性读取 service/transport
  closed 状态；`unified_exec` 的 `PreparedProcessHandles` 持有 output/closed `Notify` 与 pause `watch`。
  Codex 的交互产品面和跨平台 backend 更成熟；本平台的协议 ping、tenant/Workspace durable cursor 和
  Host replacement 是不同的 PaaS 边界，不能据此宣称整体领先。
- OpenClaw `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28`：Node Host MCP 以 `onclose` 更新 connected，
  PTY `onData` 直接入异步 output queue，并在 emit 完成前 pause。其在线 Node relay、viewer 和跨平台产品
  更成熟；当前 inspected 路径没有本项目同口径的跨 Host 持久 wait 账本。

## 边界

- 本轮没有启动 Java、Docker、数据库、消息总线或 Edge。
- 尚未把这组数据解释为 1000 活跃 Agent Run 的整机容量；这里只验证 64 个真实 Process Session 与
  1024 个 wait 的本地 Runtime 内核门禁。
