# 有界模型路由 WAL 证据（2026-08-17）

## 失败边界

ADR-0131 已证明“每次状态整文件强替换”会把 `embedded_retention` 拉到 201.76 秒并突破 180 秒硬阈值。
弱 snapshot 虽恢复到 112–119 秒，却不能证明 Provider 出站围栏和 staged response 在主机故障后仍存在。
本轮没有放宽容量阈值，而是把恢复权威改成有界追加 WAL。

新增状态回退门禁还证明：revision 连续和单条 JSON 合法并不足够。把同一候选的
`same_provider_attempts` 从 1 回退到 0、清除 inflight 后追加，旧读取逻辑会接受；新逻辑以
`changed immutable identity or rolled back state` fail-closed，防止损坏记录重新获得 Provider 调用机会。

## 已验证实现

- V1/V2 pretty snapshot 在 Provider 出站前原子迁移成 revision 1；V3 snapshot 不冒充旧版本。
- 每条 WAL 以最终换行提交；EOF 未提交尾部被忽略并在下一次 append 前截断，已提交坏行继续拒绝。
- revision 从 1 连续；Run、invocation、route binding 与候选链不可变；候选游标、失败/重试前缀、观察计数、
  同候选尝试数、Provider 选择和 completion 不得回退。
- 单条 8 MiB、32 条记录；读取前检查 264 MiB 总上限，稀疏超限文件不会先被读入内存。
- 第 33 次写入 compaction 为一条 revision 1 当前状态；已完成 WAL 禁止继续 append。
- 普通成功路径精确四条：inflight Provider fence、完整 staged response、Event+Checkpoint 后选择观察、
  completion。替代 Host 从 staged record 完成回答，Provider 调用不增加。
- completed WAL 为新 attempt 归档时，rename 后同步父目录。

## 已执行门禁

- `agent-runtime-host --lib model_route_wal_tests`：7/7。
- `agent-runtime-host --test multi_provider`：14/14；包括三协议候选、安全故障转移、同 Provider 重试、
  cooldown/half-open、staged response 恢复和四记录断言。
- `agent-runtime-host --test embedded_retention`：10/10，149.96 秒；1000 Run 保留/恢复硬阈值 180 秒未改。
  实测 hot Run 目录 16、tombstone 984、恢复扫描 669 ms、峰值 RSS 27,475,968 bytes。
- 最终 `cargo test -p agent-runtime-host`：223 通过、0 失败、1 个外部 Codex MCP fixture 显式忽略；其中
  包级 `embedded_retention` 为 152.24 秒。
- `cargo clippy -p agent-runtime-host --all-targets --all-features -- -D warnings` 与
  `cargo fmt --all -- --check`：通过。

## 对标与边界

Codex 的 rollout 单 writer、pending queue 和 flush acknowledgement 在 writer 生命周期上更成熟；OpenClaw
SQLite WAL/transaction 在并发写、迁移、quarantine 和组提交上明显领先。本实现只主张：无需数据库或外部服务
时，单 state-root owner 可同时保护 Provider 副作用和满足本机容量门禁。未验证硬件断电、共享文件系统、
Windows、跨文件事务或多进程 writer。

本轮未启动 Docker、PostgreSQL、NATS 或真实厂商服务，也未新增常驻进程。
