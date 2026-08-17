# 长期规划

- 起草日期：2026-08-17
- 基线：HEAD `fc84060`，121 个 ADR，Rust 全工作区 121 个测试二进制 / 775 通过 / 0 失败 / 6 个外部 live 显式忽略
- 总体进度：70–75%

本文件回答一个问题：**接下来该按什么顺序做，为什么是这个顺序。**

## 一条主结论

**当前的主约束不是工作量，是验证环境。**

内核在这台 M1 Pro 上已经推到能证明的边界。剩下的缺口分成两类：

- 一类**本机能做完并证明**，但按项目自己的评分规则**不提高百分比**；
- 一类**能提高百分比**，但**本机无法证明**。

因此长期规划必须按**环境门槛**分段，而不是按功能清单排列。任何忽略这一点的排期都会在第二周变成"协议正确、后端 fail-closed"的又一层。

### 存量证据：8 个 ADR 卡在同一道门槛前

`ADR-0072`、`0073`、`0075`、`0076`、`0077`、`0078`、`0079`、`0080` 连续八个 ADR 完成了 Linux 资源隔离的
capability 向量、文件协议、持久身份、pre-exec membership、fd-relative 生命周期、Manager root pinning、
启动/终态生命周期和 `Starting` 崩溃恢复。

`docs/project-goal.md` 里对应的**九段结论各自以一句免责声明结尾**：`backend_not_wired` /
`生产 backend 继续 fail-closed` / `不能替代真实 Linux 验证`。

**这八个 ADR 的全部价值，压在"有没有一台 Linux"这一个条件上。** 没有任何一行新代码能替代它，
而一旦有了，这批存量会在很短时间内一次性兑现。这是整份规划里投入产出比最高的一件事，
而它不是编码任务。

## 阶段划分

### 阶段 0 — 内核诚实性收口（**已完成** 2026-08-17，ADR-0122）

**问题。** [`tool-runtime/src/lib.rs:598-620`](../runtime/crates/tool-runtime/src/lib.rs) 在非 macOS 上把
`SandboxClass::TrustedNative` **静默降级为裸执行**：没有 seatbelt，没有 landlock，没有任何替代边界。
而 [`runtime-host/src/lib.rs:2624`](../runtime/apps/runtime-host/src/lib.rs) 与 `:2688` 注册
`workspace.write_text`、`shell.exec` 和进程会话时**没有任何 `#[cfg]` 门**。descriptor 照样声明
`TrustedNative`，implementation digest 照样绑定，审批照样 `Ask`，**没有任何一处报告容器边界未生效**。

今天挡住这个洞的只有 `docs/implementation-status.md:670` 的一句话："Linux 上没有 landlock 等价物，
因此 Worker 的 Linux 路径不得注册它们"。**那是文档，不是代码。**

这属于项目自己认定最严重的一类问题：契约说谎。`protocol/src/lib.rs:3031` 给 `Federated` 单开一个
variant 的理由正是「那些名字指的是本平台施加的容器边界，而这里一条都不适用」——`TrustedNative`
在 Linux 上违反的就是它自己写下的这条原则。

**做什么。** 把容器边界变成显式 capability，沿用 `ADR-0072` 已经建立的资源 capability 模式：
能力向量公开、缺失时在任何状态创建和进程创建**之前** typed fail-closed、要求进入 Tool 实现摘要。
同时全仓扫一遍是否还有其他"平台相关的静默降级"。

**不做什么。** 不实现 landlock。这一阶段要的不是补上 Linux 隔离，是**让隔离的缺席变得诚实**。

**出口标准。** 任何平台缺失的隔离能力，都在 spawn 前 typed fail-closed 并进入摘要；缺失路径有
显式构造的测试（与 `process_resources.rs` 证明 `backend_not_wired` 的方式相同）。

**对百分比的影响：无。** 这修的是可信度，不是能力。

> **完成状态（2026-08-17）**：容器边界已成显式 capability，非 macOS 上 `TrustedNative`
> 在 Workspace 解析与进程创建前具名 fail-closed，静默降级分支已删除；同时修正了两个执行器
> 实现摘要中写死的 `workspace_access`。见 `ADR-0122`。
> **仍未做**：任何非 macOS 的隔离后端（landlock 属阶段 2），以及注册处的提前拒绝。

---

### 阶段 1 — 对外调用契约（**已完成** 2026-08-17，ADR-0123）

> **完成状态（2026-08-17）**：出口标准五项全部达成——提交、事件订阅（分页 + 流式）、审批决定、
> 取消、跨进程崩溃恢复，均由独立网络调用方在真实回环 Provider 上闭环验证，无残留。
> 见 `ADR-0123` 与 `docs/evidence/2026-08-17-runtime-network-invocation-surface.md`。
> **不在出口标准内、仍未做**：`resume` 成功路径、Java SDK。
> **总体进度未变，仍 70–75%**——边界层不属于并发/真实厂商/跨平台/生产持久层四类证据。

**问题（当时）。** Runtime 当时**没有任何网络面**可以提交、观测、审批、取消一次 Run。

| 契约 | 实况 |
| --- | --- |
| `contracts/proto/runtime.proto` → `RuntimeControl` | 服务端 0 实现，客户端 0 引用。**空契约** |
| `contracts/openapi/openapi.yaml` → `/v1/runs`、`/v1/sessions`、`/v1/approvals` | Rust 侧 0 实现 |
| `runtime-host` 生产监听 | 仅 Unix domain socket；`TcpListener` 全在 `#[cfg(test)]` 内 |
| `model-gateway` 的 TCP/mTLS | 仅 `ModelExecution`、`McpFederation`（Worker 打进来的内部依赖）与 `McpOauthAdmin` |

要跑一次 Run，只有两条路：把 `EmbeddedRuntime` 编进自己的进程，或在同一台机器上连 Unix socket。

目标文档要求的每一个交付形态——Java 集成、云端 Runtime 服务、GUI 客户端、独立 CLI——都压在这条
不存在的契约上。

**为什么现在可做。** 零件齐了：`RuntimeControlCommand`、durable receipt、owner epoch、
`ADR-0114` 的有界 event cursor、完整 workload identity，以及 `ADR-0121` 刚建的**运维身份形状
（schema 5，run 字段全 nil）**——那正是非 Run 调用方需要的身份，但目前没有任何网络面让这种身份进来。

**做什么。** 一个 gRPC 面，只做三件事：submit、observe、control。身份用 schema 5 + mTLS，
事件走已有 cursor 契约，Run 提交复用已有的 command/receipt 链路。顺带清理两个空契约：
`runtime.proto` 和 `openapi.yaml` 要么实现，要么删除——摆在 `contracts/` 里不实现是负债。

**不做什么。** 不新增状态机，不碰 Agent Loop，不引入 Java / Docker / 外部数据库。

**出口标准。** 一个**独立进程**（非嵌入、非同机 socket）通过网络完成真实 Run 的提交、事件订阅、
审批决定、取消与崩溃恢复，全程无残留。

**对百分比的影响：无。** 这是边界层，不是内核能力。但它是后续所有形态的前置。

---

### 阶段 2 — 平台隔离兑现（**需要一台 Linux 机器**）

**门槛。** 一台可用的 Linux 主机（内核 ≥ 5.13 以启用 landlock，cgroup v2 已挂载并可 delegate）。
本地虚拟机与 Docker 被运行边界排除，因此这需要一台真实机器或一个云实例——**这是用户的决定，
不是我能替代的**。

**做什么。**
1. 兑现上述八个 ADR：cgroup v2 真后端从 `backend_not_wired` 转为可用，全部生命周期在真实
   cgroupfs 上重跑。
2. landlock 作为 Linux 的容器边界后端，接入阶段 0 建立的 capability 契约。
3. Linux 上 `workspace.write_text` 与 `shell.exec` 恢复可注册。

**出口标准。** Linux 上容器边界实测生效（越界读写被内核拒绝，不是被代码拒绝）；
cgroup 限制对真实进程树生效；九处免责声明逐条删除并替换为证据链接。

**对百分比的影响：显著。** 这是"跨平台"这一项从 0 到 1，且一次性兑现八个 ADR 的存量。

---

### 阶段 3 — 真实生态兼容（**需要外部服务与凭据**）

**门槛。** 真实厂商 API Key、真实 OAuth provider、可长稳运行的第三方 MCP server。

**当前部分证据。** ADR-0137 已用 hash-pinned Codex strict MCP 2026 stdio fixture 完成 2026 input-required 与
Host replacement continuation。ADR-0138 又用完整 npm lock 固定官方 `server-everything@2026.7.4` 与 SDK
`1.30.0`，完成 Streamable HTTP discovery、真实 Tool call、目录冻结拒绝和完整 Agent Loop。MCP 外部样本已从
0 增至 2，但仍没有非官方 SDK/手写第三方实现、真实 OAuth、长稳公网流或真实 Provider 矩阵，因此阶段未完成。

**做什么。**
- 真实厂商三协议兼容矩阵：`openai_responses` / `anthropic_messages` / `openai_compatible`
  对真实限流、`Retry-After`、冷却与错误响应的行为。目前只有 `openai_compatible` 验证过一次。
- OAuth：现有 22 条偏差测试**客户端与服务端都是自己写的**，fail-closed 边界（64 KiB body、
  4 KiB 字段、32 scopes、强制 S256、issuer 自有端点）从未面对过非自己撰写的文档。过严的边界
  在生产里表现为"合规的 provider 就是连不上"。
- 第三方 MCP server 长稳验收。

**出口标准。** 兼容矩阵的样本数是 N，不是 1。

**对百分比的影响：显著。** 这是"真实厂商"这一项。

---

### 阶段 4 — 分布式与真实容量（**需要多机环境**）

**做什么。** 1000 个**真正同时执行**的 Run（当前是 1000 claimed in-flight / 32 admitted，
`ADR-0113` 已明确记录不得用排队数冒充执行并发）；跨进程/跨节点 tenant authority；
分布式 command ledger；多进程 state-root ownership。

**对百分比的影响：显著。** 这是"并发"与"生产持久层"两项。

---

### 阶段 5 — 产品形态（前四阶段的函数）

Java SDK、CLI、GUI、Edge 解冻。**不在前面阶段完成前启动**——目标文档写死了：
「前一项没有真实闭环证据时，不用 GUI、控制面或部署编排把它包装成"已完成"」。

## 明确不做

- **Windows / ConPTY**：当前 0 处引用。在阶段 2 完成前不开这条线，否则是第二个无法证明的平台。
- **Kata / Kubernetes / OCI 沙箱生命周期**：超出内核范围。
- **Dynamic Client Registration、Client ID Metadata Document**：OAuth 的生态广度问题，
  在阶段 3 有真实 provider 之前做等于继续自证。
- **加权公平调度、配额抢占、多模态附件**：已证机制上的增量，不动对标面，随时可做但不排期。

## 顺序的理由

阶段 0 和 1 排在最前，**不是因为它们最重要**——它们不提高百分比——而是因为它们是**唯一在当前
环境下能真正完成的事**，且阶段 1 是后续所有形态的前置。**两者均已于 2026-08-17 完成，
总体进度如预期未变。** 这正是本规划开头那条主结论的实证：能做完的不动百分比，
动百分比的做不完。

阶段 2 排在 3、4 之前，因为它的门槛最低（一台机器 vs 厂商凭据 vs 多机集群），
且能一次性兑现最大的存量。

**如果只能推动一件事，那件事是获得一台 Linux 机器**，而不是写更多代码。
