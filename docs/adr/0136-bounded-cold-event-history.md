# ADR-0136：有界冷 Event 历史

- 状态：Accepted
- 日期：2026-08-17
- 范围：EmbeddedRuntime、Runtime Event Cursor、终态 retention、Session/子代理历史

## 背景

ADR-0111/0112 已能在提交精确终态 tombstone 后删除热 Run 目录，ADR-0114 也会用 tombstone watermark
明确返回 `history_gap`。但这条链只能证明“历史确实被删除”，不能在释放热状态后继续读取 Event。长期运行的
本地、嵌入式 Runtime 因而只能在“热目录持续增长”和“退休后丢失 Event 正文”之间二选一。

该缺口不能通过默认保留所有 Event 解决：用户要求 Runtime 在 M1 Pro 16 GB Mac 上长期本地运行，租户注册更多
Workspace 也不能乘法放大磁盘预算。它同样不能迫使一次 Run 依赖 SQLite、PostgreSQL 或对象存储。

## 决策

1. 在热 Run 和终态 tombstone 之间增加**可选冷 Event 层**。默认五项 archive limit 全为零，维持现有低成本
   `history_gap` 语义；启用时五项限制必须全部为正，并同时限制单 Run 字节、Workspace 条数/字节和 tenant
   条数/字节。
2. retention 只处理已经通过连续 sequence、事件摘要、完整 invocation、唯一终态和 control receipt 校验的
   Run。提交顺序固定为：
   `检查已提交 JSONL 前缀 → 写内容寻址对象 → fsync/readback SHA-256 → 提交摘要索引 → 提交终态 tombstone → 删除热目录`。
   任一步失败都不得删除热目录。
3. 冷对象保存原始已提交 JSONL 字节，文件名为内容 SHA-256；索引绑定 Run、终态 event id/sequence/digest、
   completed time、字节数和内容摘要，并对整个索引再次摘要。对象和索引在 Unix 下分别为 `0600`，目录为
   `0700`。
4. 归档扫描、复制、SHA 校验和读取使用固定缓冲区；单行继续受 Runtime Event 的硬上限约束，索引另有
   16 MiB 读取/写入硬上限。Event Cursor 不把整份冷文件装入内存。
5. Event Cursor 只有在 tombstone、索引、对象以及重新解析出的完整连续事件链全部一致时才返回冷历史。
   分页和订阅沿用同一 exclusive cursor 与有界 channel。索引承诺的对象缺失、损坏或终态不一致返回 typed
   `corrupt_log`，不得降级为无历史。
6. 容量淘汰先更新摘要索引，再删除无引用对象；被淘汰或因单 Run 字节上限从未归档的 Run 保留 tombstone，
   Cursor 返回真实 `history_gap`。降低或关闭 archive policy 时，Runtime 启动会在 tenant retention gate 内
   清理旧冷对象，不因可丢弃缓存阻止启动。
7. tenant 上限由同一 EmbeddedRuntime 注册的所有 canonical Workspace state root 共同计算；新增 Workspace
   不会获得一份新的 tenant 冷历史预算。

## 后果

- 正面：完成 Session Turn 和子代理结果不再需要钉住热 Run 目录，调用方仍可在明确预算内读取与续传原 Event。
- 正面：冷历史是可丢弃内容，终态 tombstone 继续是 replay safety 权威；淘汰不会遗忘 Run，也不会允许重放。
- 正面：默认关闭，不给 1000 Run 本地门禁增加持续磁盘增长和每 Run 归档强同步成本。
- 代价：冷文件每次分页都从头校验 SHA 和 sequence，复杂度仍为 O(归档长度)；只有 profiling 证明必要后才增加
  sparse index/chunk manifest。
- 代价：单个 Workspace 的 archive index 损坏会使该 state root 的冷读取失败关闭；本阶段没有自动 quarantine。
- 中性：这是 Event 冷层，不是完整 SQLite Session 数据库，也不提供全文查询、跨机器共享、外部 tombstone 导出、
  zstd 压缩或任意多文件事务。

## 被否决方案

- **默认启用冷归档**：首次实现使 1000 Run 门禁从约 160 秒增加到约 195 秒，并增加每 Run 强同步；与本地默认
  轻量目标冲突，因此改为显式 opt-in。
- **直接采用 SQLite/WAL**：它更适合多记录事务、迁移和并发查询，但会扩大当前阶段的存储依赖和迁移面；等
  生产 store adapter 有明确需求时再增加，不替换独立文件模式。
- **归档后不读回验证**：无法证明删除热目录前已有可读副本，违反 retention 的提交边界。
- **对象损坏时静默报告 history gap**：会把承诺过的持久历史伪装成策略淘汰，破坏审计。
- **立即加入 zstd**：OpenClaw 的实现证明压缩有价值，但当前 16 GB Mac 的首要风险是默认 I/O 与构建复杂度；
  未取得真实归档分布和压缩收益前不增加 codec/cache 生命周期。

## 对标

- **Codex `ff352fab6209`**：rollout recorder 以单 writer、Flush/Shutdown ack 管理已提交历史，Thread metadata
  持有 `rollout_path` 和 `archived_at`，归档 Thread 可继续索引和读取。本实现吸收“先提交可读历史，再改变
  生命周期”的原则；Codex 的 rollout 压缩、Thread 查询、迁移和产品生命周期仍领先。
- **OpenClaw `58b4b9430457`**：SQLite Session archive 在回收 row 前写临时文件、rename/fsync 并完整读回验证；
  session-state event 在删除前写 per-session pruned watermark，另有可选 zstd 与有界 plaintext cache。本实现
  对齐 readback 与真实 gap 原则，但只交付协议中立 Event 的有界本地冷层；SQLite 事务、迁移、quarantine、
  压缩和长期运维仍明显领先。
- **本项目差异**：tenant/Workspace 双重预算、完整 invocation/terminal digest 和 replay tombstone 分离，是为
  多租户嵌入式 Runtime 增加的边界；这不构成对两个参考项目整体成熟度的领先声明。

## 参考

- ADR-0111：终态账本 retention/GC
- ADR-0112：图感知 retention 与分段终态账本
- ADR-0114：版本化 Runtime Event Cursor
- `runtime/apps/runtime-host/src/event_archive.rs`
- `runtime/apps/runtime-host/src/{embedded.rs,retention.rs}`
- `runtime/apps/runtime-host/tests/embedded_retention.rs`
- `docs/evidence/2026-08-17-bounded-cold-event-history.md`
