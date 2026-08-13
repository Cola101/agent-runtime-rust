# stdio MCP 目录缓存与会话生命周期证据

日期：2026-08-09

## 结论

独立 Rust Host 的 stdio MCP 已补齐进程内健康目录缓存、失败会话替换、活跃租约、idle TTL 和 LRU
容量上限。目录缓存只减少安全的 `tools/list`，不会缓存或重放 `tools/call`。

## TDD 与真实进程证据

1. 关闭会话测试先因缺少生命周期配置和快照字段无法编译；实现后，第一条 session 退出，第二次
   发现明确失败并退休旧 session，第三次在新初始化 session 上复用缓存。实测启动 2 次、列目录 1 次。
2. 慢 Tool 测试让真实夹具延迟 1 秒，idle TTL 为 50ms、扫描为 10ms。调用期间快照为 1 个 active
   lease，未被扫描器回收；返回后 session 被 idle 回收且 Tool 只调用 1 次。
3. LRU 测试配置 2 个真实 stdio server、最大 session 为 1。第二个目录发现前，第一个零租约进程组
   被回收；最终仍保留 2 份目录缓存、1 个 live session、1 次 LRU eviction。
4. Host 全量测试曾发现后台扫描器阻断 `drop Host` 的自然关闭路径；增加最后客户端句柄关闭信号后，
   原有恢复测试重新通过。
5. 每个夹具都启动忽略 TERM 的 descendant；显式 shutdown、idle、LRU、失败退休和 Host drop 最终均
   证明完整进程组消失。
6. 收尾审计发现 32 个历史短路径 Unix socket；确认无进程占用后删除，并把深路径 IPC 测试由直接
   drop listener 改为正式 `release`。复跑后临时 socket、Tool 目录和夹具进程均为零。

## 能力边界

- 只缓存本地 stdio 目录；HTTP/gRPC 仍逐 Run 验证远端身份和授权。
- 健康检查是子进程存活，不是主动 MCP ping，也没有持续熔断或半开探针。
- 尚未支持 Codex 的逐 server cache opt-out、跨 Host 缓存或后台主动重连。
- 尚未覆盖 OpenClaw 的 requester 配置重验证、撤销窗口和每 runtime 串行管理。
- 本轮没有 Docker、Java、PostgreSQL、NATS 或外部服务参与。

## 已执行门禁

- `agent-runtime-host`：34 通过 / 0 失败。
- `agent-runtime-worker`：127 通过 / 0 失败。
- `agent-protocol`：59 通过 / 0 失败。
- `agent-model-gateway`：49 通过 / 0 失败 / 4 个外部 live 用例忽略。
- 四包 `cargo clippy --all-targets -D warnings`、`cargo fmt --check`、diff whitespace 与夹具进程审计：通过。
