# ADR-0042: Shared MCP discovery admission

## Status

Accepted. Implemented in the protocol-neutral Rust MCP discovery path.

## Context

RunExecution v10 limits discovery inside one Run, but a valid four-slot policy
does not bound the aggregate when many Runs use the same Runtime process. A
plain global semaphore prevents overload but lets the first noisy tenant fill
the FIFO queue ahead of later tenants.

Codex starts and catalogs a session's configured MCP servers concurrently and
has richer lifecycle/cache handling, but it is a single-user runtime and does
not schedule catalog work across tenants. OpenClaw has process-wide command
lanes with per-lane concurrency and strong pause/drain behavior; lanes are
independent and do not provide one shared capacity rotated by tenant.

## Decision

1. Each `GrpcMcpFederationClient` owns a cloneable
   `McpDiscoveryScheduler`; all client clones share the same state. The default
   aggregate ceiling is 32, and embedded hosts may inject another positive
   limit without Java, NATS, PostgreSQL or containers.
2. Every server discovery acquires shared admission by the immutable
   `tenant_id` from its Run command. Pending tenant queues are served round
   robin. FIFO order is preserved within one tenant.
3. The frozen per-Run `McpDiscoveryPolicy` remains authoritative. The shared
   ceiling may only reduce instantaneous concurrency; it cannot widen a Run's
   policy.
4. Queue wait consumes the Run's frozen total discovery budget, while the
   per-server timeout starts only after admission. Dropping a queued or active
   future is cancellation-safe: undelivered requests are skipped and active
   permits release immediately.
5. Shared capacity is operational host state and is not checkpoint identity.
   Recovery may run on a host with different available capacity, but it must
   still keep the same total deadline and per-Run policy.
6. A snapshot exposes only aggregate capacity, active requests, queued requests
   and queued-tenant count. Tenant identifiers stay inside the scheduler.

## Consequences and limits

- Aggregate discovery is bounded and a tenant with a long queue cannot starve
  another tenant already waiting.
- No background service or platform control plane is required.
- This is equal-share round robin, not weighted tenant service tiers or a
  durable distributed quota.
- The protocol-neutral async discovery supervisor now exists (ADR-0043), so a
  native Host can run multiple discoveries while applying their results
  serially. The current NATS adapter has not adopted it and still awaits
  discovery in `poll_once` and `poll_recovery_once`; production Worker
  multi-Run admission therefore remains unverified.

## References

- Codex `codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs`
- OpenClaw `src/process/command-queue.ts`, `src/gateway/server-lanes.ts`
- ADR-0040 MCP tool federation
- ADR-0041 runtime execution policy snapshot
- ADR-0043 async MCP discovery supervisor
