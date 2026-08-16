# ADR-0107：Runtime 会话权威探活与事件驱动 Process Wait

状态：Accepted（2026-08-14）

## 背景

完整工作区回归暴露了两个内核问题：stdio MCP 的目录缓存把“进程尚未退出”误当成协议可用；64 个真实
Session / 1024 个 wait 虽然只有每 Session 一个观察器，但本地写入仍要等下一次 50ms 文件扫描，在 Mac
并发 I/O 下 p50 会越过 1 秒门禁。修复必须保持本地原生、协议中立和多租户持久语义，不增加控制面或容器。

## 决策

1. stdio MCP 目录缓存只是一项优化。每次复用前必须由精确初始化会话完成有界 MCP `ping`；PID 存活、
   actor channel 存活或缓存 TTL 未过期都不能单独证明 authority 健康。
2. `ping` 超时、协议错误或 EOF 会退役整个会话进程组，并使本次复用失败。仅 discovery 可以由外层既有
   策略启动新会话；`tools/call` 继续禁止自动重放。
3. Process Wait 继续维持每 Session 一个共享观察器、任意数量 waiter。成功的本地 write 在 durable intent
   和真实 PTY/FIFO 副作用之后用 `Notify` 唤醒观察器。
4. 50ms 文件观察保留为外部 PTY 输出、跨 Host 写入、自然退出和替代 Host 恢复的权威兜底；不能只依赖
   进程内通知。
5. PTY supervisor wire protocol 保持 v3，`Start` 必须携带 expected supervisor generation，服务端在
   spawn 前比较；旧 capability 明确不兼容，不得删除旧 owner socket 后自行接管。

## 对标判断

- Codex 的 MCP 连接复用会检查 transport/service closed 状态，`unified_exec` 以 `Notify/watch` 等待输出与
  关闭；本阶段吸收其事件驱动原则，并额外要求缓存 authority 的协议级 ping 与跨 Host durable fallback。
- OpenClaw Node Host MCP 用 `onclose + connected` 管理进程期连接，PTY 在 `onData` 时直接 push，并在异步
  emit 期间 pause；其在线 relay 更成熟，但未提供本项目同口径的多租户 durable cursor/Host replacement。

## 验收

- 活进程但不响应 `ping` 时，缓存命中必须失败且会话被退役。
- 已退出会话不能授权缓存；新初始化会话成功 `ping` 后才可复用缓存，不能额外 `tools/list`。
- 64 Session / 1024 wait 保持最多 64 observer；本轮连续 3 次 p50 分别为 869.39ms、727.03ms、
  883.04ms，p95 均低于 1 秒，p100 均低于 1 秒。
- PTY generation-fence 单元测试与真实 TTY exact 测试保持通过。后续全量门禁已定位原“模糊启动”：
  supervisor 在 manifest 尚为 `Starting/unprepared` 时未能在冷启动期限内就绪，Manager 的失败收敛却只
  接受 `prepared`，因此把确定的 pre-spawn 失败错误升级为 `indeterminate`。现在两条边界分别收敛，
  `prepared` 之后仍禁止乐观归类；本 ADR 不把单机回归等同于跨平台证明。
