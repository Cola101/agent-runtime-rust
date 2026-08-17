# 本地权威文件耐久替换证据（2026-08-17）

## 源码审计

- 已有强提交：Event append、Checkpoint、Run record、Embedded control receipt、retention ledger。
- 仅 crash-atomic、非强提交：Session record、模型路由 journal、Provider 健康、子代理结果、Tool
  reconciliation。
- 本轮将 Session/Run/Checkpoint/control/retention/subagent/Tool reconciliation 收敛到一个实现；Event 保持
  独立追加协议。模型路由 journal 在本阶段没有被误报为完成，随后由 ADR-0132 的有界 WAL 补齐；Provider
  健康仍是可重建缓存。

## RED / GREEN

操作记录测试最初得到：

```text
left:  [create_dir_all, create_file, write_all, rename]
right: [create_dir_all, create_file, write_all, sync_file, rename, sync_directory]
```

失败信息为 `rename alone is atomic but not a durable commit`。修复后新增三项门禁：

1. 正常替换严格按完整顺序执行。
2. staging 文件同步失败时返回 `StateRoot`，且 rename 不发生。
3. rename 后父目录同步失败仍返回 `StateRoot`，不把耐久性不确定误报成成功。

## 性能反证与收窄

第一次尝试把模型路由 journal 和 Provider 健康的每次更新也升级为强替换。`embedded_retention` 的两项硬门禁
分别超过 90 秒和 180 秒，整组用时 201.76 秒。没有放宽阈值；撤回这两个高频/缓存路径后：

- 单独 `embedded_retention`：10/10，通过，118.70 秒。
- 完整包运行中的同组门禁：10/10，通过，112.41 秒。
- 1000 个同时在途、最多 32 个 admitted Host：通过，32.00 秒。

该反证表明模型路由必须使用追加 WAL/组提交，不能把若干 JSON 投影的 fsync 数量线性乘到每个 Turn。

## 回归范围

已覆盖 Runtime Host 单元、审批、daemon 恢复、Embedded control/多租户/全 Profile 恢复/保留/容量、gRPC
调用与恢复、Session、三协议模型路由、MCP、进程会话、子代理与取消。一次完整包运行中
`a_network_resume_recovers_an_inflight_run_over_a_replacement_runtime` 出现一次无内部诊断的时序失败；随后精确
复跑和包含该测试的剩余全组复跑均通过。该 flake 未被隐藏为全包一次性全绿，后续仍需收敛错误诊断。

本轮未启动 Docker、PostgreSQL、NATS 或真实厂商服务；没有模拟主机断电、共享文件系统或介质损坏。
