# ADR-0106：Edge 认证出站会话、签名 ACK 与原生守护进程

- 状态：已接受
- 日期：2026-08-13

## 决策

Edge Node 只建立出站的双向 gRPC 流，不开放入站管理端口。传输必须使用 mTLS，并在 TLS 之上执行
`Hello → Challenge → Device Proof → Accepted`：设备使用本地持久 Ed25519 密钥签署服务端新鲜随机数，
证明当前连接确实持有 Enrollment 绑定的设备私钥。mTLS 客户端证书和设备密钥是两条独立信任链，
其中证书负责接入，设备签名负责节点身份。

任务投递本身不产生执行权限；节点仍只执行 ADR-0104/0105 定义的签名任务信封。Enrollment grant
过期后不得创建新会话或启动新任务，已有持久终态收据仍可用于重复投递收敛。在线撤销使用控制面签名、
精确绑定 enrollment/device/node/generation 的短期信封，节点持久化撤销事实并终止会话。

Outbox 上传使用连续前缀，单批最多 256 条且 JSON 编码不超过 3 MiB。控制面 ACK 必须签名绑定
session、enrollment、node/generation、截止序号和精确批次 SHA-256；裸序号不得推进本地游标。
断线前未收到有效 ACK 的批次保留，重连后重复上传；任务信封重投由持久收据去重，不重复调用模型。

原生 `agent-edge-node` 读取绝对路径 JSON 配置，支持最多 256 个预注册多租户 Runtime Profile。
模型协议使用稳定外部名称 `openai_compatible`、`openai_responses`、`anthropic_messages`；Provider 密钥、
Enrollment grant 和 mTLS 私钥只从本机文件读取，私钥/Secret 文件在 Unix 上必须为 owner-only。

## 已验证边界

- 真实 loopback mTLS gRPC 双向流、设备 challenge proof、签名任务、真实 HTTP/SSE 模型调用、Outbox 上传及签名 ACK。
- 首连接在 ACK 前断开后自动重连；同一任务只调用一次 Provider，未确认批次重传后才清理。
- 在线签名撤销持久化；同 generation 重启被拒绝。
- 二进制配置可同时注册两个不同 tenant/application/Workspace/Profile，调试输出不包含 Provider Secret。

## 尚未完成

生产控制面 gRPC 服务、证书签发/轮换、Enrollment 自动续期、heartbeat/presence、动态能力上报、
审批/取消/暂停续传、安全收据 GC、带抖动的群体重连、非终态 generation 交接及 Workspace 三方合并。
当前 loopback 服务只是协议验收夹具，不是可部署控制面。
