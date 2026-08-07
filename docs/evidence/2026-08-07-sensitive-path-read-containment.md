# 敏感路径读容器化：ADR-0037 落地与 ADR-0036 更正

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 起因

ADR-0036 决策 4 写着「读不受限」，并把它记为实测结论，理由是每种读白名单都让进程在启动前
SIGABRT（dyld 加载失败）。它还写了一句「Codex 直接放开 `file-read*`」。

重读 Codex 源码后，**那句话是错的**，由它推出的结论也过宽：

| Codex 位置 | 事实 |
| --- | --- |
| `codex-rs/sandboxing/src/seatbelt.rs:687-695` | 有不可读根时，以 `/` 为可读根 + `(require-not …)` 挖洞 |
| `codex-rs/sandboxing/src/seatbelt.rs:467` | 每个不可读 glob 生成 `(deny file-read* (regex …))` |
| `codex-rs/sandboxing/src/seatbelt.rs:741-747` | deny 段拼在 read 段**之后**，依赖 SBPL 后规则覆盖先规则 |

两种都是「先全开、再挖洞」，都不是白名单。**关于白名单的测量成立，据此得出的「读不可限制」不成立。**

这件事现在做而不是以后做，是因为 ADR-0036 自己写明：读不受保护这一行，只在工具仅读写工作区文本时
勉强可忍。Shell 工具是路线上的下一项，而在一个读得到 `~/.ssh` 的容器上加 Shell，不是能先上线再补的事。

## 改动

`runtime/crates/tool-runtime/src/seatbelt.rs`：

```
(allow file-read*)                                  ← 保持全开
…
(deny file-read*  (subpath (param "…DENIED_READ_0")) (literal (param "…DENIED_READ_0")))
(deny file-write* (subpath (param "…DENIED_READ_0")) (literal (param "…DENIED_READ_0")))
```

受保护集合：`~/.ssh`、`~/.aws`、`~/.gnupg`、`~/.config/gh`。

- `subpath` 管内容，`literal` 管目录节点本身——只有 `subpath` 时目录仍可被 stat 和枚举。
- 同时 deny 写：工作区外本已被 `(deny default)` 挡住，但受保护目录若恰好落在**可写工作区内**，
  否则仍能经 create/unlink 探测。
- 路径以编号 `-D` 参数传入，绝不插值进 profile 文本（与 ADR-0036 决策 3 同一条规矩）。

## RED → GREEN（行为性，非编译错误）

```
RED  （只声明参数、不发 deny 规则）
  a_contained_tool_cannot_read_a_protected_directory        FAILED  ← 容器内 cat 得到私钥文件
  a_contained_tool_cannot_enumerate_a_protected_directory   FAILED  ← ls 列得出目录内容
  denied_paths_are_passed_as_parameters_and_never_interpolated FAILED
  ordinary_reads_still_work_alongside_the_denial            ok      ← 对照，改前改后都必须绿

GREEN
  8 passed; 0 failed
```

`ordinary_reads_still_work_alongside_the_denial` 是防假绿的对照：读白名单会让进程根本跑不起来，
那种「什么都没执行」会冒充成容器化，使前两条测试**因错误的原因**变绿。

测试全程只用临时目录，**没有任何一条读写或指名真实的 `~/.ssh`、`~/.aws`、`~/.gnupg`、`~/.config/gh`**。

## 过程中发现的更危险的坑：未规范化路径 = 静默失效的规则

加上 deny 规则后两条容器测试**仍然失败**。原因不是规则写错，而是：

```
temp_dir  = /var/folders/rf/…/T
canonical = /private/var/folders/rf/…/T
/var -> private/var        /tmp -> private/tmp
```

Seatbelt 判定的是内核解析后的路径。未规范化的前缀产生一条**永远匹配不上的规则**——
比没有规则更糟，因为它读起来像保护。Codex 有 `normalize_path_for_sandbox` 正是为此。

已补等价实现：解析**最长存在祖先**再拼回剩余段（受保护目录可能尚不存在，不能要求整条路径可解析）。

## 真实运行（不是单元测试）

Run `4285f96b-48c9-4944-aad2-dddd4ef2b8e2`，签名 Skill `8d732196-d5c0-4a08-9cd0-d197e2d00b52`，
AgentVersion `67a40542-9650-4e84-b1bd-f0858673d136`（只授予 `tool:workspace.write`）。

```
终态      succeeded | last_sequence=11 | finished_at 非空
事件      run.started → model.usage → model.tool_call → model.turn.completed →
          approval.required → run.resumed → tool.execution.started → tool.result →
          model.output.delta → model.usage → run.succeeded
Tool 账本 call_sse_resume_1 | completed | sandbox=trusted_native   （单行，恰好一次）
落盘      -rw------- 23 字节 sse-resume.md
Worker    HOME=/Users/cola   ⇒ 黑名单解析为真实凭据目录，非空
```

## 证据边界（区分已证与推断）

| 命题 | 状态 |
| --- | --- |
| 内核真的拒绝对受保护目录的读与枚举 | **已证**——单元测试真实 spawn `/usr/bin/sandbox-exec`，非 mock |
| 拒绝是定向的，普通读不受影响 | **已证**——对照测试 |
| 加黑名单后生产工具路径未被打破 | **已证**——上面这次真实 Run |
| Worker 能解析出非空黑名单 | **已证**——`HOME=/Users/cola` |
| 本次 Run 的 `sandbox-exec` argv 确实带了那些 `-D` 参数 | **未直接捕获**——`lib.rs:436-448` 是直线路径，唯一分支（`HOME` 是否存在）已验证；但 argv 生命周期只有毫秒，本轮没抓到 |

不把最后一行说成已证。

## 静态检查与全量

```
cargo fmt --check                      通过
cargo clippy -p agent-tool-runtime -D warnings   通过
cargo test -p agent-tool-runtime       20 通过（lib 8 + restricted_container 5 + trusted_native 7）
cargo test --workspace                 253 通过 / 0 失败
```

首次跑 workspace 时 `agent-runtime-host::daemon_recovery` 的
`a_restarted_daemon_resumes_a_run_its_predecessor_left_unfinished` 超时。**与本改动无关**：
该测试 `trusted_workspace_tool: None`，根本不经 Seatbelt 路径；它的等待预算是固定
200×25ms = 5 秒墙钟，41 个测试二进制并行时不够。单独跑 0.34s 通过，第二次全量 253 全过。
这是既有的脆弱测试，会让「全量绿」这个判据本身不可信，值得单独修，本轮未动。

## 资源占用

| 进程 | RSS |
| --- | --- |
| control-plane | 109.1 MiB |
| console | 92.6 MiB |
| postgres（含子进程） | 79.0 MiB |
| nats | 21.5 MiB |
| runtime-worker | 18.1 MiB |
| model-gateway | 14.0 MiB |
| checkpoint-gateway | 11.6 MiB |
| **合计** | **0.34 GiB** |

## 限制（明确不声称）

- 机密性**只对枚举的四个目录**成立。其余任何可读文件仍然可读。不得表述为「读已被容器化」。
- 名单硬编码在代码里，无租户级配置。
- 仅 macOS。Linux Worker 仍无容器化，`landlock` 等价物未实现。
- **`$HOME` 未设置时不发任何 deny 规则**，退回 ADR-0036 行为且无任何上报。这是缺口不是决定，
  ADR-0037「限制」一节已记录；改为启动即拒绝、或从 passwd 库解析家目录可以补上，未做。
- 未验证硬链接、或从沙箱外部创建的指向受保护目录的符号链接是否绕过。
- 未接触真实模型厂商（回环假 Provider）。
- 未针对真实 Shell 工具验证，因为还没有 Shell 工具。

## 复现

```
cd runtime && cargo test -p agent-tool-runtime     # 含真实 sandbox-exec 容器测试
# 真实主链：make dev → 发布签名 Skill → 建 Run → 审批 → 验证落盘 → make dev-clean
```

密钥仅存在于 `.local/secrets` 与会话临时文件，未进入命令行、日志、本文件或仓库。
