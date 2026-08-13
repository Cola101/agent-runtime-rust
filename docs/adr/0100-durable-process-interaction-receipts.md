# ADR-0100：持久 Process 交互收据与安全恢复

状态：Accepted（2026-08-12）

## 决策

1. Tool Executor 新增默认拒绝的 `recover_started_result` 观察接口。它只能读取执行器自己已经持久化的事实，
   不得重新执行副作用；普通 NonIdempotent/Unknown Tool 仍按原规则进入 `indeterminate`。
2. `process.start` 的 Process Session Manifest 本身是启动收据。替代 Host 只在唯一 Manifest 精确绑定
   tenant、Workspace、run、原 attempt、Tool Call、binding digest，且状态已经越过 `Starting`、没有进入
   `Indeterminate`/`start_failed` 时，交付同一 Session 的当前有界输出。
3. `process.write` 在 FIFO 或 PTY supervisor 明确完成写入后，立即原子持久化 schema-1 交互收据，再进入
   yield。收据绑定 tenant、Workspace、run、原 attempt、Tool Call、binding digest、Session、输入摘要和
   stdout/stderr 起始 cursor，并使用独立摘要、`0600` 文件和目录 `fsync`。
4. 恢复发生在新的 Worker attempt，但验证必须使用 `ToolOutcomeUncertainty.source_attempt_id`，因为 Process
   Manifest 和交互收据绑定的是发生副作用的原 attempt。恢复结果仍通过当前 attempt 的正常
   `record_bound_tool_result` 落入 Checkpoint。
5. 写入成功与收据持久化之间若崩溃，仍保持 `indeterminate`，不得猜测或重放；收据缺失、损坏、重复、
   身份/摘要/cursor 不一致也全部 fail-closed。

## 对标判断

- Codex `unified_exec` 的 session store、`exec_command` 和 `write_stdin` 具备成熟交互体验，但 inspected 主链
  没有证明 Runtime/CLI 进程替换后以持久收据交付已接受的 start/write。本平台新增的是多租户恢复语义，
  不是宣称整体执行能力超过 Codex。
- OpenClaw 以 Gateway/Node 的内存 session、finished retention 和 poll 管理交互；`process write` 仍是即时
  动作。其 Node relay、owner/viewer、Windows 和设备生命周期更完整，但没有同口径的跨 Host durable
  write receipt。

## 验收边界

- 真实进程必须证明 start-yield 中 Host 崩溃后只启动一次，并由替代 Host 返回原 Session。
- 真实进程必须证明 write-yield 中 Host 崩溃后只接收一次 stdin，并由替代 Host 返回同一 Session；该测试
  连续 10 轮通过。
- 原有一般 NonIdempotent Tool 的模糊副作用测试必须仍进入 `indeterminate`，防止恢复钩子扩大权限。
- 本 ADR 不恢复 `process.interrupt`/`process.close`，也不承诺 write 成功但 receipt 尚未落盘的窄窗可确定；
  Windows ConPTY、Node relay、GUI 和 Java 仍不在本阶段。
