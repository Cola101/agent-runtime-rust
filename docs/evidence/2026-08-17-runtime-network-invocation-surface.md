# 证据：Runtime 网络调用契约（ADR-0123）

- 日期：2026-08-17
- 机器：M1 Pro 16GB，macOS darwin 25.5.0
- 未使用：Docker、虚拟机、Kubernetes、Java、PostgreSQL、NATS、外部 API Key、真实厂商服务

## 复核出来的事实

不是从文档表格读的，是查代码得到的：

| 检查 | 结果 |
| --- | --- |
| `runtime.proto::RuntimeControl` 服务端实现 | **0** |
| `runtime.proto::RuntimeControl` 客户端引用 | **0** |
| 编译 `runtime.proto` 的 crate | **0**（连 build.rs 都没有） |
| `openapi.yaml` 的 Rust 实现 | **0**（全工作区 axum 只被 `runtime-health` 用于健康端口） |
| `runtime-host` 生产监听 | 仅 Unix domain socket；`TcpListener` 全在 `#[cfg(test)]` 内 |

即：**要跑一次 Run，只有把 `EmbeddedRuntime` 编进自己进程，或在同一台机器上连 Unix socket 两条路。**

## 测量结果

| 门禁 | 结果 |
| --- | --- |
| `--test grpc_invocation_identity` | **8 passed, 0 failed**, TEST_EXIT=0 |
| `--test grpc_invocation_loop` | **1 passed, 0 failed**, TEST_EXIT=0 |
| `cargo fmt --all -- --check` | FMT_CHECK_EXIT=0 |
| `clippy --workspace --all-targets --all-features -D warnings` | CLIPPY_EXIT=0 |
| `cargo test --workspace -- --test-threads=4` | 见下 |

### 全量门禁（第一段，fullgate19）——**不是绿的**

`cargo test --workspace --no-fail-fast -- --test-threads=4`（fullgate19）：

**126 二进制、790 passed、2 failed、6 ignored、`CARGO_EXIT=101`**

两条失败**都是墙钟预算断言，都不是正确性失败**，且都与本轮改动无关：

| 失败 | 测得值 | 判定 |
| --- | --- | --- |
| `sixty_four_sessions_keep_one_thousand_waits_bounded_and_tenant_fair`（`process_wait_multi_session_capacity.rs:438`） | p50=1.048s（门禁 <1s），p95=1.162s（<2s 通过），p100=1.165s（<4s 通过） | **已知未决问题**，本会话此前已记录并明确挂起等用户决定，不得私自放宽 |
| `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded`（`embedded_retention.rs:1223`） | `scan_elapsed >= 2s` | 本轮首次浮现，见下 |

第二条的取证：

- 该测试**前面所有正确性断言全部通过**（`run_directories_after == 16`、墓碑计数、
  `terminal_ledger_bytes < 2 MiB`），失败的只有末尾的 `scan_elapsed < 2s`。
- **单独重跑通过**：`TEST_EXIT=0`，耗时 114.11 秒；而在全量并行下同一测试耗时 216.75 秒，
  接近两倍。2 秒扫描预算是被这个负载差挤爆的。
- **本轮改动没有任何路径能影响它**：`RuntimeInvocationGrpcService` 在 `embedded_retention.rs` 中
  从未被构造，新模块被编译但不被执行；`embedded.rs`、`retention.rs` 与 Run 状态路径未改一行。
- 它与第一条属于**同一类问题**：在不受控的并行负载下断言墙钟预算。这是该类问题的第二个实例。

**两条都没有被修改、放宽或排除。** 按既定纪律，性能预算表达的是意图，改动它是用户的决定，不是我的。

本轮改动自身的门禁：`--test grpc_invocation_identity` 8/8、`--test grpc_invocation_loop` 1/1、
fmt `EXIT=0`、clippy `--workspace --all-targets --all-features -D warnings` `EXIT=0`。

机器状态：target 8.5G（新 crate 与 tonic 依赖使其自 5.8G 增长），磁盘 46Gi 可用（下限 20Gi）。

## 真实闭环证明了什么

`a_network_caller_submits_observes_and_completes_a_real_run`：调用方**只持有一个 TCP 地址和一个
bearer token**，全程没有任何指向 Runtime 的进程内 handle。

1. `Submit` 返回被接纳的 `run_id`
2. 反复 `ReadEvents` 分页前进，**按 typed 边界停止**，不靠猜事件列表
3. 终态边界为 `Terminal { status: "succeeded" }`
4. 观察到的事件类型：`run.started`、`model.provider.selected`、`model.output.delta`、`run.succeeded`
5. 从同一个面重放整段事件，读回模型真实输出

Provider 是真实回环 HTTP/SSE 服务，所以 Run 是**真的执行了**，不是模拟。

## 身份边界证明了什么

8 项，每项都是一次真实的 gRPC 调用：

- 无 token → `unauthenticated`
- **Run 形状 token 携带 `runtime.invoke` → `permission_denied`**（不是 `unauthenticated`：token 有效，
  重新认证也不会把 Run 变成运维）
- 运维 token 但只有 `mcp.oauth.admin` → `unauthenticated`（管理凭证不蕴含开 Run）
- 请求体命名他租户 → `permission_denied`
- 未注册 Profile → `permission_denied`，且消息为固定文本（不确认其是否存在）
- `ReadEvents` 与 `Control` 与 `Submit` 同等认证（只有写路径检查的面会让 token 读到别人的 transcript）
- 未知 control action → `invalid_argument`，不进入持久命令路径
- 任何拒绝消息都不含 state root、`/var`、`/tmp`

## 过程中两次自己的错误

**一、Run 形状测试最初为假绿的反面。** 首跑得到 `unauthenticated` 而非 `permission_denied`。
原因是我伪造的 schema-4 claims 把 `model_policy_digest` 留成空串，而 schema 4 要求它是 sha256——
token 在到达**形状检查之前**就以 `InvalidClaims` 被拒了。那样测的是「我造的 token 不合法」，
不是「形状被拒」。补成除形状外完全合法后才真正命中边界。测试文件里记了这一点。

**二、`Debug` 被用作线路取值。** 初版用 `format!("{:?}").to_lowercase()` 产出状态字符串。
`RunStatus::WaitingApproval` 会变成 `waitingapproval`，而系统其余部分一律写 `waiting_approval`；
更糟的是 `LocalRunState` 是 `#[serde(tag = "state")]` 的带数据枚举，`Cancelling { reason }` 会把
**操作员填写的 reason 文本**写进一个被文档描述为状态标记的字段。改为从 serde 取规范标记。

## 第二段：binary 接线（同日）

`--test invocation_surface_config` **3 passed, 0 failed**，TEST_EXIT=0。三条都 spawn**真实的
`runtime-host serve` 二进制**，不是调用库函数——被测的性质是「部署出来的东西会怎样」。

- 设了 `AGENT_RUNTIME_INVOCATION_BIND`、缺 mTLS 材料 → 拒绝启动，错误具名到缺失的变量
- 有 mTLS 材料、缺 workload 验签公钥 → 拒绝启动（不能验签的面不得接受连接）
- 不设 bind 地址 → 启动失败来自**完全无关的原因**，四个材料变量一个都没被要求
  （证明这个面默认关闭，既有安装升级不会静默多出网络监听器）

### 这一段暴露了我 `main.rs` 里的真实顺序问题

首跑红：错误是 `AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT is required`，不是缺 TLS。
因为我把 `load_invocation_surface()` 放在了 `load_config()` **之后**——
「你要了网络面却没给 mTLS」这个**唯一与安全相关的门**，被一个无关配置错误盖住了。
操作员会先修 provider endpoint、重启，才发现真正的问题。已把该检查移到所有配置之前。

**这不是测试写错，是产品的诊断顺序错了，测试把它抓出来了。**

### 全量门禁（第二段，fullgate20）——绿

**127 二进制、795 passed、0 failed、6 ignored、`CARGO_EXIT=0`**

对照 fullgate19（126 / 790 / 2 failed）：二进制 **+1**（新增配置测试文件），
通过 **+5** = 3 个新测试 + **上一轮那两条墙钟失败这次都通过了**。差值精确闭合。

这一次的通过**同时也是证据**：那两条失败是负载相关的偶发，不是确定性失败——
与「单独重跑 114s / 全量并行 216s」的取证一致。两条门禁仍未被修改、放宽或排除，
仍等用户决定。fmt `EXIT=0`；clippy `--workspace --all-targets --all-features -D warnings` `EXIT=0`。

机器状态：磁盘 42Gi 可用（下限 20Gi）。

## 第三段：真实 mTLS（同日）

`--test grpc_invocation_mtls` **3 passed, 0 failed**，TEST_EXIT=0。证书用 `rcgen` 现场生成，
不落盘、不入库、测试结束即消失。

| 测试 | 证明 |
| --- | --- |
| `a_real_run_completes_over_mutual_tls` | 一次**完整真实 Run** 穿过 mTLS：Submit → 分页排空游标 → `Terminal { succeeded }` |
| `a_client_presenting_no_certificate_is_refused` | 不出示客户端证书 → 被拒 |
| `a_client_certificate_from_another_authority_is_refused` | 出示他方 CA 签发的合法证书 → 被拒（CA 钉定是真的，不是装饰） |

**只证明成功路径的话，这个面是 TLS 而不是 mTLS。** 两条拒绝是「mutual」的全部含义所在。

### 防假绿的对照组

两条拒绝测试各自起自己的服务端。若服务端根本没起来，`connect` 会失败，断言会**因为错误的理由变绿**。
因此每条拒绝测试前先用**同一 CA 签发的另一张合法证书**连一次并断言成功——服务端确实在跑、
确实接受合法证书，差别只在证书本身。加上对照后仍然 3/3。

（这正是本轮第一段栽过的坑：Run 形状 token 测试当时也是因为错误理由才红的。）

### 全量门禁（第三段，fullgate21）——1 项失败

**128 二进制、797 passed、1 failed、6 ignored、`CARGO_EXIT=101`**

对照 fullgate20（127 / 795 / 0）：二进制 **+1**、总数 **+3** = 三个新 mTLS 测试。
797 passed + 1 failed = 798 = 795 + 3，差值精确闭合——**三个新测试全过**。

唯一失败的是 `sixty_four_sessions_keep_one_thousand_waits_bounded_and_tenant_fair`。
**它与前两轮是同一个测试，但不是同一个失败**，这点必须分清：

- 本次的延迟三项**全部通过**：p50=950.916ms（<1s）、p95=1.065s（<2s）、p100=1.089s（<4s）。
- 失败发生在 `process_wait_multi_session_capacity.rs:471`，不是 438：
  `session 55 failed close: persistent process session I/O failed: Operation not permitted (os error 1)`。

这是**第三个、此前未见的失败模式**，而且**不是墙钟预算断言**——EPERM 是真实的 OS 层错误。
隔离下连跑三次均通过（EXIT=0，3/3），因此同样只在全工作区并行负载下出现。

**成因未确定，不做猜测。** 关闭时收到 EPERM 可能指向重度进程churn 下的 PID 复用
（`ADR-0098` 曾修过一条相关竞态：身份租约释放后仍终止旧 PGID），也可能是别的。
一次观测不足以定因，本轮未做任何修改。**它应作为独立的新开项记录，不得并入已有的延迟门禁条目。**

fmt `EXIT=0`；clippy `--workspace --all-targets --all-features -D warnings` `EXIT=0`。
残留扫描：0 个遗留 `runtime-host` 进程、无遗留临时目录、磁盘 42Gi。

## 第四段：控制路径（同日）

`--test grpc_invocation_control` **3 passed, 0 failed**，TEST_EXIT=0。

| 测试 | 证明 |
| --- | --- |
| `a_network_cancel_stops_a_live_run_and_returns_a_receipt` | 网络取消真的停掉一个**活着的** Run，终态边界 `cancelled`，收据带 64 位摘要 |
| `replaying_the_same_command_id_returns_the_same_receipt` | 同 id 重放 → 同一决定（网络客户端丢响应后必然重试） |
| `the_same_command_id_cannot_be_reused_for_a_different_action` | 同 id 换 action → `failed_precondition` |

Provider 故意只接收不回应，Run 因此保持存活——否则"取消成功"会变成与 Run 自然结束的赛跑。

### 这一段找到一个真实缺陷：我自己的契约在说谎

`runtime.proto` 里我写的是「改用途会被 receipt 的 command digest 拒绝，不会被静默接受」。
第三条测试首跑：**被接受了**，返回 `state: Accepted` 的成功收据。

根因：摘要绑定检查存在于 `embedded.rs:1766-1775` 的 `control()` 中，
而 `control_detached()` **先返回 `Accepted`，再异步执行 `control()`**。
调用方拿到的是一张账本随后会拒绝的收据。

**本地适配器一直没暴露这个**——`ipc.rs` 自己先做了一遍同样的检查。网络调用方没有那样的适配器，
所以这个面一测就出来了。这也是本轮唯一一个「新面把旧缺陷照出来」的实例。

修在 `embedded`（账本的性质），不在 gRPC 层（传输的性质）；新增 typed 变体
`ControlCommandRebound` → `failed_precondition`，因为复用幂等键是调用方**自己能改的错**，
以 `internal` 返回等于把可行动的错误说成内部故障。

回归确认：`-p agent-runtime-host` 全量 **25 二进制、193 passed、0 failed**——
本地适配器既有路径未受影响。

### 全量门禁（第四段，fullgate22）——绿

**129 二进制、801 passed、0 failed、6 ignored、`CARGO_EXIT=0`**

对照 fullgate21（128 / 797 passed + 1 failed = 798 总）：+1 二进制、总数 +3 = 三个新控制测试。
上一轮那条 EPERM 失败本次未复现（与「隔离 3/3 通过」一致，仍是负载相关，仍未被改动）。
fmt `EXIT=0`；clippy `EXIT=0`；残留扫描：0 个遗留进程，磁盘 45Gi。

## 第五段：审批（同日）

`--test grpc_invocation_approval` **1 passed, 0 failed**，TEST_EXIT=0。**未改动任何产品代码。**

链路：Run 停在审批门 → 调用方**只凭事件字节**取出 `approval_id` 与
`approval.execution.binding_digest` → `DecideApproval(allow_once)` → 真实 Tool 执行（`tool.result`）
→ 模型继续 → `Terminal { succeeded }`，转录含审批后的回答。

测试**全程不读 Runtime 状态目录**。这是要点：`binding_digest` 把决定钉到那次具体的 Tool 调用上，
若它在网络上取不到，这个面就只能观察到 Run 卡住而永远无法放行。

### 过程中三次错误，都是我的，产品没错

1. **provider 能力配错**：注册了 Tool 却只声明 `Capability::Text`，Run 因此选不出候选。
   产品**正确地**过滤并 fail-closed。诊断方式是把 state root 固定下来直接读 `run.json`，
   而不是猜——首个假设（并发写导致撕裂行）被"两次运行结果完全相同"推翻了。
2. **在 payload 里找事件类型**：类型在 `event.r#type` 字段，不在载荷里。
3. **clippy 抓到的循环**：`for` + `?` 会在第一个不可解析事件处**从整个函数返回 `None`**，
   而不是继续下一个。改为 `find_map` 才是本意。这次 clippy 抓的不只是风格。

### Provider 选择失败终态补证（同日）

上述诊断暴露的缺口已经单独复现并修复。新增测试先证明原行为：候选因费用策略无法入选后，
`run.json` 为 failed，日志却只有 `run.started`，事件游标返回 `CorruptLog`。修复后 Host 将
`ProviderSelection` 与仍可恢复的普通 `Provider` 调用失败分型；前者经 Kernel 状态机提交唯一
`run.failed` 和终态 Checkpoint，游标稳定返回 `Terminal { failed }`。

这次没有把所有 Provider 错误都终止：真实 503 的压缩恢复测试继续通过，证明仍有预算的暂态失败
仍可由替代 Host 恢复。聚焦证据：`embedded_multi_tenant` 8/8、`grpc_invocation_loop` 1/1、
`replacement_host_retries_the_same_pending_compaction_without_replaying_tools` 1/1；
`an_interrupted_provider_attempt_is_never_replayed_past_its_durable_budget` 1/1；
`agent-runtime-host` library clippy `-D warnings` 与全工作区 fmt check 通过。

同一审计随后覆盖了“崩溃后发现 Provider 尝试预算已耗尽”的路径。RED 证明日志只有
`run.started`、`run.restored`、`model.provider.failed`，run 记录却已 Finished。修复把 journal 中
已经报告过的终局失败作为 staged `ModelStreamEvent::Failed` 交回 Kernel，而不是从 Host 旁路写状态。
因此 diagnostic 仍只暴露摘要、`run.failed` 恰好一次且位于日志末尾。完整 `daemon_recovery` 9/9、
`multi_provider` 14/14 通过；新增测试还直接覆盖了不经替代 Host 的 live exhaustion。真实 503 压缩恢复
聚焦测试继续通过，证明暂态与终局没有重新混在一起。

### 全量门禁（第五段，fullgate23）——绿

**130 二进制、802 passed、0 failed、6 ignored、`CARGO_EXIT=0`**

对照 fullgate22（129 / 801 / 0）：+1 二进制、+1 通过 = 审批测试本身，精确闭合。
fmt `EXIT=0`；clippy `EXIT=0`；残留扫描 0；磁盘 41Gi。

## 第六段：跨进程恢复（同日）

`--test grpc_invocation_recovery` **1 passed, 0 failed**，TEST_EXIT=0。**又是零产品代码改动。**

第一个 Runtime 起 Run → 停在审批 → **整个 tokio runtime（连同服务、执行任务、state-root 锁）被丢弃**
→ 第二个 Runtime 打开同一 state root → 调用方**只带崩溃前的 `run_id`** 重连 → 读到死掉 Runtime 写的
完整历史 → 用第一个 Runtime 签发的 `owner_epoch` 送出审批 → 工具执行 → `succeeded`。

反向断言：整段历史里 `run.started` **只出现一次**——替代者接着干，不是重跑。

### 三次纠正，全是我的测试错，产品每次都对

1. `Workspace state root already has another Runtime owner`——丢 `Arc` 模拟不了崩溃：
   **Run 自己的执行任务持有一个 `Arc` 并停在审批上**，锁不释放。单写守卫是对的。
2. `await` 掉被 abort 的 server 仍不够，同一原因。最终沿用 `daemon_recovery.rs` 的既有形状：
   第一个 Runtime 跑在独立线程的独立 tokio runtime 上，drop 掉整个 runtime。
3. `recover_unfinished_detached` 返回 0 而我断言 1。读代码确认 `AwaitingApproval`
   **被显式跳过**——等人的 Run 没有东西可重新派发。**0 才是契约**，断言据此改写。

### 全量门禁（第六段，fullgate24）——绿

**131 二进制、803 passed、0 failed、6 ignored、`CARGO_EXIT=0`**

对照 fullgate23（130 / 802 / 0）：+1 二进制、+1 通过 = 恢复测试本身。
fmt `EXIT=0`；clippy `EXIT=0`（抓到一个改写后残留的死字段，已删）；残留扫描 0；磁盘 41Gi。

## 第七段：流式订阅（同日）——阶段 1 出口标准达成

`--test grpc_invocation_watch` **3 passed, 0 failed**，TEST_EXIT=0。

流式只有做到分页做不到的事才值得加，因此断言的正是那两条，外加可续性：

1. **Run 仍在运行时事件即到达**——provider 被门控住不回应，此刻抵达的事件必来自一个**未结束**的 Run。
2. **流以 typed 生命周期边界结束**，不靠调用方从事件里推断。
3. **断流可续**：测试中途 `drop` 掉流（网络跟随者的常态），用最后序号重连，
   断言重放序号**全部严格大于**断点——排他游标被钉住，已送过的不重送。

另两条边界：流式认证与一元调用一致（无 token → `unauthenticated`）；
容量越界**被拒绝而非钳位**。

### 全量门禁（第七段，fullgate25）——绿

**132 二进制、806 passed、0 failed、6 ignored、`CARGO_EXIT=0`**

对照 fullgate24（131 / 803 / 0）：+1 二进制、+3 通过 = 三条 watch 测试。
fmt `EXIT=0`；clippy `EXIT=0`；残留扫描 0；磁盘 38Gi。

### 阶段 1 出口标准对账

roadmap 原文：「一个**独立进程**（非嵌入、非同机 socket）通过网络完成真实 Run 的提交、
事件订阅、审批决定、取消与崩溃恢复，全程无残留。」

| 项 | 状态 |
| --- | --- |
| 提交 | ✓ `grpc_invocation_loop` |
| 事件订阅 | ✓ 分页 `grpc_invocation_loop` + 流式 `grpc_invocation_watch` |
| 审批决定 | ✓ `grpc_invocation_approval`（仅凭事件字节发现参数） |
| 取消 | ✓ `grpc_invocation_control`（含幂等重放与改用途拒绝） |
| 崩溃恢复 | ✓ `grpc_invocation_recovery`（第一个 Runtime 整体消失） |
| 无残留 | ✓ 每轮门禁后扫描 0 |

## 没证明什么——**阶段 1 出口标准已达成，以下不在其内**

- **取消、审批、跨进程恢复均已成功闭环；`resume` 的成功路径未验证**
  （其前置状态构造成本高，且不在阶段 1 出口标准内）。
- Provider 确定性终局与 required MCP 初始化失败已闭合；Tool/子代理编排、存储/Checkpoint `Err`
  路径尚未逐条做同样的终态一致性故障测试，不将本轮证据外推到这些类别。
- 无 Java SDK。

## 进度

**总体进度不变，仍为 70–75%。** 这是边界层，不属于并发、真实厂商、跨平台、生产持久层四类证据中的任何一类。
