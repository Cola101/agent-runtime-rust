# 计划 0001：让 Runtime 可被桌面客户端驱动

- 日期：2026-08-18
- 预计：12–18 小时
- 范围：`runtime/**`。本批不改 `desktop/**`，不做 Credential Resolver、可分发 artifact 或 GUI
- 前序：ADR-0142（Runtime Client 契约）、ADR-0143（Session 语义与容量）

## 判据

不是"生命周期做完了"，而是：

> **Electron 客户端能启动 Runtime、看着它恢复完、跑一条 Session、关窗口时干净退出，并在下次打开时看到上次留下了什么。**

## 背景：两次"做完了，桌面用不上"

ADR-0143 收口了 Session 契约，挂在 `RuntimeClient`（进程内）与 gRPC 上。桌面客户端走的是 Unix socket，
而 `LocalRequest` 的十个变体里**没有任何 Session 操作**：

```
Submit / Attach / EventCursor / List / Approve / Deny
/ ResolveMcpInput / Cancel / Resume / Control
```

上一版生命周期计划把 `RuntimeController` 定为 Host-owner 直接持有，并明确 shutdown 不进租户 gRPC ——
**两条都对**，但推论是 Tauri（同进程 Rust）能拿到 Controller，**Electron 拿不到**。

同一个缺口即将第二次出现。本批把它合并成一个决定，放在最前面。

## 阶段 0：Owner 作用域（本批前置）

### 依据

`runtime-host.sock` 已被设为 `0o600`（`ipc.rs`）。**能连上这个 socket，就已经是这台机器上的 owner**
—— 不需要令牌、mTLS 或任何新凭据。这正是桌面客户端身份气泡那句话的实现依据：

> 没有账号，也没有登录 —— 能连上这个 socket 就是凭据

### 做什么

- 新增 `OwnerRequest` / `OwnerResponse`，与工作负载的 `LocalRequest` **分成两个枚举**，由 scope 标记的信封分发。
- 分枚举不是为了权限（`0o600` 已经给了），是为了**可读与可测**：必须有一个测试断言"任何 owner 操作都无法
  从工作负载命名空间到达"，反之亦然。
- 现有 `LocalRequest` 十个变体一个不动；线格式仍是每行一个 JSON。

### Owner 面承载

| 类别 | 操作 |
| --- | --- |
| 生命周期 | `Start` / `Snapshot` / `Shutdown` |
| Session mutation | `SessionStart` / `SessionContinue` / `SessionFork` / `SessionRollback` |
| Session 读取 | `SessionRead` / `SessionList` / `SessionHistory` |
| 持久 Run 枚举 | `ListRuns`（暴露既有 `list_run_records`） |

### 退出标准

Electron 客户端能通过 socket 完成 Session 全链并驱动生命周期。**这一条不过，后两个阶段的产出桌面依旧用不上。**

## 阶段 1：应用生命周期

沿用既定设计：状态机 `Created → Recovering → Ready → Draining → Stopped`、单次转换、多等待者结果一致、
per-Profile 恢复隔离、admission `close()`、**用户 Cancel 与 Runtime Stop 是两条互不相干的路径**、
`RuntimeShutdownReport`、SIGINT/SIGTERM 走同一个 Controller、不新增持久 lifecycle 文件、不用 sleep 判排空。

以下五条是在此之上的补充，每条都来自实际构建客户端时撞到的问题。

### 1.1 恢复期间必须可连、可问、可看

「Startup 必须在开放请求入口前完成」——不留接单窗口是对的，但推论是恢复期间**没有任何监听器**。

桌面上的后果，客户端里有现成证据：

```
LinkBanner → unreachable → "连不上 Runtime — socket 没有回应"
```

Run 多时每次启动都先报故障，然后突然好了。**用户看到的是坏了，实际是正常恢复。**

改为**监听器早开，准入晚开**：

- socket 在 `Recovering` 就绪时即可连接；
- 所有 mutation（工作负载与 owner 皆然）返回明确的"正在恢复"，**不是**拒绝、**不是**超时；
- `Snapshot` 在 `Recovering` 期间返回进度（已完成 / 总数 / 各 Profile 状态）；
- 读取按既定规则。

一个能说"我在恢复，12/40"的 socket，比一个不回应的 socket 好得多，而"不留接单窗口"这条约束毫发无损。

### 1.2 drain 期限必须可注入

"连续压力重跑 30 次，不靠放宽 timeout 收敛"态度是对的，但 **10 秒 drain 本身就是测试必须等出来的墙钟量**。

本项目实测：满载下 `process_wait_multi_session_capacity` p50 = 1.13s 对 1s 门；
`subagent_concurrency::close_cancels` 亦挂过一次。同机跑 30 次生命周期压力，10 秒期限迟早成为下一个
"隔离下全绿"的间歇失败。

`SessionStoragePolicy` 上踩过反面的同一课：

> 没人能触发的上限，就是没人验过的上限。
> **测试必须干等出来的期限，迟早会飘。**

改为：deadline 随 Controller 构造进来，默认 10s，公开范围 0–30s 不变，**测试用 50ms**。

### 1.3 `Interrupted` 必须带原因

无 Checkpoint 的 Run 记为 `Interrupted` 且不自动重放是对的，但下次启动人看到的只有"被打断"，**没有原因**。

"你自己关了应用"和"进程崩了"对用户是完全不同的两件事，现在长得一样；客户端的 `lifecycleLabel` 目前也
只能显示"被打断"。

改为：在 Run record 的 `Interrupted` 上增加来源。**不是新增持久 lifecycle 文件**，是给已有状态加一个字段。

### 1.4 关闭报告必须活过进程

`RuntimeShutdownReport` 返回给 `shutdown()` 的调用方 —— 对退出中的桌面应用，那是进程死前最后一刻，**没人读**。

而里面的数字恰恰是下次启动最该说的：留待恢复 N 个、Interrupted M 个、是否触发 deadline。

改为：落到状态目录，下次 `start()` 读出并经 `Snapshot` 交给客户端，读后清除。
**它不参与恢复判定**（Checkpoint 仍是唯一权威），只是一份给人看的交接单。

### 1.5 先修一个已知会挡路的测试

`subagent_concurrency::close_cancels_only_the_targeted_asynchronous_child_and_reaps_its_stream`
已记录在 `docs/evidence/2026-08-18-session-acceptance-atomicity.md`：四次全量挂过一次，隔离 5/5 全绿。
它测的正是**关闭子代理并回收其流** —— 与本批的任务注册、AbortHandle、PTY/子进程回收是同一片地。

它的问题不是慢，是**说不清**：两个独立的 1 秒界折叠成同一个 `return false`，断言却指名一个它区分不出的原因。

**先让它的失败能说清是哪一条，不抬高界。** 否则本批每次全量都要重新判断"这次是不是我弄的"。

## 阶段 2：桌面看得见

### 2.1 持久 Run 列表

`LocalRequest::List` 走守护进程内存里的 `order: Arc<Mutex<Vec<Uuid>>>`，**host 一重启就空**，
而磁盘上 Run 还在。客户端现在只能诚实地写：

> 这个 runtime-host 自启动以来还没有跑过 Run。
> 磁盘上可能仍有更早的 Run —— List 走的是内存里的顺序，重启后就空了。

`list_run_records(state_root)` 已存在且为 `pub`。owner 面 `ListRuns` 直接暴露它，
分页与上限沿用 Session 列表的既有约定（256）。

### 2.2 Run 的输入进日志

`run.started` 的 payload 只有 `{"status":"running"}`。**日志里没有 Run 被要求做什么**，
所以任何不是本客户端发起的 Run，"问的是什么"永远显示不出来 —— 客户端只能自己记一份，重装即失。

在 `run.started` 中带上输入（受既有 32,000 字节上限约束）。这是恢复与审计都需要的信息，不只是 UI 需要。

## 不做

- Credential Resolver、Profile 动态生命周期
- 可分发 artifact
- per-tool 策略的**写入**（读取已由既有策略快照满足）
- 模型 / 努力度切换、steer
- GUI、Edge、Java、模型协议
- 分段 Session Store

## 实现约束

- `ActiveExecution` 保存可等待的完成通知与独立 AbortHandle；用户 Cancel 与 Runtime Stop 必须是两条不同路径。
- 所有 Execute、Session Turn、Resume、Approval、MCP Input 使用同一任务注册函数，禁止各自维护 JoinHandle。
- 已等待审批或 MCP Input 的 Run 不占执行槽，关闭后仍保持可恢复、可审批。
- 不用 sleep 判排空：使用 admission snapshot、active map、Notify 与 JoinHandle。
- **owner 面不进 gRPC**。跨机器的 owner 操作是另一个安全模型，本批不碰。
- `RuntimeClient` 契约升为 **v2**（mutation 增加 Ready 前置），v1 不伪装兼容。owner socket 面独立编版本，首版 v1。
- owner 面新增的任何镜像常量必须纳入 `desktop/scripts/check-local-invocation.sh`。
- `desktop/**` 与 `runtime/**` 的隔离不变：本批只改 runtime，桌面接入放在其后。

## 测试与验收

先建立确定性 RED。

**生命周期**

- `Created` / `Recovering` / `Draining` / `Stopped` 均拒绝新 mutation，且无持久残留。
- 多个并发 `start()`、`shutdown()` 只执行一次状态转换，结果一致。
- 关闭 admission 后所有排队请求立即释放，租户公平计数归零。
- 短 Run 在 drain 期限内正常终态。
- 长 Run 超时停止后**没有** `run.cancelled`、没有用户 Cancel receipt，替换 Runtime 可从 Checkpoint 恢复。
- 等待审批的 Run 关闭、重启后仍可批准。
- 非幂等 Tool 在关闭窗口中的模糊结果不会重放，沿用既有 `indeterminate` 语义。
- 一个 Profile 恢复失败不阻塞其他租户；失败 Profile 的新 Run 返回 `Unavailable`。
- Event subscription、gRPC、Unix socket、Provider、MCP、PTY 与子进程无残留。
- SIGTERM 后进程有界退出，随后同一状态目录能重新启动。

**本批新增**

- **作用域隔离**：任何 owner 操作都无法从 `LocalRequest` 命名空间到达；反之亦然。
- **恢复期间可连**：socket 在 `Recovering` 即可连接；mutation 返回"正在恢复"而非拒绝或超时；
  `Snapshot` 返回单调不减的进度。
- **drain 期限注入**：50ms 期限下走完全部超时分支，**不靠等 10 秒**。
- **`Interrupted` 带来源**：应用退出与崩溃产生的 `Interrupted` 可区分。
- **关闭报告跨进程**：关闭时的计数在下次 `start()` 后可读，读后清除，且不影响恢复判定。
- **持久 Run 列表**：host 重启后 `ListRuns` 仍返回磁盘上的 Run；`List`（内存）与 `ListRuns`（持久）的
  差异是**声明的**，不是意外。
- **`run.started` 携带输入**：替换 Runtime 后仍可读出该 Run 被要求做什么。
- 生命周期主链连续压力 **30 次**，不靠放宽 timeout 收敛。

**门禁顺序**

```bash
cargo test -p agent-runtime-host --lib
cargo test -p agent-runtime-host --test runtime_lifecycle
cargo test -p agent-runtime-host --test owner_socket_contract
cargo test -p agent-runtime-host --test runtime_client_contract
cargo test -p agent-runtime-host --test grpc_session_contract
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

不得用增加超时、串行化全部测试或删除断言处理 flaky。缓存沿用清理后唯一的 `runtime/target`，
本批不重复清理，只记录增量与最终大小。

## 交付

- 新增 **ADR-0144**（owner 作用域与应用生命周期）；修订 ADR-0143 中因契约 v2 而改变的前置描述。
- 更新路线图、实现状态与一份可复核 evidence，**删除已完成却仍列为缺口的旧描述**。
- 对标：
  - **Codex** —— bounded thread shutdown、多等待者、任务 abort reason。
  - **OpenClaw** —— signal、ingress drain、后台 hook、子进程升级回收。
  - **本项目额外证明** —— 多租户隔离、Checkpoint 恢复、"应用退出 ≠ 用户取消"，以及**恢复期间可观测**。
- 全部门禁通过后提交并推送。

## 本批之后

固定顺序不变：**Profile / Credential 生命周期 → 干净目录可分发验收 → 正式 GUI 开发**。

桌面客户端接入本批产出（owner socket、生命周期、持久 Run 列表）单独成批，不与本批混做。
