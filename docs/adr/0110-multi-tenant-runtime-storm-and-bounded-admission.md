# ADR-0110：多租户 Runtime 混合风暴与有界准入提交点

状态：Accepted（2026-08-15）

## 背景

ADR-0102 已提供全局、tenant 与 Workspace 准入上限及 tenant round-robin，ADR-0108/0109 又统一了持久
control command。但此前证据只覆盖少量 Run，无法证明 100+ Profile 混合执行时仍满足公平、单写、收据
收敛和本机资源上限。审查还发现一个安全提交点问题：需要新执行槽的 Run/resume/cancel recovery 先写
`Running` 或 `Accepted`，再申请准入；队列已满时会返回拒绝，却留下没有执行者的持久状态。

## 决策

1. `RuntimeAdmissionSnapshot` 同时报告配置上限、当前值和进程生命周期高水位：active、queued、单 tenant
   active 与单 Workspace active。`EmbeddedRuntimeSnapshot` 只报告 Profile 数、当前/峰值 execution owner 和
   admission snapshot，不泄露租户身份、路径或凭据。
2. 新 Run、直接 resume、approval/MCP resume，以及失去 owner 后需要重新驱动的 cancel，在写 Run epoch 或
   control receipt 前先取得有界 admission permit。队列满时不创建 Run record、不创建 control receipt、
   不推进 owner epoch；同一命令可在容量恢复后重试。
3. 活动 Run 的 cancel 不进入新准入队列：它仍先写 durable receipt/cancelling intent，再触发已有 owner 的
   cancellation token。等待审批/MCP 或没有 Checkpoint 的 Run 可在当前 owner 下直接持久终止。
4. 本机门禁固定为 10 tenants、100 immutable Profiles/Workspaces、8 active、每 tenant 最多 2 active、每
   Workspace 1 active、92 queued。工作负载为 60 成功、10 allow、10 deny、10 cancel、10 crash-resume。
5. 门禁必须核对 40 个 control receipts 全部终态、精确 130 次 Provider 请求、无 `Running`/`Accepted`
   残留、前 40 次启动每个 tenant 至少获得 2 次进展，并连续采样进程 RSS 与文件描述符。
6. M1 Pro 当前回归上限为 Runtime 增量 RSS 不超过 384 MiB、FD 增量不超过 64、总时长不超过 120 秒。
   这些是防回归上限，不是生产容量或 SLA；实际测量必须单独记录。

## 对标判断

- Codex app-server 的 request serialization 按 Global/Thread/Path/Process 等资源 key 排队，连续 shared-read 可
  并行；unified exec 有 64 process 软上限，多代理可配置每 Session 并发。inspected serialization queue 是
  `HashMap<Key, VecDeque>`，没有本项目同口径的 tenant/global queue cap 或 tenant round-robin。Codex 的
  Thread/Turn、沙箱、进程和客户端产品链仍明显领先。
- OpenClaw command queue 支持 lane concurrency、priority、task timeout、abort/release、drain generation、
  active task marker 和 snapshot，在线协调语义更丰富；inspected lane queue 使用数组入队，未见队列长度
  上限或跨 tenant 公平。本项目选择 global/tenant/Workspace 三层上限，是共享多租户 Runtime 所需的差异，
  不代表整体超过 OpenClaw。

## 后果与边界

- 排队 Run 在本地进程内尚未形成 durable queue；只有取得 permit 后才建立 Run record。transport 不得在此
  之前向调用方确认 durable Run acceptance。
- execution owner 数包含 active 与 queued futures，但受 active + queue cap 约束；终态必须回到零。
- 当前只证明 100 个混合 Run，不是 1000 个同时活跃 Run，也不证明真实厂商、远端 transport 或多进程共享。
- 终态 Run、event、Checkpoint 与 control receipt 仍随历史增长；下一内核目标是 crash-safe retention/GC 与
  1000 Run 顺序 churn，不能用内存/FD 通过掩盖磁盘账本无界增长。
- 本 ADR 不引入 Edge、Java、GUI、Docker、数据库或消息总线。
