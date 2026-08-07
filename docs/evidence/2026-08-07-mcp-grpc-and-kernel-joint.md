# MCP 第五片：gRPC 端点与内核接头

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 范围

焊上两个接头里的一个半：

- **gRPC 端点**：`McpFederation` 服务与 `ModelExecution` 同进程提供（那是解封凭据的地方）。
- **内核接头**：`federated_tool_definitions()` 把发现到的目录变成内核接受的 `ToolDescriptor`。

**仍缺**：Worker 还没有去调这个 gRPC 端点。发现与执行的接线是最后一片。

## 一处对齐既有做法而不是自成一套

MCP 客户端最初和模型凭据路径分叉了两处：

| 分叉 | 后果 | 已改为 |
| --- | --- | --- |
| `key_id` 由调用方传参 | 传错只表现为「信封打不开」，排查困难 | 从公钥 DER 摘要**推导** |
| 无密钥强度下限 | 模型凭据要求 RSA-3072，这里 2048 也收 | 同样要求 ≥3072 |

第二条是实打实的安全分叉：**「这是新写的代码」不构成接受更弱密钥的理由**。
补了用例钉住 2048 位密钥被拒。

## 联邦工具的安全属性在一个地方决定

`federated_tool_definitions()` 是唯一决定这些的地方，不让服务器发什么就是什么：

| 字段 | 取值 | 为什么 |
| --- | --- | --- |
| `sandbox` | `Federated` | **这是挡住审批豁免的那一项**（内核判它） |
| `approval` | `Ask` | 逐次审批 |
| `effect` | `Unknown` | 第三方的效果按定义就是未知 |
| `implementation_digest` | 冻结的目录摘要 | Checkpoint 恢复会重算，目录变了就拒 |
| `required_scopes` | `tool:mcp:<server>` | Skill 仍然够不到 AgentVersion 没委派的服务器 |

目录里出现不属于本服务器命名空间的工具名 → **拒绝整批**。
网关已经负责限定名了，走到这里说明上游出了问题，猜比拒更糟。

## 故障注入：把 `Federated` 换成 `TrustedNative`

```
a_federated_tool_is_registered_as_federated_and_always_asks              FAILED
a_registered_federated_tool_still_asks_with_an_exemption_configured      FAILED
```

**第二条才是重点**：换掉 sandbox 类别之后，租户配置的豁免**立刻生效**，
计划从 `ApprovalRequired` 变成 `AutoApproved`。这证明「注册 → 内核闸口」整条链是承重的，
不是两段各自看起来对。

## 一个新增的公开访问器，以及为什么

给 `WorkerProcessor` 加了只读的 `tool_registry()`。测试要问的是**内核会怎么判**，
而不是「描述符字段等于我写进去的值」——后者等于把赋值语句抄一遍。
只有注册表能回答前者。

## 检查结果

```
Rust（cargo test --workspace）320 通过 / 0 失败
```

## 明确不声称

- **Worker 还不会调这个 gRPC 端点。** 服务端在、内核接头在，但没有客户端把两者接上，
  也没有在 Run 开始时去发现目录。**端到端仍未跑通。**
- 工具执行没有路由到网关：内核会给出 `ApprovalRequired`，但批准之后没有任何东西去执行它。
- 没有对真实第三方 MCP 服务器验证过。
- gRPC 层没有独立测试：它只做翻译，规则都在 `mcp` 模块里并在那里测过。
  **这意味着「翻译本身写错了」不会被现有用例发现**——要等端到端那一片。
