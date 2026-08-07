# 原生签名 Skill 端到端复验（当前代码）

日期：2026-08-06
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0
目的：在 Skill Tool 摘要兼容性修复之后，用**当前代码**重跑原生主链，取代 2026-08-02 的历史基线。

Docker、虚拟机、Kubernetes：未使用。全部为 macOS ARM64 原生进程。

## 启动的原生进程

| 服务 | 端口 | 说明 |
| --- | --- | --- |
| PostgreSQL 14.22 | 54329 | 权威状态源 |
| nats-server v2.10.20 | 4222 / 8222 | 由 `devctl bootstrap` 用 `go install` 构建到 `.local/toolchain` |
| control-plane (Java 21) | 18080 / 9090 | Control API + Scheduler |
| agent-model-gateway | 18081 | |
| agent-checkpoint-gateway | 18082 | 本地文件系统 Checkpoint |
| agent-runtime-worker | 18083 | |
| console (Vite) | 5173 | |

未注册任何 Homebrew Service。8080 未被占用（`service-runner` 显式把 Worker 健康端口设为 `127.0.0.1:18083`，覆盖代码里 `0.0.0.0:8080` 的默认值）。

## 1. 动态发布签名 SkillVersion

经真实 API `POST /v1/skills:publish`：

| Skill | SkillVersion ID | artifact_digest | signing_key_id |
| --- | --- | --- | --- |
| `native-e2e-review@1.0.0` | `5704e526-389c-4478-b45b-0bede45d2a43` | `de4b687a78c17e17f903c652f0b5bf56bd89ee3a8a52fb84f9a708b9350876fd` | `local-skill-key-v1` |
| `evidence-discipline@2.1.0` | `515e559e-c024-4dd1-b937-d8a1943d5a57` | `a183a11e684729e344e5700373e38ca237dc30a5e0b0e1f6f3f4a449e78565a5` | `local-skill-key-v1` |

## 2. 签名域独立验证

用 `.local/env/native.env` 里的 Ed25519 **公钥**，经 `openssl pkeyutl -verify -rawin` 独立验签：

```
[A] agent-runtime-skill-v1.<digest> : Signature Verified Successfully
[A] 负对照 裸 digest                : Signature Verification Failure
[B] agent-runtime-skill-v1.<digest> : Signature Verified Successfully
[B] 负对照 裸 digest                : Signature Verification Failure
```

负对照失败证明域绑定真实生效，而非碰巧通过。

## 3. 不可变 AgentVersion 有序绑定

AgentVersion `47804a36-8182-49b0-8e84-42be87961663`（version 1），`delegated_scopes = ["tool:workspace.read"]`。

声明顺序刻意与 UUID 升序、创建时间序**都相反**，使 ordinal 证据不含歧义。PostgreSQL 落库：

```
ordinal | skill_name          | semantic_version | digest
0       | native-e2e-review   | 1.0.0            | de4b687a78c17e17…
1       | evidence-discipline | 2.1.0            | a183a11e684729e3…
```

## 4. Scheduler 发出 schema v7

从 `outbox_events` 取出该 Run 的执行命令：

```
schema_version   = 7
run_id           = 7bdc0086-0084-41d8-8b5e-75d5a9343293
agent_version_id = 47804a36-8182-49b0-8e84-42be87961663
delegated_scopes = ['tool:workspace.read']
skill_snapshots[0] = native-e2e-review@1.0.0    digest de4b687a…  tools ['workspace.read_text']
skill_snapshots[1] = evidence-discipline@2.1.0  digest a183a11e…  tools ['workspace.read_text']
```

快照数组顺序 = 声明的 ordinal 顺序。

## 5. 模型收到 Agent + Skill 合并指令（硬校验）

假 Provider `deploy/tests/fixtures/openai_tool_provider.rb` 要求
`system_messages == [{role: system, content: <期望值>}]` —— **精确相等且只能有一条**。
期望值按 Worker 的合并规则构造（182 字节）：

```
Review the workspace and explain evidence before conclusions.

[Skill native-e2e-review@1.0.0]
Read files before answering.

[Skill evidence-discipline@2.1.0]
Cite the file you read.
```

Provider 证据文件：

```json
{
  "requests": 2,
  "tool": "workspace.read_text",
  "path": "README.txt",
  "result_verified": true,
  "system_instructions_verified": true
}
```

任何偏差都会让 Provider 抛错并使 Run 失败，因此这是行为校验而非日志匹配。

## 6. 主链事件序列与终态

Run `7bdc0086-0084-41d8-8b5e-75d5a9343293`，SSE 实时捕获 11 个事件：

```
 1. run.started              7. tool.execution.started
 2. model.usage              8. tool.result
 3. model.tool_call          9. model.output.delta
 4. model.turn.completed    10. model.usage
 5. approval.required       11. run.succeeded
 6. run.resumed
```

- 审批经 `deploy/native/approve-local`（`allow_once`，version 2）
- `tool.execution.started` 1 次、`tool.result` 1 次 —— Tool 恰好执行一次
- PostgreSQL 权威终态：`status=succeeded`、`last_sequence=11`、`finished_at` 非空
- 单 attempt，`owner_epoch=1`

## 7. 未声明 Tool 不可用（fail-closed）

发布 `escalating-skill@1.0.0`，声明 Worker 未安装的 `workspace.write_text`，绑定新 AgentVersion 并创建 Run `bb69c0d9-0bff-4cbb-9f2e-cf536e7b3368`。

Worker 拒收：

```
level   = WARN
message = terminating rejected execution command
error   = tool configuration is invalid: Skill escalating-skill requires unavailable trusted tool workspace.write_text
```

该 Run 停在 `queued`、`last_sequence = 0`、**零事件** —— 没有任何模型调用，Tool 从未暴露。

## 8. 强杀 Worker 后的跨 attempt 恢复

`make check-native-recovery-live`（独立隔离原生栈，随机端口）通过。真实硬杀 Worker 进程组后：

| 项 | 恢复前 | 恢复后 |
| --- | --- | --- |
| attempt_id | `8134aacb-728e-4558-931b-45324cbfa6ba` | `4bf7c85f-c8ae-4d98-ae3f-31e957cc811e` |
| owner_epoch | 1 | 2 |

事件序列 13 条，含 `run.restored`、`approval.rebound`，终态 `run.succeeded`；浏览器审批（真实 Chrome）通过；安全 Provider 故障转移 2 次；Tool 仍只执行一次。该栈 RSS 421728 KiB（0.40 GiB）。

## 9. 资源占用

主栈 7 个常驻进程：

| 进程 | RSS |
| --- | --- |
| control-plane (java) | 89.1 MiB |
| console (vite) | 49.7 MiB |
| postgres + 16 子进程 | 67.7 MiB |
| nats-server | 18.4 MiB |
| runtime-worker | 17.2 MiB |
| model-gateway | 12.9 MiB |
| checkpoint-gateway | 11.6 MiB |
| **合计** | **0.26 GiB** |

低于 4GB 上限。

## 本轮修复的两个缺陷

### P1：`with-download-proxy` 在未配置代理时拒绝执行任何命令

`configure_download_proxy()` 的 `[ -n "$DOWNLOAD_PROXY" ] || return`：无参 `return` 继承失败守卫的状态 1，脚本 `set -e` 使 wrapper 以 1 退出，`exec "$@"` 永不执行。同一模式在 `command -v scutil || return` 亦存在。

该 wrapper 是所有依赖下载的唯一入口，因此在任何未运行本地代理的机器上 `make dev` 完全不可用。既有测试只覆盖 `HTTPEnable : 1`，故从未暴露。

修复：两处改为 `return 0`；`deploy/tests/native_download_proxy_test.rb` 补两个场景（系统代理关闭、显式 `direct`）。

### P2：`check-native-recovery-live` 因共享 Playwright 目录而必然失败

`playwright.native-live.config.ts` 使用 `testDir: './e2e-live'`，运行该目录全部 spec。新增 `native-steering.spec.ts` 后，未限定 spec 的恢复测试会一并加载它，而恢复测试从不提供 `AGENT_RUNTIME_LIVE_STEERING_INPUT`，在模块加载期即抛错。

`native_run_steering_live_test.rb` 已正确限定自身 spec，恢复测试未同步。修复：恢复测试改为传 `e2e-live/native-approval.spec.ts`，与既有正确模式对称。

## 已修复：无效 Skill/Tool 绑定的 Run 永不进终态，并长期占住工作区租约

**下节记录的是发现时的原始状态；该缺陷已在同日修复并经原生实跑验证，见「租约缺陷修复验证」。**

### P1（原始现象）

第 7 节的 Run `bb69c0d9` 被 Worker 正确 fail-closed 拒收，但**从未进入失败终态**。调度器持续重投：

```
run.status   = queued（自 16:59:21 起）
dispatches   = 26 次并持续增长
owner_epoch  = 27
workspace    = 44444444-…  state = leased（租约每 30 秒续期）
```

`JdbcRunRepository.ensureAuthorizedTarget` 要求 `w.state = 'ready'`，因此该工作区**后续所有 Run 一律返回 HTTP 404**
`the requested run target is not available to the authorized application`。

即：一个声明了不可用 Tool 的 AgentVersion，可以让整个工作区拒绝服务。缺少毒丸消息处理 / 最大重试次数 / 拒收即终止的路径。

影响：发现当时的 delegated-scope 收窄实测因此无法完成（见下）。

## 租约缺陷修复验证

### 根因

Rust Worker 拒收执行命令时只对 JetStream 消息 `AckKind::Term`（`runtime/apps/worker/src/lib.rs`），**不向控制面回报任何东西**。控制面因此只看到"从未被接受的 dispatch"，而
`JdbcSchedulerRepository` 的 `"requested"` 分支**无条件重排队且无次数上限**：

```java
if ("requested".equals(dispatch.state())) {
  update runs set current_attempt_id = null ... status = 'queued'
  insertRunQueuedOutbox(...);
  return ReconcileOutcome.REQUEUED;
}
```

对比之下，**已被接受过**的 dispatch 丢失时会走 `run.indeterminate` 终态；唯独从未被接受的没有终止路径。

### 修复

对"从未产生过任何事件（`last_sequence = 0`）"的 Run 设重投上限 `MAX_UNACCEPTED_DISPATCH_ATTEMPTS = 5`。超限时发 `run.failed` 终态事件、置 `finished_at`、并**只释放本 dispatch 那一代租约**（匹配 owner/epoch/fencing，避免抢走别的 Run 合法接管的工作区），再把工作区置 `ready`。

`last_sequence = 0` 是精确判据：真实恢复场景（Worker 崩溃后重启）一定已发出事件，不会被此路径影响。

`ReconcileResult` 新增 `failed` 计数，与 `indeterminate`（有副作用且不可安全重放）语义分开；保留原有 2 参与 3 参构造器，既有断言全部不受影响。

### 先 RED 后 GREEN

新增 `runNoWorkerEverAcceptsFailsTerminallyInsteadOfBeingRequeuedForever`（真实 PostgreSQL 集成测试）。

- 修复前：`Tests run: 140, Failures: 1, Errors: 24` —— 失败信息 `[a Run no Worker ever accepts must stop being requeued] Expecting actual: "queued"`
- 修复后：`Tests run: 140, Failures: 0, Errors: 24`

24 个 Errors 修复前后**分布完全一致**（RunController 10 / ApprovalController 8 / RuntimeResource 3 / ConsoleRunTarget 2 / RunEvent 1），全部是 `@WebMvcTest` 在 Spring 上下文加载期因
`MockitoException: Could not modify all classes` 失败，与本次改动无关。**该问题已在同日单独修复，见「Java 工具链固定」。**

## Java 工具链固定（24 个控制器测试的真实根因）

### 根因不是 Mockito

表层是 `MockitoException: Could not modify all classes [... , class java.lang.Object]`，
但最深层是：

```
Starting ConsoleRunTargetControllerTest using Java 25.0.2
Caused by: java.lang.IllegalArgumentException: Java 25 (69) is not supported by the current
version of Byte Buddy which officially supports Java 23 (67)
```

本机存在**三方割裂**：

| 组件 | 实际 JDK |
| --- | --- |
| Homebrew `mvn`（自带 JVM） | **25.0.2** |
| PATH 上的 `java`（原生服务实际使用） | 21.0.11 |
| `/usr/libexec/java_home -V` | 只认 17.0.18 |
| `JAVA_HOME` | 未设置 |
| `control-plane/pom.xml` 声明 | `<java.version>21</java.version>` |

即**运行时服务跑在 Java 21，构建与测试却跑在 Java 25**，而 byte-buddy 1.14.19 最高支持 Java 23。
`deploy/native/run-java-tests` 从不固定 JDK，测试用哪个 Java 完全取决于包管理器最后装了什么，
与仓库声明无关。这是原生开发契约的缺陷，不是依赖版本问题。

### 修复

新增 `deploy/native/with-java-toolchain`，沿用仓库既有的 `with-download-proxy` 包装器模式：
从 `control-plane/pom.xml` 读取 `<java.version>` 作为唯一事实源，按
`AGENT_RUNTIME_JAVA_HOME` → `JAVA_HOME` → PATH 上的 `java` → `/usr/libexec/java_home -v`
→ Homebrew `openjdk@<major>` 的顺序解析，**每个候选都必须实际满足声明的主版本**（陈旧的
`JAVA_HOME` 导出无法绕过），然后固定 `JAVA_HOME` 与 `PATH` 再 `exec`。找不到匹配 JDK 时明确失败。

未升级 Mockito 或 byte-buddy：真正的问题是构建与运行时不共用一个 JDK，
换依赖版本只会掩盖割裂并让测试继续在未声明的 JVM 上跑。

### 先 RED 后 GREEN

新增 `deploy/tests/native_java_toolchain_test.rb`，断言经该入口运行的 Maven JVM 主版本
与 pom 声明一致，且包装后的 `java` 与 Maven 同版本。

- 修复前：`native Java tests run on Java 25 but the control plane declares Java 21`
- 修复后：`validated native Java toolchain pinning on Java 21`

### Java 全量结果

```
using Java 21.0.11
Tests run: 140, Failures: 0, Errors: 0, Skipped: 1
BUILD SUCCESS
```

唯一跳过项为 `NatsConnectionSettingsLiveTest`（显式可选的 live 测试）。

已接入 `make check-native-dev`。相关原生契约测试回归全部通过：
`native_java_toolchain_test`、`native_java_maven_contract_test`、`native_download_proxy_test`、
`native_java_test_lifecycle_test`、`native_command_contract_test`。

### 原生实跑验证

在带修复的栈上复现同一毒丸场景（Skill 声明未安装的 `workspace.write_text`）：

```
poison run 1a9af3b1-f26a-4c81-9eed-cd97f3fce3d0
  status = failed        dispatches = 5（原为 26 且持续增长）   finished = true
  run.failed  {"reason":"assignment_never_accepted","status":"failed","dispatch_attempts":5}
  workspace state = ready（原为永久 leased）
```

随后在**同一工作区**创建正常 Run `b4d9d424-cb84-4e32-8fa7-acfcee5ba48d`：

```
创建 Run -> HTTP 202（缺陷时期恒为 404）
事件 11 条：run.started → … → approval.required → run.resumed →
           tool.execution.started → tool.result → … → run.succeeded
Provider 校验：system_instructions_verified=true, result_verified=true
终态：run succeeded | workspace ready
```

即毒丸 Run 被有界终止、工作区释放、同一工作区的后续 Run 完整跑通。

## delegated scope 收窄的原生实证（受控 A/B）

租约缺陷修复后补做。设计为受控实验：**同一个 SkillVersion、同一段 Agent 指令、同一个 Worker、
同一个工作区、同一个探针 Provider**，两臂之间**只有 `delegated_scopes` 不同**。

- SkillVersion `1c9f353b-ab8a-415d-b567-8935c66bf315` — `scope-ab-review@1.0.0`，声明 `["workspace.read_text"]`
- AgentVersion GRANTED `1f032b63-dc9e-4ff4-9930-77ae523f9d49` — `delegated_scopes = ["tool:workspace.read"]`
- AgentVersion NARROWED `b0088231-d973-4529-a264-1d35ede8cd48` — `delegated_scopes = []`

探针 Provider 只记录它实际收到的请求，不做任何断言分支，两臂用的是同一个脚本。

| 观测项 | GRANTED（run `7178a974`） | NARROWED（run `5fbadd91`） |
| --- | --- | --- |
| `advertised_tools` | `["workspace.read_text"]` | `[]` |
| `tool_catalog_empty` | false | **true** |
| `read_text_advertised` | true | **false** |
| `system_message_count` | 1 | 1 |
| 系统指令内容 | `Review the workspace…\n\n[Skill scope-ab-review@1.0.0]\nRead files before answering.` | **逐字节相同** |
| 权威终态 | `succeeded`（last_sequence 3） | `succeeded`（last_sequence 3） |

两臂的系统指令**逐字节相同**，说明 Skill 指令照常注入、Skill 本身照常装载；唯一变化的是模型可见的
Tool 目录。即：Skill 声明了 Tool、Worker 也确实预装了该 Tool，但 AgentVersion 未授予对应 scope 时，
该 Tool **不会出现在发给模型的目录里**。Skill 无法凭声明扩权。

限制：两次运行都没有发生 Tool 调用，因此不产生 Checkpoint，本实验**未**覆盖
「Checkpoint 绑定的有效 Tool 目录摘要随 scope 收窄而变化」——该条仍只由 Rust 回归测试
`checkpoint_tool_catalog_binds_only_skill_tools_inside_the_delegated_scopes` 覆盖。

## 未完成事项

- **Checkpoint 有效 Tool 目录摘要随 scope 变化**：只有 Rust 回归测试覆盖，无原生实跑证据（见上）。
- 未执行 Console 与完整 Rust Workspace 的全量测试，不得据此声称其历史测试数仍然成立。
  （Java 全量已于本日复跑：140 tests / 0 failures / 0 errors / 1 skipped。）
- Skill 签名 key 轮换信任集、`signing_key_id` 纳入 canonical manifest、OCI/SBOM/扫描：均未实现。

## 复现命令

```
deploy/native/devctl bootstrap
AGENT_RUNTIME_PROVIDER_ENDPOINT=http://127.0.0.1:19099/v1/chat/completions \
AGENT_RUNTIME_PROVIDER_MODEL=native-e2e-model \
AGENT_RUNTIME_PROVIDER_API_KEY=<本地生成> \
AGENT_RUNTIME_PROVIDER_PROTOCOL=openai_compatible make dev
make check-native-recovery-live
make dev-clean
```

密钥、令牌、数据库口令均未写入本文件、日志或仓库。
