# MCP 第三片：网关出网、目录冻结与凭据解封

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 范围

ADR-0040 第三片。**MCP 调用真的发生了**——JSON-RPC over HTTP、`initialize` → `tools/list`
→ `tools/call`、密封凭据解封后作为 bearer 发出、超限响应被拒。

仍未做：gRPC 端点、Worker 侧接线、审批闸口。也就是**内核还调不到它**。

## 为什么放在网关而不是 Worker

网关是解封密封凭据的地方。放进 Worker 就等于把租户的 MCP 凭据交给
**正在执行模型建议的那个进程**——那是它唯一绝不能待的地方。

## 目录冻结：这一片真正的安全属性

`call_tool` 每次都重新 `tools/list`，与 Run 启动时冻结的摘要比对，不一致就拒绝。

一个服务器在 Run 中途加了 `delete_everything`，**不能在一个按旧目录批准过的 Run 里生效**。
Codex 用同一条规则（`codex-mcp/src/binding.rs`），本仓的 Checkpoint 绑定对原生 Tool 也是。

实测（真实 loopback HTTP 服务器）：

```
a_call_is_refused_once_the_server_changes_its_catalog ... ok
```

同一次调用在目录不变时成功，测试中途把服务器工具集改掉后 → `CatalogChanged`。

摘要覆盖**限定名 + 入参 schema**，不覆盖 description：
服务器会改措辞，一个 Run 因为一句话变了而失败是噪声；但入参 schema 变了意味着
工具接受的东西变了，那必须失败。两条单元用例分别钉住这两半。

## 两处「服务器说了算的字符串」被当作输入检查

| 检查 | 为什么 |
| --- | --- |
| 工具名不得含 `/` 或 `:` | 名字由**服务器**选，带分隔符会让一个工具的调用解析成另一个 |
| endpoint 必须 HTTPS 或 loopback，且不含 userinfo | 注册表已经挡过一次，但这里是真正建立连接的进程，是最后一道 |

另外禁用了 HTTP 重定向：endpoint 字段的意义就是「这台服务器只能到这一个主机」，
跟随重定向正好绕开它。

## 故障注入，以及一次没能证明想证明的事

| 注入 | 结果 | 判读 |
| --- | --- | --- |
| 去掉目录摘要比对 | `a_call_is_refused_once_the_server_changes_its_catalog` 失败 | **成立**，冻结闸口真的在起作用 |
| 把 AAD 置空 | 失败的是**正例**（密封凭据无法解开），重放用例照常通过 | **不成立** |

第二条要说清楚：我想证明的是「重放用例能抓住绑定被去掉」，但 AAD 一旦改动，
测试里用完整 AAD 密封的信封就整个打不开了，于是**两个用例都不再测原本的东西**——
正例炸掉，重放用例因为「反正也打不开」而通过。

所以本文件**不声称**重放用例经过了故障注入验证。它的效力来自 AES-GCM 的 AAD 语义，
以及「AAD 一变就整体失败」这个可观察事实（即它确实参与认证，不是摆设）。
要真正注入需要让密封方和解封方就一个更弱的 AAD 达成一致，那需要改测试本身，
改完就不是同一个证明了。

## 密钥不落盘

测试密钥在运行时生成（`OnceLock` 全套件一份），不提交 PEM 文件。
仓库里的私钥就是仓库里的私钥，「只是测试用」不改变这一点。

## 检查结果

```
Rust（cargo test --workspace）313 通过 / 0 失败
其中 MCP 联邦                  8 通过（全部对真实 loopback HTTP 服务器）
```

## 明确不声称

- **内核调不到它。** 没有 gRPC 端点、没有 Worker 接线、没有把联邦工具放进
  三方交集（Skill 声明 ∩ Worker 信任 ∩ 委派作用域）。这是下一片。
- **没有对真实第三方 MCP 服务器跑过。** 测试服务器是本仓写的 loopback HTTP，
  实现的是 MCP 的一个子集。真实服务器的差异（SSE 响应、`Mcp-Session-Id` 会话、
  OAuth）**都未验证**。
- **不支持 SSE 响应。** 只解析 `application/json`。Streamable HTTP 里服务器可以返回
  `text/event-stream`，那种服务器目前会被判成协议错误。
- **`initialize` 后没有发 `notifications/initialized`**，也没有携带服务器返回的
  `Mcp-Session-Id`。要求严格会话的服务器可能因此失败——这一条要等真实服务器实跑才能定论。
- 无审批闸口。ADR-0040 要求每个联邦工具都 `ask`，那属于内核，本片没有内核改动。
- 本地 stdio MCP 服务器不支持（ADR-0040 明写的取舍）。
