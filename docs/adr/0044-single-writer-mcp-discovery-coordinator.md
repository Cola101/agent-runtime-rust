# ADR-0044: Single-writer MCP discovery coordinator

## Status

Accepted. Implemented in the protocol-neutral Rust Runtime library and consumed
by the standalone Host's direct HTTP backend; NATS adapter integration remains
pending.

## Context

ADR-0043 moved MCP network discovery into concurrent tasks, but its caller still
had to manually correlate a result, re-supply the command, attach executors and
choose between starting a new Run and validating a restored Run. That sequence
was easy to reorder, and accepting a second command copy created an unnecessary
authority boundary beside the command already accepted by `WorkerProcessor`.

Codex builds one session catalog concurrently with `join_all`, reconnects failed
startup clients and can use cached tools. OpenClaw's command lanes track active
tasks, timeout, priority, generation and drain. Neither reference provides the
same cross-Run operation that applies an immutable network result through a
checkpoint-aware, protocol-neutral Kernel owner.

## Decision

1. `McpDiscoveryCoordinator` is the only component that turns an
   `McpDiscoveryUpdate` into mutable `WorkerProcessor` state.
2. Starting discovery accepts only an `attempt_id`. The coordinator obtains the
   authoritative command, base Tool registry and cancellation token from the
   already accepted or restored execution; callers cannot provide a divergent
   command copy.
3. The coordinator keeps the executor client, authoritative command and purpose
   in an attempt-keyed pending map while `McpDiscoverySupervisor` performs only
   network work.
4. For a new Run, a ready result is attached before `WorkerProcessor::start` and
   the coordinator returns the resulting `run.started` event.
5. For a restored Run, a ready result is attached and then checked by
   `verify_restored_federated_tools`; only an exact checkpointed catalog and
   discovery policy can produce `Restored`. The Kernel is not started twice.
6. Cancellation produces a bound terminal coordinator outcome without applying
   a partial catalog. Publishing, Checkpoint persistence and transport
   acknowledgement remain caller responsibilities after coordinator success.

## Consequences and limits

- Native Hosts and message adapters share the same start/recovery ordering
  without a NATS, database, Java, container or Kubernetes dependency. The
  standalone Host now proves that path with a real MCP socket.
- Network completion order remains concurrent, while Kernel mutation is applied
  serially by the caller polling the coordinator.
- A slow real MCP Run no longer prevents a fast Run from attaching its signed
  Skill Tool and reaching `ModelInvocation`.
- Recovery cannot report ready while its checkpointed federated catalog is still
  absent or has drifted.
- The current NATS adapter still performs inline discovery and has not yet been
  changed to drive coordinator completions before message acknowledgement.
- Checkpoint schema 9 additionally binds the configured MCP authority; an equal
  catalog from another endpoint cannot pass recovery.
- Drain snapshots, forced cancellation of every active discovery and lifecycle
  metrics remain less complete than OpenClaw's command-lane machinery.

## References

- ADR-0040 MCP tool federation
- ADR-0042 shared MCP discovery admission
- ADR-0043 async MCP discovery supervisor
- ADR-0045 protocol-neutral MCP backend and standalone Host
- Codex `codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs`
- OpenClaw `src/process/command-queue.ts`
