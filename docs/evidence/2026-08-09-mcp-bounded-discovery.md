# MCP 有界并发与双层截止时间

日期：2026-08-09
范围：纯 Rust Worker、真实 loopback MCP HTTP Server 与 Gateway gRPC；不依赖 NATS、Java、数据库或容器。

## 原缺陷

`discover_federated_tools` 按命令顺序逐台 `await list_tools`。协议最多允许 16 台 MCP Server，
第一台慢服务器会阻止后续所有服务器开始发现，并直接增加 Run 启动延迟。

## 测试驱动证据

1. 第一台服务器在 `tools/list` 阻塞，第二台记录自己何时收到请求。旧串行实现中，第二台在 1 秒内
   从未启动，测试按预期失败。
2. 将并发临时放宽至 16，前 4 台阻塞时第 5 台也会启动，第二个测试按预期失败。
3. 最终实现使用 4 个并发槽；第 5 台必须等槽位释放，同时汇总结果仍严格保持命令顺序。

两个测试都使用真实 TCP、MCP initialize/tools-list、Gateway gRPC 和 Worker 客户端，不断言 mock 调用。

## 实现语义

- 每个 Run 最多同时发现 4 台服务器；跨 Run 的聚合容量与租户轮转由后续 ADR-0042 的共享调度器负责。
- 网络请求并发执行，完成结果按 `McpServerSnapshot` 在命令中的 ordinal 排序后再注册。
- 不可用服务器的报告顺序、模型看到的 Tool 顺序和重复名冲突结果都不受网络完成时序影响。
- `McpDiscoveryPolicy` 可由独立 Rust Runtime Host 提供；默认每服务器 3 秒、整批 10 秒。
- 每服务器 deadline 只淘汰该服务器，已经完成的快服务器目录继续可用。
- 整批 deadline 会取消所有排队和在途 gRPC future；已完成目录保留，未完成服务器按命令顺序进入
  `unavailable`，不会让 Run 无限等待。
- v9 的有效策略在首次发现后写入 Checkpoint schema 7；v10 已在接纳前冻结完整 Runtime policy，并由
  Checkpoint schema 8 绑定。恢复时即使工具目录完全相同，只要并发上限、单服务器 deadline 或整批
  deadline 改变，就拒绝挂载并 fail-closed。

deadline 同样经过测试驱动：旧实现被 Worker 外层 4 秒计时器截断；加入单服务器 deadline 后转绿。
整批预算最初未生效，1 秒外层计时器真实失败；加入总 deadline 后转绿。随后故意清空已完成目录，
“快服务器 + 慢服务器”用例再次变红，恢复部分结果保留后转绿。

## 对标

- Codex 的 `list_all_tools` 使用 `join_all` 并发解析所有服务器，并有缓存、required/optional server 与
  startup deadline；它的单用户响应性和超时治理更成熟，但没有这里的每 Run 并发上限。
- OpenClaw 主要生成 MCP 配置并交给 Codex/Claude/Gemini CLI，核心本身没有同层级的目录发现调度器；
  因而无法直接比较并发上限，但其 stdio/CLI 生态覆盖仍更广。

## 恢复策略测试

旧实现以并发上限 4 建立并保存 Run，再以并发上限 2 发现完全相同的真实 MCP 目录，恢复挂载仍返回
`Ok(())`，测试按预期失败。schema 7 写入显式稳定格式的策略快照后，同策略可继续旧审批并执行，异策略
被拒绝；旧 schema 6 MCP Checkpoint 因无法证明发现策略而 fail-closed。

## 剩余缺口

- RunExecution v10 已在接纳前冻结策略，ADR-0042 也已实现共享容量和租户轮转；当前剩余问题是 NATS
  Worker 仍在串行接单方法中等待发现完成，慢 MCP 会延迟后续 Run 接纳，尚无异步发现监督器。
- 没有目录缓存、required/optional server、动态健康状态或按服务器单独配置 deadline，这些仍落后 Codex。
- 19 个 NATS 集成测试在未配置 `TEST_NATS_URL` 时提前返回，不能作为传输恢复证据。
