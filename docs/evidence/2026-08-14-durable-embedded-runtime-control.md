# 2026-08-14 持久 Embedded Runtime 控制证据

## 已验证

- `RuntimeControlCommand` schema 1 统一 `resume`、精确审批决定和取消；命令绑定完整多租户 invocation、
  Run、预期 owner epoch 与 command ID，收据绑定 canonical JSON SHA-256。
- 8 项 `embedded_control` 行为测试覆盖：审批后 Tool 只执行一次；活动 Run 取消；原 owner 崩溃恢复；
  `accepted` resume 的第二任 owner 再崩溃；过期 epoch、错误审批摘要与 command ID 改写；并发审批单所有者；
  并发取消无孤儿收据；存储读取错误 fail-closed。
- 并发审批测试证明只接受一个命令、只落一份收据、只出现一次 `tool.execution.started`。并发取消测试
  证明所有返回 Accepted 的命令随后都能重放到 Completed/Cancelled。
- `accepted` 命令双崩溃用例第一次按默认两次 Provider 尝试预算正确失败；显式冻结三次尝试后连续 3/3
  通过，Provider 总调用为三次。同一 control command 没有绕过模型路由预算。
- 最终 `cargo test --workspace --no-fail-fast -- --test-threads=4` 为 **667 通过 / 0 失败 / 6 个外部 live
  用例显式忽略**，共 673 项。此前 8 线程全套中既有 `agent-tool-runtime` PTY 分配用例出现一次
  identity ambiguous；相同 exact 用例随后连续 10/10 通过，4 线程全套也通过，因此仍记录为高并发
  稳定性风险，不解释为本轮 control 回归。
- workspace/all-target/all-feature Clippy `-D warnings`、Rust 格式和差异门禁通过。最终门禁完成后执行
  `cargo clean`：清理前 `runtime/target` 实测约 13 GB，Cargo 删除 67,088 个文件、18.2 GiB 逻辑数据；
  清理后 `runtime/target` 不存在，未发现项目 `node_modules`、`.local`、测试临时目录或遗留进程。

## 参考源码复核

- Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`：app-server README 与实现提供
  `thread/resume`、`turn/interrupt`；`turn_interrupt_resolves_pending_command_approval_request` 证明中断会
  结束等待中的命令审批；`core/src/tools/approvals.rs` 与 orchestrator 统一 Tool approval。其产品协议与
  客户端体验领先，本项目不声称整体超越。
- OpenClaw `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28`：`node-host/runtime.ts` 用 active invoke map 与
  AbortController 处理 cancel/input；`gateway/node-registry.invoke-stream.ts` 管理 pending invoke、input seq、
  timeout 与 cancel；Node invoke 的审批和命令分析更完整。当前 inspected 在线 invoke path 没有本项目
  同口径的 tenant-bound durable control receipt。

## 未验证与边界

- 没有启动 Java、数据库、NATS、Docker、Edge 或真实云模型；HTTP/SSE Provider 是确定性本地回环服务。
- 外部调用者 authentication/authorization、命令签名、远端重试协议和多进程/多节点 command ledger 未实现。
- 一次全工作区 8 线程门禁仍有 PTY identity ambiguous 偶发失败；它不在本轮修改文件内且 exact 10/10，
  但在修复或稳定复跑前仍是 Runtime 进程治理风险。
