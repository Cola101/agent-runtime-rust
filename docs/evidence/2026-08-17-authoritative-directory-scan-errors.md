# 权威目录扫描失败关闭证据（2026-08-17）

## RED

1. 将 `state/runs` 替换成普通文件后，旧 `list_run_records` 返回空列表；测试失败信息为：
   `the authoritative Run enumerator must not convert a scan failure into an empty list`。
2. 将 `state/sessions` 替换成普通文件，并让 Provider 选择在出网前失败；旧实现仍发布 `run.failed`；测试
   失败信息为：`a terminal event cannot be published when Session ownership cannot be checked`。

两条 RED 都使用真实 Runtime state root，不用 mock 文件系统。它们分别证明恢复入口会制造“假空”，以及
终态提交会绕过无法读取的 Session 权威。

## GREEN

- `list_run_records` 只把 `NotFound` 解释为空；其他目录和逐项读取错误返回 `StateRoot`。
- `find_active_session_turn` 使用同一规则；无法确认 Session 所有权时，执行返回错误且 committed Event
  prefix 中没有任何 Run 终态。
- 聚合恢复把坏 Profile 记录为一个 failure，`scanned_profiles=1`、`recovered_runs=0`，不伪报成功。

## 回归门禁

以下六个 Runtime Host 集成测试目标合计 52 项通过、0 失败：

- `approval_flow`：9
- `daemon_recovery`：9
- `embedded_control`：9
- `embedded_multi_tenant`：12
- `embedded_recovery_all`：3
- `embedded_retention`：10（含 1000 Run 本地保留/恢复门禁）

本轮未启动 Docker、PostgreSQL、NATS 或真实模型服务，也未运行全仓测试。结论只覆盖独立 Rust Runtime
本地权威扫描、恢复与终态提交路径。
