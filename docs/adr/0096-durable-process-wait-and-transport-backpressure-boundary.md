# ADR-0096：持久 Process Wait 与传输层背压边界

状态：Accepted（2026-08-11）

## 决策

1. 不把 OpenClaw 面向 WebSocket `bufferedAmount` 的 PTY pause/resume 复制到当前 Runtime。supervisor 的
   PTY reader 同步追加 owner-only 日志，没有用户态发送队列；磁盘写入变慢时 reader 停止读取，PTY 内核
   缓冲会自然反压 child。另加 pause 队列只会制造第二份输出状态和新的恢复歧义。
2. 新增模型可见的 Pure Tool `process.wait`，输入稳定 session ID、stdout/stderr cursor 和显式
   `yield_time_ms`。它在有新输出、会话终态或期限到达时返回，与 `process.poll` 使用同一有界输出窗口。
3. `yield_time_ms` 必须在 1—300000 之间，且不得超过 Run-frozen Tool execution timeout；本地 Tool schema
   也将最大值收紧到两者较小值。等待不更新会话 idle/activity，不伪装成用户输入。
4. wait 本身无副作用。Host 在 durable `tool.execution.started` 后退出时，replacement 可恢复同一 Tool
   Call 并重新等待；不得重启 child，也不得重新向模型请求 Tool Call。
5. 当前实现以最多每 50ms 一次的状态/文件观察换取零新依赖和跨 pipe/PTY 一致性。这只证明单会话有界，
   不构成 1000 并发 wait 的容量证明。

## 对标判断

- Codex 的 `exec_command`/`write_stdin` 已把 yield 直接并入统一 exec 产品语义，并有有界 channel 与跨平台
  PTY backend；本阶段只补齐可恢复的独立 wait，仍多一次模型 Tool Call。
- OpenClaw 的 pause/resume、高低水位、owner/viewer 和 terminal event push 解决的是连接层慢消费者；当前
  Runtime 没有该发送层，因此保留同步持久输出更简单且更符合恢复边界。若未来新增 viewer/BFF，再在传输
  适配层实现该流控，不下沉到 Kernel 日志真相源。

## 下一边界

下一阶段实现共享、事件驱动的 process wait observation，并执行 1000 个并发 wait 的 CPU、文件读取、
唤醒延迟和取消门禁；通过后再决定是否把 start/write 与 yield 合并。GUI、Java、Docker、NATS 和数据库
继续不进入独立 Runtime 必需链。
