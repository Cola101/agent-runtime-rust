# 2026-08-15 统一本地 Runtime 控制适配器证据

## 实现范围

- Unix daemon 现在持有同一个 `Arc<EmbeddedRuntime>`，不再复制 Run handle、取消 token、审批/MCP 决定或
  崩溃恢复状态机。
- `submit` 使用 detached Runtime 执行；完整 versioned control command 可经 IPC/CLI 原样提交，旧
  approve/deny/cancel/resume/MCP 命令映射为稳定 command ID 后复用同一 durable receipt。
- Attach 仅依赖持久事件日志和 Run record；daemon replacement 不需要旧进程内状态。
- Runtime core 统一恢复 accepted receipt 与 unfinished Run，并保证已落账的 accepted receipt 不会因后台
  快速失败被 transport 改报成未接受。
- 兼容迁移只接受完整 nil identity 的旧本地记录，不认领任何部分或外部 invocation identity。

## 行为门禁

- `approval_flow` 新增旧审批幂等契约：一次 Tool start、一份 receipt，重复请求返回同一结果。
- `approval_flow` 新增完整 control 契约：过期 epoch 在 receipt 前拒绝，正确命令保持 caller command ID、
  expected epoch 和 action。
- `local_ipc` 新增 daemon replacement Attach：没有旧内存 handle 仍能读取实时事件并取得终态。
- `daemon_recovery` 新增严格 legacy migration：只有完全空身份记录迁移。
- 既有 `embedded_control` 覆盖审批、MCP、取消、双崩溃、并发 owner、存储错误和 command 改写。

专项执行中曾捕获并修复三项真实问题：失败原因被扁平化、receipt 已持久但 transport 因后台快速失败返回
错误、旧 nil identity 记录无法进入新的严格 invocation 边界。最终门禁结果在本轮完成后记录于下节。

## 最终质量门禁

- Runtime Host 专项共 152 项：**151 通过 / 0 失败 / 1 个外部 Codex MCP 用例显式忽略**；其中
  `approval_flow` 8、`daemon_recovery` 9、`embedded_control` 8、`local_ipc` 5 项全部通过。
- `cargo test --workspace --no-fail-fast -- --test-threads=4` 共 677 项：**671 通过 / 0 失败 /
  6 个外部 live 用例显式忽略**。忽略项要求外部 Codex MCP 参考服务、第三方 MCP 或 TLS NATS；本轮未
  启动这些服务，也未用 mock 结果冒充 live 验收。
- workspace/all-target/all-feature Clippy `-D warnings`、Rust 格式与 `git diff --check` 全部通过。
- 最终门禁完成后，`runtime/target` 实测约 12 GB；`cargo clean` 删除 55,228 个文件、15.7 GiB 逻辑数据。
  清理后 `runtime/target` 不存在，仓库约 21 MB；未发现项目 `node_modules`、`.local`、匹配的测试临时目录
  或 Runtime/PTY/可信 Tool 遗留进程。

## 参考源码复核

- Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`：app-server 文档与测试确认
  `thread/resume`、`turn/interrupt` 和 command approval 的统一产品协议。Codex 的客户端协议、交互和
  跨平台产品链仍领先；本项目只在完整多租户 invocation、owner epoch 与 durable control receipt 这个
  内核窄面提供更明确的可嵌入契约。
- OpenClaw `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28`：`node-host/runtime.ts` 以 active invoke map 与
  `AbortController` 执行 cancel/input，`gateway/node-registry.invoke-stream.ts` 管 pending invoke、input
  sequence、timeout、cancel 与 progress。它的在线 relay 与产品覆盖领先；本项目的本地 replacement/
  receipt 不依赖当前 Gateway/Node 连接，但尚无同等级远端控制面。

## 未验证与边界

- 没有启动 Java、Edge、GUI、Docker、Kubernetes、PostgreSQL、NATS 或真实云模型；Provider/Tool 均为本机
  可审计回环实现。
- Unix 0600 socket 不是远端认证；多进程/多节点 command ledger、调用方签名、撤销和生产远端重试未实现。
- Attach 当前轮询 durable log；未来通知层只能优化时延，不能替代持久事实源。
- 历史 8 线程全套出现过一次 PTY identity ambiguous；若高并发验收复现，必须作为进程治理缺陷处理。
