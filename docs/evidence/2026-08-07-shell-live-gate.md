# Shell 工具的常驻门禁：从容器内部验证容器

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 为什么这个门禁值得存在

ADR-0036、0037、0038 三份决策的容器边界，此前都只由**直接 spawn `sandbox-exec` 的单元测试**
证明。那是真实的，但不是生产路径。生产路径上的证据一直缺一块，两份证据文件都如实记过：

> 本次 Run 的 `sandbox-exec` argv 确实带了 deny 参数 —— **未直接捕获**，argv 生命周期只有毫秒。

Shell 工具改变了这一点：**命令本身可以在真实 Worker 的容器内回答这个问题**。
上一轮的一次性探针已经这么做了，但一次性证据会像 `2026-08-02` 那批一样过期。
本轮把它固化成 `make check-native-shell-live`。

## 一个由实测决定的设计约束

最初想让门禁与机器无关：既然 deny 规则是按路径写的，那么即使 `~/.ssh` 不存在，
访问它也该被拒。实测推翻了这个设想：

```
受保护但不存在的目录：  ls: …/.ssh: No such file or directory
未受保护且不存在（对照）：ls: …/.nothing: No such file or directory
```

**ENOENT，两者不可区分。** 所以凭据拒绝只能针对本机**真实存在**的目录断言。

门禁因此：探测 `~/.ssh`、`~/.aws`、`~/.gnupg`、`~/.config/gh` 中实际存在的那些，
并在**一个都不存在时明确失败**：

```ruby
if probed.zero?
  raise "no credential directory exists on this host, so credential denial was not " \
        "exercised; this gate cannot report a pass here"
end
```

在那种机器上它证明不了任何事，报绿就是撒谎。这是「不做静默截断」那条规则的直接应用。

本机上探到 2 个（`~/.ssh` 9 项、`~/.config/gh` 2 项），门禁输出 `credential directories exercised: 2`。

## 门禁断言

与机器无关的部分：

| 断言 | 为什么 |
| --- | --- |
| 审批针对 `shell.exec`，且载荷里带着被审命令的原文 | 审批者必须看得见自己在批什么 |
| `key=[unset]` | Worker 环境（Provider 凭据、DB 口令、NATS 凭据）没有到达模型作者的命令 |
| `tmp_write=DENIED` 且 `/tmp` 标记文件不存在 | 写不出工作区 |
| `home` 以 `.agent-home` 结尾且含租户 id | `~` 不是真实家目录 |
| `cwd` 含租户 id | 命令起点是工作区 |
| `tool_states == ["completed"]` | 恰好执行一次 |
| `tool_sandboxes == ["trusted_native"]` | 没有跑到容器外 |
| 两轮 `advertised_tools == ["shell.exec"]`、`file_tools_leaked` 为假 | 三方交集：只授 shell 就只看得到 shell |
| 收尾端口全关、local root 消失 | 不留开发垃圾 |

依赖本机的部分：每个存在的凭据目录，`list` 与 `read` 都必须 `DENIED`。

两条**防假绿**的对照：

- `workspace_write=ok` —— 若连自己的工作区都写不了，上面那些 `DENIED` 可能只是「什么都没跑」。
- 审批载荷必须包含命令原文（`probe_done`），否则「审批发生了」不等于「审批的是这条命令」。

凭据访问全部重定向到 `/dev/null`，只输出判定字符串。即使容器失效，也不会有任何凭据内容
进入 stdout、事件表、日志或本文件。

## 结果

```
$ make check-native-shell-live
credential directories exercised: 2
validated the Shell Tool's containment from inside the container with complete cleanup
GATE_EXIT=0
```

## 门禁自证：它确实会失败

绿灯本身不说明问题。注入故障——在 `profile_for` 里不再发出任何凭据 deny 规则：

```
- for index in 0..denied_read_count {
+ for index in 0..0usize { let _ = denied_read_count;
```

门禁在**真实凭据目录**那条断言上开火：

```
RuntimeError: a contained command could list a real credential directory (cred0_list=READABLE)
GATE_EXIT=2
```

注意这条红是从**生产路径**得出的：Worker 起真实容器、Shell 工具在其中执行、
命令读到了本机真实存在的 `~/.ssh`。单元测试无法产生这条证据，因为它绕过 Worker。

注入代码已还原，还原后门禁重新为绿。

## 修掉的一个缺陷

首次运行在解析探针输出时崩了：

```
NoMethodError: undefined method `filter_map' for Array
```

`filter_map` 需要 Ruby 2.7，本机系统 Ruby 是 2.6.10。已改为 `map` + `compact`，
并核对了两个新文件里没有其它 2.7+ 语法。

这次失败本身是有信息的：它发生在**最后一步**，说明起栈、发布签名 Skill、绑定 AgentVersion、
建 Run、审批、Shell 工具在容器内执行、`tool.result` 回灌全部已经走通。

## 限制（明确不声称）

- **凭据拒绝这一项依赖本机存在凭据目录。** 全新的 CI 机器上门禁会失败而不是跳过——
  这是刻意的，但意味着它不能不加准备地放进 CI。
- Provider 为回环假 Provider，未接触任何真实模型厂商。
- 只覆盖一条命令一次调用。未覆盖并发命令、超时命令、fork 后脱离的后台进程。
- 未覆盖命令白名单（还没有）、交互式会话（还没有）。
- 仅 macOS。
- 门禁不验证 `sandbox-exec` 的 argv 本身，而是验证 argv **产生的效果**——
  这正是它的价值，但也意味着它无法区分「deny 规则写对了」与「别的机制恰好也拒绝了」。
  硬链接那次的教训（保护实际来自 `file-link` 未授予，而非黑名单）说明这个区别是真实存在的。

## 复现

```
make check-native-shell-live
```

无需先起开发栈；门禁自建隔离栈（独立 local root + 12 个自分配回环端口）并在收尾时清空。
