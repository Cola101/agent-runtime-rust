# 控制命令与 Kernel 终态权威证据（2026-08-17）

## RED

1. 子代理 Tool 正在等待审批时，测试把 root `run.json` 的 `target_run_id` 改成另一 Run。旧适配器从这份
   投影生成命令并返回 Accepted；Checkpoint 随后拒绝，异步清理器却把 parent 写成 failed。
2. 精确审批通过预检并写入 Accepted receipt 后，把事件日志路径替换成目录，强制 restored event append
   失败。旧路径把 receipt 完成并伪造 Run failure。
3. 取消等待审批的 Run 时，旧路径只有 `Cancelled` record，事件日志停在 `approval.required`，不存在
   `run.cancelled`。

## GREEN

- 错绑子代理审批返回 command error；Run 保持 `AwaitingApproval`，无 receipt、无 Tool start、无任何
  terminal event。
- 接纳后的存储失败保持 `ApprovalDecided + Accepted receipt + run_status=None`；等待 100ms 确认异步任务
  已退出后仍未伪造终态。
- parked cancellation 先恢复 Checkpoint，再由 Kernel 产生且只产生一个 `run.cancelled`；IPC Event Cursor
  返回 `Terminal { cancelled }`，Tool 从未启动。
- 正常 root/子代理审批、同命令重放、冲突决定、并发单 owner、daemon replacement 与二次崩溃恢复保持通过。

## 已执行门禁

- `agent-runtime-host --test embedded_control`：9/9。
- `agent-runtime-host --test approval_flow`：8/8。
- `agent-runtime-host --test subagent_approval`：3/3。
- `agent-runtime-host --test execution_cancellation`：12/12；同时把 ADR-0124 后仍保留 Host Err 预期的两条
  required MCP reverse-request 测试更新为验证 Kernel `run.failed` 与模型零出站。
- `agent-runtime-host --test standalone_run`：38 通过、0 失败、1 个外部 Codex MCP fixture 显式忽略；
  optional stdio MCP 的 list/initialize 超时继续验证完整进程组回收，后续 Provider 失败改为验证 Kernel
  持久 `run.failed`，不再期待 Host Err。
- `agent-runtime-host --test subagent_cancellation`：3/3；`subagent_concurrency`：20/20。
- 最终 `cargo test -p agent-runtime-host`：190 通过、0 失败、1 个外部 fixture 显式忽略。
- `cargo clippy -p agent-runtime-host --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt --all -- --check`：通过。

验证期间有两项稳定性信号：一次并发子代理套件的 provider 请求顺序断言失败，精确复跑、整套复跑及最终
全包均通过，继续列为观察项；一次 stdio MCP `ping` 测试把整次初始化/发现也压在 200ms 内，导致并行进程
测试下初始化误超时。测试时限改为 1 秒，仍对故意卡死的 `ping` 产生确定性 timeout，精确、lib 和最终全包
均通过。生产默认时限未改变。

## 对标结论

Codex 和 OpenClaw 都不会用错误审批输入直接制造当前工作终态；本轮消除了该偏离。本项目额外以
Checkpoint digest、子代理 lineage、owner epoch 和 durable receipt 保证替代 Host 仍执行同一决定，适合
多租户嵌入式 Runtime。总体进度仍为 70–75%；本轮修复的是控制与终态一致性，不是跨平台、真实厂商或
分布式容量证据。
