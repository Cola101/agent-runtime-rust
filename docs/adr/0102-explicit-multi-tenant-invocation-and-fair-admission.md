# ADR-0102: Explicit multi-tenant invocation and fair local admission

- Status: Accepted
- Date: 2026-08-12
- Scope: Rust protocol, Worker Checkpoint, standalone/embedded Runtime Host

## Context

The standalone Host constructed every Run with fixed `LOCAL_*` UUIDs. This was
safe only for a single-user CLI process: an embedding system could not prove
which tenant, application, workload principal, Workspace, AgentVersion, or
model policy owned a Run. The local daemon also spawned every accepted request
immediately, so one tenant could consume all process capacity. Provider routing
and MCP discovery limits do not solve Run-level admission.

The same Rust kernel must serve a desktop client, an edge node, a Java sidecar,
and a cloud Worker without accepting a caller-supplied filesystem path or
silently reverting to a fixed local tenant.

## Decision

1. Introduce protocol-neutral `RuntimeInvocationContext` v1. It contains
   immutable, non-nil tenant, application, non-secret workload identity,
   Workspace, AgentVersion, and model-policy identities. Authentication and
   rotating bearer credentials remain outside this durable structure.
2. `RunExecutionCommand` v20 carries `application_id` and
   `workload_identity_id`. v1-v19 remain read-compatible with nil defaults; a
   v20 command rejects any incomplete identity and rejects a signed Skill from
   another application.
3. `LocalRuntimeHost::start_for_invocation` is the authoritative embedding
   path. The old `start` API is an explicit single-user compatibility profile,
   not the multi-tenant default hidden inside command construction.
4. Events persist tenant/application/workload/Workspace/AgentVersion/model
   identities. Worker Checkpoint schema 26 binds application and workload
   identity and refuses cross-application or cross-principal recovery.
5. `EmbeddedRuntime` accepts only pre-registered invocation profiles. Requests
   select an exact identity; they cannot supply Workspace paths, provider
   credentials, or Tool configuration. Multiple AgentVersion/workload/model
   profiles may share roots only when tenant, application, and Workspace
   identity are identical; a different Workspace identity cannot reuse either
   persistent root.
6. `RuntimeAdmissionController` applies global, per-tenant, and per-Workspace
   active limits plus global and per-tenant queue limits. Tenant queues rotate
   round-robin; a tenant that just ran yields to another eligible tenant. A
   blocked Workspace does not head-of-line block another Workspace, and one
   Workspace has one active writer by default.
7. Admission permits are RAII receipts. Dropping an active permit immediately
   releases capacity; cancelling a queued future immediately removes its queue
   entry.

## Non-functional requirements and failure modes

- Admission state is memory-bounded by configured queue limits.
- No Host, provider request, Tool, or Workspace access occurs before profile
  lookup and admission succeed.
- A cancelled waiter cannot leak queue capacity.
- A panic or task cancellation cannot leak active capacity.
- Replacement execution must match the Checkpoint's application and workload
  identity before model or Tool egress.
- One logical Workspace has one stable state/workspace-root pair even when it
  registers multiple immutable AgentVersions.
- This controller is process-local. Distributed placement, durable queueing,
  weighted tenant plans, and cross-node ownership remain control-plane work.

## Alternatives considered

- **Add `tenant_id` only to local IPC requests.** Rejected: a client could name
  an arbitrary tenant while filesystem paths and credentials still came from a
  shared daemon configuration.
- **One daemon process per tenant.** Rejected as the only model: it avoids fair
  sharing and is unsuitable for high-density Java embedding, although process
  separation remains a deployment option.
- **Reuse Provider or MCP admission.** Rejected: those limits protect narrower
  egress operations after a Run has already consumed Runtime resources.
- **FIFO global queue.** Rejected: a burst from one tenant can starve another;
  Workspace blocking also creates avoidable head-of-line stalls.

## Consequences

The Rust Runtime now has a real multi-tenant in-process entry surface and
bounded fair backpressure without Java, NATS, PostgreSQL, Docker, or Kubernetes.
Existing single-user CLI behavior remains compatible. Cloud Java scheduling
still emits an older execution schema and is intentionally not upgraded in this
Rust-only milestone; v20 producer parity and signed token propagation through
remote Model/MCP gateways are explicit follow-up work, not claimed complete.
