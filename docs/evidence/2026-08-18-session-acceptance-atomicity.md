# Session 接受原子性与恢复权威证据（2026-08-18）

## 复核事实

本轮以确定性 RED 逐条验证既有 Session 实现，三处缺陷全部**先复现、再修改**，不是从代码阅读推断的。

| 边界 | 修改前 | 复现方式 | 风险 |
| --- | --- | --- | --- |
| 接受顺序 | active Turn 先落盘，admission 后申请 | 上限占满后第三个 Turn 必然被拒 | 分支残留指向不存在 Run 的 active Turn，永久不可继续 |
| 终态可见性 | 终态事件 → Run record → head，三级滞后 | 观察终态后立刻读 head | 调用方无从得知 Session 何时可继续 |
| 恢复权威 | 投影判据取 Run record | 打点确认早退于 `not-terminal` | 让一个投影去等另一个投影 |
| 错误映射 | 文件不存在 → `StateRoot` → `Unavailable` | 读不存在的 Session | 调用方持续重试一个永远不会成功的调用 |

残留的原始输出：

```
a refused Session Turn must not have created a Session:
RuntimeSessionHead { generation: 1, turn_count: 0,
                     active_run_id: Some(01a0133e-8230-7350-a0cd-798b5a532fd5) }
```

## 实现结果

- `decide_session_start` / `decide_session_continue` 只读判定；写入仍由 `prepare_session_*` 承担。两者同锁，
  判定不可能在两次调用之间失效。
- 已完成重试在取得任何配额之前返回。
- `claim_execution` 与 `admission.acquire` 前置于全部持久写入。
- `rollback_prepared_session_start` / `_continue` 逐字段校验后才补偿：start 仅在"单分支、generation 1、
  无历史、active Turn 正是本次 run_id"时删除 Session；continue 只清本次写入的 active Turn。
- `project_terminal_session_turn` 以 Checkpoint 的 `verify_digest()` 与终态状态为判据，与重启恢复同源；
  幂等，任何投影不了的理由都原样返回，不在读路径制造新失败。
- 投影由独立同步分片锁串行化，永远最内层且不跨 await 持有。
- `read_session_record` 区分 `ErrorKind::NotFound`；`ResourceExhausted` 与 `Unavailable` 保持分离。
- `finish_session_turn` 拆出静态 `commit_session_turn`，供投影与正常终态共用同一段提交逻辑。

## 可执行门禁

| 门禁 | 结果 |
| --- | --- |
| `--lib client::tests` | 3/3 |
| `--test runtime_client_contract` | 3/3 |
| `--test grpc_session_contract` | 1/1 |
| `--test embedded_recovery_all` | 3/3 |
| `--test standalone_run root_session_` | 2/2 |
| `--test grpc_invocation_identity` | 9/9 |
| `--test approval_flow` | 9/9 |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| `cargo fmt --all -- --check` | exit 0 |
| `git diff --check` | clean |

`grpc_session_contract` 由一个只持有 TCP 地址与 bearer token 的调用方完成整链：
Initialize → StartSession → WatchEvents（至终态边界）→ ContinueSession → ReadSession → ForkSession →
Fork 重试等价 → ListSessions → 超限拒绝 → 半截游标拒绝 → ReadSessionHistory → RollbackSession。

## 测试方法上的两条约束

- **不靠时长等待**。占用活跃槽用的是远超测试时长的 provider 延迟（故意占用资源，而非等它变慢）；队列是否
  已满读 `admission_snapshot().queued_runs`；Runtime 替换前等 `Arc::strong_count` 降到 1。三者都是精确
  可观测量，最后一个后台任务结束即收敛，外层超时只把挂死变成可读的失败。
- **守卫必须见过它失败**。投影早退位置由临时打点定位（`PROJ-EARLY-RETURN: not-terminal`），确认判据取错
  之后才改，打点随即移除。

## 环境说明

`approval_flow` 曾 9/9 全挂并耗时 60s。原因是 `agent-trusted-workspace-tool` 未在本 target 目录构建，
测试因此拿到 `trusted_workspace_tool: None`，模型的 tool call 无工具可执行，审批永不记录。构建该二进制后
9/9 通过、耗时 3.58s。**不是回归**；`--workspace --all-targets` 本身会构建它。

## 未验证与下一门禁

计划列出的 12 项测试中，以下尚无独立覆盖：

- 同一 `run_id` 携带不同输入被拒绝
- Rollback 重试；Rollback 后继续再重放旧请求
- 终态崩溃窗口恢复且 Provider 请求次数不增加
- schema v1→v2 安全迁移

`generation` 溢出的显式拒绝亦未验证。这些是下一轮的 RED。
