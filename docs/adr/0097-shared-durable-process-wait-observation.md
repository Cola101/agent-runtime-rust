# ADR-0097：共享持久 Process Wait 观察

状态：Accepted（2026-08-11）

## 决策

1. 同一个 Process Session 的全部 `process.wait` 共享一个 Runtime 内观察器。等待者使用 Tokio `watch`
   接收状态、stdout 长度和 stderr 长度变化，不再各自每 50ms 扫描 Manifest 与输出文件。
2. 观察器仍以持久文件为真相源，每 50ms 最多执行一次 sweep、Manifest 读取和两个日志长度读取。这样同时
   覆盖 child 直接写文件的 pipe 会话、外部 supervisor 写文件的 PTY 会话，以及新 Host 从磁盘重建观察的
   恢复路径；不把原 Host 的内存通知误当成可恢复事实。
3. 每个等待者先执行一次完整、带 tenant/Workspace 校验的读，避免越权订阅和 missed wakeup。共享观察器
   确认变化后，等待者只执行无恢复锁的持久输出读取，仍重新加载 Manifest 并复核 tenant/Workspace；完整
   recovery sweep 由唯一观察器承担。
4. 最后一个等待者完成、取消或超时后，观察器在一个观察周期内从 Manager 注册表移除。注册和退休使用同一
   进程内锁，禁止“观察器刚退休、订阅者却挂到旧 channel”的竞态。
5. 公开只读 `ProcessWaitObservationSnapshot`，记录 active waiters、active observers 和累计文件观察次数，
   作为容量门禁与后续运行指标的最小事实面；它不参与授权、恢复或调度决定。

## 为什么不用 OS 文件监听

当前每个 Manager 最多允许 64 个 live Process Session。每会话一个 50ms 观察器把最坏空闲扫描限制在约
1280 次/秒，且不增加 kqueue/inotify/FSEvents 的平台依赖、丢事件补偿和 watcher 生命周期恢复状态。若未来
Process Session 上限显著提高，再以相同持久游标契约替换底层观察器；本 ADR 不把轮询实现写进公开协议。

## 对标判断

- Codex `unified_exec` 用 `Notify`、`watch` 和 `broadcast` 直接推送进程内 output/state；本实现采用相同的
  共享唤醒原则，但保留持久文件观察，使 Host replacement 后不依赖已经丢失的进程内 handle。
- OpenClaw Node Host 在 PTY 输出链上使用单队列、pause/resume 和异步 chunk emit，避免消费者放大生产者；
  当前 Kernel 没有 WebSocket viewer，因此只吸收“单生产观察、多人消费”，不复制连接层流控。
- 这不是全面领先：Codex 仍有统一 start/write+yield 和跨平台 PTY，OpenClaw 仍有完整 viewer/Node relay。
  本阶段只证明多等待者共享持久观察与跨 Host 语义。

## 验收边界

- 一个真实 live Process Session 上 1000 个并发 `process.wait` 必须只有一个 active observer；250ms 空闲窗口
  的新增文件观察不超过 10 次；真实输出后全部等待者在 2 秒内完成。
- pipe、外部 PTY supervisor、取消后观察器回收、等待中的 Host replacement 都必须实跑。
- 尚未证明 64 个不同 live Session 同时各有大量等待者的整机 CPU/p95，也未实现统一 start/write+yield、
  Windows ConPTY 或 viewer transport。
