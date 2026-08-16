# 2026-08-15 终态账本保留与 1000 Run churn 证据

## 真实闭环

- 在 M1 Pro 16GB 上由一个 `EmbeddedRuntime` 顺序执行 1000 个不同 Run；每个 Run 都经过真实本地
  HTTP/SSE Provider、Agent Loop、事件提交和终态 Run record，不是直接造文件或只测 GC 函数。
- 加入 tenant 聚合性能修复后的最终 exact 数据：1000 次 Provider 请求；16 个热 Run 目录、984 个精确
  Run tombstone；`terminal-ledger.json` 1,006,785 bytes，state root 共 66 个文件 / 1,163,921 bytes。
- 进程 RSS 基线 11,862,016 bytes、峰值 29,638,656 bytes；FD 12→12→12；总耗时 123.262 秒。
  替代 Runtime 载入账本并完成维护扫描为 1,114ms。
- 最早 Run 的相同输入重放和不同输入冲突都在 Provider 前拒绝；销毁并重建 Runtime 后仍保持同一围栏，
  Provider 调用总数仍精确为 1000。

## 崩溃与安全边界

- 确定性测试在 replay barrier 已持久、artifact 尚未删除时模拟崩溃；替代 Runtime 会幂等删除并把
  `artifacts_removed` 收敛为 true，墓碑不丢失。
- `indeterminate`、非终态 Run 和带 `Accepted` 未完成 control receipt 的 Run 不进入候选集。
- 同一 Workspace state root 的第二个 live Runtime 被 Unix 租约拒绝。
- 已回收 command 的精确重放从 compact ledger 返回 Completed；同 command ID 的变更请求被拒绝。
- 单 Workspace 与同租户跨 Workspace 的目录上限均有行为测试：另一个 Workspace 的可证明终态可以被
  回收；只有活动/模糊证据时在模型调用前失败关闭，且不创建新 Run record。

## 迭代中发现的问题

- 初版在每个 Run 后解析并重写完整增长账本，1000 Run 呈 O(n²) 写放大，3 分 35 秒仍未完成，已主动
  中止。修复为 hard-cap 触发的批量维护与租约保护的内存索引后，真实门禁在 117 秒内完成。
- 初加 tenant 聚合边界时又在每个 Run 读取接近 1 MiB 的墓碑账本，门禁虽通过但退化到 177.456 秒。
  墓碑聚合只会在启动/维护提交点变化，现已将校验移到这两个权威边界；最终恢复为 123.262 秒，同时保留
  每次 Run 的 tenant 目录 hard-cap 检查。
- 第一次完成运行仅因文件预算断言写成 `<64` 而失败；实际固定开销还包含模型路由日志和 state-root lock。
  断言改为按每个热 Run 的固定文件预算计算，没有放宽总磁盘或账本大小门禁。
- 新增 state-root 租约后，全工作区暴露 3 个“replacement daemon”测试其实没有停止前任 owner。测试现已
  改为先 abort/等待旧 server、释放旧 Runtime 租约，再启动替代者；没有为了旧测试放宽生产单写围栏。
- 该结果证明“有界”，不证明单 JSON 账本适合生产长期历史；约 1 MiB/1.1s 已被记录为下一阶段的扩展风险。

## 当前质量门禁

- Retention 单元测试 2/2；除 1000 churn 外的 retention 集成测试 6/6；tenant 跨 Workspace 回收、
  无安全候选失败关闭和启动时既有墓碑聚合溢出三条路径通过。
- Runtime Host 共 164 项：**163 通过 / 0 失败 / 1 个外部 Codex MCP 用例显式忽略**。
- Rust 全工作区共 689 项：**683 通过 / 0 失败 / 6 个外部 live 用例显式忽略**。
- Clippy workspace/all-targets/all-features `-D warnings`、Rust 格式与 Git 差异检查已通过。
- 最终 `cargo clean` 删除 51,824 个文件、15.1 GiB 逻辑构建产物；清理前 `runtime/target` 实占约 12GB，
  清理后不存在。未发现项目内 `node_modules`/`.local`、匹配的 Runtime 测试临时目录或遗留 Runtime 进程；
  清理后没有重新构建。

## Codex / OpenClaw 源码对标

- Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`：thread 删除先清 log/memory/goal/dynamic tool，最后删
  spawn edge 和 thread row；清理失败保留 retry graph。日志按 10 天、thread/process content/row cap 并在
  同一事务插入后裁剪。
- OpenClaw `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28`：task retention 只裁终态，默认 terminal 7 天/lost
  1 天；cron history 每 job 上限 2000；maintenance 先 reconcile recovery/lost，再清 terminal session 和
  prune，并按 batch yield。audit store 以 SQLite transaction 同时执行 age/max-row 裁剪。
- 本项目采用两者共同的“先保留恢复/幂等权威，再删结果”和“只裁确定终态、按年龄与容量批处理”；增加
  完整多租户 invocation 与 Run/control digest 墓碑。Codex/OpenClaw 在 SQLite 事务、产品归档、树级生命周期、
  history gap/运维体验方面仍领先。
