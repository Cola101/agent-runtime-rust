# Owner 作用域与应用生命周期证据（2026-08-18）

## 复核事实

| 边界 | 修改前 | 风险 |
| --- | --- | --- |
| Session 契约的可达面 | 仅 `RuntimeClient` 与 gRPC | 桌面走 Unix socket，`LocalRequest` 十个变体里没有任何 Session 操作——做完了，够不着 |
| Controller 的持有方式 | Host-owner 进程内直接持有 | Tauri 拿得到，Electron 拿不到；同一个缺口即将第二次出现 |
| 关闭 | 无 | "退出应用"与"用户取消 Run"无从区分 |
| 恢复期间 | 监听器在恢复完成后才开 | 客户端分不清"正在恢复"与"没在运行"，有 Run 要恢复的桌面每次启动都自报故障 |
| `Interrupted` | 只说没产出 Checkpoint | 关应用与进程崩溃读起来一模一样 |
| 排空期限 | —— | 若做成常量，测试要么干等真实值、要么走不到超时分支 |

## 实现结果

- `OwnerRequest` / `OwnerResponse` 与 `LocalRequest` 分成两个枚举，scope 标记信封分发。
  **无 scope = 工作负载**，现有客户端一字不改；**命名了 scope 就只按该 scope 解析，没有任何回退**。
- owner 面承载生命周期（Start/Snapshot/Shutdown）、Session 七个操作、`ListRuns`。
  Session 操作**不要求送 invocation**。
- `RuntimeController`：一次通过的状态机、并发等待者同一答案、停止后不重启、恢复进度可见、
  关闭报告交接一次。
- 唯一任务注册点覆盖 Execute / Session Turn / Resume / Approval / MCP Input；
  停止走 `AbortHandle`，**从不触碰 `CancellationToken`**。
- 关闭顺序：关准入并释放排队者 → 等活跃执行数归零（有界）→ 强制停止 → 按 Checkpoint 分类。
- `RunInterruptCause`：`RuntimeStopped` / `HostEndedWithoutStopping` / `Unknown`（旧记录）。
- `NotReady` 是独立应答类型并带 `lifecycle` 与恢复进度；`is_mutation()` 逐变体写死，不用默认加例外。
- SIGINT/SIGTERM 经同一 Controller。

## 可执行门禁

| 门禁 | 结果 |
| --- | --- |
| `--test owner_socket_contract` | 6/6 |
| `--test runtime_lifecycle` | 6/6（全套 0.2 秒内） |
| `--test local_ipc` | 5/5 |
| `--test daemon_recovery` | 9/9 |
| `--test approval_flow` | 9/9 |
| `--test subagent_concurrency` | 20/20 |
| `--lib` | 42/42 |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| `cargo fmt --all -- --check` | exit 0 |

**真二进制实测**：`SIGTERM` → `terminated, draining` → 关闭报告 → 进程退出 → 同一状态目录重启成功，
且关闭前提交的 Run 重启后仍可见。

## 三处守卫都见过它失败

不是"跑绿了就算数"：

- **作用域隔离**——故意加上 owner→workload 回退，断言当场红，报 `Workload(List)` 从 owner scope 到达。
- **`approval_flow` 缺二进制**——把二进制临时改名（不删），失败从 60 秒变 0.00 秒并给出构建命令。
- **子代理 close 的可读性**——把父回合的界压到 0，消息变为"父回合未在一秒内到达"，
  而不是笼统的"没有回收子流"。**一个界都没有抬高。**

## 一处必须记下的更正

上一个提交声称"重启后工作负载 `List` 为空而 owner 列表不为空，差异是声明的"。**那不是产品性质。**

`serve` 配套的 `recover_unfinished()` 会从磁盘上属于本 invocation 的记录重建顺序，真实 host 重启后
带着 Run 回来。测试里看到空，只因为测试的 `start_daemon` 从不调它——**测试脚手架的产物被当成了产品性质**，
并写进了提交信息、计划与桌面客户端的空态文案。

改法：让测试按生产方式启动守护进程（当场转红），断言换成真实存在的差异——
**一串 id 说不出这个 Run 被要求做什么、走到哪了**，而那正是桌面需要的。
`ListRuns` 依然需要，理由换成了正确的那个。

待办：桌面 `Runs.tsx` 的空态文案沿用了错误说法，随迁移到 `ListRuns` 时重写。

## 一处我自己引入并修复的回归

`serve` 改为并发启动恢复后，`daemon_recovery` 的三个测试开始拿到 `NotReady { Recovering }`——
那几个测试正是把带未完成 Run 的状态根交给守护进程，恢复真的要花时间。

**行为是对的**（恢复中提交就该被告知），要改的是测试：按契约等待就绪，而不是与之赛跑。9/9 恢复。

## 第三个同形状的负载敏感失败

全量最后一次跑出 `embedded_retention::one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded`
失败：**182.73 秒对 180 秒预算，差 1.5%**。隔离重跑 **132.02 秒**，余量 27%。

本会话至此有三个形状完全相同的失败，都是墙钟预算、都在全量并发下越界、都在隔离下通过：

| 测试 | 全量 | 隔离 |
| --- | --- | --- |
| `process_wait_multi_session_capacity` | p50 1.13s 对 1s 门 | 通过 |
| `subagent_concurrency::close_cancels` | 1 秒界超时 | 5/5 通过 |
| `embedded_retention::one_thousand_runs` | 182.73s 对 180s | 132.02s |

**一条都没有放宽。** 三者都不是竞态而是预算：在一台同时跑着几十个测试二进制的机器上，
墙钟预算衡量的是机器有多忙，不是被测代码有多快。这是一个需要产品决定的问题
（预算该不该按负载归一化，或这些测试该不该独占运行），不是可以顺手调大的数字。

关于第三个是否由本轮引入：任务注册表每次分离 spawn 多一次 HashMap 插入与删除，
一千个 Run 合计数毫秒，解释不了 50 秒的差；隔离下 132 秒也与"负载所致"一致。
但**没有同机的改动前基线**——这是推理加一次隔离测量，不是对照实验，据此记录。

## 未验证与下一批

- 恢复期间 mutation 得到 `NotReady` 这条，验的是**停止端**（关闭后）而非恢复端：
  空状态根上的恢复以微秒计，与之赛跑的测试会飘。两端走的是同一道门。
- 桌面客户端尚未迁移到 owner 面。迁移后其镜像的 7 个 UUID 与
  `desktop/scripts/check-local-invocation.sh` 可一并删除。

下一批固定顺序：**Profile / Credential 生命周期 → 干净目录可分发验收 → 正式 GUI 开发**。
