# 有界冷 Event 历史证据（2026-08-17）

## 稳定 RED

在真实 Session Continue 链完成后强制 `retain_terminal_runs_per_workspace=0`，既有 retention 会先提交
tombstone 再删除第一轮 Run 热目录。随后用公开 Event Cursor 读取该退休 Run，修复前稳定失败于：

```text
retention must preserve verified cold Event history before deleting the hot Run
```

当时 Cursor 只返回空事件、`history_gap=true`。这证明缺口位于真实 Session→Run→retention→公开 Cursor
消费链，不是新建孤立 helper 的假需求。

## 已验证语义

- 启用显式 byte/count policy 后，终态 Event 已提交前缀会流式写入内容寻址对象；SHA-256 readback 与摘要索引
  在热目录删除前完成。
- 归档后的退休 Run 可由 Event Cursor 分页，也可由 bounded subscription 先输出 Event、再输出
  `Retired(history_gap=false)` Boundary。
- Workspace count cap 淘汰最旧对象后，旧 Run 保留 tombstone 并精确变为 `history_gap=true`；报告只把真正
  提交过的冷历史计作 eviction，从未归档的超限候选不伪装成淘汰。
- 单 Run 超过 byte cap 不阻止安全退休；结果是 tombstone + 明确 gap，而不是无上限分配内存或保留热目录。
- 同一 tenant 的两个 Workspace state root 共享一个 archive count/byte 预算，第二个 Workspace 不能放大上限。
- 对象篡改返回 typed `corrupt_log`；关闭 archive policy 后重启会清除损坏冷对象、保留终态 tombstone，并把
  读取语义恢复为明确 gap。
- Unix 归档目录/index/object 权限分别验证为 `0700/0600/0600`；索引读取另有 16 MiB 硬上限。

## 已执行门禁

- `cargo test -p agent-runtime-host --test embedded_retention`：13 通过、0 失败；1000 Run 长门禁未降规模，
  完整文件耗时 163.12 秒。
- `cargo test -p agent-runtime-host`：231 通过、0 失败、1 个需要外部 Codex MCP fixture 的测试显式忽略；
  全包内 1000 Run 门禁 171.75 秒，1000 in-flight / 32 admitted 门禁 43.96 秒，总耗时 378.21 秒。
- `cargo clippy -p agent-runtime-host --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt --all -- --check` 与 `git diff --check`：通过。

## 对标与未验证

Codex 的 rollout/Thread store 提供更成熟的长期归档、压缩、索引和客户端生命周期；OpenClaw 的 SQLite/WAL
提供多记录事务、迁移、并发 writer、archive readback、zstd 与 quarantine。本阶段只证明无外部数据库的
多租户有界 Event 冷层，不宣称替代它们的完整 Session store。

未验证范围包括主机掉电、共享文件系统、跨机器 owner、Windows 权限语义、介质损坏恢复、自动 quarantine、
zstd、外部对象存储和真实生产长期分布。冷归档默认关闭，总体 Rust 内核进度仍维持 70–75%。
