# ADR-0128：多 Profile 孤儿 Run 恢复与终态收据收敛

- 状态：Accepted
- 日期：2026-08-17
- 范围：Embedded Runtime、多租户启动恢复、控制收据、Runtime replacement

## 背景

单 Profile `runtime-host serve` 已在监听前执行 `recover_unfinished()`，但一个嵌入式进程可以注册多个
tenant/application/Workspace Profile。若要求 Java、CLI 或未来 GUI 自己枚举 Profile、判断孤儿 Run 并安排
恢复，就会把 Runtime 的 owner epoch、Checkpoint、取消优先级和公平准入语义泄漏给每个适配器。

复核还发现两个竞态：并发恢复扫描可能对同一 Run 重复派发；本进程仍在投影 Kernel 终态时，扫描器可能把
活跃 Run 当成孤儿。另一个窗口是终态事件已经可见、控制收据仍为 `Accepted`，导致立即重放得到非终态收据。

## 决策

1. `EmbeddedRuntime::recover_all_unfinished_detached()` 扫描所有已注册 Profile，并返回扫描数、接纳的 Run 数和
   逐 Profile typed failure。调用适配器不枚举状态目录，也不复制恢复状态机。
2. 每个 Profile 先形成 Run 级计划，再按 Profile 轮转派发；一个租户有大量孤儿 Run 时不能先占满共享准入。
   某 Profile 的读取或派发失败只停止该 Profile 的剩余计划，其他租户继续。
3. 全量扫描和单 Profile 扫描共用一个异步恢复门，确保同一进程内 exactly-once 计划接纳。仍在本进程
   `active` 表中的 Run 不属于 orphan，由当前执行者完成终态投影；替代进程的 `active` 表为空，真正孤儿仍会恢复。
4. Kernel 终态事件仍是权威提交点。`control_detached` 发现同一 Run 已有终态事件时，先收敛 Run record 与所有
   `Accepted` receipt，再向重放调用方返回 `Completed`，不得暴露事件终态与收据状态分裂。
5. 该 API 是进程内生命周期入口。何时调用仍由宿主启动流程决定；本 ADR 不引入跨机器 owner 选举、分布式
   command ledger 或控制面扫描器。

## 对标

- **Codex `ff352fab6209`**：`InitialHistory::Resumed` 和 ThreadManager 从持久历史恢复 Thread，SDK/Thread 生命周期
  成熟；其 inspected 主链不是面向共享多租户 Profile 的公平孤儿扫描器。
- **OpenClaw `58b4b9430457`**：main-session restart recovery 已有自动扫描、cycle/revision、owner claim、charged
  attempt、退避和通知，产品闭环更完整。
- 本项目当前窄面优势是 tenant Profile 失败隔离、轮转公平、owner epoch、冻结 Provider 尝试预算和持久收据
  同时生效；但宿主自动编排、跨机器唯一 owner、用户通知和长期运行证据仍落后 OpenClaw。

## 代价与未覆盖

- 恢复扫描在一个进程内串行，牺牲扫描并发换取明确的副作用边界；真正执行仍走共享公平准入并可并行。
- state root 损坏会在报告中保持失败，每次扫描仍会再次报告；本轮不自动修复或隔离磁盘目录。
- 测试使用本机真实 HTTP/SSE Provider 与 Runtime replacement，不是主机掉电、跨机器或真实厂商验证。
- Java SDK、宿主启动钩子、控制面告警和运维 UI 仍未实现。
