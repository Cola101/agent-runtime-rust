# 无 UI Runtime Client 契约证据（2026-08-18）

## 复核事实

| 边界 | 修改前 | 风险 |
| --- | --- | --- |
| 进程内嵌入 | 客户端直接拿 `EmbeddedRuntime` | 配置、持久层和执行方法混在一个实现 API |
| gRPC 初始化 | 无 | 不兼容客户端只能调用后才发现缺方法/语义 |
| input 上限 | gRPC 1 MiB；Kernel 32,000 bytes | 可能先写 durable Run，再由 Kernel 拒绝 |
| stream error | Embedded error 直接到 adapter | host 路径和内部细节可能跨边界 |

## 实现结果

- `RuntimeClient v1` 统一进程内和 gRPC 的 initialize/submit/control/read/watch/recovery 入口；执行方法只存在于
  `InitializedRuntimeClient`，未协商的进程内调用者无法开 Run。
- initialize 强制 min/max contract overlap 和 required capability subset；capability 数量、名称与字符集有界。
- descriptor 公开真实 input/action/page/stream 上限，能力按字典序稳定输出。
- 32,001-byte input 在任何 durable Run 状态创建前拒绝；随后读取该 Run 得到 `NotFound`。
- typed control 也执行与 gRPC 相同的 64 KiB action 上限。
- client error 只输出稳定 code/message；测试构造含私有路径的内部错误，路径未越过边界。
- submit status 取 actionable Event Cursor state，延续 ADR-0141 的 owner-release 不变量。

## 可执行门禁

| 门禁 | 结果 |
| --- | --- |
| Runtime Host 单元测试 | 36/36 |
| RuntimeClient 初始化/错误单元测试 | 3/3 |
| 无 UI 进程内真实模型 Run（initialize → submit → watch → terminal） | 1/1 |
| gRPC identity + initialize negotiation | 9/9 |
| gRPC Run/审批/控制/MCP 输入/恢复/流式/mTLS/config 相关门禁 | 26/26 |

第一次批量 gRPC 门禁因上次版本专属缓存清理后缺少测试夹具
`agent-trusted-workspace-tool must be built` 终止；重建明确的 workspace fixture 后，同一审批测试及剩余门禁通过。
这不是产品失败，也未被计入 GREEN。

## 对标结论

- Codex 已有成熟 initialize + capabilities 和完整 Thread/Turn 产品协议；本项目只完成 Runtime port，Session 客户端
  面仍落后。
- OpenClaw 已有 min/max protocol、methods/events/capabilities 和 Gateway snapshot；本项目只保留执行内核需要的
  最小协商，不复制 presence 或 channel 状态。
- 本项目的 invocation/tenant/owner epoch/receipt 继续比两个参考项目更显式地面向共享多租户 Runtime，但这不等于
  客户端产品能力领先。

## 未验证与下一门禁

- 未跑全工作区；本轮只改变 Runtime client/proto/gRPC 边界，使用直接受影响的真实 Run、控制、恢复和 mTLS 门禁。
- 未使用外部 API Key、Docker、Java、GUI、Edge 或系统服务。
- 下一门禁固定为 Session/Thread client contract；随后是 Profile/关闭生命周期、credential resolver 与可分发 artifact。
- 当前仍不能宣称 Desktop-Ready，Rust Runtime 总体仍为 70–75%。
