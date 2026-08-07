# MCP 第四片：内核闸口，以及一个被反例逼出来的真漏洞

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 范围

ADR-0040 决策 6：**每个联邦工具都必须逐次审批，任何豁免都不适用**。
本片把这条变成内核里的规则，并且证明在此之前它**不成立**。

## 一个我差点当成「已经安全」的漏洞

先写测试，参数用的是 `{"query": "ls"}`。**测试直接通过，没有任何内核改动。**

如果就此收工，结论会是「联邦工具本来就安全」。查实之后不是：

```rust
let exempt = descriptor.approval == ApprovalMode::Ask
    && auto_approval == AutoApproval::ProvablyReadOnlyShellCommand
    && execution.call.arguments.get("command") ... == ProvablyReadOnly;
```

豁免只看**策略值**和**是否有个叫 `command` 的参数**。它完全不关心这个工具是不是联邦的。
之所以通过，只是因为我给的参数键叫 `query` 而不是 `command`。

把参数改成 `{"command": "ls -la"}` —— 一个工具接受名为 `command` 的参数**再正常不过** ——
测试立刻变红：**一个第三方工具被一个为 shell 写的分类器豁免了审批**。

这是今天第三次同一形态：**样例选窄了，测试就什么都没测。**
前两次是「敌意名单没有以数字开头的」和「无竞争者的租户」。

## 修法：判 sandbox 类别，不判名字前缀

```rust
&& descriptor.sandbox != SandboxClass::Federated
```

**不用名字前缀判**。名字是注册时选的字符串；sandbox 类别是**平台自己对「这东西怎么被约束」
下的结论**。要挡住一个安全豁免，判据应该取自后者。

## 为此新增了 `SandboxClass::Federated`

原有三个变体全是本平台施加的隔离形态（RestrictedContainer / Kata / TrustedNative）。
联邦工具**跑在别人的机器上**，这三种一个都不适用。

复用 `RestrictedContainer` 会是一个**可能致害的谎**：任何读描述符去判断「这个 Tool 怎么被约束」
的代码都会得到错误答案。新变体让「我们约束的」和「不归我们约束的」可区分。

## 故障注入

```
去掉 `descriptor.sandbox != SandboxClass::Federated` 一行
  → an_approval_policy_cannot_exempt_a_federated_tool  FAILED（12 通过 1 失败）
```

只有那一条失败，其余全绿——闸口精确，没有连带。

## 检查结果

```
Rust（cargo test --workspace）315 通过 / 0 失败
Java（run-java-tests）        166 通过 / 0 失败 / 1 跳过
```

## 明确不声称

- **内核认识联邦工具，但没有任何东西去注册它们。** 没有 gRPC 端点、没有 Worker 把
  网关的目录转成 `ToolDescriptor`。第三片的联邦客户端和本片的内核闸口**还没有接上**。
- 因此**端到端仍未跑通**：注册 → 发现 → 审批 → 调用这条链，四段都在，中间两个接头未焊。
- 没有对真实第三方 MCP 服务器验证过（同第三片）。
- 联邦工具的 `implementation_digest` 用冻结目录摘要充当，语义上讲得通
  （它标识「被调用的是什么」，也确实会随目录变化），但**没有测试钉住这个约定**——
  今天没有任何代码强制注册方必须填目录摘要而不是别的什么。
