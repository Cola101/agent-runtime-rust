# 对真实第三方 MCP 服务器的兼容性验证

日期：2026-08-08
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0
被测服务器：三台，全部 Streamable HTTP 传输
- `@modelcontextprotocol/server-everything` 2026.7.4（官方参考实现）
- `mcp-archery`（用户自有，SDK ^1.24.3）
- `mcp-email`（用户自有，SDK ^1.24.3）

## 结论先说：我们的客户端此前**无法与任何符合规范的服务器通话**

在此之前所有 MCP 测试都打在本仓自己写的假服务器上——那只证明了客户端和自己一致。
换成官方参考实现之后，**第一个请求就是 406**，四处不兼容逐个暴露：

| # | 症状 | 根因 | 若不修 |
| --- | --- | --- | --- |
| 1 | `HTTP 406 Not Acceptable` | 只发 `accept: application/json` | **连不上任何符合规范的服务器** |
| 2 | `Protocol("expected value at line 1 column 1")` | 响应是 `text/event-stream`，我们按裸 JSON 解析 | 同上 |
| 3 | `HTTP 400` | `initialize` 返回的 `Mcp-Session-Id` 没有回带 | 握手之后每一个请求都被拒 |
| 4 | 无 | 没发 `notifications/initialized` | **本服务器容忍**（见下） |

前三条是 hard failure：**任何一条不修，联邦功能对真实生态就是零可用**。
之前六片的全部绿灯没有覆盖到这一层，因为假服务器是照着我们客户端的行为写的。

## 修法

- `Accept: application/json, text/event-stream`——Streamable HTTP 两种都可能回，
  客户端必须都能读。参考实现的错误消息原话就是「Client must accept both」。
- 新增 `decode_event_stream()`：从 SSE 帧里取第一个带 `result` 或 `error` 的 `data:`。
  取第一个带结果的而不是最后一行，因为流里还可能有通知和进度帧，那些不是本次请求的答案。
- `initialize` 返回服务器签发的 `Mcp-Session-Id`，后续请求带上。
  **每次调用重开一个会话**：复用会跨调用持有一份我们不掌控生命周期的服务端状态，
  而「在一个已过期会话里静默执行了」比多一次握手糟得多。
- 补发 `notifications/initialized`。它是通知没有 result，所以走单独的 `notify()` 路径。

## 逐条故障注入（对真实服务器）

| 注入 | 结果 |
| --- | --- |
| Accept 改回只写 `application/json` | `Unreachable("server answered HTTP 406")` |
| 不解析 SSE | `Protocol("expected value at line 1 column 1")` |
| 不回带 `mcp-session-id` | `Unreachable("server answered HTTP 400 …")` |
| 不发 `notifications/initialized` | **两条用例仍然全过** |

第四条**不成立**，如实记：参考服务器容忍这条通知缺席，所以本文件
**不声称**它经过了必要性验证。发它是因为规范要求，不是因为这台服务器需要。

## 测试为什么是 `#[ignore]` 而不是「环境变量没设就跳过」

跳过在汇总里**计为通过**，于是「兼容性套件通过了」在一台从未跑过它的机器上也会成立。
`#[ignore]` 不会被计入通过数。跑法写在模块注释里：

```bash
AGENT_RUNTIME_MCP_COMPAT_ENDPOINT=http://127.0.0.1:3001/mcp \
  cargo test -p agent-model-gateway --test mcp_real_server_compat -- --ignored
```

## 三台服务器，不是一台

只对一台服务器验证，证明的是「修好了那一台」。用户环境里还有两台**别人写的**
服务器（`mcp-archery`、`mcp-email`），一并测了发现路径：

```
compat: http://127.0.0.1:3001/mcp   -> 8 tools, digest d98c443032…
compat: http://127.0.0.1:17829/mcp  -> 8 tools, digest 61332f09b7…
```

对这两台同样注入验证：

| 注入 | 结果 |
| --- | --- |
| Accept 只写 `application/json` | `HTTP 406` |
| 不回带 `mcp-session-id` | `HTTP 404`（该实现对未知会话回 404 而非 400） |

**这才让「修的是协议不是某个实现的怪癖」这句话有依据。**
注意第二条两处返回码不同（参考实现 400、这两台 404），
所以我们的错误处理不能依赖具体状态码——目前也确实没有依赖。

## 只做发现，不调用工具

这两台连着真实系统（`mcp-email` 走 IMAP 接真实邮箱）。
`tools/list` 只读服务器自身，而 `tools/call` 会做那个工具该做的事。
协议兼容性需要的是前者，所以多服务器用例**只做发现**，
这一点写在测试的文档注释里而不是留给人推断。

启动前确认 3001 / 17829 空闲；用完按**进程工作目录**核对确属本会话启动的那两个再停，
没有按名字或端口盲杀。

## 顺带量到的真实目录形状

参考服务器 12 个工具，名字如 `echo`、`get-annotated-message`、`gzip-file-as-resource`。
**没有一个包含 `/` 或 `:`**，`MAX_TOOLS = 64` 也宽裕。
这两条此前只是我的假设，现在有一个真实实现的数据。

## 一个既有缺陷，以及我先前两次绿灯报告需要更正

跑全量 Java 时出现 `expiredUnacceptedDispatchIsLostAndRunIsRequeued` 失败
（`requeued=2` 而非 1）。本轮**只改了 Rust**，所以先查证：
把工作区 Rust 改动 stash 掉、在 HEAD 上跑全量——**同样失败**。

所以这是既有缺陷，而且意味着：**我在前两次汇报里说的「Java 167 全绿」
是靠用例执行顺序侥幸通过的**，那两次报告在这一点上不准确。

根因：`reconcileExpired()` 按设计全库扫描，而这条用例断言全局计数
`ReconcileResult(1, 0)`——**它实际上在断言别的用例留下了什么**。
suite 变大之后就不再成立。

改成只断言属于自己的部分：`requeued() >= 1`，加上紧随其后三条按租户限定的断言
（dispatch 变 `lost`、run 变 `queued`、outbox `run.queued` 计数为 2），
后者本来就精确证明了「这个 Run 被重排队了」。全量连跑两次绿。

**同一文件里还有九处同样形态的全局计数断言**（723、738、1239、1289、1408、1477、
1580、1655、1718 行），本轮**没有改**——它们今天没红，但带着同一个隐患。

## 检查结果

```
真实服务器兼容（--ignored）  3 通过 / 0 失败（参考实现发现+调用，三台发现）
Rust（cargo test --workspace）335 通过 / 0 失败
Java（run-java-tests）        167 通过 / 0 失败 / 1 跳过（连跑两次）
```

## 明确不声称

- **三台服务器都是 Streamable HTTP，且两台用同一版官方 SDK。**
  独立 SSE 传输、stdio 传输，以及非 SDK 实现（自己手写协议的服务器）都未测。
- **工具调用只在参考实现上验证过。** 另两台只跑了发现，因为它们连着真实系统。
- **OAuth 完全未测。** 参考服务器不要求鉴权，所以密封凭据这条路径在**真实服务器上
  从未走通过**——本轮验证的是无凭据的开放服务器。
- **会话恢复（`Last-Event-ID`）未测。** 参考服务器支持它，我们没用。
- 长连接 / 服务器主动推送的通知与进度帧只是被解析器跳过，没有被消费，
  也没有用例覆盖「跳过它们之后仍能拿到正确结果」之外的行为。
- 参考服务器由 npm 安装到临时目录，**不是仓库的一部分**，因此这条用例
  在未准备该服务器的机器上无法运行——这是 `#[ignore]` 的代价，也是它诚实的地方。
