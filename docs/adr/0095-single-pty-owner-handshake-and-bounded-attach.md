# ADR-0095：单一 PTY Owner、协议握手与有界 Attach

状态：Accepted（2026-08-11）

后续：ADR-0096 复核后将 WebSocket 高低水位留在未来 viewer transport，Kernel 改为持久
`process.wait`；本 ADR 的下一边界保留当时待验证的问题，不再是当前实施顺序。

## 决策

1. 删除 Runtime Host 持有 PTY master 的新建路径；`tty=true` 必须先取得外部 supervisor，普通 pipe
   session 继续允许由 Host 管理。Runtime 不再维护两套 PTY 恢复语义。
2. supervisor 控制协议升级为 v2。Host 必须通过 `Hello` 同时校验精确协议版本与
   `start/status/write/resize/lifecycle` 能力；能响应但不兼容的端点不得被删除或替换。
3. supervisor 在 owner-only、摘要保护的生命周期文件中记录 generation、PID、协议、能力、活跃会话数、
   `ready/stopping/stopped` 和退出原因。替代 supervisor 显式记录前任是否干净退出；仍存活的非终态前任
   一律拒绝接管。
4. 新增模型可见的 Pure Tool `process.attach`。调用方必须给出不超过冻结 chunk 上限的 `max_bytes`；
   Runtime 分别返回 stdout/stderr 的有界尾部、起止游标与截断标志，不改变会话 Manifest。
5. `process.poll` 保持增量游标语义，`process.attach` 只用于重连或恢复后的有界回看；两者都不能绕过
   tenant、Workspace、实现摘要、owner generation 与 session identity 校验。

## 失败语义

- 未配置 supervisor 的 PTY start 在创建持久 session 和 child 之前 fail-closed。
- 旧协议、缺能力、损坏生命周期或活着的冲突前任均返回配置错误，不抢 socket、不启动 child。
- supervisor 异常消失仍沿用 ADR-0094：回收原进程组并把模糊执行持久化为 `indeterminate`。

## 对标判断

- Codex 的 PTY/进程组、跨平台 backend、有界 channel 与统一 exec/yield 产品链仍更成熟；本阶段只在跨
  Runtime Host 的 generation、持久生命周期和 fail-closed 恢复上形成更明确的本地契约。
- OpenClaw 的 terminal attach、bounded ring、owner/viewer、pause/resume、WebSocket 高低水位与 Node relay
  仍更完整；本阶段对齐了有界回看与显式生命周期，但没有复制其连接层产品模型。

## 下一边界

先实现 supervisor 输出的高低水位背压与可观察 pause/resume，并用慢消费者和 Host replacement 实跑；
随后才评估 Windows ConPTY。GUI、Java、Docker、NATS 和数据库不进入该阶段。
