# 复审 P0×2 + P1 修复：MCP 身份绑定与 SSRF 防护

日期：2026-08-08
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 三条全部属实，先记账

复审给的三条我逐条查证，**没有一条需要辩解**：

| 条目 | 查证结果 |
| --- | --- |
| P0 MCP gRPC 未校验工作负载身份 | `mcp_grpc.rs` 里**没有任何** verifier / authorization 字样 |
| P0 任意 HTTPS 可触发 SSRF | `require_permitted_endpoint` 只看 scheme，`https://169.254.169.254/` 直接过 |
| P1 控制面拒收 `tool.execution.auto_approved` | `SUPPORTED_TYPES` 里确实没有 |

另外核实：`tenant_run_quotas` 无 FK 无 RLS 属实；`runtime/target` 8.5GB 属实。

**P0-1 是我自己引入的洞**：第三片写网关联邦客户端时我把安全规则都放进了 `mcp` 模块，
第五片写 gRPC 层时我在证据里写了「它只做翻译，规则都在别处」——**「只做翻译」这句话
让我漏掉了「翻译层也是信任边界的入口」**。

## P0-1：身份绑定

服务端原来直接用请求体里的 `tenant_id`。任何能连上端口的东西都可以指名任意租户，
让网关解封**那个租户的**凭据、调用**那个租户的**服务器。

改法沿用 `ModelExecution` 已有的形状，并把绑定做全：

- proto 补上 `attempt_id` / `worker_id` / `worker_incarnation_id`——原来只有 tenant 和 run，
  **物理上无法做完整绑定**。
- 校验 bearer → `RequiredCapability::new("model-gateway", "mcp.federate", true)`。
- `claims.authorizes(&binding)` 比对全部五项。
- **下游用的是 `claims.tenant_id`，不是请求体的**。两者一旦分叉，签过名的那个才算数。

`mcp.federate` 是独立于 `model.execute` 的作用域：**能调模型不等于能去够租户的第三方服务器
并让它的密封凭据被解开**。一个作用域的话，将来永远没法只收回其中一项。

失败码分开：令牌无效是 `Unauthenticated`；令牌有效但要求扮演它不持有的身份是
`PermissionDenied`——后者回 `Unauthenticated` 会让调用方去重新认证，而那条路走不通。

五条用例：无令牌、他租户令牌、缺 `mcp.federate`、同租户他 Run、正确绑定。

## P0-2：SSRF

**scheme 不是边界。** `https://169.254.169.254/` 是合法 HTTPS，也是云元数据服务；
`https://10.0.0.1/` 是合法 HTTPS，也在部署自己的内网里。租户注册任意一个，
这个网关就成了**带着自己网络位置**的请求转发器。

改成解析主机名并检查**每一个**解析出的地址。一个名字解析出一个公网地址和一个私网地址时，
按私网那个拒绝——「至少有一个是好的就放行」正是 DNS 重绑定需要的形状。

`is_publicly_routable` 写成**否定式匹配**（列出不可路由的段）而不是白名单：
没被想到的网段应该落在「拒绝」一侧；写成白名单它会落在「放行」一侧。
覆盖私网、回环、链路本地、多播、文档段、CGNAT（100.64/10）、基准测试段（198.18/15）、
0.0.0.0/8，以及 IPv6 的 ULA / 链路本地 / **IPv4 映射地址按其映射到的 v4 地址判定**。

### 一处比复审要求更进一步的收紧

原来 loopback 是硬编码放行的（本地开发和测试需要）。写这一片时想清楚了：
**租户注册 `http://127.0.0.1:8080` 会让网关去打自己的内部端口，那本身就是提权。**

改成**默认拒绝、由部署显式开启**（`AGENT_RUNTIME_MCP_ALLOW_LOOPBACK`）。
开发和测试传 true，生产什么都不写就得到安全的那一个。
而且只认字面量 `localhost` / `127.0.0.1` / `::1`，**不认「解析到回环」的名字**——
那是同一个重绑定花招换了个位置。

## P1：控制面拒收内核事件

`tool.execution.auto_approved` 由内核在豁免生效时发出，但不在白名单里。
一旦启用自动审批，控制面会拒收一条**合法**事件——而中途拒收一条事件会在序号上留下缺口，
后续对账把缺口读成丢失。

补了一条用例，遍历内核工具生命周期的**全部**事件类型，而不是只补上缺的那一个：
只补一个的话，下一个漏掉的类型仍然只有在生产里才会暴露。

## 故障注入

| 注入 | 结果 |
| --- | --- |
| 关掉 `claims.authorizes(&binding)` | 他租户、他 Run 两条用例失败 |
| 去掉解析地址检查（只留 scheme） | SSRF 用例失败 |
| 移除 `tool.execution.auto_approved` | 新的事件类型用例失败并指名该类型 |

## 一处我自己的测试写错

身份用例第一版签名只签了 `payload`，而 verify 签的是 `v2.{payload}`。
结果是三条失败、报 `Unauthenticated`——**读起来像认证逻辑有问题，其实是测试签错了字节**。
已修，并把这条写在测试注释里。

## 令牌作用域用例保持严格

`Ed25519WorkloadTokenIssuerTest` 用 `containsExactlyInAnyOrder` 钉死作用域集合，
加了 `mcp.federate` 后该用例变红。**这是它该有的行为**：令牌里的每个作用域都是持有者获得的权限，
增加一个必须是一次显式改动，宽松断言会让它悄悄溜过去。已更新期望集，没有放松断言。

## 检查结果

```
Rust（cargo test --workspace）331 通过 / 0 失败
Java（run-java-tests）        167 通过 / 0 失败 / 1 跳过
```

## 明确不声称

- **Worker 主链仍未接线。** `NatsWorker` 不会自动发现 MCP；审批通过后也没有代码去调
  `call_tool` 并把结果送回模型。这是复审排在第 3 位的任务，本轮未做。
- **没有对真实第三方 MCP 服务器跑过。**
- **SSRF 防护有一个已知残余风险**：检查在连接前解析，`reqwest` 连接时会再解析一次，
  两次之间存在 TOCTOU 窗口。彻底堵住需要把解析结果钉进连接（自定义 resolver 或
  预解析后直连 IP 并带 SNI），本轮没做。
- `tenant_run_quotas` 的 FK / RLS、幂等竞态、8.5GB 构建缓存均未处理，仍在队列上。
