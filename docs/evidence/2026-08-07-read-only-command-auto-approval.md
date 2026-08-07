# 只读命令免审批：实现与真实模型验证

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 问题

ADR-0038 让每一次 `shell.exec` 都要人工审批，并在自己的限制一节里记了这笔账。
实际后果比那句话更重：连着批准 `ls`、`wc -l`、`git status` 的人，到第十次不会再评估内容——
要么闭眼点过，要么把闸关掉。**总是弹出的审批等于没有审批。**

## 判断依据：审批还在保护什么

ADR-0036/0037 之后，容器已经挡住：工作区外的写、全部出网、四个凭据目录的读。
剩下由审批保护的只有一件事——**用户自己在工作区里的文件**。

所以：**写不了任何东西的命令，没有留给人审的内容。**

Codex 的自动批准也是这个形状——依据是「动作被约束住」而非「名字可信」
（`core/src/safety.rs` 里 `AutoApprove` 来自补丁被限制在可写路径内）。
它的通用机制是一整套策略语言 `codex-execpolicy`（Starlark 解析器、规则匹配、网络规则、前缀规则）；
那是为它的工具面宽度服务的，这里只有一个工具、一个问题。

## 实现

策略**按工具显式声明**，默认 `Never`：

```rust
pub enum AutoApproval { Never, ProvablyReadOnlyShellCommand }
```

- Worker **不能自行决定**豁免——那等于替租户做授权决定。豁免随工具定义下发。
- 豁免写进**审批策略快照**，所以账本能看出「当时生效的是哪条豁免」，
  而不是只看到「没请求审批」。
- 只有 `shell.exec` 声明它。两个文件工具保持 `Never`。

分类器**不写 shell 解析器**。解析器必须在引号、展开、优先级、分词上全对，
而其中任何一处细微错误就是「本想拦住却放行了」。接受的文法极小：

- 分隔符 `|` `&&` `||` `;`，且**每一段**都要各自通过
- 词要么是裸词，要么整体在单引号内（sh 在单引号里不做任何展开）
- 其余 shell 字符一律问人：`> < & $ ` ( ) { } \ " ~ * ? [ ] ! #`
- 可执行文件必须是裸名，**不接受路径**——路径不等于它看起来的那个名字

名单只收「任何 flag 都到不了写能力」的命令。`sort -o`、`sed -i`、`tee`、`awk`
**刻意不收**：收了就要写逐 flag 规则，而漏掉一个 flag 就是一次静默的写。

`git` 是唯一例外（只读子命令，且子命令前不得有任何选项）——
`git -c alias.x='!rm -rf /' x` 是披着读外衣的写能力。

## 门禁自证：分类器双向都会失败

单元测试 13 条全过不足以说明问题，一个「一律判 ask」的实现也能过掉所有「必须问」的用例。
两次故障注入：

| 注入 | 预期 | 实测 |
| --- | --- | --- |
| 恒返回 `RequiresApproval` | 「允许」类用例变红 | **5 条 FAILED** |
| 只看第一个词（不检查链的其余部分） | 链式攻击用例变红 | `a_chain_is_only_read_only_if_every_part_is` **FAILED** |

第二条尤其重要：它对应 `ls; rm -rf /` 这类攻击——只看首词的分类器会放行。

内核层另有两条测试：同一条 `ls -la`，声明策略的工具走 `Execute`、未声明的走 `ApprovalRequired`；
以及策略快照确实记录了豁免。

## 真实模型验证

用**真实厂商**（`deepseek-v4-flash` 经 `ai.ctaigw.cn`）、**`ask` 同意模式**——
这样任何需要审批的调用都会停下来，能直接看出白名单放行了什么。

| 提问 | 模型自己写的命令 | 结果 |
| --- | --- | --- |
| 用 shell 数 data.txt 的行数 | `wc -l data.txt` | **无 `approval.required`**，`pending_approval=None`，直接 `tool.execution.requested → started → result`，答「3 lines」正确 |
| 用 shell 删掉 data.txt | `find . -name "data.txt" -type f 2>/dev/null` | **停在 `approval.required`**，`status=waiting_approval` |

反向那条比设计还严：模型想先找文件再删，而这条命令被拦有**三重**原因——
`find` 不在名单、含双引号、含 `2>` 重定向。任一条单独就足够，
说明拒绝路径不依赖单点判断。

两条命令都是模型自己写的，不是我构造的输入。

## 顺带扩的一处范围（明说）

`runtime-host` 此前只注册读写两个工具，没有 shell，导致白名单在真实模型上无法验证。
已把 `shell.exec` 也注册进去。两个理由：桌面路径本就该和云端有同样的工具（边界是容器，
而那是同一个容器）；不这么做，本轮成果就只有单元测试、没有真实证据。

同时修了一个顺带发现的缺陷：`input_schema` 原先按 `WorkspaceAccess` 分支，
而 shell 也是 `ReadWrite`，会拿到 `{path, text}` 这个错误的 schema。已改为按工具名。

## 检查结果

```
cargo test --workspace              289 通过 / 0 失败（上轮 274，+15）
cargo fmt --check                   通过
cargo clippy --workspace -D warnings 通过
```

## 限制（明确不声称）

- **读不再被审查。** 被豁免的命令能读工作区和大部分文件系统，而且没有人看得到它读了什么。
  这是豁免的真实代价，不是假设。要收窄需要凭据目录之外的读容器化，
  而 ADR-0037 已记录那在这一层不可行。
- 分类器不看被允许命令的参数，所以 `cat /etc/passwd` 是豁免的——被容器约束，但无人审查。
- 名单写死在代码里，无租户级配置，加一条命令要改代码并发版。
- 名单小到会让人不适：`find`、`sed`、`awk`、`sort`、双引号、通配符都要问。
  这是刻意的出错方向，但代价真实。
- 未在 `/bin/sh` 之外的 shell 上验证。
- 没有「豁免触发频率」的观测数据，因此无法判断名单是否在实践中过小。
- 云端完整链路（Java + PostgreSQL + NATS）仍因 nats-server 构建的网络问题未验证。

## 复现

```
cd runtime && cargo test -p agent-kernel --test read_only_shell_policy
# 真实模型：AGENT_RUNTIME_LOCAL_TOOL_CONSENT=ask，其余同
# docs/evidence/2026-08-07-real-model-vendor-end-to-end.md 的复现段
```
