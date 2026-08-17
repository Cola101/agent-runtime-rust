# Checkpoint 绑定终态事件发布证据（2026-08-17）

## 稳定 RED

真实 Root Session 先完成包含 MCP Tool 的第一 Turn，再完成第二 Turn。测试保留第二 Turn 的 terminal
Checkpoint，精确删除 Event Log 最后一条 `run.succeeded`，并把 Session head 恢复到仍持有该 Run 的 active
状态，重建实际的 `Checkpoint committed / Event not published` 崩溃窗口。修复前替代 Host 稳定失败：

```text
terminal Checkpoint republishes its exact Event before Session commit:
Checkpoint("terminal root Session event log disagrees with its Checkpoint")
```

子代理专项同样保留终态 child Checkpoint、删除 child terminal Event，并恢复父 Checkpoint/事件前缀。修复前
替代 Host 把终态 child 当普通恢复任务，稳定失败为：

```text
replacement recovers terminal child:
Execution("worker checkpoint identity does not match the replacement command")
```

## GREEN

- Worker Checkpoint schema 27 把原始 terminal EventEnvelope 纳入摘要；损坏 event id、身份、sequence、type
  或 payload digest 会被拒绝。
- Root Session 仅在 active head、branch generation、history digest 与 input 都匹配后执行发布收敛。
- Event 前缀必须具备完整 tenant/application/workload/workspace/agent/model 身份、连续 sequence 且没有其他
  terminal；恢复追加的是 Checkpoint 内同一 event id，不生成新事件。
- 子代理先验证替代 command 的角色、输入、历史、权限、预算、Tool/Skill/MCP 目录和 owner epoch，再直接收集
  终态；不会调用 Provider 或 Tool。
- schema 1—26 若已有 terminal Event 可继续读；若 Event 缺失则明确 fail-closed。

## 已执行门禁

- Worker terminal receipt/binding/corruption：1/1。
- Root Session Checkpoint→Event 故障窗口：1/1；完整 `standalone_run`：39 通过、0 失败、1 个外部 Codex
  MCP fixture 显式忽略。
- 子代理同一故障窗口：1/1；完整 `subagent_concurrency`：20/20。
- `cargo test -p agent-runtime-worker`：163 通过、0 失败。
- 最终 `cargo test -p agent-runtime-host`：224 通过、0 失败、1 个外部 Codex MCP fixture 显式忽略；
  1000 Run retention 门禁约 151 秒，阈值未放宽。
- `cargo clippy -p agent-runtime-worker -p agent-runtime-host --all-targets --all-features -- -D warnings`：通过。

首次 Runtime Host 全包在高负载容量用例之后出现一次既有网络审批测试的通用 `Internal`；精确用例随后
连续 10/10 通过，第二次完整全包也通过。当前没有证据把它归因于本轮 schema/恢复变更，但根因未定位，仍按
flake 风险保留，不把一次最终绿色外推为生产稳定性证明。

## 对标与边界

Codex 用 rollout 单 writer 与 flush ack 缩小 publication 窗口；OpenClaw 用 SQLite writer queue、WAL 与事务
提交 transcript/Session 状态。本实现没有复制它们的产品存储，而是在纯本地文件模式下把原始 Kernel terminal
identity 放进摘要有效的 Checkpoint，使已证明的跨文件窗口可收敛。硬件掉电、Windows、共享文件系统、跨机器
owner 和任意多文件事务仍未验证；总体进度维持 70–75%。
