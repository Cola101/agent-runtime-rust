# ADR-0129：JSONL 提交标记与崩溃尾部修复

- 状态：Accepted
- 日期：2026-08-17
- 范围：Runtime Event Log、Event Cursor、流式订阅、审批/恢复续写

## 背景

Runtime 的 `events.jsonl` 是 Checkpoint、Run record 与控制收据之外的事件权威。单行写入完成后已有
`sync_data()`，新文件也同步父目录，但进程可能在 `write_all()` 中途被终止，留下没有换行的 JSON 前缀。
此前完整回放会把该前缀当 JSON 解析并使整个 Run 不可读；流式游标则直接报 `CorruptLog`。若后续继续
append，新事件还会与半行拼接，永久破坏历史。

## 决策

1. JSONL 的最终 `\n` 是单行提交标记。只有以换行结尾的行属于持久事件；EOF 处有界但未换行的字节是
   未提交崩溃尾部，不参与回放、游标或终态判断。
2. 下一次 append 前先检查最后一个字节。若缺少提交标记，从 EOF 反向分块寻找最后一个换行，将文件截断到
   该提交边界并 `sync_data()`，随后才 append 新行并再次同步。正常文件只读取最后一个字节，不扫描全日志。
3. 修复范围严格限制为最后一个未换行片段。任何已换行的空行、超限行或坏 JSON 继续 fail-closed；事件游标
   仍验证 tenant/Workspace 身份、连续 sequence、payload digest 与唯一终态。
4. 流式订阅在遇到未提交尾部时保存最后一个完整行的字节偏移。运行中 Run 重开文件后从该偏移继续，不能
   从头重放或因 sequence 不匹配误报损坏。
5. 即使尾部碰巧构成完整 JSON，只要没有换行也视为未提交并丢弃。Runtime 不从“磁盘上看起来完整”反推
   `sync_data()` 已成功。

## 对标

- **Codex `ff352fab6209`**：`rollout::recorder::ensure_rollout_is_newline_terminated` 在已有 rollout 尾部缺少
  换行时直接补 `\n`；测试同时覆盖合法和非法未终止尾部。合法 JSON 会被保留，非法行由后续解析路径跳过。
  本项目吸收其“append 前必须先处理尾部”的结构，但不保留未提交行，因为本事件日志直接决定多租户 Run
  恢复与副作用证据，不能把语法完整等同于持久提交。
- **OpenClaw `58b4b9430457`**：当前 Session transcript 权威路径使用 SQLite writer queue、事务与 WAL；JSONL
  更多是导出/兼容制品。它在存储引擎原子性和成熟迁移上领先，但要求 SQLite，不符合本阶段独立 Rust Host
  无外部数据库即可完成 Run 的边界。

## 代价与未覆盖

- 崩溃发生在 JSON 序列化完成但换行尚未写入时，该行会重做或由上层恢复，而不会被猜成已提交。
- 本轮模拟进程级 torn write；没有模拟主机掉电、文件系统撒谎或扇区损坏。
- JSONL 没有独立 frame CRC。已提交行仍依赖 JSON、身份、sequence 与 payload digest 检测损坏；若未来需要
  跨机器共享存储，应采用具备事务/校验的日志或数据库，而不是继续扩展本地 JSONL。
