# 关闭 ADR-0037 的 `$HOME` 缺口

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 起因

ADR-0037 落地时自己留了一条缺口，写在它的「限制」一节里：

> `$HOME` 未设置时不发任何 deny 规则，退回 ADR-0036 行为且无任何上报。

形态与同一轮里刚栽过的坑完全一致——**看起来有保护、实际没有**。未规范化路径是静默失配，
`$HOME` 缺失是静默不发规则；两者对读日志的人都表现为「容器化已启用」。

在加 Shell 工具之前必须关掉：Shell 工具让「读得到 `~/.ssh`」从理论风险变成一条命令。

## RED（行为性，非编译错误）

`crates/tool-runtime/tests/containment_survives_missing_home.rs` 单文件单测试
（Cargo 给每个集成测试文件独立进程，所以移除 `HOME` 时没有并发读者——多线程下改 env 是未定义行为）：

```
test credential_denials_survive_an_unset_home_variable ... FAILED

containment silently degraded to no credential denials with $HOME unset; args =
["-p", "(version 1)\n(deny default)\n…\n(allow file-read*)\n(allow file-write-data …)\n",
 "-D", "AGENT_RUNTIME_WORKSPACE=/private/var/folders/…",
 "--", "/private/var/folders/…/trusted-tool"]
```

失败输出就是缺陷本身：整个 profile 里**一条 deny 规则都没有**，而启动照常返回 `Ok`。

## 修复

| 改动 | 位置 |
| --- | --- |
| 家目录改由 `std::env::home_dir()` 解析（Unix 上 `$HOME` → passwd 回落） | `seatbelt.rs` `containment_home` |
| 解析不出则返回 `ContainmentUnavailable`，**拒绝启动**而非降级 | `seatbelt.rs` `required_read_denials` |
| 调用点由 `unwrap_or_default()` 改为 `?` 传播 | `lib.rs` `prepare` |
| 新错误变体 `ToolExecutionError::ContainmentUnavailable` | `lib.rs` |
| Worker 错误码 `tool_containment_unavailable` | `apps/worker/src/lib.rs` |

**编译器逼出的一个决定**：新变体让 Worker 的错误分类变成非穷尽。给它独立错误码而不是并入
`tool_execution_failed`，因为这是**容器化建立不起来**，不是工具本身出错——运维需要区分这两件事。

已核对该错误码只用于回灌给模型的工具结果载荷（`apps/worker/src/lib.rs:3751`），**不驱动重试**，
所以不存在「重试着重试着跑成无容器」的路径。

## GREEN

```
cargo test -p agent-tool-runtime          20 通过 / 0 失败
  credential_denials_survive_an_unset_home_variable                     ok
  an_unresolvable_home_refuses_instead_of_producing_an_empty_denial_set ok
  a_resolvable_home_yields_the_full_denial_set                          ok
  the_home_directory_resolves_to_an_absolute_path                       ok
cargo fmt --check                          通过
cargo clippy --workspace -D warnings       通过
cargo test --workspace                     257 通过 / 0 失败（上轮 253，+4 即本轮新增）
```

## 真实运行（回归）

Run `6722f04a-05a8-472e-a977-61a940bb2494`，签名 Skill `3e443b4a-ffa0-4d93-8bae-e4dab7045e60`，
AgentVersion `9387c58a-b31a-4e2d-864f-1388cde41167`（只授予 `tool:workspace.write`）。

```
终态       succeeded | last_sequence=11
Tool 账本  call_sse_resume_1 | completed | sandbox=trusted_native   （单行，恰好一次）
tool.result {"path":"sse-resume.md","text":"sse resumption verified","bytes":23} is_error=false
落盘       -rw------- 23 字节 sse-resume.md
容器化失败事件 0 条（查 run_events 中 tool_containment_unavailable）
```

换掉家目录解析来源、加入新错误变体之后，主链未回归。

## 证据边界（区分已证与未证）

| 命题 | 状态 |
| --- | --- |
| `HOME` 未设时黑名单仍然生成 | **已证**——真实进程移除 `HOME` 后走真实 `prepare()`，argv 中 deny 参数非空 |
| 家目录彻底解析不出时拒绝启动而非返回空列表 | **已证**（单元测试） |
| 加了新错误变体后主链未回归 | **已证**——上面这次真实 Run |
| 拒绝路径在真实主机上触发过 | **未证**——每台测过的机器都解析得出家目录；这条只有单元测试，没有观测 |
| 本次 Run 的 `sandbox-exec` argv 确实带了 deny 参数 | **未直接捕获**——argv 生命周期只有毫秒，与上一份证据同一限制 |

后两行不说成已证。

## 一个自己的检查错误

核对落盘时我写了 `find … -newermt "-10 minutes"`。macOS 的 `find` 不支持相对时间，
命令退出 0 且无输出，`||` 回退分支因此没触发，看起来像「文件不存在」。
直接 `ls` 才看到文件在。**是我的检查写错了，不是产品问题**——记在这里因为同一个坑我踩过不止一次。

## 资源占用

| 进程 | RSS |
| --- | --- |
| console | 193.4 MiB |
| control-plane | 121.3 MiB |
| postgres（含子进程） | 93.8 MiB |
| nats | 22.3 MiB |
| runtime-worker | 19.6 MiB |
| model-gateway | 15.7 MiB |
| checkpoint-gateway | 12.8 MiB |
| **合计** | **0.47 GiB** |

## 限制（明确不声称）

- 机密性仍**只对四个目录**成立，其余可读文件仍可读。
- 名单硬编码，无租户级配置。
- 仅 macOS；Linux 无 `landlock` 等价物。
- 未验证硬链接、或从沙箱外部创建的指向受保护目录的符号链接是否绕过。
- 未针对真实 Shell 工具验证，因为还没有 Shell 工具。
- 回环假 Provider，未接触真实模型厂商。

## 复现

```
cd runtime && cargo test -p agent-tool-runtime
# 真实主链：make dev → 发布签名 Skill → 建 Run → 审批 → 验证落盘 → make dev-clean
```

密钥仅存在于 `.local/secrets` 与会话临时文件，未进入命令行、日志、本文件或仓库。
