# 2026-08-15 多租户 Runtime 混合风暴证据

## 场景与结果

- M1 Pro 16GB 纯本地原生执行：10 tenants、100 immutable Profiles/Workspaces，同时进入同一个
  `EmbeddedRuntime`。
- 准入固定为 8 active、每 tenant 2 active、每 Workspace 1 active、最多 100 queued；实际高水位为
  **8 active / 92 queued / 100 execution owners**，单 tenant 未超过 2，单 Workspace 未超过 1。
- 负载为 60 个普通成功、10 个 allow、10 个 deny、10 个活动 cancel、10 个 owner crash 后 resume。
  最终 **40 个 control receipts 全部 Completed**，100 个 Run 全部终态；Provider 请求精确 **130 次**，
  没有 Tool、resume 或 cancel 重放。
- 前 40 次真实 Provider 启动中每个 tenant 至少获得 2 次进展，证明全量 backlog 下没有 tenant 饥饿；
  既有 A1→B1→A2 单元门禁继续固定严格 round-robin 规则。
- 2ms 连续资源采样的一次最终 exact 运行：RSS `25,853,952 → 43,220,992` bytes，增量约 16.6 MiB；
  FD `11 → 27 → 11`；总时长 14.538 秒。回归上限分别为 384 MiB、64 FD 和 120 秒。

## 压力中发现并修复的问题

饱和队列测试最初真实失败：resume command 已写 `Accepted` receipt 后才发现 global queue full，因此留下
Accepted receipt 与推进后的 Run。修复后，需要新执行槽的 Run/resume/approval/MCP/cancel recovery 都先
取得 permit，再写 durable acceptance：

- 新 Run 被满队列拒绝后没有虚假的 `Running` record。
- resume/cancel 被满队列拒绝后没有 receipt、owner epoch 不变。
- 容量释放后，完全相同的 command ID 可以成功重试并只产生一份 Completed receipt。
- 活动 Run cancel 仍使用已有 owner，不因新队列容量而延迟或丢失 durable intent。

## 质量门禁

- `embedded_runtime_storm` 3/3：100 Profile 混合风暴、饱和 resume、饱和 cancel 全部通过。
- Runtime Host tests 共 155 项：**154 通过 / 0 失败 / 1 个外部 Codex MCP 用例显式忽略**。
- 最终 Rust 全工作区共 680 项：**674 通过 / 0 失败 / 6 个外部 live 用例显式忽略**。
- Clippy workspace/all-targets/all-features `-D warnings`、Rust 格式与 Git 差异检查全部通过。
- 验收完成后执行 `cargo clean`，删除 46,910 个文件、13.5 GiB 逻辑构建产物；`runtime/target`
  已不存在，仓库总占用约 21 MiB，未发现项目内 `node_modules`/`.local` 或匹配的 Runtime 测试临时目录，
  也没有遗留 Runtime 进程。清理后没有重新构建。

## Codex / OpenClaw 源码复核

- Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`：app-server
  `request_serialization.rs` 按资源 key 排队并支持连续 shared-read；unified exec 固定 64 process 软上限，
  multi-agent 可限制每 Session 并发。inspected serialization queue 未见全局/tenant 队列上限或 tenant 公平。
- OpenClaw `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28`：`process/command-queue.ts` 提供 lane 并发、优先级、
  timeout、abort/release、drain generation 与 active task snapshot；inspected lane queue 未见长度上限或
  tenant round-robin。

## 未验证与下一风险

- 这是 100 个混合 Run 的本机回环证据，不是 1000 active Run、真实厂商或生产 SLA。
- 没有验证多进程/多节点共享 ledger、远端认证或 Linux cgroup；macOS RSS/FD 是进程级观测。
- Profile 当前在构造时静态注册；动态注册/退役仍未设计。
- 终态 Run、events、Checkpoint 和 receipts 的磁盘保留仍无界。下一目标是按 tenant/Workspace 安全保留、
  tombstone 与 crash-safe GC，再跑 1000 Run 顺序 churn 证明磁盘和恢复扫描不会线性失控。
- 本轮没有启动或修改 Edge、Java、GUI、Docker、PostgreSQL 或 NATS。
