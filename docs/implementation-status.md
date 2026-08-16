# 实施状态

更新时间：2026-08-15

本文件区分“已实现并有证据”“仅有契约或骨架”“尚未实现”，避免把六个月目标误报为当前能力。

## 进度（2026-08-15 重算）

此前这里写着「私有 Beta 约 **96%**」，更新时间停在 08-02。那个数字**不可信**，已作废：
它是按「已列出的验收项覆盖率」算的，而那份验收项清单本身没有覆盖工具面、MCP、桌面客户端和
规模验证。分母定小了，分子自然好看。

旧的 08-07 数字也已被后续 70 多项 ADR 和真实门禁淘汰。当前重算只统计本阶段明确要求的 Rust Runtime，
不把暂停的 Edge、Java、GUI 计入分母：

| 坐标系 | 估计 | 依据 |
| --- | --- | --- |
| 当前 Rust Runtime 内核目标 | **约 70–75%** | Agent Loop、三协议模型、安全故障转移、Tool/MCP/审批、持久恢复、Session/子代理、预算、公平准入、进程治理、图感知 retention、1000 在途/32 admitted、Event Cursor、模型可用的有界 MCP Resources/Prompts/Templates，以及 OAuth 凭证域第一阶段已有真实闭环；OAuth discovery/完整登录面、外部兼容矩阵和跨平台资源治理仍未证明 |
| Codex 执行内核可比能力 | **约 65–70%** | 核心 Turn/Tool/Checkpoint/Session/子代理、MCP Resources/Templates 模型入口及 OAuth refresh 事务第一阶段已对齐；工具广度、完整 OAuth discovery/login、跨平台 sandbox/exec、客户端与 SQLite Thread 产品链落后 |
| OpenClaw 可比协同语义（不含 Edge） | **约 55–60%** | durable 多租户身份、恢复和副作用围栏较强；SQLite Session、归档/history gap、Gateway 运维、跨平台产品能力落后 |

三个数字差距大是正常的，因为分母不同。**引用时必须带上坐标系**，单说一个百分比没有意义。

该百分比是技术 Alpha 后期的主观范围，不是 Beta SLA、代码行覆盖率或功能清单勾选率。新的并发、真实厂商、
跨平台或生产持久层证据会改变它；单个新增测试不会自动提高百分比。

2026-08-15 MCP OAuth 凭证域第一阶段完成：RunExecution schema 21 与 Worker→Gateway Protobuf 只携带
`oauth_credential_id`，该稳定句柄与 tenant、Server UUID、endpoint 一起进入授权摘要，静态 envelope 与 OAuth
模式互斥且旧 schema fail-closed。Model Gateway 新增 PKCE S256 begin/complete、AES-256-GCM owner-only 文件账本、
revision CAS、credential-scoped `flock`、先持久化 exchange/refresh intent、跨进程刷新单飞、先落盘再暴露 Token、
过期无 refresh 的显式收敛、stale 401 摘要保护、本地 revoke，以及刷新/交换崩溃后不重放的
authorization-required 恢复。真实回环授权端点 + MCP Server 证明并发两次解析只发生一次 refresh，Gateway 用
句柄完成认证 MCP discovery，磁盘不含 access/refresh token、授权码或 state；独立 Host 会拒绝 Gateway-owned
handle。Gateway 二进制可选读取状态目录与 base64 32-byte 主密钥文件，密钥不放环境变量。该阶段仍是**部分
OAuth**：Protected Resource / Authorization Server metadata discovery、动态客户端注册、管理 gRPC/CLI callback、
将 MCP HTTP 401 自动接到 rejected-token CAS、远端 revocation、外部 Server 兼容矩阵及非 Unix store adapter
尚未完成，因此总体进度暂不提高。最终 Rust 全工作区精确列出 729 项：723 通过、0 失败、6 个外部 live
用例显式忽略；Clippy workspace/all-targets/all-features `-D warnings`、格式与 all-targets check 通过。证据见 ADR-0118 与
`docs/evidence/2026-08-15-credential-domain-mcp-oauth-stage-one.md`。

2026-08-16 MCP OAuth 第二阶段完成：discovery 走标准两跳（RFC 9728 Protected Resource Metadata → RFC 8414
Authorization Server Metadata），`WWW-Authenticate` challenge 只能收窄到 MCP endpoint 自身 origin，跨源在发出
任何请求之前拒绝；`resource` 必须精确等于 endpoint，issuer 必须自有 authorization/token endpoint，缺
`S256` 不退化为 plain，body ≤64KiB、字段 ≤4KiB、scope ≤32，不跟随重定向。discovery 结果冻结进
PendingAuthorization，callback 只从记录读 endpoint，因此中途替换 metadata 无法改变授权码兑换地址。凭证解析
边界改为携带 token digest，真实 MCP 401 + `invalid_token` 经 CAS 精确标记 `authorization_required`，而 403、
`insufficient_scope`、网络与协议错误不改变凭证状态；认证失败一律不重放。tools/resources/prompts/lifecycle 四个
入口共用同一解析与上报边界。管理 gRPC/CLI、callback 承载、远端 revocation、Dynamic Client Registration 与真实
外部 Server 兼容矩阵仍未实现，**全部证据均来自受控回环 server**，因此总体进度维持 70–75% 不变。本轮 Rust
全工作区精确列出 742 项：**736 通过、0 失败、6 个外部 live 用例显式忽略**（119 个测试二进制）；Clippy
workspace/all-targets/all-features `-D warnings` 与 fmt check 通过。途中修复了一个与本阶段无关的
`agent-tool-runtime` flake（定点采样竞态，改为有界轮询）。证据见 ADR-0119 与
`docs/evidence/2026-08-16-mcp-oauth-discovery-and-rejection-feedback.md`。

2026-08-17 MCP OAuth 管理面与远端撤销完成：新增 `McpOauthAdmin` gRPC（begin/complete/status/revoke），要求
独立 capability `mcp.oauth.admin` 而非复用 `mcp.federate`——能调用租户工具的 token 不应自动能销毁该租户的授权。
管理面与 federation 共享同一 coordinator 实例，运维撤销对在途调用立即可见。响应不含 access token、refresh
token、authorization code、PKCE verifier 或 OAuth state；完成授权只回 revision。tenant 取自已验证 claims 而非
请求体，请求体只能断言 tenant/application/workload identity，run 相关字段一律来自 claims，因此请求无法拓宽
自己的 token。RFC 7009 远端撤销：revocation endpoint 在授权时冻结（revoke 时重新 discovery 会把待作废的
refresh token 交给一个此刻才解析出的地址），先原子提交本地 `Revoked` 并释放 lease，再做有界 best-effort 远端
调用，远端失败只如实回报不复活本地；存在 refresh token 时优先撤销它以作废整个 grant。**已知限制**：workload
token 没有运维态身份形状（`verify` 拒绝 run/attempt/worker 为 nil 的 claims），管理 token 实质是绑定 Run 的
token 多带一个 scope，隔离建立在 scope 而非独立身份上。本轮 Rust 全工作区精确列出 748 项：**742 通过、0 失败、
6 个外部 live 用例显式忽略**（120 个测试二进制）；Clippy workspace/all-targets/all-features `-D warnings` 与
fmt check 通过。途中另修复两个与本阶段无关的 `agent-tool-runtime` 缺陷：定点采样竞态，以及终止原因分类不一致
（恢复路径无条件写 `RecoveredMissing`，不查看输出长度，导致因输出超限被终止的会话可能被报告为"消失"）。
全部证据仍来自受控回环 server，真实外部 OAuth Server 兼容矩阵未开展，因此总体进度维持 70–75%。证据见
ADR-0120 与 `docs/evidence/2026-08-17-mcp-oauth-admin-surface-and-remote-revocation.md`。

同日补充 provider 偏差硬化：用脚本化回环服务器验证 10 条真实世界偏差，其中 8 条现有实现已正确（issuer 尾斜杠、
PRM 缺 `scopes_supported`、无 `revocation_endpoint` 时降级为仅本地撤销、未知字段忽略、多参数 challenge、
非 JSON token body 拒绝、`authorization_servers` 多项只取第一项、`Content-Type` 建议性）；2 条过严已放宽：
`expires_in` 现接受数字字符串、`scope` 现接受数组。**放宽严格限定在解析层**，解析后仍走同一套上界与字符校验，
并以 `missing_s256_is_still_refused_after_leniency` 断言只声明 `plain` 的 provider 仍被拒绝，证明宽松未蔓延到
PKCE 强度。矩阵随后扩到 20 条，新增判定包括：**授权服务器与资源不同 origin 被允许**（标准生产形态，只有
challenge 指定的 metadata URL 受同源约束，因为那是攻击者可控输入）；HTTP 200 携带 `error`、空 `access_token`、
body 短于 `Content-Length` 后断流三种伪成功均被拒绝；refresh 轮换与非轮换分别沿用正确的 token；
`token_type` 非 Bearer 被拒绝；同一 credential 第二次 begin 作废第一次且旧 state 不再可兑换；`expires_in`
落在 30 秒刷新偏移窗口内时刷新而非呈示；authorization endpoint 已带 query 参数时 OAuth 参数扩展既有 query 而非
另起一个（丢掉 provider 的 `tenant=` 判别参数会把用户送进错误的授权上下文，而其余参数看起来都对）。
RFC 8707 `resource` 参数记为**有意的已知不兼容**：要求它的 provider 会拒绝兑换，那是可诊断的失败；而向不预期
它的授权服务器发送 resource indicator 可能改变签发的 audience，那既不可见也不可诊断。
矩阵过程中发现的观测性缺口**已闭合**：`McpOAuthCredentialStatus::Active` 与 `McpOauthStatusResponse` 现在携带
granted scopes，运维可看出 provider 是否静默收窄了权限；scope 名不是凭证材料，因此拓宽的是可见性而非暴露面。
另补两项并发交叉断言：撤销在并发刷新下保持终局（在途 refresh 不能复活已撤销的凭证），以及任何 revision 都
不曾持有的 digest 无论何时落地都被拒绝——结论建立在 6 次 × 8 线程重复运行上，而非单次绿。
全工作区精确列出 772 项：**766 通过、0 失败、6 个外部 live 用例显式忽略**（121 个测试二进制）；
Clippy `-D warnings` 与 fmt check 通过。**脚本化模拟不构成真实外部兼容证据，总体进度仍维持 70–75%。**

2026-08-17 运维身份形状完成（ADR-0121）：workload token 新增 schema 5，必须命名 tenant、application 与
workload identity，而 run、attempt、worker、incarnation、model policy、session、workspace、agent version
**必须全部为 nil**。该"必须缺席"即机制本身——`authorizes` 逐字段全等比较，Run token 的 `run_id` 非 nil 永远
无法满足运维绑定，运维 token 反之亦然，因此 **`authorizes` 一行未改**。管理面另外显式要求
`claims.is_operator()`：携带 `mcp.oauth.admin` 的 Run 形态 token 现按**形状**拒绝，而旧契约下那恰恰就是运维
token 的样子。这解除了 ADR-0120 如实记录的"隔离只建立在 scope 上"限制，使其变为**结构性隔离**。两个方向各有
测试（Run 形态不能管理、运维形态不能 federate），契约层另有 3 项测试逐字段钉住形状规则（8 个执行字段逐一
验证）。schema 2/3/4 行为未变，既有 6 项契约测试未改动即通过。全工作区精确列出 776 项：**770 通过、0 失败、
6 个外部 live 用例显式忽略**（121 个测试二进制）；workspace Clippy `-D warnings` 与 fmt check 通过。
控制面签发运维 token 的路径尚未实现，真实外部调用方认证未开展，**总体进度仍维持 70–75%**。

2026-08-15 Runtime-owned MCP 只读 Tool 与 Resource Templates 阶段完成：模型现在可在真实 Agent Loop 中调用
`list_mcp_resources`、`read_mcp_resource`、`list_mcp_resource_templates`、`list_mcp_prompts` 与
`get_mcp_prompt`。Server 必须同时拥有 Run 冻结的 Resources/Prompts capability 和独立
`mcp:read:<server>` scope；既有 `tool:mcp:<server>` 不会静默扩大为内容读取权。五个入口固定为 Runtime-owned
`Pure + Allow + Federated`，但仍走普通 Tool event、Checkpoint、取消与恢复链；远端 Prompt 只作为低权限
Tool Result，不进入 system/developer 层。Resource Templates 已贯通 HTTP 2025/2026、stdio、Gateway gRPC、
Worker 与独立 Host；模型可见结果硬限 128 KiB，超限确定性失败而不伪装截断。真实回环闭环验证了一个无
remote Tool authority 的 MCP Server 能完成五次读取并终结 Run。最终全量门禁为 721 项：715 通过、0 失败、
6 个外部 live 用例显式忽略；Clippy all-targets/all-features、格式和差异门禁通过。总体进度仍保持
70–75%，因为 OAuth、真实外部 Server 长稳分页、内容分级/DLP 与跨平台资源治理仍未完成。证据见
ADR-0117 与 `docs/evidence/2026-08-15-runtime-owned-mcp-read-tools-and-resource-templates.md`。

2026-08-15 MCP Resources/Prompts 可调用面阶段完成：协议中立 Resource/Prompt 类型与 Resources
`list/read`、Prompts `list/get` 已贯通 HTTP 2025/2026、真实 stdio 子进程、Model Gateway gRPC 与 Worker。
每次云调用绑定完整 workload identity、Run 授权的 Server snapshot 与冻结目录摘要，凭证只在 Gateway 解封；
单页最多 64 项、cursor 2 KiB、read 16 个 content、prompt 32 条 message，未知 schema/role、畸形 JSON、
超界响应、越权 Server 和 capability 漂移全部 fail-closed。Runtime 不自动拉完 cursor 链，也不扩大
Roots/Sampling 权限。专项覆盖真实 HTTP、stdio 2025/2026、认证 Gateway→Worker 全链与消费端 wire/bounds；
最终 Rust 全工作区精确列出 718 项：712 通过、0 失败、6 个外部 live 用例显式忽略；Clippy
workspace/all-targets/all-features `-D warnings`、格式和差异门禁通过。17 GiB `target` 作为可复用增量缓存保留。
其中模型可自主调用的 Runtime-owned 只读入口与 Resource Templates 已由后续 ADR-0117 完成；OAuth 与真实
外部 Server 长稳分页仍缺，所以总体进度不因单一能力面完成而上调。证据见 ADR-0116 与
`docs/evidence/2026-08-15-bounded-mcp-resources-and-prompts.md`。

2026-08-15 MCP 能力目录与反向权限防火墙阶段完成：协议中立
`McpServerCapability::{Tools,Resources,Prompts}` 进入 HTTP 2025/2026、stdio、Model Gateway gRPC 与 Worker；
Resources/Prompts-only Server 初始化为合法空 Tool 目录且不会收到 `tools/list`。directory schema 2 摘要绑定
受支持表面与 Tool schema；Worker 对 schema 1 只推断历史 Tools-only，对未知 schema/capability、空目录和
能力/Tool rows 矛盾全部 fail-closed。Gateway 在每个新调用会话上重新验证 Tools capability，发现后降权不会
误发副作用。Roots/Sampling 继续默认拒绝，凭证仍只在 Gateway 打开。专项 HTTP、真实 stdio 子进程、认证
Gateway→Worker、3 项 wire contract 与 2 项调用时降权门禁均通过。ADR-0115 与
`docs/evidence/2026-08-15-mcp-capability-directory-and-reverse-authority.md` 记录证据。Resources/Prompts
调用已由后续 ADR-0116 完成；OAuth onboarding/refresh/revoke 尚未实现，因此总体进度仍保持 70–75%。该阶段 Rust 全工作区精确列出
708 项：702 通过、0 失败、6 个外部 live 用例显式忽略；Clippy workspace/all-targets/all-features
`-D warnings`、格式与差异门禁通过。14 GiB `target` 作为可复用增量缓存保留，未执行 `cargo clean`。

同轮全量门禁修复 PTY pre-spawn 失败分类：supervisor 在 manifest 仍为精确 `Starting/unprepared` 时启动
失败，现持久收敛为 `Terminated/start_failed`，不再伪造模糊副作用；若已越过 `prepared` 或状态不可读，
仍 fail-closed 为 `indeterminate`。本地 Host 冷启动 supervisor 的有界等待调整为 10 秒，实际快速路径不增加
固定延迟。默认高并发曾观察到一次 macOS 进程组 close `EPERM`；专项与 8 线程全量门禁通过，仍列稳定性风险。

2026-08-15 版本化 Runtime Event Cursor 阶段完成：schema 1 请求绑定完整 invocation、Run、exclusive
sequence 与 1..256 limit；页面显式返回 next/earliest/highest、has-more、history-gap、事件和 running/
cancelling/waiting-approval/suspended/interrupted/terminal/retired 状态。unsupported schema、invalid request、
not found、cursor ahead、identity mismatch、corrupt log 与 storage unavailable 均为 typed error。retired gap
只由 tombstone terminal watermark 证明，不能从序号距离猜测。订阅输出改为 Event/Boundary；Legacy Attach
已迁移到同一 bounded persistent tail reader，不再每 20ms 全量重读，但保留旧 wire 响应兼容。sequence gap、
摘要清空、foreign invocation、cursor ahead、慢消费者与 retired 两种 cursor 均有 executable evidence。
ADR-0114 与 `docs/evidence/2026-08-15-versioned-runtime-event-cursor.md` 记录当前证据；随机分页仍为
O(日志长度)验证，长单 Run sparse index 尚无 profiling 依据，不提前实现。最终 Rust 全工作区 696 项中
690 通过、0 失败、6 个外部 live 用例显式忽略；Clippy、格式与差异门禁通过。

2026-08-15 本地容量口径已校正并完成扩展门禁：20 tenant、200 Workspace/Profile、1000 个 claimed
in-flight Run 中只有 32 个 Host/Provider admitted，peak queued=968；首轮覆盖 20/20 tenant，取消释放后的
晋升波覆盖 16 tenant。500 个排队 future 撤销、16 个 active durable cancel、484 个成功，最终 owner、
admission、事件订阅和缓冲均归零。慢事件消费者已从 unbounded live sink 改为 fsync JSONL + bounded cursor
subscription：单订阅≤256、进程≤256 个订阅/1024 缓冲槽、事件行≤256 KiB。exact RSS 14.6→48.5 MB、
FD 211→278→211、38.300 秒。该证据见 ADR-0113 与
`docs/evidence/2026-08-15-bounded-1000-inflight-runtime-capacity.md`；它不是 1000 个同时 Provider/Tool 执行，
因此进度仍保持 70–75%，不因更换口径虚增。最终 Rust 全工作区 695 项中 689 通过、0 失败、6 个外部
live 用例显式忽略；Clippy workspace/all-targets/all-features `-D warnings`、格式与差异门禁通过。

2026-08-15 Unix daemon/CLI 已收敛为统一 Runtime 控制适配器：Submit、resume、精确审批、cancel 与 MCP
输入全部委托 `EmbeddedRuntime`，完整 control command 可直接穿透 IPC；旧命令使用确定性 command ID，
重放命中同一 durable receipt。daemon 不再持有独立 Run handle、取消 token 或恢复状态机，Attach 改从
已提交事件日志与 durable Run record 续传，替代 daemon 无需旧内存状态。Runtime core 统一负责 accepted
receipt 恢复、owner epoch、取消优先级、审批/MCP 恢复和后台失败终态化；只有完全空身份的 legacy 本地记录
可迁移，外部或部分身份记录不会被兼容路径认领。专项证据见 ADR-0109 与
`docs/evidence/2026-08-15-unified-local-runtime-control-adapter.md`。这仍是本机 Unix transport，不是远端
认证、分布式 ledger 或 GUI/Java/Edge 集成。最终全工作区为 671 通过、0 失败、6 个外部 live 用例显式
忽略；Clippy workspace/all-target/all-feature `-D warnings`、格式与差异门禁通过。

同日多租户混合容量阶段完成：10 tenant、100 Profile/Workspace 在一个 `EmbeddedRuntime` 内形成 8 active /
92 queued，单 tenant active≤2、单 Workspace active=1；60 成功、20 审批、10 cancel、10 crash-resume 最终
产生 40 个 Completed control receipt 与精确 130 次 Provider 请求，无 Running/Accepted 残留。一次 M1 Pro
exact 运行 RSS 从 25,853,952 增至 43,220,992 bytes，FD 11→27→11，14.538 秒完成。压力测试先暴露
“先写 accepted 再发现队列满”的真实缺陷；现在新 Run 与需要新执行槽的 control 都在 durable acceptance
和 epoch 推进前取得 permit，拒绝后同一命令可安全重试。证据见 ADR-0110 与
`docs/evidence/2026-08-15-multi-tenant-runtime-storm.md`。最终 Rust 全工作区为 674 通过、0 失败、6 个外部
live 用例显式忽略；Clippy workspace/all-targets/all-features `-D warnings`、格式与差异门禁通过。该结果
不是 1000 active Run 或生产 SLA。

同日终态账本 retention/GC 第一阶段完成：`EmbeddedRuntime` 只为事件序列、payload digest、完整 invocation
与唯一终态全部一致的 Run 建立精确 tombstone；Run/input、owner epoch、terminal event 和 Completed control
command digest 在删除热目录前 durable commit，删除后再提交 cleaned 状态，崩溃窄窗可幂等 repair。活动、
等待审批、`indeterminate` 和存在未完成 `Accepted` receipt 的 Run 不会成为候选。Unix state-root 生命周期
租约拒绝第二个 live owner；Workspace 与同进程 tenant 的目录/墓碑上限均落到执行路径，另一个 Workspace
的合格终态可释放 tenant 容量，否则新 Run 在 Provider 前失败关闭。1000 个真实 HTTP/SSE 顺序 Run 的一次
exact 证据为：16 个热目录、984 个墓碑、约 1.16 MiB state、RSS 11.9→29.6 MiB、FD 12→12，总耗时
123.262 秒，替代 Runtime 扫描 1,114ms。证据见 ADR-0111 与
`docs/evidence/2026-08-15-terminal-ledger-retention-and-1000-run-churn.md`。这仍不是 1000 active Run、
跨进程 tenant 配额或生产归档；单 JSON ledger、Session/子代理 unmanaged 目录和非 Unix 单写仍是下一缺口。
最终 Runtime Host 为 163 通过、0 失败、1 个外部用例忽略；Rust 全工作区为 683 通过、0 失败、6 个外部
live 用例忽略，Clippy workspace/all-targets/all-features `-D warnings` 通过。

同日图感知回收与分段终态账本阶段完成：root Session Turn 与新子代理 Run 现在写统一多租户 Run record；
活动 Session、非终态父 Checkpoint 的 pending/active/reservation 和未完成 control receipt 构成强恢复边，
完成 Session/子代理的 typed transcript/result 只保留来源 ID，不再永久钉住热目录。schema 2 以 manifest、
最多 256 Run 的 immutable segment 和 bounded active segment 替代单 JSON 全量重写；schema 1 可在 manifest
提交前保留旧权威、提交后再删除。1000 个真实顺序 Run 为 110.617 秒、最终扫描 0.934 秒、16 热目录/
984 墓碑、约 1.17 MiB、RSS 12.5→27.2 MiB、FD 12→12；4 tenant×3 Workspace×32 Run 共 384 次真实
HTTP Agent Loop 在 36.64 秒内完成，每 Workspace 最终 6 热目录/26 墓碑，替代 Runtime replay fence 保持。
首轮实现曾因重复读取封存段造成 170.49 秒和扫描门禁失败；首次全工作区门禁又发现父/child 双恢复的
stale owner epoch 竞争，现已按恢复图根调度修复，而非放宽围栏。最终 Runtime Host 为 168 通过、0 失败、
1 个外部用例忽略；Rust 全工作区为 688 通过、0 失败、6 个外部 live 用例忽略，Clippy、格式与差异门禁
通过。证据见 ADR-0112 与
`docs/evidence/2026-08-15-graph-aware-retention-and-segmented-ledger.md`。仍未证明 1000 同时 active Run、
公开 history gap、冷归档读取、外部 tombstone 转储和非 Unix 跨进程单写。

2026-08-14 Runtime 内核回归补齐两个活性边界：stdio MCP 的新鲜目录不再凭进程/actor 存活复用，必须由
精确初始化会话在 deadline 内返回协议级 `ping`；活进程但协议卡死同样退役。Process Wait 仍保持每
Session 一个共享观察器，但本地 write 在 durable intent 与真实副作用后直接通知观察器，50ms 文件扫描
继续承担外部输出和 Host replacement 的恢复兜底。64 Session / 1024 wait 连续 3 轮 p50/p95/p100 均低于
1 秒；PTY wire v3 的 pre-spawn generation fence 6 项与真实 TTY exact 连续 10 次保持通过。该数字不是
1000 Agent Run 容量，也没有触发旧百分比重算。证据见 ADR-0107 与
`docs/evidence/2026-08-14-runtime-liveness-and-process-wait.md`。

同日协议中立 Runtime 控制阶段补齐 `EmbeddedRuntime` 的持久 Run record 与统一 control contract：schema 1
命令绑定 command ID、完整 tenant/application/workload/Workspace/AgentVersion/model policy、Run、预期
owner epoch 和 resume/精确审批/cancel action；摘要收据以 `accepted → completed` 收敛。状态写入前先取得
单 Run 执行 owner，并发审批只接受一个且 Tool 只执行一次；并发取消共享 cancellation token，所有已接受
收据都会完成。原执行者和 Accepted command 的替代执行者连续崩溃后，同一 command ID 可在冻结的 Provider
尝试预算内以更高 epoch 恢复。Run record 与收据使用文件和目录同步，非 NotFound 读取错误 fail-closed。
8 项专项测试与 Runtime Host 完整测试通过；最终全工作区 4 线程门禁为 667 通过、0 失败、6 个外部 live
用例忽略，Clippy/格式/差异门禁通过。此前 8 线程门禁有一个既有 PTY identity ambiguous 偶发失败，同一
exact 用例随后 10/10 通过，因此仍列为高并发进程治理风险。证据见
ADR-0108 与 `docs/evidence/2026-08-14-durable-embedded-runtime-control.md`。

2026-08-12 显式多租户调用与公平准入阶段完成第一段闭环：`RuntimeInvocationContext` 与 RunExecution v20
冻结 tenant/application/non-secret workload identity/Workspace/AgentVersion/model policy；本地事件与 Worker
Checkpoint 26 保存同一资源链，跨 application 恢复在模型前拒绝。`EmbeddedRuntime` 只使用预注册 Profile，
未注册 Workspace 在 Host 或网络前拒绝；全局、tenant、Workspace active limit、全局/tenant queue limit、
取消退队和 round-robin 已由真实 A1→B1→A2 回环 Provider 顺序证明。同一 tenant/application/Workspace
可共享稳定根目录注册多个 AgentVersion，另一 Workspace 身份不能复用该目录。默认并行 workspace 测试、
all-target check、Clippy `-D warnings`、格式、JSON 与差异门禁均通过。该能力是进程内 Runtime 准入，
不是外部用户认证、分布式调度或云端配额。

2026-08-13 完整工作负载身份阶段补齐上一段远端缺口：claims schema 4、ModelInvocation schema 5、
MCP/Checkpoint binding schema 2 将 tenant/application/workload/Run/Session/Workspace/AgentVersion/
attempt/Worker incarnation/ModelPolicy 连成同一签名授权链。MCP token 还绑定 server ID、name、endpoint、
sealed credential envelope、protocol revision 与 client capabilities 的 canonical digest；带 MCP 的 v20
任务在 Worker admission 即要求 `mcp.federate`。Tool context 继承完整身份，本地 daemon 只恢复和控制其
不可变 invocation Profile 所有的记录；legacy 请求会清空可选身份字段，不能借兼容路径升级权限。
当前仍不是外部调用方认证、主动 token 撤销、Java v20 producer 或分布式 Workspace owner。
完整证据见 ADR-0103 与 `docs/evidence/2026-08-13-complete-workload-identity.md`。

同日 Edge 本地持久基础层完成：`edge-task-v1` 使用 Ed25519 将完整多租户 invocation、目标 node/generation、
Run、Workspace owner epoch、输入和最长 24 小时授权窗绑定为一个一次性任务。节点在执行前持久化 Accepted
收据，经预注册 `EmbeddedRuntime` 跑真实 HTTP/SSE Agent Loop，再把完整 Runtime 事件与终态收据原子写入
连续 outbox。节点重启后的重复任务不会再次调用 Provider；Runtime 已终态但 Edge receipt 未落盘的崩溃
窄窗也能从事件恢复。state root 绑定精确 node/generation、Unix 单写者，拒绝事件/Outbox 缺口和超过 1 MiB
的事件载荷；每个 Workspace 还持久执行 owner epoch 高水位，较旧 owner 在出网前被拒绝。Runtime profile
会规范化状态目录避免租户路径别名，事件与 Checkpoint 提交点同步落盘且读取错误 fail-closed。该结论只是
signed task + durable local outbox substrate。随后 ADR-0105 已补齐持久 Ed25519 设备密钥、challenge
注册请求、控制面签名 grant、24 小时离线授权上限、capability manifest/批准子集、task schema 2 的
enrollment 精确绑定，以及同设备同 node 的单调 generation 换代。`VerifiedEdgeEnrollment` 只能由验签器
产生，不能由 embedding 伪造；本机真实闭环已覆盖 device identity → enrollment → signed task → Runtime
→ outbox。ADR-0106 现已补齐真实 mTLS gRPC 出站流、设备 challenge proof、断线重连、精确批次签名
ACK、持久在线撤销和可注册多个租户 Profile 的原生 daemon；Enrollment 过期不能建立新会话或启动新任务。
当前控制面服务仍是测试夹具，尚无证书/Grant 自动轮换、heartbeat/presence、动态能力、审批续传。证据见
ADR-0104、ADR-0105、`docs/evidence/2026-08-13-signed-edge-task-runtime-loop.md` 与
`docs/evidence/2026-08-13-edge-device-enrollment.md`、ADR-0106 和
`docs/evidence/2026-08-13-edge-authenticated-outbound-session.md`。

同日协议中立上下文压缩阶段进一步完成：RunExecution schema 13 / runtime-policy schema 3 冻结压缩
阈值，Checkpoint schema 17 保存待处理和已应用记录。真实 HTTP/SSE + MCP 证明旧 Tool 前缀进入无 Tool
摘要请求、最近完整 Tool 对保留、摘要保持 user 权限；首个摘要 HTTP 503 后替代 Host 重发相同请求且
Tool 不重放。当前目标测试为 Host standalone 20、Host subagent concurrency 17、Worker assignment 56、
Protocol 64 项全绿。该段数字是压缩阶段完成时的历史快照；后续显式历史修复阶段已在下文重算。
当时完整 workspace、Clippy 和残留门禁也已收口：**423 通过 / 0 失败 / 5 个外部 live
用例显式忽略**，共 428 项；Clippy
workspace/all-targets/all-features `-D warnings`、Rust 格式和差异门禁均通过。

同日子代理完整 transcript 胶囊进一步完成：RunExecution schema 14 / result digest v3 绑定 assistant
narrative、Tool Call、Tool Result 与终态 Assistant；Worker Checkpoint schema 18 可在终态事件发布前保存
同一 typed transcript。真实回环 HTTP 模型 + 可信原生 Tool 证明后续 `agent.send` 精确继承历史；父结果
写入前崩溃后，替代 Host 可从子终态 Checkpoint 恢复且不重放 Tool。通用历史修复、Fork/Rollback、
provider reasoning/private item 与多模态仍未实现。

同日显式历史修复阶段完成：RunExecution schema 15 只在 external/truncated import 边界修复 Tool 配对，
禁止 System 注入和歧义 Call ID；Worker Checkpoint schema 19 绑定 source/repaired digest 与修复计数。真实
HTTP 请求与替代 Host 恢复证明合成缺失 Result、移除孤立 Result 且不执行历史 Tool；修改原始导入会在
模型前拒绝。自动修复权威 Checkpoint、重复 provider Call ID、通用 root Thread Fork 与 Rollback 仍未实现。
本阶段最终全量门禁为 **428 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 433 项；
Host standalone 22、Host subagent concurrency 17、Worker assignment 56，协议历史修复 2、执行契约 34
均全绿。Clippy workspace/all-targets/all-features `-D warnings`、Rust 格式、JSON、差异与残留门禁通过。

同日 generation-bound Fork 阶段完成：`agent.fork` 绑定 source generation 与 completed activation ordinal，
创建独立 generation 1 句柄；Checkpoint schema 20 保存 generation 与幂等 Fork 收据。真实 HTTP + 原生
`workspace.read_text` 证明 typed Tool 对继承且旧 Tool 不重放，源/分支历史独立；Fork 结果写入前替代
Worker 恢复同一 handle/event。通用 root Thread Fork、before/latest boundary 和 Rollback 仍未实现。
本阶段最终门禁为 **430 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 435 项；Worker assignment
57、Host subagent concurrency 18 均全绿。Workspace check、Clippy workspace/all-targets/all-features
`-D warnings`、Rust 格式、JSON、差异与残留门禁通过。

同日 generation-fenced Rollback 阶段完成：`agent.rollback` 在同一稳定 handle 上从 generation 1 的
`[0,1]` 创建 generation 2 head `[0]`，后续 Turn 使用不复用的 ordinal 2。Checkpoint schema 21 以唯一
archived Turn 和 generation ordinal head 保存旧代，摘要篡改与无引用归档 fail-closed；`agent.history`
可读取旧代，`agent.send` 新 schema 绑定调用方 generation，旧代命令与迟到结果不能落入新 head。真实
HTTP + 原生 `workspace.read_text` 证明旧 Tool 总计只执行一次；Rollback 结果写入前替代 Worker 恢复同一
receipt/event。通用 root Thread/Session Rollback 仍未实现，下一内核缺口是跨 handle 树级预算保留。

2026-08-10 树级预算保留阶段完成：Checkpoint schema 22 新增父 Run 权威 reservation ledger，每个
pending/active/queued 子执行精确占一个 Token、费用和时长预留；`agent.spawn`、`agent.send`、
`agent.fork` 与父模型准入读取同一余额。结果结算、队列 close、父 cancel/timeout/终态释放预留；恢复会从
待处理工作独立重算并拒绝缺失、额外或篡改账本。真实 HTTP Host 的两个稳定 handle 后续请求实测获得
400 与 300 Token ceiling，终态账本归零。下一内核缺口改为通用 root Thread/Session Fork/Rollback。

同日 root Thread/Session 不可变分支阶段完成：RunExecution schema 16 新增权威 `session_branch`，稳定
Session 下的 sibling branch、generation、完成 Turn 与历史摘要不再和单次 Run 混用；Worker Checkpoint
schema 23 绑定同一 head。独立 Host 的 Continue/Fork/Rollback 会先持久化 active Turn，拒绝活动 Turn、
过期 generation、漂移 Checkpoint 与迟到终态提交；Rollback 归档旧 generation 后只移动有效 head。
真实 HTTP/SSE + MCP 证明 source/Fork/Rollback 历史 Tool 总计只执行一次；Provider 503 后替代 Host 恢复，
以及终态 Checkpoint 已发布但 Session head 尚未提交的崩溃窗口，均不会重放模型或 Tool。下一内核缺口
改为独立 Host 的协议中立多 Provider 调度与安全故障转移。

同日独立 Host 多 Provider 阶段完成：同一协议中立 IR 在进程内驱动 OpenAI Responses、Anthropic
Messages 与 OpenAI-compatible；地域、数据等级、能力、健康和费用在网络前过滤，最多 8 个候选按 Run
策略冻结。只有零事件的 retryable 策略内错误可跨 Provider，部分文本/Tool/Usage 后停止切换。原子 route
journal 保存候选游标、失败摘要、选择和 staged events；替代 Host 可从 fallback cursor 继续或直接应用
已收响应。普通回合与 context compaction 共用这条路径，二进制配置只保存 API Key 环境变量名。真实
HTTP/SSE + MCP 已覆盖三协议连续切换、五类过滤、半途断流、两类崩溃窗口及摘要故障转移。下一内核缺口
改为持久 Provider 健康、同 Provider 重试、退避、冷却和 half-open 探针。

同日持久 Provider 生命周期阶段完成：route journal schema 2 在 egress 前保存同 Provider 总尝试次数、
inflight 标记、退避记录和截止时间；替代 Host 把未决调用计为已消费，不能通过反复重启突破预算。独立
健康文件只对 rate-limit、timeout、unavailable 累计失败，阈值或 `Retry-After` 打开 cooldown；到期后同一
单写 state-root 只租出一个 invocation-bound half-open 探针。认证/账单失败不 fallback、不污染共享健康。
真实 HTTP 已证明 503 后同 Provider 恢复、跨 Host 冷却、429 提前开路、并发单探针和认证隔离。下一内核
缺口改为协议中立 Rich Model Item 与推理状态连续性。

同日协议中立 Rich Model Item 阶段完成：Reasoning 将可读 summary 与来源绑定的 opaque private state 分离，
Refusal 保持 typed item；OpenAI Responses 可回放 id/encrypted content，Anthropic thinking/signature 不会进入
可见文本。private state 仅在 route、协议、模型和格式完全匹配时回放，否则在出网前剥离并产生不含数据的
审计事件；该事件不算 committed output，不会误阻断安全 fallback。Protobuf、Worker transcript、Checkpoint
replacement、compaction tail 和 root Session Continue/Fork/Rollback 均已验证保留。下一内核缺口改为有界
并行 Tool 执行与确定性结果提交。

同日有界并行 Tool 阶段完成：RunExecution schema 17 / runtime-policy schema 4 冻结 1–16 路、默认 4 路
Tool 并发，Worker Checkpoint schema 24 保存 source-order commit queue 和乱序 staged results。当前仅相邻
`Pure` Tool 可重叠，规划、scope、审批及所有副作用 Tool 均为串行屏障；started 事件和 Checkpoint 先于
真实子进程启动。真实 HTTP/SSE + 两个真实子进程证明执行区间重叠但 Tool Result 仍按模型调用顺序回灌；
第二个结果已暂存、首个仍运行时中断 Host，替代 Host 只重试未完成调用并确定性收敛。下一内核缺口改为
模糊副作用的显式 `indeterminate` 终态与人工 reconciliation；可选 NATS 路径本轮未做外部服务实跑。

同日模糊副作用 reconciliation 阶段完成：started 已持久但结果未知的 `NonIdempotent` / `Unknown` Tool
不再作为 Host 内部恢复错误逸出，而是形成带 effect、binding、原 attempt 与 started event/sequence 的
稳定 `run.indeterminate`，并保留 `replay_safe=false` 和终态 Checkpoint 证据。原 Run 永不复活；schema 1
人工命令以连续版本记录 Applied/NotApplied/Unresolved，精确重复幂等、冲突 fail-closed，最终裁决只会把
低权限 Tool Result 带入新的确定性 Run。真实 child write+fsync、Host abort、替代 Host、Applied 与
Unresolved→NotApplied 已证明旧 Tool 不重放。下一内核缺口改为持久 Tool Process Session；NATS 适配共享
终态代码但本轮未启动外部 broker。

同日持久 Tool Process Session 第一阶段完成：显式可信可执行文件注册五个模型可见 Tool，稳定 UUID、
tenant/Workspace/实现摘要、原子 digest Manifest、独立进程组、继承 identity lock、FIFO stdin 和双输出
cursor 共同形成持久会话。真实 owner 测试进程直接退出后 replacement manager 继续原 PID；独立 Host 在
start 结果进入 Checkpoint 后替换，Agent Loop 继续 write/poll/close 且 start 只发生一次。Manifest 篡改、
越租户/Workspace、非法 cursor 均 fail-closed；close、SIGINT 与自然 leader 退出覆盖整个进程组，64 槽只
统计活跃会话。下一内核缺口改为 session deadline/idle TTL、逐租户配额、orphan sweeper 与资源预算；PTY
和 GUI 继续暂停。本阶段最终全量门禁为 **484 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共
489 项；Clippy workspace/all-targets/all-features `-D warnings`、Rust 格式、JSON、差异与残留门禁通过。

同日持久 Process Session 资源治理阶段完成：schema 2 将治理摘要、绝对 deadline、idle 活动、逐 stream
输出上限、CPU 秒、可选内存上限和 typed termination reason 写入 digest Manifest。跨进程容量锁分别约束
全局、tenant 与 canonical Workspace；满额只拒绝新会话，不淘汰 live process。真实 child 在无 poll 下按
deadline 终止；stdin 延长 idle、poll 不延长；owner 测试进程 `exit(73)` 后 replacement sweeper 按原时钟
终止同一 PID；真实 HTTP/SSE Agent Loop 看见 `execution_deadline` 并完成后续模型回合。macOS 只验证 CPU
与 `RLIMIT_FSIZE`，显式内存限额会拒绝，Linux `RLIMIT_AS` 尚无 live 证据。本阶段全量门禁为 **494 通过 /
0 失败 / 5 个外部 live 用例显式忽略**，共 499 项。下一内核缺口改为 Linux 硬资源边界与可移植进程
监管后端；PTY 和 GUI 继续暂停。

同日可移植资源 capability 第一阶段完成：`ProcessSessionResourceCapabilities` 明确区分 output-file、
CPU-time、memory、process-count 与 whole-tree accounting；`max_processes` 和整树计量要求进入 governance
及 Tool implementation digest。macOS `UnixRlimit` 只声明前两项，memory/PID/整树要求均在 state-root、
Provider 和 child 创建前返回 typed capability 错误。真实 Host 3 项进程闭环保持全绿。本阶段全量门禁为
**497 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 502 项。Linux cgroup v2 backend、Linux 构建和
live 进程树资源证据仍未完成，不得把 capability contract 宣称为 Linux 隔离。

同日 Linux cgroup v2 协议边界阶段完成：显式 backend config、五个限制控制文件的预校验/精确写入、
`O_NOFOLLOW` 最终路径保护、当前进程 membership token 和严格 `usage_usec` 解析已有行为测试；非 Linux
在 state-root 前拒绝，Linux 也保持 `backend_not_wired`，避免部分实现被误启用。真实 Host 进程闭环 3/3
保持通过。第二次全量门禁为 **502 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 507 项。第一次
全量曾出现一次子代理崩溃恢复 socket 关闭超时；聚焦、整套并发测试和全量复跑通过，但该稳定性风险尚未
证明修复。当前仍无 Linux build/cgroupfs/live 证据，下一缺口是 Manifest identity、无竞态 membership、
整树 CPU 监管、`cgroup.kill`、恢复/清理与真实 Linux 门禁。

同日 Host-owned cancellation 阶段完成：测试先在 Tokio Runtime 仍存活时稳定复现 Host abort 后 parent/child
模型 TCP 连接超过 5 秒不关闭；根因是 JoinHandle Drop 只 detach，Host Drop 未取消子树。Host 现在从 caller
token 派生私有 child domain，正常 shutdown 先取消再等待，异常 Drop 取消并 abort 登记子任务。相同测试
0.18 秒通过，replacement 恢复同一 `agent_id` 且不重放 spawn；四套关键 Host 测试 62/62，全工作区
**502 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 507 项。此前 cgroup 阶段记录的 socket 风险现已
闭环，下一缺口重新聚焦 cgroup Manifest/恢复/终止与真实 Linux 门禁。

同日持久资源身份与 pre-exec membership 阶段完成：Process Session Manifest schema 3 新增确定性的
`resource_identity` 和 `observed_cpu_usage_micros`；签名 schema 2 终态可安全迁移为 Unix rlimit 身份。
父进程安全打开 membership fd，真实 child 在 exec 前用 `write(2)` 写 `0`；新组配置失败撤销空目录，既有
组拒绝接管。Process Session/能力/治理/崩溃/Host 聚焦门禁分别 7/5/8/1/3 项全绿，全工作区
**507 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 512 项。普通文件只证明协议和 pre-exec 次序；
生产 cgroup 后端仍 fail-closed，下一缺口是 identity 驱动的 kill/recovery/CPU/cleanup 与真实 Linux 门禁。

同日 cgroup 身份驱动监管阶段完成：安全读取 `cpu.stat usage_usec` 与 `cgroup.events populated`，schema 3
Manifest 持久化单调整树 CPU 计数并在超额时形成 `cpu_limit`；Linux 身份终止写 `cgroup.kill=1`，等待
populated 清零及 identity lease 释放。start supervisor、interact、recover 与 sweep 均携带冻结 backend，
身份不一致 fail-closed。全工作区单线程门禁为 **511 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共
516 项；check、Clippy `-D warnings` 与格式通过。默认并发全量曾使一条既有异步子代理恢复测试挂起；根因
不是已证明的 socket 泄漏，而是测试在 Provider 接受父/子连接前只凭 Checkpoint 就触发 abort。增加真实
建连就绪边与分阶段 deadline 后，并发套件连续 20 轮 400/400、默认并发全量 511/0/5 通过。普通文件仍
不等于真实 cgroupfs，生产 backend 继续禁用。

同日 fd-relative cgroup 生命周期阶段完成：delegated root 与 session group 均以目录 descriptor 作为权限
锚点；创建/失败回滚、限制配置、pre-exec membership、CPU/存活观测和 `cgroup.kill` 全部改为
`mkdirat/openat/unlinkat` 相对访问，旧 PathBuf 生命周期入口已删除。三类 rename/replacement 攻击测试均先
RED 后 GREEN。包级并发门禁同时暴露并修复了 Tool timeout 回收时重新 `getpgid` 的 leader-exit 窗口，真实
child/grandchild timeout/cancel 连续 10 轮 20/20。当前默认并发全工作区 **515 通过 / 0 失败 / 5 个外部
live 用例显式忽略**，共 520 项；格式、check 与 Clippy `-D warnings` 通过。Manager 生命周期 root pinning、
成功终态空组清理与真实 Linux cgroupfs 仍未完成，生产 backend 继续禁用。

同日 Manager 生命周期 cgroup root identity 阶段完成：公开 backend config 与私有 resolved backend 分离，
Manager 只打开一次 delegated root，并以 `Arc` 传播到 watcher、governance supervisor 和全部前台操作。路径在
Manager 创建后被 rename/replacement，后续 sweep 仍读取原 root 的 2,000,000 微秒而非 replacement 的 7。
`agent-tool-runtime` 69 项全绿；当前默认并发全工作区 **516 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，
共 521 项；格式、check 与 Clippy `-D warnings` 通过。生产 Linux backend 仍 fail-closed；启动/终态组生命周期、
active schema 2 replacement 和真实 Linux cgroupfs 未完成。

同日 Linux cgroup 启动/终态生命周期阶段完成：`process.start` 先持久化 intent，再通过 Manager-owned root
准备确定性组并安装 pre-exec `cgroup.procs=0`；准备、membership、spawn 失败会 fd-relative 回滚。终态先
持久化，再幂等移除空组；watcher/close/governance 立即尝试，替代 Host 的 terminal sweep 继续重试。三条
TDD 分别证明旧启动会真实绕过 cgroup、终态旧 sweep 会漏组，以及路径替换不能重定向 cleanup。
`agent-tool-runtime` 72 项全绿；当前默认并发全工作区 **519 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，
共 524 项；格式、check 与 Clippy `-D warnings` 通过。生产 Linux backend 仍 fail-closed；`Starting` 崩溃窗、
cleanup journal、active schema 2 replacement 和真实 Linux cgroupfs 未完成。

同日持久资源阶段与 `Starting` reconciliation 阶段完成：Process Session Manifest schema 4 新增摘要保护的
resource phase，并约束 state/backend/phase 组合。Linux start 依次持久化 `unprepared/prepared/active`；终态
先写 `cleanup_pending`，fd-relative 清理成功后再写 `cleaned`。替代 Host 对无身份且组缺失的 `Starting`
收敛为 `RecoveredMissing`；对 populated 或控制器歧义组尝试 `cgroup.kill` 并持久化 `Indeterminate`，禁止
自动重放可能已执行的 Tool。schema 1/2/3 迁移入口保留，schema 3 Linux `Starting` 只标记
`legacy_unknown`。`agent-tool-runtime` 76 项全绿；默认并发全工作区 **523 通过 / 0 失败 / 5 个外部 live
用例显式忽略**，共 528 项；格式、check 与 Clippy `-D warnings` 通过。生产 Linux backend 仍 fail-closed；
直接 legacy migration 夹具和真实 Linux cgroupfs 门禁未完成。

同日 schema 5 启动边界与旧 `Starting` 安全迁移阶段完成：Unix 与 Linux 都必须在 spawn 前独立持久化
`prepared`，再发布 `Running/active`；schema 2/3/4 的旧 `Starting` 全部迁移为 `legacy_unknown`。替代 Host
只有在当前 schema 明确为 `unprepared` 且身份缺失时才能形成 `RecoveredMissing`；prepared/legacy 即使
资源为空或缺失也必须形成 `Indeterminate`，不得用进程消失推断 Tool 从未执行。真实 active schema-2 Unix
进程已由替代 Manager 重附着原 PID 并原子升级为 schema 5，不会重启 Tool。`agent-tool-runtime` 82 项全绿；
默认并发全工作区 **529 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 534 项；格式、check 与 Clippy
`-D warnings` 通过。生产 Linux backend 仍 fail-closed，真实 cgroupfs 门禁和同步 spawn 失败的 typed
`start_failed` 尚未完成。

同日 schema 6 持久启动失败阶段完成：真实同步 `Command::spawn` 错误不再留下 `Starting/prepared`，而是先
写入 `Terminated/start_failed` 和失败 Session ID，再清理资源；Linux 清理失败会保留 `cleanup_pending` 供
替代 Host 重试。活动 schema-5 Unix 进程已通过摘要校验、原 PID 重附着和 schema-6 原子重写，未重启 Tool。
全量门禁同时暴露并修复 `subagent_approval` Provider 单次 TCP read 假设；完整 `Content-Length` 请求读取后
在全工作区并发环境复验通过。`agent-tool-runtime` 84 项全绿；默认并发全工作区 **531 通过 / 0 失败 / 5 个
外部 live 用例显式忽略**，共 536 项；格式、check 与 Clippy `-D warnings` 通过。当前 Manager 层类型化错误
仍在 ToolExecutor/Worker 边界被压平为通用执行失败；生产 Linux backend 继续 fail-closed。

次日确定性启动失败的类型传播阶段完成：`ProcessSessionToolExecutor` 将 Manager 的 `StartFailed` 保持为
带 Session ID 的 `ProcessSessionStartFailed`；共享转换器只把已证明未越过副作用边界的失败变成模型可见
Tool Result，并剔除私有 OS 原因。Worker 事件使用稳定 `process_session_start_failed` 分类；独立 Host 的
真实 loopback HTTP Agent Loop 已从失败 Tool 继续到成功终态，Provider 请求和持久 `tool.result` 内容一致。
首次全量回归还发现取消进程树测试依赖固定 2 秒；改为观察真实孙进程启动后再取消，连续 10 轮全绿且
继续验证孙进程停止活动。`agent-tool-runtime` 仍为 84 项；默认并发全工作区 **533 通过 / 0 失败 / 5 个
外部 live 用例显式忽略**，共 538 项；格式、check 与 Clippy `-D warnings` 通过。生产 Linux backend 仍
fail-closed。

同日在线 Tool 执行失败的副作用确定性阶段完成：Worker/Host 新增统一的 effect-aware 入口，先校验
attempt、Tool call、binding 与持久 started 证据；确定性未执行失败及 `Pure/Idempotent` 错误形成脱敏
`tool.result`，其他 `NonIdempotent/Unknown` 错误形成 `run.indeterminate` 并保留人工 reconciliation 所需
的原调用证据。真实 loopback HTTP Agent Loop 已执行一次文件写入后故障，返回 indeterminate、持久化终态
Checkpoint，再经 `Applied` 裁决启动新 Run；原 Tool 总计只执行一次，私有执行器错误未进入事件或模型。
本阶段新增 4 项运行语义测试和 1 项重启竞态守卫。首次默认并发全量暴露出故意终止 Host 后的零字节
Provider 连接会被测试端误当成正式请求；现在只跳过完全未发送任何字节的废弃连接，半截 HTTP 仍然失败。
原重启用例连续 10 轮通过。最终默认并发全工作区 **538 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 543 项。

同日 MCP 接受调用后的响应丢失阶段完成：stdio client 只在请求未入队时重连；actor 已接受
`tools/call` 后响应通道丢失会返回未知结果，不再启动 replacement process 重发。真实 Streamable HTTP
MCP server 在执行一次副作用后截断响应，独立 Host 形成 `run.indeterminate`，终态 Checkpoint、单次
`tool.execution.started`、零 `tool.result` 和单次 MCP 调用均已验证。本阶段新增 2 项测试；最终默认并发
全工作区 **540 通过 / 0 失败 / 5 个外部 live 用例显式忽略**，共 545 项。

同日操作员权威 MCP effect 阶段完成：RunExecution v18 在每个 server snapshot 内冻结按 Tool 名称配置的
effect map；旧 schema 夹带、未被签名 Skill 声明或本地 allowlist 未授权的覆盖均 fail-closed。Worker 只从
该 Run 快照读取 effect，所有 MCP Tool 仍保持 `Ask + Federated`；第三方 `readOnlyHint/idempotentHint`
不会改变审批、失败或重放语义。effect map 进入 server binding digest，替代 Host 拒绝策略漂移。真实 HTTP
MCP 在执行一次副作用后断流：缺省 Unknown 仍 indeterminate；显式 Idempotent 返回脱敏错误 Tool Result、
进入第二模型回合并成功终止，两条路径均只调用 MCP 一次。最终全量计数见下方本次实测。

同日取消/超时副作用确定性阶段完成：Worker 在普通 `cancelled/timed_out` 终态前检查已持久的 Tool start
和冻结 effect；已开始的 `NonIdempotent/Unknown` Tool 形成 `run.indeterminate`，同时保存
`interrupted_by` 与调用方请求状态，未开始或 replay-safe Tool 保持原终态。真实 Shell 进程树和
Streamable HTTP MCP 连接均在中断后关闭；第二个 RED 证明旧终态事件背后的 Checkpoint 仍是 Running，
现已改为先持久化 indeterminate Checkpoint 再发布事件。最终全量计数见下方本次实测。

同日 MCP 请求级 cancellation/progress 阶段完成：Streamable HTTP 与 stdio `tools/call` 都携带唯一
progress token，严格校验匹配、有限、单调进度，并经有界非阻塞队列形成持久
`tool.execution.progress` 事件和 Checkpoint。真实 HTTP server 与 stdio fixture 均观察到绑定原 request ID
的 `notifications/cancelled`；stdio 先给协作退出窗口，再以完整进程组回收兜底。Unknown Tool 取消后的
最终状态仍是 `indeterminate`。云 gRPC 目前只能取消 unary RPC，尚未传输 MCP progress/cancel 通知。

同日 MCP capability negotiation/default-deny 阶段完成：HTTP/stdio 初始化严格验证协议 `2025-06-18`
和当时唯一支持的服务端 `tools` 能力；客户端仍不声明 sampling、elicitation 或 roots。真实 MCP peer 在 discovery 和
`tools/call` 中发送越权反向请求，均收到精确 request ID 的 `-32601`，随后会话退役。发现违规零模型
调用；已开始 Unknown Tool 的违规只调用模型一次并形成持久 `run.indeterminate`，后续伪造 success 不被
接受。Resources/Prompts 属于客户端查询的服务端能力，旧分类已纠正；ADR-0115 后续已让这两类能力可协商并
进入冻结目录，但尚未暴露 list/read/get。下一内核缺口是 Run-frozen、可审批
且可恢复的 elicitation 回路；sampling/roots 继续关闭。

同日 MCP 2026 MRTR 阶段完成：RunExecution v19 冻结 wire revision、`elicitation` capability 与 delegated
scope；Checkpoint v25 分别保存 pending、resolved 与 continuation dispatch 证据。真实回环模型和 TCP MCP
服务跨 Host replacement 完成 `input_required → 用户回答 → 原 Tool 续传 → Tool Result → 模型终态`，opaque
state 原样返回。随后 ADR-0091 将同一无状态轮次接入 Worker↔Model Gateway gRPC：真实 TCP MCP 的输入
请求、opaque state、回答与完成结果均穿过 Gateway 和 `FederatedToolExecutor`。旧 2025 反向 elicitation、
sampling/roots 继续默认拒绝；NATS recovery poll 的自动续传调度仍未验证。ADR-0092 进一步新增显式
`stdio2026`：内置真实子进程与 Codex 严格外部 fixture 均跨 Host replacement 完成两轮 Agent Loop；HTTP
URL elicitation 同样跨 replacement 完成，且 Runtime 未承载外部授权 secret content。

2026-08-12 Process Session 持久关闭恢复阶段完成：`process.close` 在任何终止副作用前持久化 tenant、
canonical Workspace、run、原 attempt、Tool Call、binding、Session、参数摘要和 cursor 绑定的 close intent。
Manifest 继续作为资源状态真相；替代 Host 可从 Running/Terminating 继续身份围栏 TERM→KILL，只在
`Terminated/Closed` 后交付原 Tool 结果。真实忽略 TERM 的进程已证明 Host 在关闭中崩溃后，进程只启动
一次、Provider 只收到 start/close，Run 最终成功。自然退出不制造关闭收据，一般 NonIdempotent 副作用仍
进入 `indeterminate`。本轮聚焦全量为 `agent-tool-runtime` 103 通过、`agent-runtime-host` 125 通过、
1 个外部 live fixture 按条件忽略；格式与两包 all-targets Clippy `-D warnings` 通过。独立 Host 仍使用
固定本地 tenant，下一内核缺口转为显式多租户调用上下文与公平准入。

拉低最多的两项是**工具面**和 **MCP**——它们决定「Agent 能干多少事」，而此前的投入几乎都在
「Agent 跑得稳不稳」上。稳定性与安全性上本项目局部已超过 Codex（多租户、fencing、签名 Skill、
更严的命令环境隔离），能力广度上差一个数量级。

### 实测计数（本次重算时执行，非引用历史）

```
cargo test --workspace 575 通过 / 0 失败 / 6 个外部 live 用例显式忽略
deploy/native/run-java-tests        143 通过 / 0 失败 / 1 跳过
ADR                                 94 份
证据文件                            90 份
常驻 live 门禁                      6 条
模型可见工具                        3 个基础 Tool；显式配置后另有 6 个 process.* Tool
```

Console 与完整 Console E2E 本次**未**重新执行，因此不在上表中，也不得据此声称其状态。

## 已实现并验证

| 模块 | 当前能力 | 证据 |
|---|---|---|
| Rust Kernel | Run 状态机、终态保护、审批暂停/恢复、非幂等模糊结果、事件序号；累计 Token/费用预算越界形成不可重试的分类终态。模糊副作用终态显式携带 effect 与 `replay_safe=false` | `runtime/crates/kernel/tests/run_state_machine.rs`、`ADR-0069` |
| 模糊 Tool 终态与人工裁决 | replacement Host 将唯一 unsafe started Tool 收敛为带完整执行证据的不可变 `run.indeterminate`；Applied/NotApplied/Unresolved 使用版本化协议和原子本地收据，最终裁决以 Tool Result 启动新 Run，旧 Tool 永不自动重放 | `ADR-0069`、`runtime/crates/protocol/tests/tool_reconciliation_contract.rs`、`runtime/apps/{worker,runtime-host}/tests/{assignment,standalone_run}.rs`、`docs/evidence/2026-08-10-indeterminate-tool-reconciliation.md` |
| Tool 中断副作用确定性 | cancellation/时限先关闭真实执行资源，再按已持久 start 与冻结 effect 选终态；unsafe Tool 为带原始中断原因的 `run.indeterminate`，replay-safe/未开始工作保持 cancelled/timed-out；独立 Host 在事件前持久终态 Checkpoint | `ADR-0087`、`runtime/apps/worker/tests/assignment.rs`、`runtime/apps/runtime-host/tests/execution_cancellation.rs`、`docs/evidence/2026-08-11-interrupted-tool-uncertainty.md` |
| MCP 请求取消与进度 | HTTP/stdio Tool call 携带唯一 progress token；匹配进度经 32 槽非阻塞队列写入单调 Run 事件并逐次 Checkpoint；取消发送绑定原 request ID 的 `notifications/cancelled`，再执行连接/进程组硬清理；unsafe Tool 仍 indeterminate | `ADR-0088`、`runtime/apps/{model-gateway,worker,runtime-host}/src`、`runtime/apps/runtime-host/tests/execution_cancellation.rs`、`docs/evidence/2026-08-11-mcp-request-cancellation-and-progress.md` |
| MCP 能力协商与反向请求默认拒绝 | HTTP/stdio 只接受精确协议和服务端 `tools` capability；客户端空 capability 下的 sampling/elicitation/roots 请求按原 ID 回 `-32601` 并退役会话。发现违规阻止模型出口；已开始 unsafe Tool 保持 durable indeterminate，后续 success 不可信 | `ADR-0089`、`runtime/apps/{model-gateway,runtime-host}/src/{mcp.rs,stdio_mcp.rs}`、`runtime/apps/{model-gateway,runtime-host}/tests/{mcp_federation.rs,execution_cancellation.rs}`、`docs/evidence/2026-08-11-mcp-negotiated-capabilities-and-default-deny.md` |
| MCP 服务端能力目录 | HTTP 2025/2026、stdio、Gateway gRPC 与 Worker 统一冻结 Tools/Resources/Prompts；Resources/Prompts-only 是合法空 Tool 目录且不触发 `tools/list`。directory schema 1 只兼容 Tools-only，schema 2 传递支持表面；未知或矛盾响应 fail-closed。Roots/Sampling 继续默认拒绝 | `ADR-0115`、`contracts/proto/model_gateway.proto`、`runtime/apps/{model-gateway,runtime-host,worker}`、`docs/evidence/2026-08-15-mcp-capability-directory-and-reverse-authority.md` |
| MCP OAuth 凭证域第一阶段 | RunExecution v21/Protobuf 只传稳定句柄；Gateway 内 PKCE、AEAD 文件账本、CAS + OS lease、owned exchange/refresh、跨进程 singleflight、先提交再暴露、崩溃不重放、stale 401 摘要保护与本地 revoke。真实回环 OAuth→refresh→认证 MCP 闭环；独立 Host 拒绝该 handle。metadata discovery、管理 API、401 transport 联动、远端 revoke、真实外部兼容与跨平台 store 尚缺 | `ADR-0118`、`runtime/apps/model-gateway/src/mcp_oauth.rs`、`runtime/apps/model-gateway/tests/mcp_oauth_lifecycle.rs`、`execution_contract.rs`、`mcp_server_authorization.rs`、`docs/evidence/2026-08-15-credential-domain-mcp-oauth-stage-one.md` |
| MCP OAuth 第二阶段：discovery 与拒绝反馈 | `WWW-Authenticate` challenge → RFC 9728 Protected Resource Metadata → RFC 8414 Authorization Server Metadata → S256 PKCE begin，全程真实回环 HTTP。challenge 命名的 metadata URL 必须与 MCP endpoint 同源且在发请求前校验；`resource` 必须精确等于 endpoint；issuer 必须自有 authorization/token endpoint；不跟随重定向；body ≤64KiB、字段 ≤4KiB、scope ≤32；缺 `S256` 不退化为 plain。discovery 结果冻结进 PendingAuthorization，callback 从记录读取 endpoint，替换攻击失败。`resolve_credential` 改为携带 token digest，401 + `invalid_token` 经 CAS 精确标记 `authorization_required`；403、`insufficient_scope`、网络与协议错误不改变凭证状态；认证失败零重放。管理 gRPC/CLI、callback 承载、远端 revoke、Dynamic Client Registration 与真实外部 Server 兼容矩阵仍未实现 | `ADR-0119`、`runtime/apps/model-gateway/src/mcp_oauth.rs`、`runtime/apps/model-gateway/src/mcp.rs`、`runtime/apps/model-gateway/tests/mcp_oauth_discovery.rs`、`docs/evidence/2026-08-16-mcp-oauth-discovery-and-rejection-feedback.md` |
| MCP 2026 可恢复用户输入 | RunExecution v19 冻结 revision/capability/scope；Checkpoint v25 在询问、回答、续传三个边界持久化。HTTP form/url 与 stdio form 均跨 Host replacement 原样续传 opaque state；URL 不承载 secret content；Codex 严格外部 stdio fixture 已完成真实 Agent Loop。续传后的 Unknown 副作用模糊失败仍 indeterminate。无状态 gRPC 轮次桥已实跑真实 MCP→Gateway→Worker ToolExecutor；NATS recovery poll 自动续传仍未验证 | `ADR-0090`—`ADR-0092`、`contracts/events/run-execution-requested.v19.example.json`、`runtime/{crates/protocol,apps/model-gateway,apps/worker,apps/runtime-host}`、`standalone_run.rs`、`mcp_end_to_end.rs`、`docs/evidence/2026-08-11-{durable-mcp-mrtr,mcp-mrtr-grpc-bridge,mcp-2026-stdio-url-compatibility}.md` |
| Runtime IR | 供应商无关模型请求/流事件、错误分类、Tool 描述、assistant Tool Call / tool result、Reasoning、Refusal 与来源绑定 private state；跨 Provider omission 是无 opaque 数据且不提交输出的审计事件 | `ADR-0067`、`runtime/crates/protocol/tests/model_ir.rs`、`runtime/apps/model-gateway/tests/{openai_responses,anthropic_messages,failover}.rs` |
| Runtime 执行策略 | RunExecution v18 在权威 root Session branch 之上冻结 runtime-policy schema 4、有界 Tool 并发和操作员 MCP effect；ModelInvocation v4 带策略摘要传至 Gateway。Worker Checkpoint schema 24 进一步绑定 source-order Tool commit queue、未完成请求和 staged results；恢复拒绝策略、目录、审批、历史、branch、预算、并行或 MCP effect 语义漂移。独立 Rust Host 使用同一策略且不依赖控制面 | `ADR-0041`、`ADR-0047`、`ADR-0052`—`ADR-0068`、`ADR-0086`、`contracts/events/run-execution-requested.v18.example.json`、`execution_contract.rs`、`history_repair_contract.rs`、`assignment.rs`、`standalone_run.rs`、`subagent_concurrency.rs`、`multi_provider.rs`、`docs/evidence/2026-08-11-run-frozen-mcp-tool-effects.md` |
| 独立 Host 多 Provider 路由与健康 | 同一 IR 驱动三协议候选；按健康、地域、数据等级、能力和费用过滤并冻结链。仅零事件且策略允许的 retryable 错误可重试/切换；部分输出禁止重放。route journal schema 2 原子保存 cursor、attempt/inflight、退避、失败摘要、选择与 staged events；Provider 健康状态跨 Host 保存 cooldown、`Retry-After` 和 half-open lease。普通 Turn 与压缩摘要共用路径；认证/账单不 fallback、不污染 circuit。CLI JSON 只引用密钥环境变量 | `ADR-0065`、`ADR-0066`、`runtime/apps/{model-gateway,runtime-host}/src/{openai_compatible.rs,lib.rs,main.rs}`、`runtime/apps/runtime-host/tests/{multi_provider,daemon_recovery,standalone_run}.rs`、`docs/evidence/2026-08-10-{standalone-multi-provider-safe-failover,persistent-provider-health-retry-cooldown}.md` |
| Root Session 不可变分支 | 稳定 Session 与单次 Run 分离；每分支独立 generation、完成 Turn、typed Tool transcript 和摘要。Continue/Fork/Rollback 先落 active binding，Rollback 保留旧 generation；活动 Turn、旧 generation、漂移 Checkpoint 和迟到终态均拒绝。终态 transcript Checkpoint 先于事件；替代 Host 可在 Provider 失败和终态/head 提交窄窗恢复且不重放历史 Tool | `ADR-0064`、`contracts/events/run-execution-requested.v16.example.json`、`runtime/crates/protocol/{src/lib.rs,tests/execution_contract.rs}`、`runtime/apps/{worker,runtime-host}/src/lib.rs`、`runtime/apps/{worker,runtime-host}/tests/{assignment,standalone_run}.rs`、`docs/evidence/2026-08-10-root-session-immutable-branches.md` |
| 协议中立上下文压缩 | typed transcript 保留 assistant narrative、Tool Call 与绑定 Tool Result；只在移除前缀和保留尾部两侧 Tool 对都完整时切分。摘要请求不暴露 Tool，结果以普通 User 消息回灌并保留原 System；来源/前缀/尾部/策略摘要在 provider egress 前 Checkpoint。摘要现与普通 Turn 共用冻结多 Provider 路由；真实 HTTP 503 可切到 fallback，新 Host 也可使用同一边界恢复且不重放 Tool，用量计入原 Run 预算并产生 `context.compacted` | `ADR-0058`、`ADR-0065`、`contracts/events/run-execution-requested.v13.example.json`、`runtime/apps/{worker,runtime-host}/src/lib.rs`、`runtime/apps/{worker,runtime-host}/tests/{assignment,standalone_run}.rs`、`docs/evidence/2026-08-09-protocol-neutral-context-compaction.md`、`docs/evidence/2026-08-10-standalone-multi-provider-safe-failover.md` |
| 显式历史导入与修复 | schema 15 将 external/truncated 原始消息放在独立低权限边界；只移动唯一归属 Result、合成缺失 Result、丢弃孤立/重复 Result，System/非法角色/重复 Call ID fail-closed。source/repaired digest 与四类计数进入 Checkpoint v19 和本地结果；相同导入可跨 Host 恢复，漂移在模型前拒绝，历史 Tool 永不进入执行队列 | `ADR-0060`、`contracts/events/run-execution-requested.v15.example.json`、`runtime/crates/protocol/{src/lib.rs,tests/history_repair_contract.rs}`、`runtime/apps/{worker,runtime-host}/src/lib.rs`、`runtime/apps/runtime-host/tests/standalone_run.rs`、`docs/evidence/2026-08-09-explicit-history-import-repair.md` |
| MCP 共享准入 | gateway client 的全部 clone 共享默认 32 槽的进程内发现调度器；逐 tenant round-robin，保留逐 Run 冻结并发，整批 deadline 覆盖排队时间，超时取消立即归还容量；真实两租户/五服务器 socket 验证总上限、公平和取消后复用 | `ADR-0042`、`runtime/apps/worker/src/mcp_gateway.rs`、`runtime/apps/worker/tests/mcp_end_to_end.rs`、`docs/evidence/2026-08-09-mcp-fair-admission.md` |
| MCP 异步发现与单写协调 | 无 NATS 依赖地并行执行逐 attempt 网络发现；后台任务只返回不可变结果。Coordinator 仅接受 attempt ID，从 Worker 权威状态取得命令与取消令牌，串行完成目录挂载；新 Run 挂载后才 Start，恢复 Run 挂载后必须通过 Checkpoint 目录与发现策略校验。真实慢 Run 阻塞时快 Run 进入 ModelInvocation，真实恢复只在精确目录重建后返回可恢复 | `ADR-0043`、`ADR-0044`、`mcp_discovery_supervisor.rs`、`mcp_discovery_coordinator.rs`、`mcp_end_to_end.rs`、`docs/evidence/2026-08-09-mcp-single-writer-coordinator.md` |
| 独立 Host MCP 闭环 | `McpFederationBackend` 将 Kernel/Coordinator 与 gRPC 解耦；独立 Host 支持 credential-free HTTP 与持久 stdio session。真实模型自主调用 Tool，结果回灌后成功；新 Host 重建目录并恢复且不重放。v11 区分 required/optional，安全目录发现可在冻结预算内重试；schema 10 绑定 retry policy、required 与 server authority。stdio 目录缓存命中前要求真实 MCP `ping`，另有失败 session 替换、active lease、idle TTL、zero-lease LRU 和生命周期快照；Tool 调用不缓存、不重试，退出/失败/淘汰均回收完整进程组 | `ADR-0045`—`ADR-0048`、`ADR-0107`、`runtime-host/src/lib.rs`、`stdio_mcp.rs`、`standalone_run.rs`、`docs/evidence/2026-08-14-runtime-liveness-and-process-wait.md` |
| 独立 Host 角色子代理（有界并发闭环） | 二进制读取有界角色 JSON；同一 Tool turn 的相邻 `agent.spawn` 最多 8 路真实并发，完整批次先写 Checkpoint，普通 Tool 保持顺序屏障。子 Run 使用独立 Worker、谱系、事件与 Checkpoint，只取得角色指令、权限/MCP/可再派生角色子集和预算；Token/费用预留按父余额累计，实际子用量经摘要结算且结果重放不重复计费。结果按原 Tool Call 顺序回送；单子失败保留兄弟结果。父取消关闭批次全部模型流；部分完成崩溃后只恢复无结果收据子任务。子 Tool 审批仍经父 IPC 持久路由并跨崩溃幂等恢复 | `ADR-0049`、`ADR-0050`、`ADR-0051`、`ADR-0052`、`runtime/apps/runtime-host/src/{lib,ipc}.rs`、`runtime/apps/runtime-host/tests/{standalone_run,subagent_approval,subagent_cancellation,subagent_concurrency}.rs`、`runtime/apps/worker/tests/assignment.rs`、`docs/evidence/2026-08-09-{standalone-role-subagent,standalone-nested-approval,bounded-parallel-subagents}.md` |
| Host-owned cancellation | caller token 下派 Host 私有 child domain；显式 shutdown 取消后等待，异常 Drop 取消并 abort 未完成子任务。真实 parent/child HTTP 连接先关闭，replacement 再恢复同一 async handle，spawn 不重放 | `ADR-0074`、`runtime/apps/runtime-host/src/lib.rs`、`runtime/apps/runtime-host/tests/subagent_concurrency.rs`、`docs/evidence/2026-08-10-host-owned-cancellation-domain.md` |
| 持久异步子代理会话 | 显式 async spawn 先写 Checkpoint 再返回稳定 `agent_id`；wait/send/close、FIFO 与 interrupt 保持有界幂等。`agent.fork` 从 completed ordinal 创建独立、预算收缩的新句柄；`agent.rollback` 在同一 handle 上递增 generation，旧代不可变保留。所有 handle 的 pending/active/queued 工作共享 schema-22 Token/费用/时长预留账本，result/close/cancel/恢复精确结算或重建。真实 HTTP + 原生 Tool 证明 Fork/Rollback 不重放，双 handle 实测只获 400+300 而非 400+400。仍缺 reasoning/private item、多模态和专用投递退避 | `ADR-0054`—`ADR-0063`、`runtime/apps/{worker,runtime-host}/src/lib.rs`、`runtime/apps/runtime-host/tests/{standalone_run,subagent_concurrency}.rs`、`runtime/apps/worker/tests/assignment.rs`、`runtime/crates/protocol/tests/{subagent_recovery_contract,history_repair_contract}.rs`、`docs/evidence/2026-08-09-{subagent-transcript-capsule,explicit-history-import-repair,generation-bound-subagent-fork,generation-fenced-subagent-rollback}.md`、`docs/evidence/2026-08-10-tree-wide-subagent-budget-reservation.md` |
| 可恢复执行时限 | Worker 用单调 active slice 计时，Checkpoint schema 12 保存累计执行时间、恢复桥和暂停态；活跃崩溃间隔保守计费，UTC 回拨 fail-closed，审批 required/rebound 暂停、决定后恢复。父时钟覆盖完整子树并下传剩余上限；到期沿共享 token 关闭模型、Shell 进程组、MCP discovery/call 与子代理，统一产生唯一 `run.timed_out`。独立 Host 全边界已实跑；可选 NATS Worker 已接线但本阶段未启动外部 NATS 验收 | `ADR-0053`、`runtime/apps/{worker,runtime-host}/src/lib.rs`、`runtime/apps/runtime-host/tests/{approval_flow,daemon_recovery,execution_cancellation,subagent_concurrency}.rs`、`runtime/apps/worker/tests/assignment.rs`、`docs/evidence/2026-08-09-recoverable-tree-duration.md` |
| Tool 策略 / Checkpoint | delegated scope、allow/deny/ask、副作用、隔离等级、调用与实现摘要联合绑定；审批保持顺序屏障。相邻 Pure 调用按冻结上限真实并发，乱序完成先暂存、按原 Tool Call 顺序提交；Checkpoint 恢复只重试未完成 Pure 调用 | `ADR-0068`、`runtime/crates/kernel/tests/tool_registry.rs`、`runtime/apps/worker/tests/assignment.rs`、`runtime/apps/runtime-host/tests/standalone_run.rs`、`checkpoint.rs` |
| 持久进程会话治理 | schema 2 持久化绝对 deadline、idle、全局/tenant/Workspace 配额、CPU/输出/可选内存上限及终止原因；容量满 fail-closed，不 LRU 淘汰 live process。原 Host 动态监督，replacement sweep 按原预算终止同一 PID；真实 Agent Loop 看见 typed 终止结果。macOS 无内存硬限额，Linux 内存路径未 live 验证 | `ADR-0070`、`ADR-0071`、`runtime/crates/tool-runtime/tests/{persistent_process_session,process_session_governance,process_session_sweeper_crash}.rs`、`runtime/apps/runtime-host/tests/process_session_loop.rs`、`docs/evidence/2026-08-10-persistent-process-session-governance.md` |
| 持久 Process Session / PTY | `process.start {tty:true}` 只有独立 supervisor 可持有 master；v3 Hello/Start 在 spawn 前绑定 expected generation。start/write/wait 共享有界 yield/cursor/observer，本地 write 在副作用后通知共享观察器，文件真相保留跨 Host fallback。start 使用唯一 Manifest 收据，write 使用提交后原子收据，close 使用副作用前 intent + `Terminated/Closed` Manifest；真实 Host replacement 已证明 start 只启动一次、write 只发送一次、关闭中崩溃后也不重发 close。64 live Session / 1024 wait 与 identity lease 门禁保持通过。仍缺 Windows、viewer/Node relay | `ADR-0094`—`ADR-0101`、`ADR-0107`、`runtime/crates/tool-runtime/src/{lib,process_session{,/pty_supervisor}}.rs`、`runtime/apps/runtime-host/{src/lib.rs,tests/process_session_loop.rs}`、`docs/evidence/2026-08-14-runtime-liveness-and-process-wait.md` |
| 进程资源 capability | 公开 backend guarantee vector；operator 可要求 PID 上限和整树计量，缺少能力时在任何状态或进程创建前 typed fail-closed。capability 与要求进入治理/Tool 摘要。当前 Mac 只保证 CPU-time 与 coarse output-file，Linux cgroup 未实现 | `ADR-0072`、`runtime/crates/tool-runtime/tests/process_resource_capabilities.rs`、`docs/evidence/2026-08-10-explicit-process-resource-capabilities.md` |
| Linux cgroup v2 协议边界 | 显式 backend config；预校验后写五个限制文件，最终组件拒绝 symlink，membership 写 `0`，CPU parser 要求唯一 `usage_usec`。非 Linux 和尚未接完生命周期的 Linux 均在状态创建前拒绝；普通文件测试不等于真实 cgroup enforcement | `ADR-0073`、`runtime/crates/tool-runtime/src/process_resources.rs`、`runtime/crates/tool-runtime/tests/process_resource_capabilities.rs`、`docs/evidence/2026-08-10-linux-cgroup-v2-protocol-boundary.md` |
| 持久资源身份与 pre-exec membership | schema 3 摘要绑定确定性 backend identity 和 aggregate CPU 观测位；schema 2 终态迁移为 Unix rlimit。父进程预开 controller fd，真实 child 在 exec 前写 membership；配置失败回滚空组、既有组拒绝接管。生产 cgroup 仍禁用 | `ADR-0075`、`runtime/crates/tool-runtime/src/{process_resources,process_session}.rs`、`runtime/crates/tool-runtime/tests/{persistent_process_session,process_session_governance}.rs`、`docs/evidence/2026-08-10-durable-resource-identity-and-pre-exec-membership.md` |
| cgroup 身份驱动监管与终止 | schema 3 identity 驱动 `cpu.stat` 单调计量、`cgroup.events` 存活和 `cgroup.kill` 整组终止；CPU 超额形成 durable `cpu_limit`，backend 与 Manifest 不一致拒绝访问。普通文件测试已完成，真实 Linux 与终态组清理未完成，生产后端仍禁用 | `ADR-0076`、`runtime/crates/tool-runtime/src/{process_resources,process_session}.rs`、`docs/evidence/2026-08-10-identity-driven-cgroup-observation-and-termination.md` |
| fd-relative cgroup 生命周期 | root/group directory descriptor 固定单次操作权限边界；`mkdirat/openat/unlinkat` 覆盖创建、失败回滚、限制、membership、观测与 kill，路径替换不能重定向已打开操作。Manager pinning 与终态清理由 ADR-0078/0079 完成；真实 Linux 仍缺，生产后端禁用 | `ADR-0077`、`runtime/crates/tool-runtime/src/{process_resources,process_session}.rs`、`docs/evidence/2026-08-10-fd-relative-cgroup-lifecycle.md` |
| Manager 生命周期 cgroup root identity | 公开配置与私有 resolved backend 分离；Manager 一次打开 root 并用 `Arc` 传播到 watcher/supervisor/前台操作，后续路径替换不能改变长生命周期权限锚点。启动/终态及崩溃资源阶段已由 ADR-0079/0080 接通；真实 Linux 仍缺，生产后端禁用 | `ADR-0078`、`runtime/crates/tool-runtime/src/process_session.rs`、`docs/evidence/2026-08-10-manager-lifetime-cgroup-root-identity.md` |
| Linux cgroup 启动/终态生命周期 | start 在 spawn 前通过 Manager root 准备组并安装 pre-exec membership；pre-spawn 失败回滚。终态先持久化，再 fd-relative 幂等清理，替代 Host sweep 可重试。持久 cleanup phase 已由 ADR-0080 完成；真实 Linux 仍缺，生产后端禁用 | `ADR-0079`、`runtime/crates/tool-runtime/src/{process_resources,process_session}.rs`、`docs/evidence/2026-08-10-linux-cgroup-start-and-terminal-lifecycle.md` |
| 持久资源阶段与 Starting reconciliation | Manifest schema 4 绑定 unprepared/prepared/active/cleanup_pending/cleaned；缺失组与控制器歧义分型。替代 Host 对干净 pre-spawn 崩溃收敛为 RecoveredMissing，对可能已执行的组先 kill 再持久化 Indeterminate，禁止自动重放。旧 schema 与跨 backend 歧义已由 ADR-0081 收紧；真实 Linux 仍缺 | `ADR-0080`、`runtime/crates/tool-runtime/src/{process_resources,process_session}.rs`、`runtime/crates/tool-runtime/tests/{persistent_process_session,process_session_governance}.rs`、`docs/evidence/2026-08-10-durable-process-resource-phase-and-starting-reconciliation.md` |
| Schema 5 启动歧义与旧版本迁移 | 每个 backend 都在 spawn 前持久化 prepared；schema 2/3/4 Starting 迁移为 legacy_unknown。只有当前 unprepared 且无身份可判定 RecoveredMissing，prepared/legacy 的空、缺失或存活资源都保守形成 Indeterminate。真实 active schema-2 Unix 进程可由替代 Manager 重附着并升级，不会重启 Tool。同步 spawn 失败已由 ADR-0082 收口；真实 Linux 仍缺 | `ADR-0081`、`runtime/crates/tool-runtime/src/process_session.rs`、`runtime/crates/tool-runtime/tests/{persistent_process_session,process_session_governance}.rs`、`docs/evidence/2026-08-10-schema-five-launch-boundary-and-legacy-starting-safety.md` |
| Schema 6 持久启动失败 | 同步 spawn 错误先持久化 Terminated/start_failed，再清理资源；调用方获得失败 Session ID。活动 schema 5 经摘要校验、原进程重附着并重写 schema 6。类型传播已由 ADR-0083 补齐；真实 Linux 仍缺 | `ADR-0082`、`runtime/crates/tool-runtime/src/process_session.rs`、`runtime/apps/runtime-host/tests/subagent_approval.rs`、`docs/evidence/2026-08-10-schema-six-durable-process-start-failure.md` |
| 确定性启动失败类型传播 | ToolExecutor 保留 StartFailed 身份；Worker/Host 只公开安全 code/message/session_id。只有确定性 pre-side-effect 失败可回到 Agent Loop；其他模糊非幂等错误不通过该转换器。真实 loopback HTTP Agent Loop 与持久事件日志闭环已通过 | `ADR-0083`、`runtime/crates/tool-runtime/src/{lib,process_session}.rs`、`runtime/apps/{worker,runtime-host}/src/lib.rs`、`docs/evidence/2026-08-11-typed-deterministic-process-start-failure.md` |
| 在线 Tool 失败确定性 | Worker/Host 按冻结 effect 和 durable started 证据分流执行器错误：确定性未执行及 Pure/Idempotent 返回脱敏 Tool Result，其他 NonIdempotent/Unknown 形成绑定 `run.indeterminate`。真实 HTTP Agent Loop 执行文件副作用后故障，终态 Checkpoint、Applied 裁决、独立 continuation 和零重放已闭环 | `ADR-0084`、`runtime/apps/worker/{src/lib.rs,tests/assignment.rs}`、`runtime/apps/runtime-host/src/lib.rs`、`docs/evidence/2026-08-11-effect-aware-live-tool-failure.md` |
| MCP 接受后响应丢失 | stdio 只对未入队请求及安全 discovery 重连，已接受 `tools/call` 的 actor loss 禁止 replacement 重发；真实 Streamable HTTP MCP 执行副作用后截断响应，Host 持久 `run.indeterminate` 且调用一次、无 Tool Result | `ADR-0085`、`runtime/apps/runtime-host/src/stdio_mcp.rs`、`runtime/apps/runtime-host/tests/standalone_run.rs`、`docs/evidence/2026-08-11-mcp-accepted-call-response-loss.md` |
| MCP Tool effect authority | RunExecution v18 仅接受操作员 Run 快照中的 per-server Tool effect；缺省 Unknown，签名 Skill/委派 scope/本地 allowlist 三重约束，MCP annotations 无权降级。所有 MCP Tool 仍 Ask + Federated；binding digest 拒绝恢复漂移。真实 HTTP 断流验证 Unknown/Idempotent 两条终态且均零自动重放 | `ADR-0086`、`contracts/events/run-execution-requested.v18.example.json`、`runtime/crates/protocol/tests/execution_contract.rs`、`runtime/apps/worker/tests/federated_tools.rs`、`runtime/apps/runtime-host/tests/standalone_run.rs`、`docs/evidence/2026-08-11-run-frozen-mcp-tool-effects.md` |
| Model Routing / Provider Registry | 地域、数据等级、能力、健康和预算过滤；租户级三协议 Provider、写入即封装的 BYOK、最多 8 个有序候选；只在尚无输出且 429/超时/不可用时切换，认证/账单/协议错误或部分输出后禁止重放 | `ADR-0028`、`provider_registry.rs`、`failover.rs`、对应 Rust/Java 集成测试 |
| OpenAI-compatible Adapter | 真实 HTTP/SSE、文本与 Tool Call 流、Tool Call/Result 多轮历史、用量计费、完成原因、取消、双层超时和错误分类；凭证及私有对象 URI 出口受控 | `runtime/apps/model-gateway/tests/openai_compatible.rs` |
| OpenAI Responses Adapter | typed input items、Function Call/Output 历史、`text.format`、reasoning effort、summary/encrypted continuation、typed refusal、Tool Call、用量计费和严格终止事件 | `ADR-0067`、`runtime/apps/model-gateway/tests/openai_responses.rs` |
| Anthropic Messages Adapter | `system`/Messages 转换、Tool Use/Result、分片 Tool JSON、thinking/signature 与 redacted thinking 的私有保留/同源回放、用量和严格 `message_stop` | `ADR-0067`、`runtime/apps/model-gateway/tests/anthropic_messages.rs` |
| Provider 协议选择 | Gateway 和原生 Supervisor 通过稳定协议名选择三类 Adapter；旧本地配置缺少协议时兼容默认 Chat Completions | `runtime/apps/model-gateway/tests/provider_protocol.rs`、`deploy/tests/native_supervisor_lifecycle_test.rb` |
| Worker→Model Gateway | 版本化 gRPC 流、Ed25519 短期身份、tenant/run/attempt/worker/incarnation 精确绑定；v3 身份把不可变 ModelPolicy 快照摘要绑定到 RunExecution v4，Worker 只能转发密文，不能替换端点或解密 BYOK；真实双向 TLS、流式事件和取消；401 每代最多恢复一次 | `contracts/proto/model_gateway.proto`、`runtime/crates/workload-identity/tests/token_contract.rs`、`provider_registry.rs`、`model_gateway_transport.rs` |
| Checkpoint Gateway | 独立 Rust gRPC 数据面；Worker 只持短期工作负载令牌，Gateway 独占 S3/MinIO 凭证；内容地址、大小、tenant/run/attempt/worker/incarnation 和 read/write scope 均验证；真实 mTLS、MinIO PUT/GET、跨租户/旧实例拒绝、对象缺失与损坏恢复分流已测试 | `contracts/proto/checkpoint_gateway.proto`、`runtime/apps/checkpoint-gateway/tests/grpc_contract.rs`、`minio_transport.rs`、`runtime/apps/worker/tests/checkpoint_gateway_transport.rs`、真实 NATS `transport.rs` |
| Java Run API | OIDC Scope、tenant/application 声明、显式模型策略、幂等 Run 创建、Run 列表 | `RunControllerTest`、`RunServiceTest` |
| Runtime 资源配置 | JWT 固定 Tenant/Application；Workspace→Agent→不可变 AgentVersion→Provider→ModelPolicy→Session API 由服务端生成 ID并验证同一 Application；Provider API Key 是 write-only，API/日志/数据库均不保存明文；Agent 指令、子代理角色目录和 Provider 候选均进入不可变配置 | `ADR-0027`、`ADR-0028`、`ADR-0030`、`RuntimeResourceControllerTest`、`JdbcRuntimeResourceRepositoryIntegrationTest`、`execution_contract.rs` |
| Skill Registry / 动态激活 | Tenant/Application 下发布不可变 SkillVersion；控制面用独立 Ed25519 密钥签名 canonical artifact，AgentVersion 固定有序绑定；Scheduler 下发 RunExecution v5，Worker 验签、校验平台/最低版本，再把 Skill Tool 声明与预装可信目录及 delegated scope 求交集；Skill 不能扩权，Checkpoint 绑定有效指令和有效目录摘要 | `ADR-0029`、`V20__signed_skill_registry.sql`、`Ed25519SkillArtifactSignerTest`、`execution_contract.rs`、`assignment.rs`、原生真实恢复主链 |
| PostgreSQL | 租户复合外键、RLS、ModelPolicy 同 Workspace 约束、Run + Outbox 原子提交、事件续传查询 | 独立数据库的原生 PostgreSQL 集成测试 |
| Outbox / JetStream | 完整 RunQueued v1 快照、claim lease、失败释放、PubAck 后确认、NATS 消息 ID 去重 | 原生 PostgreSQL/NATS 集成测试与真实 NATS 暂停恢复 |
| Workspace Lease | owner epoch、fencing token、过期接管、旧所有者续租失败 | `JdbcRunRepositoryIntegrationTest` |
| Scheduler | 健康容量过滤、Workspace 单写租约、幂等 dispatch、定向执行命令；生成含 Provider、Skill、数据库谱系和权限内子代理角色目录的 RunExecution v7；根任务取得 AgentVersion 权限，子任务只取得所选角色的职能指令和权限子集；恢复保持相同不可变绑定 | `JdbcSchedulerRepositoryIntegrationTest`、`Ed25519WorkloadTokenIssuerTest`、`execution_contract.rs` |
| 子代理身份基础 | V21 用租户复合外键、唯一 delegation、深度 0–3 和角色约束保存 Run 谱系；v7 命令携带 root/parent/delegation/depth/role 及权限内角色目录；Worker Checkpoint v3 绑定谱系和角色目录并拒绝恢复漂移；AgentVersion 可配置最多 16 个不可变角色及权限子集 | `ADR-0030`、`V21__agent_run_lineage.sql`、`RuntimeResourceServiceTest`、`JdbcSchedulerRepositoryIntegrationTest`、`execution_contract.rs`、`assignment.rs` |
| 子代理原子准入 | 父 Run 行锁内校验终态、深度、角色、权限子集、每父最多 8 个活动子任务和 Token/费用/时长保守预留；同一 delegation 精确重放幂等、不同意图冲突；子 Run 与 RunQueued Outbox 同事务提交，并发请求不会超卖预算 | `ADR-0030`、`JdbcSubagentAdmissionRepository`、`JdbcSubagentAdmissionRepositoryIntegrationTest` |
| 子代理挂起交接 | Worker 将权限内角色暴露为内建 `agent.spawn`，生成确定性 delegation 和摘要绑定请求，先把父 Run 挂起并写入 Checkpoint v3；V22 在检查点事务内原子完成请求登记、子 Run/Outbox 创建、父 dispatch 挂起、容量归还及 Workspace 租约释放；挂起父状态留驻 Worker 但不计容量、不续租，避免把 Broker PubAck 误当控制面提交确认 | `ADR-0032`、`V22__durable_subagent_handoff.sql`、`assignment.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| 子代理结果与取消 | V23 将子终态事件、受限结果、摘要、恢复 attempt 和回执保存为权威账本；Reconciler 为父 Run 获取新 owner epoch/fencing 并发送 Recovery v2；Worker 验证结果与挂起 Tool Call 后以原 call ID 回灌模型历史，持久化恢复/结果事件与运行中检查点；父取消会递归定向活动子树，未派发后代原子终止，挂起父的当前 attempt 可精确确认取消终态，迟到结果不能复活父 Run | `ADR-0033`、`V23__durable_subagent_results.sql`、`subagent_recovery_contract.rs`、`assignment.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| 持久 Run steering | `POST /v1/runs/{id}:steer` 与 Console 支持同一公开 Run 内重定向；PostgreSQL 以 Tenant/Application 与幂等键保存命令，定向当前 attempt/incarnation；Worker 取消旧模型流，先把输入和回执写入 Checkpoint 再继续，Recovery v3 会把未完成命令重绑新围栏且不重复输入；过期/永久拒绝先发布精确绑定负回执，控制面只让合法回执把账本收敛为 rejected | `ADR-0034`、`V24__durable_run_steering.sql`、`V25__run_steering_outcomes.sql`、`RunControllerTest`、`JdbcSchedulerRepositoryIntegrationTest`、真实 NATS `transport.rs`、Console Vitest 与真实 Chrome、`docs/evidence/2026-08-02-native-steering-outcomes-and-one-command-run.md` |
| 工作负载身份续期 | PostgreSQL 保存 generation/expiry；心跳进入半租期阈值才原子轮换并写 Outbox；命令与签名 Token 共用一个显式 `issued_at`，Worker 验签全部绑定和三项能力后原子替换，旧代际拒绝、精确重投幂等；真实 JetStream 目标实例消费与原生恢复主链均验证 | `ADR-0019`、`V11__workload_identity_renewal.sql`、`Ed25519WorkloadTokenIssuerTest`、`JdbcSchedulerRepositoryIntegrationTest`、`assignment.rs`、真实 NATS `transport.rs` |
| Rust Worker Transport | 定向 JetStream 消费、过期租约拒绝、重复 attempt 幂等、心跳与 accepted 事件；生产连接强制 NATS TLS、CA、角色凭证，Worker 只读预建 Stream 且连接状态进入 readiness | `runtime/apps/worker/tests/assignment.rs`、真实 NATS `transport.rs`、`nats-security/tests/live_tls.rs` |
| Worker 启动实例隔离 | 稳定 Worker 与每次启动 incarnation 分离；V9 保存实例历史和单向前进的 current 指针；执行、恢复、取消、审批、accepted 与续租均精确绑定实例；Kubernetes StatefulSet 用独立 PVC 保存稳定 UUID，损坏身份 fail-closed | `V9__worker_incarnations.sql`、`execution_contract.rs`、`assignment.rs`、`worker_identity.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| Worker 安全下线 | SIGTERM/SIGINT 先原子撤销 readiness 与新任务准入；draining heartbeat 携带 deadline，控制面持久化单向状态并排除新调度/恢复；活动 assignment 继续续租和完成，90 秒到期发布最新安全 Checkpoint 后退出；Kubernetes 保留 30 秒 teardown 余量 | `ADR-0021`、`V12__worker_draining.sql`、`assignment.rs`、真实 SIGTERM/JetStream 测试、`validate_kubernetes.rb` |
| 恢复事故与故障证据 | V13 持久化 waiting capacity、recovery requested、recovered、indeterminate 及原始故障时钟；按租户输出 15 分钟 SLO 快照；健康与 SLO 使用控制面心跳接收时间；750ms 缩放租约、真实 NATS pause/resume、Checkpoint Store 短暂不可用后的 JetStream 重投递均已验证 | `ADR-0022`、`V13__recovery_incidents.sql`、`JdbcSchedulerRepositoryIntegrationTest`、`NatsJetStreamMessageBusIntegrationTest`、真实 NATS `transport.rs` |
| 恢复指标与告警 | V14 以不含租户标识的事务汇总桶提供全局恢复快照；Prometheus 低基数 Gauge、采集陈旧与错误指标；9090 独立管理端口、健康匿名、指标专用凭证；可选三类 PrometheusRule | `ADR-0023`、`V14__recovery_metric_rollup.sql`、`RecoveryMetricsCollectorTest`、`ManagementEndpointIntegrationTest`、`deploy/kubernetes/observability/` |
| macOS 原生开发基线 | 零 Compose/Docker/Kubernetes 本地命令图；项目级 PostgreSQL/NATS、外网统一走系统 `127.0.0.1:10808`、回环直连；项目 CA、mTLS、独立 Ed25519 工作负载/Skill 身份、RSA 本地 JWT、RSA-3072 Provider 凭证密钥和 bcrypt 角色凭证；Provider 通过正式控制面 API 封装并原子绑定开发策略；`make dev-run` 单命令真实主链已验收；最新 steer 主链 RSS 399504 KiB（390.1 MiB），完整故障恢复主链 677040 KiB（661.2 MiB）；`dev-clean` 清除 PID、端口、日志、全部本地密钥、状态和构建/测试产物 | `ADR-0024`、`ADR-0029`、`deploy/native/{devctl,with-download-proxy,configure-local-model-provider,service-runner}`、native Ruby/live Chrome 门禁 |
| Worker Execution Supervisor | 每 attempt 异步模型 RPC、主循环串行 Kernel 事件、并发取消、流错误分类、终态 PubAck 后释放容量 | `runtime/apps/worker/tests/model_gateway_transport.rs`、真实 NATS `transport.rs` |
| Worker Tool Turn 编排边界 | Tool 定义按 delegated scope 暴露；完整 Tool Call 后规划策略；审批绑定调用摘要；Tool Result 保持 call ID 进入下一轮 ModelInvocation | `runtime/apps/worker/tests/assignment.rs`、`runtime/crates/kernel/tests/tool_registry.rs` |
| 受限容器 Tool Runtime | OCI digest 固定、绝对引擎路径、无 Shell argv、只读 Workspace、断网/只读根文件系统/去 capability、stdin 参数传递、超时/取消、有界输出及结果双重绑定 | `runtime/crates/tool-runtime/tests/restricted_container.rs` |
| 可信原生 Tool Runtime | 本地显式 opt-in；可信根目录、固定可执行文件 SHA-256、执行前重验、无隐式 Shell、清空环境、Workspace 绑定、超时/取消和有界输出；一次性 `workspace.*` 与持久 `process.*` 都使用普通 scope/审批/绑定摘要。持久会话另有 UUID、digest Manifest、输出 cursor、跨 Host identity/PGID 恢复及整组回收 | `ADR-0025`、`ADR-0070`、`runtime/crates/tool-runtime/tests/{trusted_native,persistent_process_session}.rs`、`runtime/apps/runtime-host/tests/process_session_loop.rs` |
| 原生 Skill/Provider/Tool/审批/恢复实跑 | API 动态发布签名 SkillVersion 并绑定 AgentVersion；模型精确收到合并后的 Agent + Skill 指令，只看到 Skill 声明的可信 Tool；两个 Provider 完成两次 429 安全切换；强杀 Worker 后 owner epoch 1→2，真实 Chrome 审批后 Tool 只执行一次，13 个 SSE 事件完整重放；独立临时根目录最终自动清理 | `docs/evidence/2026-08-02-native-signed-skill-activation.md`、`native_trusted_tool_recovery_live_test.rb`、Console 单元/三视口 E2E |
| 自动 Tool 闭环 | 模型 Tool Call 完成后自动规划并持久化执行请求；Tool Result PubAck 后自动携带 tool-role 历史发起下一模型回合；策略/人工拒绝均生成模型可见错误 | `runtime/apps/worker/tests/transport.rs`、`assignment.rs`、`tool_execution_supervisor.rs` |
| Tool Execution Ledger | V7 权威账本记录 planned/started/completed、调用摘要、副作用和沙箱；started PubAck 后才启动进程；结果摘要错配不入库；Worker 丢失会记录具体模糊副作用证据 | `JdbcSchedulerRepositoryIntegrationTest`、真实 NATS `transport.rs` |
| Worker Checkpoint | v3 快照保存 Kernel sequence、Protobuf transcript、待处理/执行中 Tool、待审批/子代理请求、角色目录、Tool Catalog 摘要、累计 Token/费用和待发布预算终态，并兼容 v1/v2；用量事件先持久化，崩溃恢复后不会再次调用模型；Zstd 后小对象内联、大对象生成内容寻址引用；新 attempt 以更高 owner epoch 和新 fencing 恢复，目录漂移与非幂等模糊执行 fail-closed | `ADR-0031`、`ADR-0032`、`runtime/apps/worker/tests/assignment.rs`、`runtime/apps/worker/tests/transport.rs`、`runtime/crates/protocol/tests/checkpoint_contract.rs` |
| Checkpoint 权威索引 | V10 `run_checkpoints` 使用 RLS、复合 dispatch 外键、双摘要、编码与压缩前后长度；只允许 payload/ref 二选一，恢复 Outbox 保持对象引用；仅最新 sequence、相同状态/租约且无模糊副作用时判定 SAFE | `JdbcSchedulerRepositoryIntegrationTest`、`checkpoint_contract.rs`、`RunCheckpointMessageTest` |
| 自动围栏恢复 | Checkpoint 获得 PubAck 后由控制面持久化；过期 accepted dispatch 可在其他健康 Worker 或同一稳定 Worker 的新实例上创建新 attempt、递增 owner epoch、轮换 fencing 与工作负载身份；新实例发布 `run.restored` | `runtime/apps/worker/tests/transport.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| 恢复中的 Tool / 审批 | pure/idempotent Tool 在新 attempt 重新产生 requested/started 屏障；pending approval 原子重绑新 Worker，并以 `approval.rebound` 重建新 Tool Ledger；若决定已持久化但旧 Worker 尚未接收，最新 approved/denied 审批和同版本决定会重绑、重发给 replacement attempt | `ADR-0011`、`runtime/apps/worker/tests/assignment.rs`、`transport.rs`、`JdbcSchedulerRepositoryIntegrationTest` |
| Kernel 执行入口 | Worker 为 accepted attempt 创建 Kernel 状态机，幂等产生 `run.started`，并从 dispatch 构造绑定身份、租约和预算的首轮 ModelInvocation | `runtime/apps/worker/tests/assignment.rs`、`model_gateway_transport.rs` |
| 租约与恢复协调 | fenced assignment 心跳续租、dispatch 多 attempt 历史、requested 重排、SAFE accepted 自动接管、模糊非幂等失败终止 | `JdbcSchedulerRepositoryIntegrationTest` |
| Worker Event 入库 | 校验 event/attempt/sequence/digest，JetStream 重投不会重复写入 | `JdbcSchedulerRepositoryIntegrationTest`、`NatsRunQueuedConsumerIntegrationTest` |
| 取消控制 | 未调度 Run 原子取消；运行中、等待审批或挂起的 Run 通过 Outbox 定向当前 Worker/attempt，命令有有效期且可重复投递；父取消递归覆盖全部未终态子孙并封闭子代理结果账本 | `RunControllerTest`、`JdbcRunRepositoryIntegrationTest`、`JdbcSchedulerRepositoryIntegrationTest`、真实 NATS `transport.rs` |
| 持久审批与恢复命令 | `approval.required` 原子入库并进入 waiting；待审批按 tenant/application 与 Scope 隔离；决定 API 使用版本锁。V17 将 `allow_session` 限于 pure/idempotent，并精确绑定 Session、Workspace、AgentVersion、参数与 Tool 策略；重复调用由控制面生成当前调用的 allow-once，参数/策略漂移重新审批；RLS 隔离 Grant | `ADR-0026`、`ApprovalControllerTest`、`ToolApprovalScopeTest`、`JdbcSchedulerRepositoryIntegrationTest`、`TenantIsolationIntegrationTest`、`openapi_approval_contract_test.rb` |
| 模型事件与唯一终态 | 模型增量/Tool Call/用量/完成/分类失败生成单调事件；Tool Call 只结束模型回合；首终态胜出，终态 PubAck 后才释放 Worker 槽位 | `runtime/crates/kernel/tests/run_state_machine.rs`、`runtime/apps/worker/tests/assignment.rs` |
| 终态资源释放 | PostgreSQL 同一事务持久化终态事件、完成 dispatch、释放 Workspace 租约并归还容量 | `JdbcSchedulerRepositoryIntegrationTest` |
| SSE | `Last-Event-ID` 转换为持久化序号、轮询补传、终态关闭 | `RunEventControllerTest`、数据库集成测试 |
| Console | Run 状态、Workspace、预算列表；可从授权 Project 完成 Workspace→Agent→签名 SkillVersion→AgentVersion→1–8 个有序 Provider→ModelPolicy→Session；Skill 表单不暴露摘要、签名或租户 ID，只能显式选择预装可信 Tool；运行中 Run 可提交有 32 KiB UTF-8 门限、幂等键及明确反馈的 steer 输入；完成后立即成为 Run target | 24 个 Vitest、TypeScript、ESLint、Vite build、Chrome 390/768/1440 三视口配置+运行 E2E、原生 live Chrome 门禁与截图 |
| NATS 安全 | Rust/Java 客户端分别使用 PEM CA/PKCS12 TrustStore；真实 TLS、bcrypt 正确/错误密码、Worker Subject 越权和 JetStream 管理越权均验证；三节点路由配置要求 mTLS | `deploy/nats/`、`NatsConnectionSettingsTest`、`deploy/tests/verify_nats_tls.sh` |
| 语义健康检查 | `/live` 与 `/ready` 分离；Gateway 绑定 gRPC Socket 后才 ready；Worker 将 NATS 连接状态映射为 readiness | `runtime/crates/runtime-health/tests/http_health.rs`、三个 Runtime 进程入口 |
| Kubernetes 运行基线 | 双副本 Gateway Deployment/Service/HPA/PDB、三副本 Worker StatefulSet/PVC/PDB、SecretProviderClass、非 root/只读根、资源限额和必要流量网络策略；控制面 Skill 签名私钥与 Worker 验签公钥分别从 Vault 物化，部署契约校验环境变量、Secret 键和 Vault 对象三层一致 | `deploy/kubernetes/base/`、`deploy/tests/validate_kubernetes.rb`（29 个渲染资源） |
| 生产容器历史基线 | Dockerfile 与 Kubernetes 清单仅保留为未来生产交付材料；本地 Makefile 不暴露镜像构建目标，`dev`、`test`、`check` 和 live 恢复门禁均不得调用 Docker | `runtime/Dockerfile`、`control-plane/Dockerfile`、`native_command_contract_test.rb` |
| 契约与部署 | OpenAPI、Protobuf、原生 macOS 生命周期、Kata RuntimeClass、默认拒绝网络策略 | 本地原生门禁与生产清单 CI |

## 只有契约或进程骨架

- Scheduler 已下发 Provider 与签名 SkillVersion 快照；Worker 已动态激活 Skill 指令和预装可信 Tool 子集。当前仍不是任意制品装载器：不执行租户上传脚本。Edge 已有签名任务、持久 outbox 和真实 mTLS 出站 daemon 核心；生产控制面服务与证书生命周期未实现。
- Worker 已通过 `CheckpointPayloadStore` 接入独立 Checkpoint Gateway，并保证外部对象先于 Event/Checkpoint 发布；本地 MinIO、身份续期、双向 TLS 和 Kubernetes 双副本基线已验证。尚未在真实集群验证 Gateway 滚动维护、证书轮换和对象生命周期，因此仍不是生产闭环。
- OpenAPI 与 Java 已实现 Workspace、Agent、AgentVersion、Provider、ModelPolicy 和 Session 创建 API；当前仍缺资源列表、详情、重命名/归档、失败后续建和删除治理，Console 动态步骤失败只会明确保留已成功资源，不会自动补偿或恢复到下一步。
- Kubernetes Base 已有 Gateway Deployment/HPA/PDB、Worker StatefulSet/PVC、SecretProviderClass、网络策略和有界 draining 配置，并通过渲染契约；尚无云集群 apply、CSI/Vault 联调、镜像 digest Overlay、真实 eviction 与节点级故障注入证据。
- 三个 ARM64 Runtime 镜像已在本地干净 Rust 1.88 Builder 构建并验证非 root/fail-closed；尚未构建或签名 x86_64 多架构清单，也未推送到 OCI Registry 或按 digest 更新生产 Overlay。
- 原生 PostgreSQL/NATS、Java、三个常驻 Rust 服务、可信 Tool 二进制和 Vue 已真实同时运行；两个动态确定性
  Provider 已完成有序 429 安全切换→Tool→审批→Checkpoint→强杀 Worker→新 attempt/owner epoch→结果回灌→终态。
  小 Checkpoint 按协议内联 PostgreSQL，大于 512 KiB 的载荷使用内容寻址文件后端；真实浏览器已完成
  浏览器→真实控制面→恢复后 Worker 的审批闭环。外部真实模型尚未验收；确定性回环 Provider 只证明协议和执行语义，不证明第三方模型质量或稳定性。
- Java 本地默认门禁最近一次执行 137 个测试（其中 1 个可选 live 测试显式跳过）；7 个原容器集成测试类使用临时原生 PostgreSQL/NATS；控制面与
  Worker 使用不同 TLS 角色，Worker 越权主题和 JetStream 管理操作被拒绝。每类数据库相互隔离，NATS
  暂停恢复使用经 PID 身份校验的原生信号，测试结束后进程、端口和运行态均已清理。

## 尚未实现，不得对外声明

- 模糊副作用在独立 Host 已有完整本地闭环；可选 NATS adapter 虽共享 `TerminateIndeterminate` 的发布、
  Checkpoint 与 ack 顺序，本轮没有启动外部 NATS 验证 PubAck/重投递窗口。人工命令当前只有本地持久收据，
  不包含 Java API、OIDC/RBAC、PostgreSQL RLS、审计 UI 或多副本事务，因此不得宣称生产控制面处置已完成。
- 独立 Host 已可静态配置能力、地域/数据等级、健康和价格，并在出网前过滤；同 Provider 退避、
  `Retry-After`、连续失败 cooldown、half-open 单探针及跨 Host 尝试预算已实现。仍缺后台主动健康探测、
  Auth Profile/逐凭据轮换、专用 circuit 公共事件、按 Provider 精确出站策略和分布式健康状态。
- Anthropic 已保留并同源回放 thinking/signature 与 redacted thinking，但仍未覆盖 OpenClaw 的 cache 控制、
  thinking budget 激活、Provider 特定 refusal、OAuth/Foundry 与图片预算兼容层；结构化输出和图片输入会 fail-closed。
- 工作负载令牌已完成 incarnation、audience、scope、ModelPolicy 摘要绑定、持久化代际续期与 401 有界恢复；gRPC mTLS、NATS TLS/角色 ACL 和租户 BYOK 密文边界已验证。尚缺主动撤销、签名密钥与证书自动轮换、逐操作最小授权、Vault/KMS 真实联调和 Provider 级精确出站策略。
- Reconciler 已接通 SAFE Checkpoint 自动接管、持久恢复事故、按租户 SLO 快照及不含租户标签的全局 Prometheus 告警指标，Worker 也已实现计划内 draining；当前只有缩放租约、传输层故障和离线告警规则证据，真实 Prometheus/Alertmanager、Kubernetes 节点级故障注入、生产 15 分钟恢复时限证明、加权公平调度和多租户配额抢占尚未完成。
- 受控出网 HTTP 与 MCP 的 Tool 制品；Kubernetes Job/Kata 沙箱生命周期及 OCI 签名验证。
  **写文件与 Shell 已实现**（ADR-0036、0038），但都只在 macOS 上受容器约束，
  Linux 上没有 `landlock` 等价物，因此 Worker 的 Linux 路径不得注册它们。
- **Shell 恢复为全部逐次审批。** ADR-0039 的只读白名单已于 2026-08-07 撤回：
  复审发现名单把 `git branch -D`、`git tag -d`、`git diff --output=`、`uniq in out`、
  `file -C` 判为只读，构成**审批绕过**。豁免机制保留但已关闭（`AutoApproval::Never`），
  重新启用的前提是策略成为**租户决定并签入执行快照**，而不是 Worker 里的常量。
- **真实厂商云端主链已验证**（2026-08-07）：PostgreSQL + NATS + Java 控制面 + Worker +
  Model Gateway，模型自主决定工具调用。仅一个厂商一个模型；Anthropic Messages 与
  OpenAI Responses 已有真实回环 HTTP/SSE Adapter 和独立 Host 多协议切换证据，但未使用真实厂商端点。
  容器只对 `~/.ssh`、`~/.aws`、`~/.gnupg`、`~/.config/gh` 拒读，其余用户可读文件仍可读，
  所以「Shell 已容器化」不得表述为「Shell 已隔离」。
- 持久进程会话已具备 `start/write/resize/poll/wait/attach/interrupt/close`、普通 pipe 会话跨 Host reattach，
  以及唯一独立 supervisor 持有 master 的 Unix PTY 跨 Host 续接。v2 能力握手、clean/unclean 生命周期、
  有界尾部 attach、deadline、配额和输出上限均已验证；supervisor 丢失会回收并标记 `indeterminate`。
  `process.wait` 可按 cursor 等待输出/终态并在 Host replacement 后安全恢复；同一 Session 的 1000 个 wait
  已共享一个持久观察器，250ms 空闲观察与 2 秒全量唤醒门禁通过，最后一个取消者也会回收观察器。64 个
  live Session / 1024 wait 的观察次数、取消隔离、p50/p95/p100 和进程回收已连续 10 轮通过；这证明有界
  与无饥饿，不是整机 CPU 基准或生产加权公平调度。start/write/wait 的统一有界 yield 已补齐并通过真实
  Agent Loop；start/write 的丢结果收据恢复已经完成，仍没有 close 终态收据、连接级 viewer、Node relay 或 Windows ConPTY，不能
  宣称完整对齐 Codex `unified_exec` 或 OpenClaw Terminal/Node Host。OpenClaw 的 WebSocket 高低水位将在
  未来存在 live viewer transport 时由适配层实现，不复制进当前 Kernel 日志层。
- **MCP HTTP 联邦主链已实现**（ADR-0040）：逐 Run 发现、命名空间/委派作用域、逐次审批、目录冻结、
  Gateway 代持凭据、DNS 地址固定、RunExecution v10 在接纳前冻结最多 4 台并发及 deadline，
  Checkpoint schema 8 精确冻结运行策略，schema 9 进一步冻结工具目录、发现策略和 MCP Server
  authority，schema 10 冻结 discovery retry 与 required 标记；恢复后执行器重建均有 Rust 测试。Worker 默认执行单服务器 3 秒、
  整批 10 秒的双层 deadline；整批到期会保留已完成目录并取消其余请求。同目录若使用不同的并发或
  deadline 策略恢复会 fail-closed。网关客户端 clone 还共享默认 32 槽的租户轮转调度器，两个真实
  Run 的请求合计受限，取消后容量可立即复用。
  协议中立 Supervisor 与单写 Coordinator 已证明慢、快 Run 并行发现；Coordinator 从已验收 attempt
  取得权威命令，完成结果串行挂载 Kernel，新 Run 只在挂载后 Start，恢复 Run 只在精确目录和发现策略
  重建后返回可恢复。当前 NATS Worker 的 assignment/recovery poll 尚未驱动 Coordinator，因此仍未
  证明该可选传输的多 Run 异步接纳和 ack 顺序，慢 MCP 在该适配器里仍会延迟后续接单。
  **独立 Host 已直连 credential-free HTTP 和本地 stdio MCP**：使用相同 Coordinator，真实完成 Tool
  Call、结果回灌、新 Host 恢复且不重放；二进制可读取有界 JSON 配置。stdio 使用环境白名单、持久
  session、初始化/请求取消和 TERM→KILL 进程组回收，command/args/env/cwd 纳入 authority digest。
  RunExecution v11 已实现 required/optional、仅 discovery 的有限重试与逐 Server 启动状态；required
  连续失败在模型前拒绝，optional 失败可观测并继续，重试等待不占共享准入槽。stdio 已实现默认 30 分钟
  目录缓存；每次命中必须通过真实 MCP `ping`，失败 session 退休后只有新初始化会话成功探活才可复用；
  另有 active lease、默认 10 分钟 idle TTL、默认 32 session 的
  zero-lease LRU 以及显式/Drop 关闭。HTTP/stdio 请求级 cancel/progress 已进入持久事件链；独立 HTTP
  Host 已支持 2026 MRTR form/url contract、持久输入和跨进程续传；显式 `stdio2026` 使用相同能力冻结、
  metadata 与 MRTR parser，并已通过 Codex 外部严格服务。云 gRPC 仍保持 unary，但已能用新请求
  传输下一 MRTR 轮次；NATS recovery poll 尚未自动调度该续传。Resources list/read 与 Prompts list/get
  已通过协议中立 Rust/gRPC 契约贯通 HTTP/stdio/Gateway/Worker，但尚无模型内核入口与 Resource Templates。
  仍未实现 remote stdio、OAuth onboarding、2025 held-open
  elicitation、sampling/roots、Codex server cache opt-out、后台主动重连、requester 配置重验证/
  撤销、pin-aware 显式代理、持续健康/熔断与真实外部 MCP 长稳验收，
  因此生态广度仍明显落后 Codex/OpenClaw。v11 Run 不能从低于 schema 10 的 Checkpoint 恢复；任何
  低于 schema 9 的 MCP Checkpoint 也因不能证明远端 authority 而 fail-closed。
- 真实模型厂商端到端：**`openai_compatible` 已验证一次**（2026-08-07，DeepSeek 经网关，
  `runtime-host` 路径，与云端共用协议转换代码）。三协议与多 Provider 故障转移已由真实回环 HTTP/SSE
  验证执行语义。**仍未验证**：Java 控制面 + PostgreSQL + NATS 的完整云端链路对真实厂商、真实厂商的
  `openai_responses`/`anthropic_messages`，以及三协议对真实厂商限流、`Retry-After`、冷却/探针和错误响应的兼容矩阵。
- 外部调用方认证尚未实现；进程内调用必须选择预注册的完整 invocation Profile，且已有全局/tenant/
  Workspace active limit、全局/tenant queue limit 与 round-robin。协议中立 control schema 已统一本地
  resume、精确审批决定和 cancel，并有 tenant-bound durable receipt；调用 adapter 的认证/签名、远端
  delivery、分布式 command ledger 和多进程 state-root ownership 尚未实现。signed workload token、远端
  Model/MCP/Checkpoint gRPC、Tool context 与 daemon 已绑定完整 application/workload/Workspace 身份；
  控制面 v20 producer、主动撤销/密钥轮换、持久配额与跨节点公平调度仍未实现。
- Checkpoint 对象缺失、内容损坏及一次短暂 `Unavailable` 后 JetStream 重投递已做故障分流；尚缺对象保留/垃圾回收、真实 Gateway 进程或存储实例丢失和跨可用区故障注入。
- Skill 的生产 OCI 制品上传、SBOM、恶意扫描、平台审核、租户签名链与公共/私有 ACL；当前已完成控制面签名的结构化 SkillVersion 和可信 Tool 激活，不接受上传脚本。
- 子代理的 Worker 发起、父挂起检查点、原子子 Run 入队、Workspace 正反向租约交接、子结果回送、
  父 Run 新围栏恢复、父取消向子树传播和同 Run 持久 steer 已经在云控制面路径实现。独立 Rust Host
  现已完成父→角色子 Run→原 Tool Call→父、活动子模型取消、取消意图跨进程持久化、Tool/MCP 在途
  取消终态、在途同身份重启、结果回执恢复、子 Tool 审批的父路由/跨崩溃幂等恢复，以及单父最多 8 路
  并发、累计预算预留/实际用量结算、批次取消、部分完成恢复和可恢复树级 active-time 时限。独立 Host
  另已完成显式 async spawn、稳定句柄、非取消 wait、终态后 send、独立 close、关闭边恢复、父终态回收，
  以及 Checkpoint-first 消息幂等收据、运行中有界队列、持久 interrupt、句柄级 typed transcript、只读
  分页查询和三类确认后崩溃恢复。子 Run 的 narrative、Tool Call/Result 与终态 Assistant 已纳入 digest v3
  和 Checkpoint v19，并能在父结果写入前崩溃后恢复。external/truncated 历史已有独立显式修复边界，
  但不会改写句柄权威状态。handle Fork/Rollback、跨 handle 树级预算预留和 root Session Fork/Rollback
  已完成；provider reasoning/private item 已能穿过同一 typed history；仍不含多模态附件、root Session
  IPC/CLI 命令面、OpenClaw
  式 reset/queue cleanup、专用投递退避与只读 Workspace 快照，因此不得声明与
  Codex/OpenClaw 等量的完整协作生命周期。
- Steer 的 API、控制面账本、Worker Checkpoint-first 应用、恢复重绑、过期/永久拒绝负回执和真实浏览器中途 steer 已实现；尚缺队列压缩、限速、富输入/附件和面向 generation 的运维治理。
- Worker 已按模型 Usage 累计并 Checkpoint Token/费用，越界后不会再次调用模型；独立 Host 所有子代理 handle 已按父实际用量和
  全树未完成预留统一准入，并将摘要绑定的子实际用量结算回父。时长预算现由可恢复的树级 active-time 时钟执行，审批等待不计时；
  可选 NATS 发布路径尚无本轮外部服务实跑。控制面的子代理可委派 Token/费用预算仍未扣除父 Run 自身已经实际消耗的
  用量，因此云端路径仍是保守分配账本，不是全局精确余额。
- Edge Node 的生产控制面服务、证书/Enrollment 自动轮换、heartbeat/presence、动态能力上报、离线任务投递、审批/暂停续传、Accepted 任务自主扫描、安全收据 GC、非终态 generation 交接及 Workspace 三方合并。设备密钥、challenge enrollment、批准能力面、终态后的 generation 换代、签名任务信封、本地去重收据、Workspace epoch 高水位、持久 outbox、mTLS 出站重连、签名 ACK 与在线撤销已实现。
- 1000 活跃 Run 压测、故障注入、Multi-AZ、备份恢复和 SOC 2 Ready 证据。

达到私有 Beta 仍必须按 `docs/architecture/acceptance.md` 完成容量、安全、恢复和真实产品闭环验收。

每阶段与 Codex CLI、OpenClaw 的差异复核见 `docs/reference-comparison.md`。
