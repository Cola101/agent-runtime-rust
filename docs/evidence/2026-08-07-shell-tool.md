# Shell 工具：实现、真实运行，以及一条一直没能证明的证据终于闭合

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 本轮做了什么

按 [ADR-0038](../adr/0038-shell-tool.md) 实现 `shell.exec`，并做真实端到端运行。

## 关键设计判断（都写进了 ADR，此处只记为什么）

**Shell 仍是我们自己按摘要固定的二进制的第三个操作**，不是把 `/bin/sh` 注册进可信仓库。
ADR-0025 的供应链边界因此不破：模型的命令走 stdin 的有界 JSON，不走 argv。

**ADR-0025 的「不用 shell、不拼接参数」这条规则在这里不适用**，值得说清楚。它存在的理由是
「别让构造出来的参数改写**我们**组合的命令」；而 Shell 工具里没有我们组合的命令——整条命令
本来就该由模型作者化。参数解析从来不是边界，它只是维持另一处边界的手段。真正的边界是容器。

**环境从空开始，而不是过滤继承。** 执行器本就 `env_clear()`，工具只加回两个变量：
`PATH`（系统目录，**刻意不含工作区**，所以模型丢在命令旁边的二进制无法按名调用）和 `HOME`。
这比 Codex 严：它继承 core 集合再按 `*KEY*`/`*SECRET*`/`*TOKEN*` 模式排除
（`protocol/src/shell_environment.rs:82-84`）——那是继承之上的黑名单，我们是白纸。
Worker 进程里有 Provider 凭据、数据库口令和 NATS 凭据，不该由「变量名是否命中模式」来兜底。

**`HOME` 指向工作区内的 `.agent-home` 而非工作区根。** 实测出来的：`HOME` 指根时，
第一次跑 `git` 或 `python3`，macOS 框架就会在里面建 `Library/Caches/…`，污染模型自己的目录。

**非零退出是结果，不是工具错误。** 混为一谈会让「命令失败」与「副作用未知」不可区分，
而 fail-closed 只该管后者。

## RED → GREEN（行为性）

```
RED   8 条 shell.exec 测试全红（工具拒收该 tool name：unsupported_tool）
GREEN 8 条全绿，且既有 read/write 7 条未回归
```

覆盖：命令执行与退出码、非零退出不变成工具错误、cwd 在工作区、
**父进程环境不到达命令**、HOME 在工作区内且不是真实家目录、空命令与超长命令被拒、
输出被截断且标记 `stdout_truncated`、结果绑定回发起它的 tool call。

Worker 的工具清单测试原本钉死两个工具名，正常履职拦下了我。已扩展，并**显式钉住 shell 的策略**
（`non_idempotent` / `ask` / 独立 scope / 独立执行器），免得将来被悄悄放宽而无人察觉。

## 真实运行：命令本身就是容器探针

Run `ee70b231-88c9-4e9b-a0d7-fbb3e804bf98`，Skill `cd0f1e38-c941-4cc0-8f6e-f39786184d04`，
AgentVersion `bc94c5da-bfba-4874-960e-9426c9309760`（**只授予 `tool:shell.exec`**）。

前两轮证据里我都只能写「本次 Run 的 `sandbox-exec` argv 未直接捕获」——argv 只活毫秒。
有了 Shell 工具，命令本身可以在**真实 Worker 的容器内**回答这个问题。探针输出（原文）：

```
cwd=…/.local/state/workspaces/11111111-…/44444444-…
home=…/44444444-…/.agent-home
key=[unset]
envcount=5
ssh_list=DENIED
ssh_read=DENIED
gh_list=DENIED
tmp_write=DENIED
workspace_write=ok
probe_done
```

逐条对应：

| 探测 | 结果 | 证明了什么 |
| --- | --- | --- |
| `cwd` | 真实工作区 | 命令起点正确 |
| `home` | `.agent-home` | `~` 不是真实家目录 |
| `key=[unset]` | 未继承 | Provider API Key 没到达模型作者的命令 |
| **`/Users/cola/.ssh` 列目录** | **DENIED** | ADR-0037 的黑名单在**真实**凭据目录上生效 |
| **`/Users/cola/.ssh/*` 读文件** | **DENIED** | 同上 |
| **`/Users/cola/.config/gh`** | **DENIED** | 同上 |
| 写 `/tmp` | **DENIED** | 写不出工作区 |
| 写工作区 | ok | 该能做的仍能做 |

**这三条 DENIED 是针对本机真实存在的凭据目录**（`~/.ssh` 9 项，`~/.config/gh` 2 项），
不是临时目录构造的替身。凭据内容全程重定向到 `/dev/null`，只输出判定字符串，
所以即使容器失效也不会有任何凭据内容进入 stdout、事件表或本文件。

旁证：`/tmp/agent-escape-probe` 不存在。

其余：

```
终态       succeeded | last_sequence=11
Tool 账本  call_shell_probe_1 | completed | sandbox=trusted_native  （单行，恰好一次）
工作区     .agent-home  README.txt  shell-ran.txt
Provider   turn1/turn2 advertised_tools=["shell.exec"]  file_tools_leaked=false
```

最后一行是三方交集：只授予 `tool:shell.exec`，模型只看到 `shell.exec`，两个文件工具零泄漏。

## 检查结果

```
cargo test --workspace              269 通过 / 0 失败（上轮 261，+8 为本轮新增）
cargo fmt --check                   通过
cargo clippy --workspace -D warnings 通过
deploy/native/run-java-tests        143 通过 / 0 失败 / 1 跳过，BUILD SUCCESS
```

## 资源占用

| 进程 | RSS |
| --- | --- |
| control-plane | 98.2 MiB |
| postgres（含子进程） | 75.8 MiB |
| console | 38.2 MiB |
| nats | 20.5 MiB |
| runtime-worker | 18.3 MiB |
| model-gateway | 14.0 MiB |
| checkpoint-gateway | 11.6 MiB |
| **合计** | **0.27 GiB** |

## 限制（明确不声称）

- **这是本平台迄今授予的最宽能力。** 容器允许的一切，命令现在都能做：读任何非凭据的
  用户可读文件、在工作区内任意写、启动任何系统二进制。
- **没有命令白名单**，每条命令都要审批。真实工作里这很重。Codex 有白名单，我们故意先不做——
  白名单是对审批流的优化，审批流得先存在且可信。
- **没有交互式/长驻会话**。一条命令一个结果。Codex 有 `unified_exec` 带 session id，我们没有。
- `/bin/sh` **无法**像自有二进制那样按摘要固定；只能用绝对路径引用，其余依赖系统完整性保护。
- 超时用的是通用工具超时，不是 shell 专用预算。
- **未测试 fork 后脱离的后台进程**：它受容器约束，但没有被跟踪或回收。
- 仅 macOS。Linux 上在 `landlock` 之前不得注册此工具。
- 回环假 Provider，未接触真实模型厂商。
- `envcount=5` 里除 `PATH`/`HOME` 外的三个是 `sh` 自己加的（`PWD`/`SHLVL`/`_`），
  不是继承来的；这一点只从 `key=[unset]` 与计数推断，没有逐个列举核对。

## 复现

```
cd runtime && cargo test -p agent-trusted-workspace-tool --test shell_exec
# 真实主链：make dev → 发布声明 shell.exec 的签名 Skill →
#            AgentVersion 只授予 tool:shell.exec → 建 Run → 审批 → 读 tool.result → make dev-clean
```

密钥仅存在于 `.local/secrets` 与会话临时文件，未进入命令行、日志、本文件或仓库。
