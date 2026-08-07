# SSE 续传固化为常驻门禁

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 为什么要固化

`2026-08-07-sse-event-resumption.md` 是一次性手工验证。08-02 那批证据就是这样过期的：
代码继续变，证据留在原地，下一个人无从判断它还成不成立。

续传是唯一一条**必须真断连才能证明**的路径。更便宜的层次全都通过而缺陷仍在：

- `RunEventControllerTest` 只证明请求头到达了 service；
- `JdbcRunRepositoryIntegrationTest#eventReplayStartsStrictlyAfterLastEventId` 只证明仓储查询。

两者都没能发现「未知游标返回 500」——那个缺陷是靠真实断连才暴露的。

## 门禁

```
make check-native-sse-resumption-live
→ deploy/tests/native_sse_resumption_live_test.rb
```

自建隔离栈：独立 local root、12 个自分配回环端口，与开发栈互不干扰。断言：

| # | 断言 | 为什么需要 |
| --- | --- | --- |
| 1 | 捕获 A 的结束原因是 `client_disconnected` | 超时截断与成功断点在输出上无法区分 |
| 2 | 捕获 B 止于终态事件 | 续传必须能走完 |
| 3 | **A ∩ B 为空** | **决定性**：从头重放同样会以终态收尾 |
| 4 | A + B == 数据库全量且顺序一致 | 不丢事件 |
| 5 | 数据库序号连续无空洞 | 权威日志自身完整 |
| 6 | B 长度 > 1 | 覆盖了断连之后新产生的事件 |
| 7 | 不带游标重放 == 全量，且长度 == A+B | 负对照：收窄确由游标造成 |
| 8 | 未知游标返回 404 且**非 5xx** | `EventSource` 遇 5xx 会带同一坏游标无限重连 |
| 9 | 问题类型为 `urn:agent-runtime:problem:event-cursor` | 客户端要能区分「游标过期」与「Run 不存在」 |
| 10 | 收尾时端口全关、local root 消失 | 不留开发垃圾 |

Run 停在 `approval.required` 处才开始捕获，这样断连时**后续事件尚不存在**，
续传覆盖的是真正的新事件而非缓冲区里的旧数据。

## 结果

```
$ make check-native-sse-resumption-live
validated SSE event resumption across a real disconnect with complete cleanup
GATE_EXIT=0
```

## 门禁自证：它确实会失败

绿灯本身不说明问题——恒绿的门禁等于没有门禁。注入故障：让捕获 B **不发**游标，
等价于服务端忽略 `Last-Event-ID`：

```
- capture_sse(api, run_id, token, last_event_id: cursor, ...)
+ capture_sse(api, run_id, token, last_event_id: nil, ...)
```

正是那条决定性断言开火，并指名了被重发的事件：

```
RuntimeError: Last-Event-ID was not honoured; the resumed stream re-delivered
  ["019fd9c1-fc54-7190-a473-b519f008acdb",
   "019fd9c1-fdf1-7c93-9049-c81545f957f4",
   "019fd9c2-0125-7753-9c78-728f0b207704"]
GATE_EXIT=2
```

三条正是捕获 A 已收的那三条。注入代码已还原，还原后门禁重新为绿。

## 修掉的两个缺陷

两个都是我起草这个测试时引入的，都由真实运行暴露：

**一、构建被算进了总超时。**
`supervisor clean` 会删掉共享构建产物（`devctl:680`），所以每次跑都是冷构建——
Rust workspace + 控制面 + Console 全量编译。我最初把 900s 超时套在构建外层，跑不完就被判超时。
超时的用途是抓**停止推进的 Run**，不是和编译器赛跑。构建已移出超时，
`Timeout.timeout(300)` 只约束运行逻辑，与三个现有 live 测试同口径。

**二、硬编码了一个不可达的下载代理。**
我照抄了 `native_one_command_run_live_test.rb:81` 的
`AGENT_RUNTIME_DOWNLOAD_PROXY => "http://127.0.0.1:10808"`。

读 `with-download-proxy:10-14` 才发现，这个变量一旦设值就**整体覆盖**系统代理探测：

```sh
DOWNLOAD_PROXY=${AGENT_RUNTIME_DOWNLOAD_PROXY:-}
case "$DOWNLOAD_PROXY" in
  direct|off) DOWNLOAD_PROXY=""; return ;;
  ?*) return ;;          # ← 设了值就到此为止，不再看系统代理
esac
```

本机系统代理 `HTTPEnable:0`、10808 不可达，于是：

```
go: ... proxyconnect tcp: dial tcp 127.0.0.1:10808: connect: connection refused
supervisor start failed (1)
```

正确做法是**不设**这个变量，让 wrapper 自己解析（有系统代理就用，没有就直连）。
这正是「使用 `with-download-proxy`，不要在各模块重复实现代理」那条规则的字面要求——
在测试里钉死一个端点就是重复实现。

## 一个未验证的静态发现

同一硬编码还出现在：

- `deploy/tests/native_one_command_run_live_test.rb:81`
- `deploy/tests/native_dev_bootstrap_test.rb:73`

凡是 10808 不可用的机器，它们应当在同一处失败。**本轮没有实跑这两个测试**，
所以这是源码读出来的推断，不是运行结论；修法是同样删掉那一行，未做。

## 一处未归因的空残留

四次门禁跑完后，仓库根出现 `.local/cache 3`——**零文件的空目录**，名字带 macOS
同名冲突的 " 3" 后缀。它躲过清理是因为 `devctl clean` 要求 `.agent-runtime-local-root`
标记文件，无标记就拒绝清理该目录（`devctl:672`）。

来源没有查清。`devctl:481` 的 `$LOCAL_ROOT/cache/go/...` 在隔离测试里应当落在临时目录而非仓库根，
所以静态阅读解释不了它。**不编造原因**，如实记为未归因残留；已手工删除，仓库回到 6.4M。

若后续再次出现，值得查的方向是：谁在 `AGENT_RUNTIME_LOCAL_ROOT` 未生效的情况下调用了 `devctl`。

## 限制（明确不声称）

- Provider 为回环假 Provider，未接触任何真实模型厂商。
- 门禁只覆盖**一次**断连。多次连续断连、以及 30 分钟 `STREAM_TIMEOUT` 到期后的续传未覆盖。
- 未覆盖跨租户游标（拿 A 租户的事件 id 去 B 租户的 Run 续传）。
- 未覆盖事件裁剪后的续传——当前无裁剪机制。
- SSE 数据载荷仍不含序号，顺序只由 `id:` 表达；门禁靠回查数据库得到序号，
  客户端自行检测空洞目前做不到。是否把序号放进载荷未决。

## 复现

```
make check-native-sse-resumption-live
```

无需先起开发栈；测试自建隔离栈并在收尾时清空。密钥仅存在于测试临时目录，
失败诊断输出中的 Provider 密钥会被替换为 `[REDACTED]`。
