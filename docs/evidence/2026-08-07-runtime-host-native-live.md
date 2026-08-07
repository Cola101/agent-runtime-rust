# runtime-host 原生实跑验收

日期：2026-08-07
对应决策：[ADR-0035](../adr/0035-standalone-rust-runtime-host.md)

此前四轮 runtime-host 的证据全部是 `cargo test`——14 个测试绿，但**没有任何人真正启动过它**。
本轮用真实进程、真实 Unix socket、真实 CLI 跑一遍。

无 Java 控制面、无 PostgreSQL、无 NATS、无 gRPC、无 Docker。

## 实跑立刻抓到两个测试没抓到的缺陷

### 已修：state root 稍深，守护进程就根本起不来

第一次 `serve` 直接失败：

```
Error: StateRoot("path must be shorter than SUN_LEN")
```

`sockaddr_un.sun_path` 限制 socket 路径长度（macOS 104 字节、Linux 108）。
`default_socket_path` 把 socket 放在 state root 内，因此**任何稍深的状态目录都让守护进程无法启动**。
桌面应用的 `~/Library/Application Support/<vendor>/<app>/<profile>` 会立刻撞上。

Rust 测试一直用 `tempfile::tempdir()` 的短路径，永远碰不到这条边界——这是单测结构性看不见的缺陷。

修复：路径超限时退回到 `$TMPDIR` 下由 state root 摘要派生的确定性短名。守护进程与客户端调用同一个
函数，因此发现性不变。补了回归测试
`a_deeply_nested_state_root_still_yields_a_bindable_control_socket`（先 RED 后 GREEN）。

### 未修，只记录：同一 state root 没有单实例锁

实跑中我先启动 C1、再启动 C2 时（C1 尚未被杀），C2 **照常启动并接管了那个 Run**——
`bind()` 会删掉它认为是陈旧的 socket 再绑定，于是两个守护进程共用一个 state root，
都可能执行同一个 Run。本轮未修复；桌面应用双击两次图标就会触发。

### 未修，只记录：守护进程退出不清理自己的 socket

实跑结束后 `$TMPDIR` 里留下 5 个 `agent-runtime-host-*.sock`。绑定时会删除它认为陈旧的 socket，
因此功能上不致命，但长期运行会在临时目录里累积文件，且与「无单实例锁」叠加时，
残留 socket 让人无法从文件系统判断哪个守护进程是活的。

## Scenario A：真实审批链路

Provider 用的是**仓库自己的严格校验 fixture** `deploy/tests/fixtures/openai_tool_provider.rb`
——云端主链用的同一个。它要求系统消息与期望值**精确相等且只有一条**，并要求可信 Tool 已通告，
第二轮还要校验 Tool 结果被绑定回灌。

```
$ agent-runtime-host serve
runtime-host listening on /var/folders/.../agent-runtime-host-946ee795aa9c86fe.sock (resumed 0 unfinished run(s))

$ agent-runtime-host submit "Read README.txt and summarize the evidence."
{"type":"accepted","run_id":"019fd7d0-3e7d-70d2-8849-7618beccf26e"}
```

`AGENT_RUNTIME_LOCAL_TOOL_CONSENT=ask`，Run 停泊在审批闸（真实文件 `runs/<id>/run.json`）：

```json
{
  "state": { "state": "awaiting_approval",
             "approval_id": "019fd7d0-3ee8-7f02-9ed6-bd55bc63769f",
             "binding_digest": "385b8829d504388529c9d0bce5a10bba172e11eb42e9523ea012a1b2d5d58b16" },
  "owner_epoch": 1
}
```

```
$ agent-runtime-host approve 019fd7d0-3e7d-70d2-8849-7618beccf26e
{"type":"accepted","run_id":"019fd7d0-3e7d-70d2-8849-7618beccf26e"}
```

终态 `finished/succeeded`，`owner_epoch 1 → 2`。真实事件日志 13 条：

```
run.started → model.usage → model.tool_call → model.turn.completed → approval.required
→ run.restored → approval.rebound → run.resumed
→ tool.execution.started → tool.result → model.output.delta → model.usage → run.succeeded
```

`run.restored` 与 `approval.rebound` 证明批准走的是真实的跨 attempt 重绑，不是内存里的捷径。

Provider 证据文件：

```json
{ "requests": 2, "tool": "workspace.read_text", "path": "README.txt",
  "result_verified": true, "system_instructions_verified": true }
```

## Scenario C：真实 SIGKILL 后接管

Provider 故意把第一个调用永久晾住，模拟死在模型调用中途的守护进程。

```
杀前记录：{'state': 'running'} epoch=1
SIGKILL -> C1 pid=28450 ；确认进程消失
崩溃后、替代进程启动前：{'state': 'running'} epoch=1
$ agent-runtime-host serve
runtime-host listening on ... (resumed 1 unfinished run(s))
接管后终态：{'state': 'finished', 'status': 'succeeded'} epoch=2
```

事件：`run.started → run.restored → model.output.delta → run.succeeded`。

（第一次做这个场景时我 pid 解析写错，把 "B1" 里的 "1" 当成了 pid，守卫正确拒绝了对 pid 1 的
kill；随后用全新 state root 重做，先确认进程消失再启动替代进程，才是上面这份干净证据。）

## 客户端重连回放

真实 CLI 带游标 attach 已完成的 Run：

```
$ agent-runtime-host attach 019fd7d0-3e7d-70d2-8849-7618beccf26e 10
  seq=11 model.output.delta
  seq=12 model.usage
  seq=13 run.succeeded
  -> succeeded
```

精确从 seq=11 开始，前 10 条按游标跳过。

## 资源占用

| 进程 | RSS |
| --- | --- |
| agent-runtime-host serve（单个） | 约 11.5 MiB |
| 三个并存实例合计 | 34.6 MiB |

远低于 4GB 上限。作为对照，完整云端栈（7 进程）实测 0.26 GiB。

## 门禁

- `cargo fmt --all -- --check` — 通过
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — 零告警
- `cargo test --workspace` — 238 passed, 0 failed, 1 ignored

## 未完成（明确不声称）

- **无单实例锁**（见上），桌面场景必然触发。
- **退出不清理 socket**（见上）。
- **Skill 未接入本地模式**：信任模型已定（控制面签发、离线携带、本地只持验证公钥），密钥分发与
  制品导出未实现。
- **取消只支持已停泊的 Run**。
- **审批模式只有 allow-once / deny**，无会话级记住。
- 无子代理、无并发上限、无配额、无调用方认证（除 socket 0600）。
- 本轮所有 Provider 均为回环假 Provider；**未接触任何真实模型厂商**。

## 复现

```
cargo build --manifest-path runtime/Cargo.toml \
  -p agent-runtime-host -p agent-trusted-workspace-tool
# 启动一个回环 OpenAI 兼容 Provider，然后：
AGENT_RUNTIME_LOCAL_STATE_ROOT=... AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT=... \
AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT=http://127.0.0.1:PORT/v1/chat/completions \
AGENT_RUNTIME_LOCAL_PROVIDER_MODEL=... AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY=... \
AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN=.../agent-trusted-workspace-tool \
AGENT_RUNTIME_LOCAL_TOOL_CONSENT=ask \
agent-runtime-host serve
agent-runtime-host submit "..."   # 另开一个终端
agent-runtime-host approve <run-id>
agent-runtime-host attach <run-id> <after-sequence>
```

密钥仅存在于会话临时目录的文件中，未进入命令行、日志、证据或仓库。
