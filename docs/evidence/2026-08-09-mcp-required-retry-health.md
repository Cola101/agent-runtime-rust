# MCP required/optional、有限重试与发现健康证据

日期：2026-08-09

## 结论

RunExecution v11 已把 MCP required/optional 和发现重试预算冻结为可恢复身份。真实本地 stdio 主链证明：

- 第一次初始化失败、第二次成功时，模型只在目录就绪后调用 Tool，Run 成功；
- required server 连续失败时，在模型请求前拒绝；
- optional server 连续失败时，Run 可继续且结果明确报告 unavailable、尝试次数和错误；
- required 标记在恢复时漂移会被 Checkpoint 拒绝，模型请求数为零；
- 所有测试子进程及其 TERM-ignoring descendants 均被回收。

## TDD 证据

1. 协议测试先因缺少 `max_attempts_per_server` 和
   `initial_retry_backoff_ms` 无法编译；加入 schema 2 和 v11 降级门禁后通过。
2. 真实 stdio 重试测试最初在第一次关闭 stdout 后直接失败；实现仅发现重试后进入第二次初始化。
3. 首次 GREEN 尝试发现直接子进程已退出时，按 PID 查询 PGID 无法回收 descendants；改为 spawn 时冻结 PGID 后通过。
4. required 标记漂移测试最初越过恢复检查并到达模型 Provider；把 v11 required 纳入 server binding digest、Checkpoint 升至 schema 10 后在模型前失败。
5. 总发现 deadline 测试最初把未返回的服务器都报告为 1 次尝试；新增失败断言后改为 0 个已完成尝试。

## 已执行门禁

- `cargo test -p agent-runtime-worker`：127 通过 / 0 失败。
- `cargo test -p agent-runtime-host`：30 通过 / 0 失败。
- `cargo test -p agent-protocol`：59 通过 / 0 失败。
- `cargo test -p agent-model-gateway`：49 通过 / 0 失败 / 4 个需外部 Server 或凭据的 live 用例忽略。
- `cargo clippy -p agent-protocol -p agent-runtime-host -p agent-runtime-worker -p agent-model-gateway --all-targets -- -D warnings`：通过。
- `cargo fmt --all`、diff whitespace 与残留 stdio fixture 进程审计：通过。

## 能力边界

- 状态是 Run 启动时的 discovery health，不是常驻主动健康探针。
- 自动重试只覆盖安全的目录发现，不覆盖 `tools/call`。
- 尚无健康目录缓存、后台重连、idle TTL、LRU、requester lease、OAuth 和完整 MCP 方法面。
- Java 控制面仍生产旧 schema；v11 当前由协议中立 Rust Host/Worker 路径验证，本阶段未扩控制面。
