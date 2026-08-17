# 统一终态 Run 收敛证据（2026-08-17）

## 稳定 RED

真实 loopback Provider 完成 one-shot Run 后，测试精确截断该模型请求最后一条 route WAL completion，保留已提交
staged response、旧的非终态 Checkpoint 与 terminal Event。替代 `LocalRuntimeHost::resume` 在修复前稳定再次
进入 Agent Loop，Event Log 新增 `run.restored`、重复输出和第二个终态事件。这证明 ADR-0133 的 adapter 投影
收敛没有覆盖 Direct Host 的 preterminal Checkpoint。

第二个窗口精确删除 one-shot Run 唯一 terminal Event、保留终态 Checkpoint。修复后 replacement 发布的是
Checkpoint 内原始 `EventEnvelope`，事件字节、event id、sequence、attempt 和 digest 全部相同，Provider 不重放。

## 已验证语义

- 所有 Kernel 终态先提交 schema 27 Checkpoint，再发布 Event；普通 Run、Session 和子代理不再分叉。
- 终态 `resume` 先验证完整 replacement command，再只观察原终态；改变 Agent 指令、MCP authority 或历史导入
  会在 Provider/Tool 前以 Checkpoint 身份错误失败关闭。
- Event 已提交但 route WAL 未 completion 时，唯一已结算 WAL 被幂等封口；合法 `.json.partial` 被视为未提交
  staging。人为加入第二个 unfinished 权威 WAL 时 replacement 明确拒绝，Event Log 一个字节也不改变。
- in-flight Provider WAL 不被伪装为完成；终态 Checkpoint 阻止 Run 重放，原始不确定证据继续保留。
- Direct Host、daemon recovery、Embedded control/recovery 和 gRPC replacement 继续使用同一 Kernel 终态。

## 已执行门禁

- `cargo test -p agent-runtime-host`：226 通过、0 失败、1 个需要外部 Codex MCP fixture 的测试显式忽略；其中
  1000 Run retention 160.57 秒、1000 in-flight / 32 admitted 42.47 秒，既有阈值未放宽。
- `cargo test -p agent-runtime-worker`：163 通过、0 失败。
- `cargo clippy -p agent-runtime-host --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt --all -- --check` 与 `git diff --check`：通过。

第一次 Runtime Host 全包在新语义下暴露两项旧测试仍期待终态 Run 再进入执行后才发现身份漂移；实现实际已在
任何 egress 前以 Checkpoint 拒绝。断言改为精确接受 `LocalRuntimeError::Checkpoint` 后，两项定点与第二次完整
全包均通过。这是错误分层更新，不是放宽恢复绑定。

## 对标与未验证

Codex 的 rollout 单 writer/flush ack 与 OpenClaw 的 SQLite writer queue/transaction 都以单一提交权威避免
重复生命周期。本实现将这项原则落实到无外部数据库的多租户本地文件模式，并明确分开 terminal Checkpoint、
Event、route WAL 和 adapter projection 的收敛职责。未验证范围仍包括真实厂商 Provider、SIGKILL 精确指令级
故障注入、主机掉电、共享文件系统、跨机器 owner、Windows 和任意多文件原子事务；总体进度维持 70–75%。
