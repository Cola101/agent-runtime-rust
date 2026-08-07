# SSE 事件续传的独立验证，以及由此发现的游标错误映射缺陷

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 为什么单独跑这一项

`2026-08-07-native-contained-write-e2e.md` 的「限制」一节写明：

> 我的 SSE 捕获超时截断，事件证据取自数据库而非 SSE；SSE 续传能力本轮**未**独立验证。

那次用 `curl --max-time 300`，而 `--max-time` 截断与「续传正确」在输出上无法区分——短捕获既可能是成功断点，也可能是超时。
本轮换成一个**只因收满 N 条才停止**的客户端，使短捕获成为真实断连，从而让这一项可判定。

代码未变，所以本轮不重跑那 18 项；只补这一项，并修掉过程中发现的缺陷。

Docker、虚拟机、Kubernetes：未使用。未注册任何 Homebrew Service。

## 续传契约（源码）

| 位置 | 事实 |
| --- | --- |
| `RunEventController.java:29` | `Last-Event-ID` 以 **事件 UUID** 而非序号接收 |
| `RunEventStreamService.java:51` | 打开时先 `replay`，游标为空则从头 |
| `JdbcRunEventRepository.java:28` | UUID 经 `findSequence` 解析成序号 |
| `JdbcRunEventRepository.java:33` | `sequence > ? order by sequence`，租户 + Run 双重限定 |
| `JdbcRunEventRepository.java:66` | 未知游标抛 `EventCursorNotFound`，不静默从头重放 |

已有覆盖仅两处：`RunEventControllerTest` 验证请求头被透传，`JdbcRunRepositoryIntegrationTest`
的 `eventReplayStartsStrictlyAfterLastEventId` 验证仓储层。**真实 HTTP 客户端断连重连全仓无覆盖。**

## 真实运行

Run `3d3e11da-dab4-4a37-a825-543551c050c9`，签名 Skill `b3840a11-c876-44d1-af65-741766ce87d3`，
AgentVersion `9e45e203-2a75-4b25-b6a9-082ed643f38c`（只授予 `tool:workspace.write`）。

Run 在 `approval.required` 处停住，这是一个**确定性**的断连窗口：不批准就不会再有事件。

| 阶段 | 动作 | 结果 |
| --- | --- | --- |
| 捕获 A | 不带游标，收满 3 条后 `socket.close` | 3 条，`stop_reason=client_disconnected_after_3` |
| — | 批准 `019fd95d-006c-7cc2-ad99-b0fce90d7bf9` | 断连之后才产生事件 6–11 |
| 捕获 B | `Last-Event-ID` = A 末条 | 8 条，止于 `run.succeeded` |

数据库权威序列（11 条）：

```
 1 run.started        5 approval.required      9 model.output.delta
 2 model.usage        6 run.resumed           10 model.usage
 3 model.tool_call    7 tool.execution.started 11 run.succeeded
 4 model.turn.completed 8 tool.result
```

A 收 1–3，B 收 4–11。逐条比对结果：

```
捕获的每个事件 id 都在数据库中存在        PASS
A 是主动断连而非超时                    PASS
B 以终态事件收尾                        PASS
B 确实带了 Last-Event-ID                PASS
A 的事件与数据库前 N 条逐条相同           PASS
无重复：A ∩ B 为空                      PASS
无缺失：A ∪ B == 数据库全量              PASS
续传严格接续：B 首条 == A 末条 + 1        PASS
序号连续无空洞                          PASS
B 覆盖了断连之后新产生的事件              PASS
```

## 两个负对照

单看上表不足以定论——若服务端**忽略**游标而每次从头重放，B 仍会以终态收尾。故补：

| 对照 | 预期 | 实测 |
| --- | --- | --- |
| 同一 Run，**不带**游标重连 | 全量 11 条 | **11 条** |
| 同一 Run，**带**游标重连 | 仅 8 条 | **8 条** |

11 与 8 的差恰为 A 已收的 3 条。收窄确实由游标造成，而非其他原因。

## 过程中发现的缺陷（已修）

第二个负对照——未知游标——返回 **HTTP 500，空 body**。

`EventCursorNotFound` 在 `ApiExceptionHandler` 中**从未被映射**，而同文件里
`RunNotFound`、`RunTargetNotFound`、`ApprovalNotFound` 等同类异常全部有映射。这是遗漏而非设计选择。

后果不止状态码难看：浏览器 `EventSource` 把 5xx 视为服务端故障并**自动重连，且重发同一个
`Last-Event-ID`**。游标一旦失效（事件被裁剪、跨 Run 复制、客户端持久化了陈旧值），客户端会永久重连，
形成对控制面的持续压力。这是可用性缺陷。

先写 RED：

```
RunEventControllerTest.unknownLastEventIdIsRejectedAsClientErrorSoEventSourceStopsReconnecting
→ jakarta.servlet.ServletException: ... EventCursorNotFound   （未映射，冒泡成 500）
```

修复：`ApiExceptionHandler` 补 `@ExceptionHandler(EventCursorNotFound.class)` → **404** +
独立问题类型 `urn:agent-runtime:problem:event-cursor`。用独立类型而非通用 `:run`，
是因为客户端需要区分「游标过期，丢掉游标从头读」与「Run 不存在，停止」——补救动作不同。

真实栈复验（注意：`supervisor restart` **只重启不重建**，必须先 `mvn package`，
否则会拿旧 jar 误判修复失败——我第一次正是这样）：

```
修复前  HTTP 500  （空 body）
修复后  HTTP 404  {"title":"Event cursor was not found",
                   "type":"urn:agent-runtime:problem:event-cursor","status":404}
```

回归：修复后正常续传仍为 8 条并止于终态。

Java 全量（`deploy/native/run-java-tests`，自行分配端口起独立 PG/NATS）：
**Tests run: 143, Failures: 0, Errors: 0, Skipped: 1 — BUILD SUCCESS**。

一个操作陷阱记在这里：直接 `mvn -o test` 会有 20 个 `NativeIntegrationEnvironment`
初始化错误，因为集成测试需要 `SPRING_DATASOURCE_URL` 等由 `run-java-tests` 注入的变量。
那 20 个错误是调用方式错误，不是回归。另外 `cmd > log; echo $?` 后再接 `grep`，
整体退出码取的是 `grep` 的——我据此一度误读成"绿"。

## 顺带确认（非本轮新增，但同一 Run 上成立）

```
Provider 侧      turn1 advertised_tools=["workspace.write_text"] tool_result_messages=0
                 turn2 advertised_tools=["workspace.write_text"] tool_result_messages=1
Tool 账本        call_sse_resume_1 | completed  （单行，恰好一次）
终态             succeeded | last_sequence=11 | finished_at 非空
容器内写入        -rw------- 23 字节 sse-resume.md → "sse resumption verified"
```

## 资源占用

| 进程 | RSS |
| --- | --- |
| control-plane | 273.8 MiB |
| postgres（含子进程） | 97.8 MiB |
| console | 39.2 MiB |
| nats | 22.2 MiB |
| runtime-worker | 16.6 MiB |
| model-gateway | 13.2 MiB |
| checkpoint-gateway | 4.3 MiB |
| **合计** | **0.46 GiB** |

按 pid 文件逐个测量并计入子进程；`ps | awk` 按进程名聚合会混入 Maven 与无关 node 进程，不可用。

## 限制（明确不声称）

- Provider 为回环假 Provider，未接触任何真实模型厂商。
- 只验证了**一次**断连重连。多次连续断连、以及 30 分钟 `STREAM_TIMEOUT` 到期后的续传未验证。
- 未验证跨租户游标（拿 A 租户的事件 id 去 B 租户的 Run 续传）；`findAfter` 有租户限定，但无实跑证据。
- 未验证事件被裁剪后的续传行为——当前无裁剪机制。
- SSE 数据载荷**不含序号**，顺序只由 `id:` 表达。本轮靠事件 id 回查数据库得到序号；
  客户端若要自行检测空洞，目前做不到。是否把序号放进载荷未决。

## 复现

```
deploy/native/devctl bootstrap
AGENT_RUNTIME_PROVIDER_ENDPOINT=http://127.0.0.1:PORT/v1/chat/completions \
AGENT_RUNTIME_PROVIDER_MODEL=... AGENT_RUNTIME_PROVIDER_API_KEY=<本地生成> \
AGENT_RUNTIME_PROVIDER_PROTOCOL=openai_compatible make dev
# 发布签名 Skill → 绑定 AgentVersion(仅 tool:workspace.write) → 建 Session/Run
# 停在 approval.required → 客户端收 3 条后 close → 批准 → 带 Last-Event-ID 重连
make dev-clean
```

密钥仅存在于 `.local/secrets` 与会话临时文件，未进入命令行、日志、本文件或仓库。
