# 审批策略从 Worker 常量移到租户配置

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 起因

复审给的 P1：

> 自动审批是 Worker 硬编码，并非租户策略。所有获得 Shell 权限的租户都会自动套用该豁免，
> 控制面和租户管理员无法关闭。

这条成立，而且比表面更严重——它不只是「不方便配置」，是**一个本属于租户的安全决定被放在了
租户看不见也够不着的地方**。

## 改成的链路

```
租户 → POST /v1/agents/{id}/versions  tool_approval_policies
     → agent_versions.spec（jsonb，与 delegated_scopes 同处，无需迁移）
     → Scheduler 投影进 RunExecution v8
     → 内核从命令取策略并应用
```

`auto_approval` 已从 `ToolDescriptor` **彻底移除**。留着它就是两个真相来源，
而 Worker 那一份正是错的那个。

## 「缺省即问」在每一层各自成立

不是只在一处判断，而是三层各自独立地把「没说」读成「要问」：

| 层 | 缺省行为 |
| --- | --- |
| API 记录 | 缺失的 map 兜底为空 |
| 命令契约 | 工具不在 map 里即视为受门禁 |
| 内核查找 | 查不到回落 `Never` |

任何单独一层被绕过，安全解读仍然成立。

## 四条守住边界的规则，各有测试

| 规则 | 为什么 |
| --- | --- |
| 未知策略值**解码即失败**，不退化为默认 | 一个拼写错误否则就是一次授权 |
| 声称 pre-v8 却携带 v8 字段一律拒收 | 否则降级命令能把豁免偷渡过一个自认在讲无策略契约的 Worker |
| 策略必须指向该 AgentVersion **真正委派**的工具 | 否则是错误，或是「等 scope 一变就自动生效」的预授权 |
| **子代理无条件拿到空 map** | 角色级豁免是另一个没人做过的决定；继承父级会让子代理拿到它自己那个角色从未被审过的东西 |

最后一条用 dispatch 测试里的断言钉住，不是写注释。

## 一个错误被以正确方式暴露

四处 SQL 投影要和四处行读取对齐，我第一遍漏了两处。集成测试以 **44 个 SQL 语法错**报出来，
而不是静默变成空 map。这是这类错误该有的暴露方式——如果列缺失时退化为「无策略」，
它会一直安静地正确到某天有人真的配了策略为止。

另有一处控制器测试的打桩按旧参数签名写的，参数一变就不再匹配、返回 null，也拦下了我。

## 一处刻意的容错

策略列读不出来时退化为空 map 而非抛异常。理由：读不懂的字段其安全解读是「无豁免」，
而为一个**只会移除审批**的字段让整个 dispatch 失败，是错误的失败方向。

## 检查结果

```
Java（run-java-tests）      146 通过 / 0 失败 / 1 跳过
Rust（cargo test --workspace）297 通过 / 0 失败
OpenAPI 资源配置契约         通过
OpenAPI 审批契约             通过
```

## 明确不声称

- **没有任何地方开启豁免。** Shell 在任何位置都不声明策略，每条命令仍逐次审批。
  本轮做到的是「让该配置的人能配置」，不是「打开它」。
- 重新打开还需要一份经得起复审的命令名单，而那份名单**目前不存在**——
  上一份把 `git branch -D`、`uniq in out` 等五条可写命令判成了只读。
- 本文件记录的是代码与测试；策略从 API 一路到 Worker 的**真实运行验证**见下方一节，
  若该节为空则表示尚未完成，不得据本文件声称已验证。

## 真实运行验证

全栈（PostgreSQL + NATS + Java 控制面 + Rust Worker + Gateway）上实跑。

**正向**：经真实 API 创建带策略的 AgentVersion（HTTP 201），落库为

```
agent_versions.spec->'tool_approval_policies'
  = {"shell.exec": "provably_read_only_shell_command"}
```

建 Run 后，Scheduler 下发的 Outbox 事件：

```
run.execution.requested | schema_version 8 |
  {"shell.exec": "provably_read_only_shell_command"}
```

租户在 API 写的那条策略，**原样**出现在 schema v8 的执行命令里。

**负向**（两条都在真实栈上）：

| 请求 | 结果 |
| --- | --- |
| 策略指向未委派的 `shell.exec` | **HTTP 400**，`detail` 为「tool approval policy names a tool this AgentVersion does not delegate: shell.exec」 |
| 策略值为 `trust_me` | **HTTP 400**，`detail` 为「unsupported tool approval policy: trust_me」 |

## 顺带修的一个缺陷：输入错误返回 500

上面两条负例**第一次实跑时返回的是 500**，校验生效了但状态码是错的。
`IllegalArgumentException` 在 `ApiExceptionHandler` 中**从未被映射**——
这意味着**所有**资源配置的输入校验都以服务端故障的形式返回，
既让客户端去重试一个永远不会成功的请求，也把一次参数校验记成了一起事故。

这与 SSE 那轮的 `EventCursorNotFound` 是同一类缺陷、同一个文件。先写 RED
（打桩抛 `IllegalArgumentException`，断言 400 与问题类型），确认异常冒泡成 500，
再加映射：400 + `urn:agent-runtime:problem:resource-input`，`detail` 用服务自己的消息——
那些消息本来就是写给调用方看的。

重新构建控制面后在真实栈复验：500 → **400**，且错误信息可直接用于排错。
