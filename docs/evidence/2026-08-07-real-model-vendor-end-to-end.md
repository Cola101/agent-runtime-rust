# 第一次真实模型厂商端到端

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 为什么这件事重要

在此之前，**全部**主链证据都用回环假 Provider——我写死它返回什么 tool_call、什么内容。
`openai_compatible` 协议转换的正确性只由契约测试保证，从未被真实厂商响应验证过。
这一条一直写在 `implementation-status.md` 的「尚未实现」里。

本轮由用户配置了真实厂商凭据，这条缺口第一次被填上。

## 配置（凭据本身未读取、未记录）

```
endpoint  https://ai.ctaigw.cn/v1/chat/completions
model     deepseek-v4-flash
protocol  openai_compatible
api key   仅由 .local/secrets/provider-api-key 经环境变量传入
```

## 测试的是哪条路径

**`runtime-host`**，它 `use agent_model_gateway::{…}`，与云端 Worker **共用同一份协议转换代码**。

**明确不覆盖**：Java 控制面、PostgreSQL 审批账本与事件表、NATS 分发、Worker↔Gateway 的 gRPC。
本轮无法覆盖它们，原因见下方「被阻塞的部分」。

## 真实运行

Run `019fdb66-1bf5-7e22-8af6-df46fcdb8643`，工作区里放一个事实文件：

```
Agent Runtime Platform evidence file.
The project has 38 ADRs and 5 live gates.
```

提问：`Read README.txt and tell me how many ADRs the project has.`

真实事件序列（27 条，取自 `events.jsonl`）：

```
run.started
model.output.delta ×6        "Let me start by reading the README.txt file."
model.usage                  input_tokens=312 output_tokens=57
model.tool_call              {"name":"workspace.read_text","arguments":{"path":"README.txt"},
                              "id":"call_f1429c84339f4618ba24f892"}
model.turn.completed
approval.required            binding_digest=faf1a82a… effect=pure sandbox=trusted_native
run.resumed
tool.execution.started
tool.result                  {"path":"README.txt","bytes":80,
                              "text":"…The project has 38 ADRs and 5 live gates.\n"}
model.output.delta ×11
model.usage                  input_tokens=404 output_tokens=19
run.succeeded                reason=stop
```

最终回答：

> Based on the evidence from the README.txt file, the project has **38 ADRs**.

## 这次证明了什么，为什么这个证明是硬的

| 命题 | 证据 |
| --- | --- |
| 真实厂商的流式 SSE 被正确解析 | 27 条事件、两次 `model.usage` 带真实 token 数 |
| 真实模型**自主**决定调用工具 | `model.tool_call` 的 `id` 是厂商生成的（`call_f1429c84…`），不是我写死的 |
| 工具目录以厂商能理解的形式送达 | 模型选对了工具名并给出了合法参数 `{"path":"README.txt"}` |
| 审批闸对真实调用生效 | `approval.required` 携带真实 `binding_digest` |
| **工具结果真的回到了模型** | 答案 `38` 只存在于我刚写入工作区的文件里，不在模型的先验知识中 |
| 结果绑定未串位 | `tool.result` 的 `binding_digest` 与 `approval.required` 逐字符相同 |
| token 计量对真实用量生效 | 312/57 与 404/19，两轮递增符合追加历史的形状 |

最后一行那个 `38` 是这次运行里最硬的一条：它排除了「模型凭常识答对」的可能。

## 凭据未泄漏

```
grep -rlF "<key>" <整个运行状态目录>  → 命中 0 个文件
```

key 从未进入命令行回显、stdout、事件表、checkpoint 或本文件。

## 被阻塞的部分：完整云端链路

`make dev` 起栈失败，卡在构建 nats-server：

```
go: downloading github.com/klauspost/compress v1.17.9
read "https://proxy.golang.org/…/v1.17.9.zip": unexpected EOF
read "https://proxy.golang.org/…/v1.17.9.zip": read: connection reset by peer
（第三次直接挂死，10 分钟超时）
```

三次失败，两种不同的失败模式，都指向 `proxy.golang.org` 这条链路不稳（源 IP 显示为
`198.18.0.1`，像是经过 CGNAT/转发）。系统代理当前为关闭状态（`HTTPEnable:0`），
而移交文档里写明外部下载应走 `127.0.0.1:10808`。

**没有绕过它**。可选做法各有代价，需要用户决定：

- 打开系统代理（移交文档描述的预期配置）；
- 或为 Go 设置可达的模块镜像——这会改变模块来源，对一个在意签名制品与 NOTICE 合规的项目
  来说是供应链决策，不该由我单方面做。

在此之前，云端链路（Java 控制面 + PostgreSQL + NATS）对真实厂商的验证仍然**未完成**。

## 限制（明确不声称）

- 只跑了**一次**，一个模型，一个网关。未验证多 Provider 故障转移、限流退避、
  真实错误响应（4xx/5xx）的分类，也未验证 `openai_responses` 与 `anthropic_messages` 两条协议。
- 未覆盖 Java 控制面、PostgreSQL、NATS、gRPC（见上）。
- 用的是 `allow-once` 同意模式，未走真实人工审批交互。
- 未验证长上下文、多轮工具链、并发 Run。
- `cost_micros` 为 0——该网关未返回计费信息，或转换未提取；未追查。

## 复现

```
AGENT_RUNTIME_LOCAL_STATE_ROOT=<dir> AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT=<dir> \
AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT=$(cat .local/secrets/provider-endpoint) \
AGENT_RUNTIME_LOCAL_PROVIDER_MODEL=$(cat .local/secrets/provider-model) \
AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY=$(cat .local/secrets/provider-api-key) \
AGENT_RUNTIME_LOCAL_TOOL_CONSENT=allow-once \
AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES=tool:workspace.read \
AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN=<path> \
runtime/target/debug/agent-runtime-host run "<prompt>"
```
