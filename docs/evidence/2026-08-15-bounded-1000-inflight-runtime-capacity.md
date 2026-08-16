# 2026-08-15 有界 1000 在途 Runtime 容量证据

## 结论

一个纯本地 `EmbeddedRuntime` 已接住 20 tenant、200 Workspace/Profile、1000 个 claimed in-flight Run，
但只允许 32 个 Host/Provider 进入执行。该门禁明确验证“1000 在途 / 32 admitted”，不把 968 个排队者冒充
1000 真正同时执行。

## Exact 结果

```text
inflight=1000
profiles=200
tenants=20
active_peak=32
queued_peak=968
first_wave_tenants=20
promoted_tenants=16
aborted_queued=500
cancelled_active=16
succeeded=484
event_count=4
event_subscription_peak=2
event_buffer_slots_peak=3
rss_baseline_bytes=14647296
rss_peak_bytes=48529408
fd_baseline=211
fd_peak=278
fd_final=211
elapsed_ms=38300
```

500 个排队 future 被撤销、16 个 active Run 通过 durable control receipt 取消、484 个 Run 成功，合计恰好
1000；最终 execution owner、admission active/queue、事件订阅与缓冲槽均为 0。

## 行为边界

- global active=32、tenant active≤2、Workspace active≤1；首轮 32 个执行覆盖所有 20 个租户。
- 取消 16 个执行后，新晋升波覆盖 16 个租户，证明释放容量没有被单租户独占。
- capacity=1 的订阅在 Run 结束前不读取，Agent 仍完成；之后从持久 JSONL 补齐连续事件。
- 第二个订阅以 sequence 作为 exclusive cursor 重连，尾部 event ID 与首个订阅完全一致。
- capacity=0 与超过 256 的订阅被拒绝；进程总订阅和总缓冲槽另有硬上限。
- 事件写入与订阅读取共享 256 KiB 单行上限，避免 writer 接受 subscriber 永远无法消费的载荷。
- Rust 全工作区 695 项中 689 通过、0 失败、6 个外部 live 用例显式忽略；Clippy、格式和差异门禁通过。

## 尚未证明

- 不是 1000 个同时 Provider 连接、Tool 进程或真实厂商调用；后者只适合专用/分布式环境。
- 本轮没有 Tool 子进程、真实厂商限流、跨进程共享 admission 或生产 SLA。
- 当前订阅 API 的正常关闭、历史已回收和 history gap 仍需版本化终态契约，不能让外部适配器猜测。
- 旧 `replay_events`/IPC attach 仍可一次性读取完整热日志；它尚未迁移到本轮有界订阅，属于下一阶段明确债务。
- Java、GUI、Edge、Docker、PostgreSQL、NATS 均不在本轮依赖或验收范围。

## 对标

- Codex 的模型流 bounded channel 和 resource-key serialization 更成熟；本 Runtime 增加了共享多租户
  global/tenant/Workspace 公平上限与 durable cursor fanout。
- OpenClaw 的 Session admission、连接流控、心跳合并和产品化 Gateway 更成熟；本 Runtime 的窄面优势是
  事件身份绑定完整 tenant/application/workload/Workspace/AgentVersion/model policy，且慢消费者不影响执行。
- 这只是多租户嵌入内核的窄面领先，不代表工具广度、MCP、客户端或运维能力整体领先。
