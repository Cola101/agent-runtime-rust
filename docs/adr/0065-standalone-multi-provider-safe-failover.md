# ADR-0065: Standalone multi-Provider routing and crash-safe failover

## Status

Accepted and implemented in the protocol-neutral standalone Rust Host. Real
loopback HTTP/SSE tests cover all three supported Provider protocols, policy
filtering, safe failover, partial-output refusal and Host replacement.

## Context

The standalone Host previously embedded one OpenAI-compatible adapter. The
shared Model Gateway could rank Provider candidates and perform safe failover,
but that did not prove the independent Runtime could do so without Java, NATS,
PostgreSQL or a separate Gateway process. Context compaction also called the
first adapter directly, creating a hidden routing bypass.

Codex `ff352fab6209` has a rich Provider registry plus bounded request/stream
retry and WebSocket-to-HTTP transport fallback, but a Turn normally has one
effective Provider rather than a tenant-style cross-Provider candidate chain.
OpenClaw `58b4b9430457` has ordered fallback candidates, classified failover,
Auth Profile cooldown/probing and a committed-work guard that stops switching
after output. Its lifecycle is broader, while its candidate progress is not a
Rust Worker Checkpoint bound to this Runtime's Run identity.

## Decision

1. A local model policy contains 1–8 explicit candidates. Each candidate binds
   a stable id, protocol, endpoint, model, region, accepted data classes,
   capabilities, health, latency, price and bounded response/idle timeouts.
2. Every model request derives required capabilities from the provider-neutral
   IR. Routing filters health, region, data class, capabilities and cost before
   network egress, then ranks the eligible candidates and freezes the prefix
   allowed by the Run's `max_provider_attempts` policy.
3. OpenAI Responses, Anthropic Messages and OpenAI-compatible Chat Completions
   are instantiated in-process behind the same `ProviderAdapter` boundary.
   Ordinary Agent Turns and internal transcript-compaction requests use the
   same routing function.
4. Cross-Provider fallback is permitted only when the failed candidate emitted
   zero stream events, classified the error retryable, and the frozen Run
   policy explicitly allows that error kind. Authentication, billing,
   protocol, context or capability failures do not silently switch. Any
   partial text, Tool Call, usage or completion disables fallback.
5. Each invocation has an atomic filesystem route journal bound to Run id,
   invocation digest, non-secret routing-config digest and frozen candidate
   ids. It stores the candidate cursor, classified failure digests, observation
   receipts, selected Provider and staged model events. Secrets and raw error
   messages are never stored.
6. Failure/selection events enter the Kernel event sequence and a Worker
   Checkpoint before the journal marks them reported. A successful or terminal
   stream batch remains staged until every event or compaction mutation is
   applied and checkpointed. A replacement Host therefore continues from the
   durable cursor or applies the staged batch without another Provider call.
7. The binary accepts `AGENT_RUNTIME_LOCAL_MODEL_ROUTING_CONFIG`. Its JSON
   contains only secret environment-variable names; each Provider credential
   is resolved at process start. The older three single-Provider environment
   variables remain a compatibility fallback.

## Consequences

### Positive

- The independent Runtime now exercises the same three protocol adapters and
  policy dimensions as the shared Gateway without adding an external service.
- Failover cannot erase partial output or replay a Tool-producing model turn.
- A crash after a known failure cannot restart the candidate chain, and a crash
  after response receipt cannot force a second Provider request.
- Routing observations are auditable without persisting API keys or raw
  Provider diagnostics.

### Negative and incomplete

- Persistent attempt budgets, `Retry-After`, exponential backoff, cooldown and
  half-open probing were subsequently implemented by ADR-0066. Auth Profile
  rotation and per-credential health remain incomplete.
- The route journal is a Host-local companion to the Worker Checkpoint, not yet
  part of the cloud Checkpoint Gateway object or a distributed transaction.
- Provider-neutral IR still lacks several Codex Responses reasoning/private
  items and OpenClaw's Provider-specific thinking/cache/refusal compatibility.
- Tests use real loopback protocol servers. They prove transport and execution
  semantics, not third-party model quality, availability or billing behavior.

## References

- Codex `codex-rs/model-provider-info/src/lib.rs`
- Codex `codex-rs/core/src/responses_retry.rs`
- Codex `codex-rs/core/src/client.rs`
- OpenClaw `src/agents/model-fallback-candidates.ts`
- OpenClaw `src/agents/model-fallback-runner.ts`
- OpenClaw `src/agents/model-fallback-attempt.ts`
- OpenClaw `src/agents/failover-error.ts`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/src/main.rs`
- `runtime/apps/runtime-host/tests/multi_provider.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
