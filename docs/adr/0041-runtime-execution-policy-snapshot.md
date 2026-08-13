# ADR-0041: Runtime execution policy snapshot

## Status

Accepted. Implemented in the protocol-neutral Rust Runtime.

## Context

Before RunExecution v10, three decisions that change a Run's observable
behaviour lived as process-local constants:

- MCP discovery concurrency and its per-server/total deadlines lived in the
  Worker;
- model fallback count and eligible error classes lived in Model Gateway;
- Tool execution timeout lived separately in Worker and `runtime-host`.

The command could therefore remain byte-for-byte identical while a restart,
replacement Worker or embedded Host changed its execution semantics. Freezing
the MCP catalog in a later Checkpoint was not enough: the first discovery and
the first model or Tool call had already inherited Host defaults.

## Decision

1. RunExecution v10 must carry one typed `RuntimeExecutionPolicySnapshot` before
   admission. It has three v1 sections: MCP discovery, model failover and Tool
   execution.
2. Values are bounded at the protocol boundary: MCP concurrency 1–16,
   per-server deadline at most 60 seconds, total deadline at most 300 seconds,
   model attempts 1–8, fallback only on retryable rate-limit/timeout/unavailable,
   and Tool timeout at most one hour.
3. Older commands remain readable but may not carry the v10 field. A v10 command
   without it is rejected; this prevents downgrade smuggling and implicit
   inheritance.
4. Worker uses the command snapshot for real MCP scheduling and Tool execution.
   An explicit legacy discovery-policy argument cannot override a v10 command.
5. ModelInvocation v4 carries the exact JSON plus SHA-256 digest. Model Gateway
   validates it, rejects the field on older invocation schemas and caps the
   frozen Provider chain before network egress.
6. Worker Checkpoint schema 8 stores the whole snapshot. Any policy drift on a
   replacement attempt is `CheckpointIdentityMismatch`; a v10 Run cannot resume
   from a pre-schema-8 Checkpoint that cannot prove the original policy.
7. The standalone Rust Host emits v10 commands, signs an explicit built-in
   Skill snapshot for only its installed trusted Tools, and persists the same
   runtime policy without Java, NATS, PostgreSQL or containers.
8. ADR-0047 extends the MCP section to policy schema 2 in RunExecution v11:
   bounded discovery attempts and initial backoff are now part of the same
   Checkpoint-bound identity. Schema 1 remains single-attempt for compatibility.

## Reference comparison

Codex exposes mature per-MCP startup and Tool timeouts and locks selected session
configuration, but those settings are not one portable, checkpoint-bound Run
policy spanning MCP, model fallback and native Tool execution. Its MCP lifecycle,
cache and local stdio coverage remain broader.

OpenClaw has a substantially richer model-fallback engine, including configured
candidate chains, provider/auth cooldown and detailed error classification. It
also invalidates CLI session reuse when its canonical MCP resume hash changes.
Those are important references, but the decisions remain split across session
and runner configuration rather than one typed cross-host recovery identity.

## Consequences

- A Run now has deterministic host-level semantics across admission, execution
  and recovery.
- The standalone Host follows the same v10/Checkpoint path as Worker instead of
  retaining a legacy local-only contract.
- Pre-schema-8 Checkpoints cannot be upgraded into a v10 Run without evidence;
  they fail closed.
- Global MCP discovery backpressure is now an operational, process-local
  scheduler shared by gateway-client clones (ADR-0042). It is deliberately not
  part of the Run identity because changing available host capacity affects
  queue latency, not Tool semantics; the frozen total deadline still bounds
  that queue wait. Per-server MCP overrides, model backoff/cooldown schedules,
  output byte ceilings and subagent concurrency remain future policy work.
