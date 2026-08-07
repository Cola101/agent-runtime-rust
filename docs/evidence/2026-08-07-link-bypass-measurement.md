# 链接是否绕过读黑名单：实测，以及一个被自己证伪的结论

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 起因

ADR-0037 的「限制」里留着一条：

> 未验证硬链接、或从沙箱外部创建的指向受保护目录的符号链接是否绕过。

这是容器化边界上唯一一条已知未验证的绕过路径。Shell 工具一旦可用，`ln -s` 就是一条命令。

**Codex 里没有任何 `file-link` 或 symlink 相关处理**（`codex-rs/sandboxing/` 全文检索无命中），
所以这件事没有可抄的答案，只能测。

## 威胁模型（决定了测试该怎么写）

工具本身就是不可信的那一方，它跑在沙箱**内部**，并且被允许在工作区里写。
所以最真实的攻击不是"外部有人放了个链接"，而是**工具自己在工作区里建链接再读它**——
完全在它已获授权的能力范围内，不需要任何外部帮手。

三条探针：工具自建 symlink、工具自建硬链接、工作区里已存在的 symlink。

## 实测结果

| 路径 | 结果 |
| --- | --- |
| 沙箱内 `ln -s` 指向受保护文件 | **创建成功**（写工作区本就允许） |
| 读该 symlink | **Operation not permitted** —— 内核按解析后的路径应用 deny |
| 工作区里预先存在的 symlink | **Operation not permitted** |
| 沙箱内 `ln` 建硬链接 | **Operation not permitted**，链接未建成 |
| 对照：symlink 指向普通文件并读 | **成功** |

**两条路径都不绕过。** 不需要改实现。

## 一个被证伪的结论

我最初把硬链接被拒解释为「`link(2)` 要访问源文件，被 deny 挡住了」。**这是错的**，
故障注入当场推翻：

```
无 deny 规则：  ln: …/ws/hard: Operation not permitted        ← 失败在目标端
有 deny 规则：  ln: …/.ssh/id_ed25519: Operation not permitted ← 失败在源端
```

真相是：**沙箱内根本创建不了硬链接**，因为 `(allow file-write* …)` 不授予 `file-link`。
这是 ADR-0036 基础 profile 就有的性质，与本轮的凭据黑名单无关。

后果是那条测试**通过的原因与它的名字无关**——它无法检测黑名单的回归，属于假绿。
已改名为 `a_contained_tool_cannot_create_a_hard_link_at_all`，注释写明真实机制，
断言改为「没有出现 HARD_CREATED」。它仍然值得留着：如果将来有人授予 `file-link`
或放宽写规则，它会响——而那恰恰是硬链接路线被打开的前提。

## 门禁自证：这些测试确实会失败

移除 deny 规则后重跑：

| 测试 | 注入后 | 判定 |
| --- | --- | --- |
| `a_contained_tool_cannot_read_a_protected_directory` | **FAILED** | 有断言力 |
| `a_contained_tool_cannot_enumerate_a_protected_directory` | **FAILED** | 有断言力 |
| `a_symlink_the_tool_plants_itself_does_not_reach_a_protected_path` | **FAILED** | 有断言力 |
| `a_symlink_already_present_in_the_workspace_does_not_reach_a_protected_path` | **FAILED** | 有断言力 |
| `following_a_symlink_to_an_ordinary_file_still_works` | ok | 正确，对照本就不该受影响 |
| 硬链接那条 | ok | **暴露了上面那个问题**，已改写 |

注入代码已还原，还原后 15 个用例全绿。

## 顺手修掉的一个隐含契约

写测试时控制用例先红了，原因是我的 `run_contained` 用了 `ReadOnly` profile，
工作区根本不能写。追下去发现一个真实问题：**`wrap_launch` 不规范化工作区路径**，
依赖调用方先 canonicalize（生产里 `lib.rs` 确实做了），但这个义务没写在签名或文档任何地方。

本轮刚栽过的坑就是「未规范化路径 = 静默失效的规则」。已把规范化移进 `wrap_launch` 内部，
不再依赖调用方自觉。

另外两条链接测试原本跑在 `ReadOnly` 下——那样它们会因为「工作区不可写」而通过，
又是假绿。已全部改为 `ReadWrite`，让工作区真的可写，被拒才说明是链接本身被拒。

## 检查结果

```
cargo test -p agent-tool-runtime --lib   15 通过 / 0 失败
cargo fmt --check                        通过
cargo clippy --workspace -D warnings     通过
cargo test --workspace                   261 通过 / 0 失败（上轮 257，+4 为本轮新增）
```

## 证据边界

| 命题 | 状态 |
| --- | --- |
| 工具自建 symlink 无法读到受保护路径 | **已证**——真实 `sandbox-exec`，且确认链接确实建成了 |
| 工作区内预存 symlink 无法读到受保护路径 | **已证** |
| 沙箱内无法创建硬链接 | **已证**，但保护来自基础 profile 不来自黑名单 |
| 链接解析本身未被整体打坏 | **已证**——对照用例读到 `ordinary-content` |
| 上述测试确实能失败 | **已证**——故障注入四红一绿，符合预期 |
| 本轮未做真实主链运行 | 实现未改（只改了测试与路径规范化位置），全量 261 绿 |

最后一行不含糊：本轮**没有**跑 `make dev` 真实 Run。实现层唯一改动是把工作区规范化
从调用方移进 `wrap_launch`，生产调用方原本就传规范化路径，行为等价。

## 空 `.local` 残留：第二次出现，仍未归因

上一轮结束时仓库根出现零文件的 `.local/cache 3`，当时未归因。本轮结束时又出现零文件的
`.local/run`（`.local` 0B，`find -type f` 计数为 0）。

已排除的：

- Rust 侧无任何代码写 `.local`（全仓检索 `runtime/` 的 `.rs` 无命中）。
- `runtime-host` 的 `state_root()` 强制要求 `AGENT_RUNTIME_LOCAL_STATE_ROOT`，无 `.local` 默认值。
- `devctl clean` 不调用 `prepare_directories`，所以不是「清理时又建回来」。
- 删掉后单跑 `cargo test -p agent-tool-runtime --lib` **未重现**。

创建这些目录的只有 `devctl:78` 的 `prepare_directories`，它由 `prepare_local_identity`、
`bootstrap_native_tools`、`start_infra` 三处调用——本轮都没显式跑过。

**没查到底就不写结论。** 两次都是零文件空目录，无数据、无密钥、无构建产物，
已手工删除。若第三次出现，值得挂一个 `fswatch` 抓创建者，而不是继续猜。

### 后续：第三次出现时已查明（2026-08-07 晚些时候）

不是缺陷，是既定行为。`deploy/native/run-java-tests:58`：

```sh
AGENT_RUNTIME_LOCAL_ROOT="$TOOLCHAIN_ROOT" "$DEVCTL" bootstrap
```

`TOOLCHAIN_ROOT` 默认为 `$PROJECT_ROOT/.local`（第 10 行），即**仓库的** `.local`；
这是有意的共享工具链缓存，让 nats-server 二进制跨测试轮复用。而 `run-java-tests`
的清理（23–28 行）只清 `TEST_ROOT`，**从不清 `TOOLCHAIN_ROOT`**。

第三次出现的目录形态与之完全吻合：`cache/go` 来自 `devctl:481` 的 Go 模块缓存，
`state/{checkpoints,workspaces,postgres,nats}` 来自 `prepare_directories`（`devctl:469` 调用）。

**结论修正**：前两次记为「未归因残留」的东西是既定行为，不是缺陷。
但由此得出一条真实的判据问题——**跑过 Java 测试之后，「`.local` 已清空」不再是有效的干净性检查**，
因为共享工具链本就该留在那里。这一点此前没有写在任何地方。

## 限制（明确不声称）

- 未测试通过 `/dev/fd`、`/proc` 类路径、或已打开的文件描述符继承来绕过。
- 未测试挂载点、firmlink、APFS 快照路径。
- 未测试大小写不敏感文件系统上的大小写变体（`~/.SSH`）。
- 仅 macOS。Linux 无等价物。
- 机密性仍只覆盖四个凭据目录。

## 复现

```
cd runtime && cargo test -p agent-tool-runtime --lib
```
