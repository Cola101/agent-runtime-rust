# Edge 认证出站会话证据（2026-08-13）

## 结论

Rust Edge 已从“本地账本库”推进为可配置的原生出站守护进程核心：mTLS gRPC 双向流、设备活体证明、
签名任务、真实 Runtime、持久 Outbox、签名 ACK、断线重连和在线撤销形成一条可执行链。该证据不包含
生产控制面服务或证书生命周期，因此不能表述为生产可用 Edge 网络。

## 行为证据

- `edge_transport::mutual_tls_session_proves_device_executes_task_and_prunes_only_signed_ack`
  使用临时 CA、服务端证书和客户端证书完成真实 mTLS，服务端验证设备 challenge proof，任务进入真实
  `EmbeddedRuntime` 和 HTTP/SSE Provider，只有精确批次签名 ACK 后本地 Outbox 才清空。
- `edge_transport::reconnect_resends_unacked_batch_without_reexecuting_the_task`
  第一次连接在上传后、ACK 前断开；第二次连接重复投递任务并收到同一批次，Provider 调用计数仍为 1。
- `edge_transport::signed_online_revocation_terminates_session_and_survives_restart`
  在线撤销精确绑定当前 Enrollment/node generation，落盘后同代节点不能重新打开账本。
- `edge_daemon_config::native_daemon_config_builds_real_enrolled_runtime_without_inline_secret`
  原生配置建立两个不同租户的预注册 Profile，Provider Secret 从 owner-only 文件读取，Debug 输出不泄漏。
- Enrollment 过期测试证明：过期 grant 不能创建新 session proof，也不能由仍有效的 task token 延长。
- Outbox 编码测试证明：上传选择连续且受 3 MiB 限制的前缀。

## 对标结论

- Codex 的远程控制、Thread/Turn、审批和取消产品生命周期更成熟；本实现提供其本地会话模型未提供的
  通用多租户 Edge task/enrollment/outbox 密码学边界。
- OpenClaw 的 GatewayClient、heartbeat、presence、动态 inventory、配对运维和跨平台 Node Host 更成熟；
  本实现的优势仅在“跨进程持久结果 + 精确批次签名 ACK + 多租户 invocation”这一窄面。

## 质量门禁

- `cargo test -p agent-edge-node --quiet --no-fail-fast`：30 通过，0 失败。
- `cargo check --workspace --all-targets --quiet`：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- 全仓动态测试未全绿，但失败不在 Edge：两个 Runtime Host/Tool Runtime 时序项串行复验通过；
  `closed_session_reconnects_before_reusing_healthy_catalog` 与
  `sixty_four_sessions_keep_one_thousand_waits_bounded_and_tenant_fair` 串行仍失败，分别是既有 stdio MCP
  关闭缓存边界与 64 Session/1024 wait 延迟门禁。本阶段不扩大范围修改，不能据此宣称全仓动态门禁通过。
