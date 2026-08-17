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

## 没证明什么——**阶段 1 未完成**

- **控制路径只证明了拒绝。** 审批、取消、resume 的成功端到端，以及跨进程崩溃恢复，未在该面验证。
- 无流式订阅，调用方只能分页轮询。无 Java SDK。

## 进度

**总体进度不变，仍为 70–75%。** 这是边界层，不属于并发、真实厂商、跨平台、生产持久层四类证据中的任何一类。
