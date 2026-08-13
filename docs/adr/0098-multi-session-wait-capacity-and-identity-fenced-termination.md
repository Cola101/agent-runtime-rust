# ADR-0098：多 Session Wait 容量与身份围栏终止

状态：Accepted（2026-08-11）

## 决策

1. Process Session 的本地容量门禁固定使用 64 个真实进程、8 个 tenant、64 个独立 Workspace 和 1024 个
   并发 `process.wait`。每个 Session 只允许一个共享观察器；等待者数量不得放大持久文件扫描。
2. 容量验收同时检查 250ms 文件观察次数、每 Session/tenant 完成数、p50/p95/p100 唤醒时延、每 Session
   取消一个等待者后的兄弟隔离，以及最终全部观察器和进程回收。它证明有界与无饥饿，不等价于生产调度器的
   加权公平性或整机 CPU 基准。
3. Unix 进程组的 TERM/KILL 必须以继承的 `identity.lock` 仍被持有为前置条件。身份租约已经释放时，旧 PGID
   只能视为失效引用并安全返回；`EPERM` 后若身份同时释放，也不得把它升级为会话 I/O 失败。
4. TERM→KILL 后必须等身份租约释放才发布成功；超出有界等待仍持有租约时返回 `Indeterminate`，不得静默
   宣称整棵进程树已经回收。
5. 同一 Runtime 进程内，成功的 `process.write` 在持久 write intent 和真实 PTY/FIFO 写入之后唤醒该
   Session 的共享观察器；50ms 持久文件轮询继续作为外部进程输出、Host replacement 和跨进程写入的兜底。
   唤醒不得移到副作用之前，也不得退化成每个 waiter 一个观察器。

## 对标判断

- Codex `ff352fab6209` 的 `unified_exec` 也把进程表软上限设为 64，并以 `Notify/watch` 驱动输出/关闭等待；
  容量满时可清理候选条目。本平台保留 64 上限与共享唤醒，但多租户 live Session 容量满时拒绝新工作，不
  淘汰仍由 tenant/Workspace 拥有的真实进程。
- OpenClaw `58b4b9430457` 的 Node Host 用串行 progress queue、sequence 和 PTY pause/resume 约束输出，
  适合云端 Node relay；当前没有等价的跨 Host durable cursor/identity lease。因此只吸收有界生产与取消隔离，
  不复制其 Gateway 会话所有权到 Kernel。
- 本平台在 durable tenant/Workspace 边界和身份围栏上更明确，但统一 start/write+yield、Windows、viewer
  transport 和完整 Node relay 仍落后两个参考项目。

## 验收边界

- 64 个真实 Session 上 1024 个 wait 必须保持 64 个 observer；250ms 文件观察不超过 512 次。
- 每个 Session 取消一个 wait 后，其余 960 个 wait 必须继续收到各自输出；p50 < 1s、p95 < 2s、p100 < 4s。
- 同一容量测试的设计验收必须连续 10 轮通过，并对身份已释放的旧 PGID 有确定性回归测试。本次局部变更
  至少连续 3 轮复测唤醒时延，再由完整工作区门禁确认没有跨包回归。
- 下一阶段只合并 `process.start` / `process.write` 与有界 yield，不进入 GUI、Java、容器或云控制面。
