# ADR-0023：低基数恢复指标与受保护的运维端点

## Status

Accepted

## Context

ADR-0022 已将恢复事故变成按租户、受 RLS 保护的权威记录，但运维系统仍无法持续抓取和告警。直接把
`tenant_id` 放入 Prometheus 标签会造成高基数、扩大租户标识暴露面；让 Scheduler 使用 `BYPASSRLS`
扫描事故表则会扩大数据库权限。采集失败若把 Gauge 清零，还会把监控故障伪装成系统健康。

Codex 的 `codex-otel` 和 Exec Server `ProcessMetricGuard` 使用有界结果属性输出进程指标，并支持
OpenTelemetry logs/traces/metrics；OpenClaw 的 restart trace、event-loop health 与 Worker heartbeat
保留最近健康状态和失效原因。两者都提供了有价值的低基数诊断语义，但没有多租户 PaaS 的跨 Worker
恢复 SLO 与 RLS 边界。

## Decision

1. PostgreSQL V14 新增不含租户、Run 或 Workspace 标识的 `recovery_metric_buckets`。V13 事故表的
   INSERT/UPDATE/DELETE 触发器在同一事务内按 `last_confirmed_healthy_at` 维护精确计数；全局采集只读
   该汇总表，不给 Scheduler `BYPASSRLS`。
2. 按租户的详细 SLO 查询继续受 RLS 保护；全局 Prometheus 只发布 open、overdue、
   waiting_capacity、recovery_requested、oldest age，以及采集成功时间和错误累计值。任何指标均禁止
   `tenant_id` 标签。
3. 采集器轮询失败时保留最后一次成功快照，同时增加 refresh error counter；告警用最后成功时间判定
   陈旧，不能把数据库或采集器故障显示为零事故。
4. Actuator 使用独立管理端口 9090。健康探针允许匿名读取；Prometheus 必须使用与 OIDC 用户 JWT
   分离的专用 Basic Auth 凭证，密码由 `MANAGEMENT_SCRAPE_PASSWORD` 注入，仓库不提供默认值。
5. 可选 Kubernetes observability overlay 提供恢复 SLO 超时、持续等待容量和指标陈旧三条
   `PrometheusRule`。Base 不强依赖 Prometheus Operator CRD。
6. 本阶段只证明真实 Spring Boot 进程、PostgreSQL V14、Prometheus 文本和离线规则渲染。管理端口的
   Kubernetes Service/NetworkPolicy、TLS、Prometheus 实例联调及节点级 15 分钟恢复演练仍是独立门禁。

## Consequences

### Positive

- 平台级恢复告警不需要越过 RLS，也不会把租户标识写入时序标签。
- 事故写入和汇总更新原子一致，聚合不会因异步扫描窗口漏记。
- 采集故障可见且不会清零最后一次真实状态。
- 用户认证域与运维抓取凭证分离，控制面主端口不承载指标端点。

### Negative

- 触发器增加事故写路径复杂度，后续 Schema 变更必须同步维护汇总契约。
- Basic Auth 只有在管理端口被内网隔离并使用受保护传输时才可用于生产；当前尚无集群联调证据。
- 多副本 Scheduler 会暴露相同全局 Gauge，Prometheus 规则必须使用 `max` 等幂等聚合。

## Alternatives Considered

- **Prometheus 直接查询每租户事故并打 tenant 标签**：高基数且扩大租户信息暴露，拒绝。
- **Scheduler 获得 BYPASSRLS 扫描 V13**：扩大控制面数据库攻击面，拒绝。
- **定时全表异步汇总**：可能与事故状态提交不同步，无法给出精确当前值，拒绝。
- **采集失败时归零**：会把监控故障伪装成恢复健康，拒绝。
- **在公共 API 端口匿名暴露 Prometheus**：配置漂移时泄露 JVM 和运行状态，拒绝。

## References

- Codex：`codex-rs/otel`、`codex-rs/exec-server/src/telemetry.rs`
- Codex：`codex-rs/exec-server/src/local_process.rs` 的 `ProcessMetricGuard`
- OpenClaw：`src/gateway/restart-trace.ts`、`src/gateway/server/event-loop-health.ts`
- OpenClaw：`src/worker/worker-connection.ts` 的 heartbeat fail-closed 语义
- 本平台：`V14__recovery_metric_rollup.sql`、`RecoveryMetricsCollector.java`
- 本平台：`ManagementEndpointIntegrationTest`、`RecoveryMetricsCollectorTest`
- 本平台：`deploy/kubernetes/observability/prometheus-rule.yaml`

