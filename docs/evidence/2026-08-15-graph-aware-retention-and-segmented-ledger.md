# 2026-08-15 图感知回收与分段终态账本证据

## 结论

Rust Runtime 的 root Session Turn 与子代理 Run 已进入统一 Run record、恢复引用图和 terminal tombstone
治理。完成历史只保留 digest-bound provenance，不再永久钉住热 Run 目录。单 JSON 账本已通过崩溃安全迁移
升级为 manifest、256-Run 不可变封存段和有界 active segment。

## 行为证据

| 验证 | 结果 | 证明边界 |
| --- | --- | --- |
| `retention::tests` | 5/5 | 活动 Session 强引用、完成来源弱引用、旧账本 600 条迁移、封存段不可变、未清理墓碑修复 |
| `completed_session_history_survives_hot_run_artifact_retention` | 1/1 | 首个 Session Run 热目录删除后，替代 Host 仍从内嵌 transcript 继续第二 Turn |
| `two_subagents_are_inflight_before_either_child_completes` | 1/1 | 两个真实并行 child Run 均有终态 Run record，可回收；父 Checkpoint/result 仍存在 |
| 1000 Run churn | 1/1 | 真实 HTTP/SSE Agent Loop、自动 retention、替代 Runtime replay fence、RSS/FD/扫描门禁 |
| multi-tenant multi-Workspace churn | 1/1 | 4 tenant × 3 Workspace × 32 Run，所有 state root 独立有界，替代 Runtime 可验证 |

## 1000 Run exact 数据

```text
runs=1000
hot_run_directories=16
tombstones=984
ledger_bytes=1008106
state_files=70
state_bytes=1165242
recovery_scan_ms=934
rss_baseline_bytes=12500992
rss_peak_bytes=27246592
fd_baseline=12
fd_peak=12
fd_final=12
elapsed_ms=110617
```

相对 ADR-0111 的单文件 exact 结果（123.262 秒、替代扫描 1.114 秒），分段实现总耗时下降约 10.3%；替代
扫描缩短 180ms。文件数从 66 增至 70，是 manifest、active 与 3 个封存段的固定代价；
状态总量仍约 1.17 MiB。

第一次实现曾真实失败：1000 Run 总耗时 170.49 秒，最终维护扫描超过 2 秒。根因是 repair、容量统计和候选
扫描重复反序列化全部封存段。修复后：

- repair 只读取 manifest + active，因为封存段结构上只允许 `artifacts_removed=true`；
- 容量统计从 manifest descriptor + active 得出；
- 完整段校验仍在启动 tombstone index 与候选扫描执行；
- terminal Run 不再为子代理强引用扫描重复解析大 Checkpoint。
- active append 只验证并提交有界 active segment，不再重新哈希、校验全部历史。

因此没有通过放宽 2 秒门禁掩盖回归。

## 多租户长周期 exact 数据

```text
tenants=4
workspaces_per_tenant=3
runs_per_workspace=32
total_real_http_runs=384
elapsed_seconds=36.64
hot_run_directories_per_workspace=6
tombstones_per_workspace=26
provider_requests=384
replacement_replay_fence=verified
```

该证据验证一个进程中的共享多租户 Runtime 和 12 个规范化 state root，不是 1000 active Run、跨进程租户
配额或生产 SLA。

## 崩溃与迁移顺序

- 旧 `terminal-ledger.json` 在 manifest durable commit 后才删除；600 条测试迁移为 2×256 封存段 + 88 active。
- 封存段内容与 descriptor digest/计数绑定；后续追加 10 条只改变 active，首段文件摘要保持不变。
- tombstone durable、制品尚未删除的窗口由 `repair_committed_tombstones` 幂等完成。
- Session 文件或非终态 Checkpoint 读取/解析错误会终止 maintenance，不把错误当作“没有引用”。

## 全工作区回归发现

首次全工作区门禁发现 `subagent_approval` 的真实 stale owner epoch 回归：新增 child Run record 后，重启扫描
同时把父 Run 和父 Checkpoint 所拥有的 child Run 当作独立恢复根，形成两个 child owner。修复后 Runtime
先解析非终态父 Checkpoint 的 pending/active/reservation 引用，只调度恢复图根，由父沿 spawn tree 恢复
child；没有弱化 epoch 校验。原失败 exact 用例连续 6/6 通过，第二次全工作区门禁为 694 项中 688 通过、
0 失败、6 个外部 live 用例显式忽略。Clippy workspace/all-targets/all-features `-D warnings`、格式与差异
门禁均通过。

## 仍未证明

- 本轮是 1000 顺序 Run 与 384 多租户顺序 Run，不是 1000 同时 active Run。
- 没有冷归档读取、公开 `historyGap`、tombstone 外部转储或无限期存储方案。
- 文件段读取仍是 O(segment count) 的启动索引；SQLite/可插拔持久层尚未进入当前目标。
- 非 Unix state-root 跨进程单写未补齐。
- Java、GUI、Edge、Docker、PostgreSQL、NATS 均未作为本轮依赖或验收对象。

## 对标判断

- **Codex**：已吸收“先发现完整引用树、最后删除图权威”的失败可重试原则；Codex 的 SQLite 查询、完整
  Thread 产品归档/删除和 rollout cold tier 仍更成熟。
- **OpenClaw**：已吸收“Session 行先提交、只清理最后引用的制品、磁盘预算与历史缺口分离”的原则；
  OpenClaw 的 SQLite Session、压缩归档、`historyGap` 和运营策略仍领先。
- **本 Runtime 的窄面优势**：tombstone 绑定完整 tenant/application/workload/Workspace/AgentVersion/
  model policy、Run/input、owner epoch 和 terminal event digest；这比两份参考源码的单用户本地来源证明更适合
  共享多租户嵌入，但不代表产品整体领先。
