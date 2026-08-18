# Session 接受原子性与恢复权威证据（2026-08-18）

## 复核事实

本轮以确定性 RED 逐条验证既有 Session 实现，三处缺陷全部**先复现、再修改**，不是从代码阅读推断的。

| 边界 | 修改前 | 复现方式 | 风险 |
| --- | --- | --- | --- |
| 接受顺序 | active Turn 先落盘，admission 后申请 | 上限占满后第三个 Turn 必然被拒 | 分支残留指向不存在 Run 的 active Turn，永久不可继续 |
| 终态可见性 | 终态事件 → Run record → head，三级滞后 | 观察终态后立刻读 head | 调用方无从得知 Session 何时可继续 |
| 恢复权威 | 投影判据取 Run record | 打点确认早退于 `not-terminal` | 让一个投影去等另一个投影 |
| 错误映射 | 文件不存在 → `StateRoot` → `Unavailable` | 读不存在的 Session | 调用方持续重试一个永远不会成功的调用 |
| Run id 重用 | `Configuration` 字符串 → `Internal` | 同一 `run_id` 换输入 | 调用方自己的错被报成"运行时坏了" |
| Rollback 重试判据 | `history.starts_with(prefix)` | 回滚后再追加一个 Turn，重放旧回滚 | 重放被当成重试并**静默丢弃**后来的 Turn |
| generation 围栏 | 重试判据用 `saturating_add` | u64::MAX 分支 | 分支会与自己的后继比较相等，误判为重试 |

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
- 新增 `EmbeddedRuntimeError::SessionTurnRebound` → `Conflict`，与既有 `ControlCommandRebound` 同构：
  调用方重用了幂等键，这是它能据以行动的事实，不该混进 `Configuration` 字符串里变成 `Internal`。
- Rollback 重试判据由 `starts_with` 收紧为**相等**，并追加"没有活跃 Turn"；`saturating_add` 换成
  `checked_add`。回滚到前缀之后又追加过 Turn 时，历史是 `[prefix, later]`——它确实以 `prefix` 开头，
  旧判据因此把重放当成重试，答成功并丢掉 `later`。

## 可执行门禁

| 门禁 | 结果 |
| --- | --- |
| `--lib client::tests` | 3/3 |
| `--test runtime_client_contract` | 7/7 |
| `--test grpc_session_contract` | 1/1 |
| `--test embedded_recovery_all` | 3/3 |
| `--test standalone_run root_session_` | 2/2 |
| `--test grpc_invocation_identity` | 9/9 |
| `--test approval_flow` | 9/9 |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| `cargo fmt --all -- --check` | exit 0 |
| `git diff --check` | clean |
| `cargo test --workspace --all-targets --all-features` | 856/856（122 套件、8 ignored） |

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

## 第二轮加固（同日）

第一轮收口了语义，这一轮收口的是**可靠性与容量边界**，不增加任何产品功能。

| 边界 | 修改前 | 风险 |
| --- | --- | --- |
| 投影失败 | 每种投影不了的理由都静默返回 | Checkpoint 已落盘却验不过的 Turn 被报成"还在跑"，分支永远卡住而无人有理由去查 |
| 文件删除 | 直接 `std::fs::remove_file` | 未同步的目录仍会列出已删除的文件 |
| Fork 重试 | generation 检查排在查找 target 之前 | 源分支继续或回滚过之后，重试被拒，调用方以为 Fork 失败而它就在那儿 |
| 存储容量 | 无上限 | Session 数、分支数、归档 generation、记录字节全部无界；单文件 Store 每个 Turn 重写整份记录 |
| 容量拒绝 | 无 | 无 |
| `u64::MAX` generation | 走到"陈旧 generation"冲突 | 范围错误被报成与无辜分支的冲突 |

### 实现结果

- `durable_file::remove` —— 删除并同步父目录；4 个失败注入单测覆盖正常路径、已不存在、删除失败、目录同步失败。
- 投影只豁免三件事：Session 尚不存在、本 invocation 看不见它、**没有 Checkpoint**（Turn 确实还欠着工作）。
  其余一律上报；提交失败只吞 generation 围栏。
- Fork 按请求自己命名的 generation 取历史（当前或已归档）；target 内容一致即返回，哪怕源已继续或回滚。
  target 不存在且源已移动时仍拒绝。
- `SessionStoragePolicy`：Workspace 1,000 / 租户 10,000 / 每 Session 32 分支 / 每分支 16 归档 generation /
  单记录 8 MiB / 单 Turn 预留 2 MiB。全部在 `Initialize` 公布。
- `LocalRuntimeError::SessionCapacity` → `ResourceExhausted`。它既不是调用方的错也不是存储坏了：请求合法、
  状态完好，只是没地方了。告诉 `Conflict` 它会换内容重试，告诉 `Unavailable` 它会原样重试，两者都没用。
- 上限是 `EmbeddedRuntime::new_with_policies` 的构造参数而非常量。**没人能触发的上限就是没人验过的上限**；
  现有 30 处构造点一处未改。

### 新增门禁

| 门禁 | 结果 |
| --- | --- |
| `durable_file` 失败注入 | 9/9 |
| Checkpoint 验不过 → `DataLoss`（read 与 continue 皆是） | 1/1，且**验证过它会失败**：改回安静版即转红 |
| Fork 重试跨源分支 continue 与 rollback | 1/1 |
| 三道上限各 N 成功 / N+1 拒绝，拒绝前后 Provider 调用数不变 | 1/1 |
| 记录字节上限走到边界，拒绝后无残留 active Turn | 1/1 |
| `u64::MAX` generation → `InvalidRequest` | 1/1 |
| `--test runtime_client_contract` | 11/11 |

## 覆盖清单

计划列出的 12 项已全部覆盖。最后两项及其构造方式：

- **终态崩溃窗口恢复且 Provider 请求次数不增加**。窗口按持久格式在磁盘上重建，而不是去抢一个真实崩溃：
  Turn 完成后把 `active_turn` 写回记录、清空 `history`，这正是崩溃留下的状态，且完全确定。随后由替换
  Runtime 启动恢复，断言 head 被补完、**历史摘要与原先逐字相同**、分支恢复可继续，而计数 Provider 的
  请求数**一次未增**。该实现原本就是对的——这一项是补缺失的覆盖，不是驱动修复。
- **schema v1→v2 安全迁移**。两个真实 Session 各自跑完一个 Turn 后被降级为 v1 形状（`store_version`
  降为 1、删掉 `invocation` 字段）。本地默认身份能读回自己的 v1 记录；任何其他身份都不能——记录没有
  owner **不等于**同意被任意认领。跨身份按 id 直接读同样是 `NotFound`。

`generation` 溢出的显式拒绝已在 Rollback 重试判据上改为 `checked_add`，但**尚无独立测试**——构造
u64::MAX 分支需要直接写 Session 记录，留待下一轮。

## 两处负载敏感的间歇失败，均不属本轮

四次全量的实况：**854/854 绿 → 1 失败 → 856/856 绿 → 1 失败（另一个测试）**。两个失败都在
`agent-runtime-host` 的 Session 路径之外，隔离重跑均全绿，且都是时间界断言。本轮**一处都没有放宽**。

### 其二：`process_wait_multi_session_capacity::sixty_four_sessions_...`

```
64 sessions / 1024 waits: p50=1.130572709s, p95=1.131459375s, p100=1.140962417s
wake latency gate failed（门槛 p50 < 1s）
```

隔离重跑 3/3 通过。这条本来就在"待决定"清单上，属 `crates/tool-runtime`，与 Session 无关。
延迟门在满载机器上是否仍是正确门槛，是产品决定，不是可以顺手调大的数字。

### 其一：`subagent_concurrency::close_cancels_only_the_targeted_asynchronous_child_and_reaps_its_stream`

隔离重跑 5/5 通过。

机制在测试的假 provider，不在产品：

```rust
let child_closed  = timeout(Duration::from_secs(1), ...);
let parent_result = timeout(Duration::from_secs(1), listener.accept());
let (closed, parent_result) = join!(child_closed, parent_result);
let Ok(true) = closed                  else { return false };
let Ok(Ok((mut p, _))) = parent_result else { return false };
```

两条不同的失败路径都折叠成 `return false`，断言却统一报"没有回收子流"——它无法区分究竟是子流未被
回收，还是父回合未在 1 秒内到达。全量并发下这两个 1 秒界都可能被超。

**本轮未修改它。** 正确的修法是让失败说清是哪一条，而不是抬高那两个界——后者正是规矩禁止的
"靠延长 timeout 处理 flaky"。这属于"应用安全关闭与恢复"那一轮的工作，在此登记以免下次被当成新问题。

## 一处未复现的观察

收紧 Rollback 判据后的第一次运行里，`a_session_client_is_retry_safe_fenced_paged_and_restartable`
在 `.expect("Fork")` 处失败过一次。当时的过滤把失败消息截掉了，没有留下原因；随后连跑 7 次全绿，
无法复现。**不记为已解决**，记为未复现观察；若再出现，行号可直接定位。
