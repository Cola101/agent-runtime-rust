# 2026-08-15 版本化 Runtime Event Cursor 证据

## 已验证

| 场景 | 结果 |
| --- | --- |
| IPC bounded page | limit=1 返回 1 条、`has_more=true`、next=1，终态仍为 succeeded |
| exclusive reconnect | Legacy Attach 与新 subscription 均按 sequence 续传，事件 ID 精确一致 |
| typed boundary | terminal、waiting approval、retired 均不依赖 channel close 推断 |
| retired history | cursor=0 得到 `history_gap=true`；cursor=terminal sequence 得到 false |
| typed failures | cursor ahead、foreign invocation、sequence gap、缺失当前 digest 均分类拒绝 |
| 慢消费者 | capacity=1 到 Run 结束后才读取，Agent 仍完成并可补齐事件与 terminal boundary |
| Legacy compatibility | Submit/Attach 与 daemon replacement 5/5 通过，Attach 内部已迁移到 bounded tail |

最终 Rust 全工作区 696 项中 690 通过、0 失败、6 个外部 live 用例显式忽略；Clippy、格式与差异门禁通过。

第一次专项编译只暴露新 Page/StreamItem 不应派生 `Eq`（`LocalEvent` 含 JSON Value）；删除不成立的派生后通过。
审批专项首次单包执行的 3 个失败来自可信 Tool 二进制没有构建；显式构建该工作区成员后 8/8 通过，不是
Runtime 行为失败，也没有弱化测试。

## 1000/32 回归 exact

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
rss_baseline_bytes=12566528
rss_peak_bytes=46989312
fd_baseline=211
fd_peak=278
fd_final=211
elapsed_ms=38187
```

显式 Boundary 与持久 reader 没有改变公平准入和资源收敛；1000 仍表示 claimed in-flight，真实 admitted 为
32，不宣称 1000 真执行。

## 尚未验证或保留风险

- 随机分页会从文件头验证到 cursor，长单 Run 的分页 CPU 仍需 profiling；实时订阅不存在重复全量扫描。
- 低层 `LocalRuntimeHost::replay_events` 和 `EmbeddedRuntime` compatibility shim 仍供内部恢复与暂停中的 Edge
  consumer 使用；Runtime Host/IPC 新集成面不再依赖它。删除 shim 的条件是 Edge 单独迁移，不能在本阶段
  破坏工作区构建。
- 没有 Java/SSE/GUI 实现；本轮只提供其可依赖的 Rust 与 Unix IPC wire contract。
- 真实厂商长流、百万事件日志、跨进程 event index 与冷归档读取尚未验证。

## 对标判断

- 对 Codex：保留 bounded channel 与 listener/Run 解耦；本项目额外提供多租户 invocation binding、durable
  sequence cursor 和 retired proof。Codex 的完整 App Server 协议、Thread store 与客户端生态仍领先。
- 对 OpenClaw：直接采用“先写 prune watermark、再删事件，gap 只由真实删除证明”的原则；本项目 tombstone
  将 terminal event digest 与完整租户身份一起绑定。OpenClaw 的 SQLite 查询、运营维护和产品集成仍领先。
