# 完整云端链路 × 真实厂商 × 模型自主决策

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 我先前报的那个「阻塞」不存在

上一轮我说云端链路卡在 `nats-server` 构建（`proxy.golang.org` 连接被重置），
并把「开系统代理还是换 Go 模块镜像」当作需要用户决定的事推了出去。

**那是我没读完脚本。** `devctl` 本来就有第二条获取路径：

```
AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD=archive
```

它从 GitHub releases 取官方预编译二进制，并校验**钉死在脚本里的 SHA256**（`devctl:516-519`）。
一次成功。

这条路径在供应链上**不比源码构建弱，反而更强**：SHA256 是钉死的常量，
而 `go install` 依赖 `proxy.golang.org` 与 `sum.golang.org` 两个在线服务的当时状态。

顺带澄清：Go **不是本项目的技术栈**，全仓零行 Go 代码。它唯一的用途是把 NATS
（Go 写的消息代理）从源码编译出来。

**已修**：`bootstrap_method` 默认 `auto` 原本**优先选 Go**。在这台机器上 Go 三次全败
（连接重置、unexpected EOF、十分钟挂死）、archive 一次成功，这个默认顺序是反的。
现在 `auto` 优先 archive，Go 路径保留且可用 `AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD=go` 显式选择。

先写了会失败的测试：默认路径下放一个必然失败的假 `go`，若被调用则测试报错。
RED 时它确实被调用、整条 bootstrap 挂掉；修后不再触及。
另有一条既有测试原本依赖「`auto` 会选 Go」，已改为显式声明——它真正要测的是
Go 路径把缓存保持在项目状态内，不是默认选谁。

## 本轮验证的内容

此前所有主链证据用的都是**回环假 Provider**——我写死它返回什么工具调用、什么内容。
这是第一次由**真实厂商的模型自主决定**要不要调工具、调哪个、参数是什么。

链路：PostgreSQL + NATS JetStream + Java 控制面/Scheduler + Rust Worker + Model Gateway + Console。
厂商：`deepseek-v4-flash`，经 `https://ai.ctaigw.cn/v1/chat/completions`，`openai_compatible` 协议。

模型策略绑定已从数据库确认，确实指向该真实端点，不是回环。

Run `27776e73-b6ec-49af-8adc-6e247bee3775`，
Skill `638a098e-e575-4cc1-8420-7cfeb99d6526`，
AgentVersion `010bd3af-72f5-43f3-b592-2daea95f221e`（**只授予 `tool:shell.exec`**）。

## 模型自己写的四条命令，以及白名单的判定

| 序号 | 命令 | 结果 |
| --- | --- | --- |
| 19 | `ls -la` | **自动放行** |
| 25 | `cat README.txt` | **自动放行** |
| 26 | `cat data.txt` | **自动放行** |
| 35 | `find . -type f -name ".*" -o -type f \| head -20` | **停在审批** |

第 35 条之前的 `approval.required` 计数为 **0**——ADR-0039 的白名单在生产路径上生效。
第 35 条被拦是因为 `find` 不在名单；`head` 虽在名单也救不了它，因为规则是
**链中每一段都必须各自通过**。

四条命令全部是模型自己写的，不是我构造的输入。

## 终态

```
status            succeeded
last_sequence     163
finished_at       非空
tool_executions   4 条，全部 completed，各恰好一次
approval.required 1 次（共 4 次工具调用）
```

模型的最终答复内容正确：它读出 `data.txt` 的 `alpha/beta/gamma/delta`，
与我预置的文件内容逐行一致；也读出了 `README.txt` 的固定文案。

## 一个附带观察（未深究，不夸大）

事件 25 与 26 是**同一轮里的两个 `model.tool_call`**——模型在一次响应中请求了两个工具，
两个都被执行（事件 28-30、31-33）。

我此前把「Tool 并行调度」记为「Codex 有、我们没有」的差距项。
这次说明**模型层面确实会一次发多个工具调用，而我们的 Worker 执行了它们**。
但我**没有验证**这两次执行是并行还是顺序的，也没有验证有无并发上限或调度策略。
在验证之前，不能据此声称并行调度已实现。

## 限制（明确不声称）

- 只跑了一次，一个厂商，一个模型。协议转换在**其他厂商**（Anthropic Messages、
  OpenAI Responses）上仍只有契约测试，无真实厂商证据。
- 未验证真实厂商下的故障转移、限流、认证失败分类。
- 未做强杀恢复——本轮聚焦真实厂商与白名单，恢复能力的证据来自此前用假 Provider 的轮次。
- 上述并行工具调用的观察未经验证。
- 本轮消耗了真实厂商的额度（4 次工具调用 + 若干轮模型请求）。

## 复现

```
AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD=archive deploy/native/devctl bootstrap
AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD=archive make dev     # 读 .local/secrets 里的真实凭据
# 发布声明 shell.exec 的签名 Skill → AgentVersion 只授 tool:shell.exec → 建 Run
# 观察 run_events：model.tool_call 与 approval.required 的对应关系
```

凭据仅存在于 `.local/secrets`，未进入命令行、日志、本文件或仓库。
