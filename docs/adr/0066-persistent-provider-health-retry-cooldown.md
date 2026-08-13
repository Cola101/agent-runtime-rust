# ADR-0066: Persistent Provider health, bounded retry and half-open probing

## Status

Accepted and implemented in the protocol-neutral standalone Rust Host. Real
loopback HTTP tests cover same-Provider retry, durable cooldown, `Retry-After`,
single half-open admission, authentication isolation and replacement recovery.

## Context

ADR-0065 froze and journaled a cross-Provider route, but a zero-output failure
could only advance to the next candidate. A replacement Host could retry the
last candidate without a process-independent count, while new Runs forgot
recent transient failures. That made retry storms and repeated recovery
possible even though partial-output failover was already fenced.

Codex `ff352fab6209` implements bounded same-Provider request/stream retry,
error-provided delay or exponential backoff, reconnect observations and
WebSocket-to-HTTP fallback. OpenClaw `58b4b9430457` adds ordered fallback,
Auth Profile cooldown, transient cooldown probes and session suspension. The
standalone Runtime needs both the bounded retry discipline and a durable
single-writer Provider lifecycle without adopting OpenClaw's user Gateway.

## Decision

1. The immutable local model policy now includes total same-Provider attempts,
   bounded exponential backoff, consecutive-failure threshold, cooldown,
   maximum accepted `Retry-After` and half-open probe lease. The policy is part
   of the non-secret route-binding digest.
2. Route journal schema 2 stores the same-Provider attempt count, scheduled
   retries, retry deadline and in-flight Provider id. Before network egress the
   attempt is persisted. A replacement treats an unresolved in-flight request
   as one consumed ambiguous attempt and never exceeds the frozen budget.
3. Same-Provider retry is allowed only before any model event, for a retryable
   error kind allowed by the Run failover policy. Backoff is exponential and
   capped; a valid HTTP `Retry-After` may lengthen it within the configured cap.
   Cancellation and the Run duration deadline interrupt the wait.
4. `rate_limited`, `timeout` and `unavailable` failures update an atomic
   `model-provider-health.json` store keyed by route-binding digest and Provider
   id. Authentication, billing, protocol, context and capability failures do
   not increment that shared circuit and do not trigger cross-Provider replay.
5. Reaching the threshold, or receiving `Retry-After`, opens cooldown. New
   invocations exclude an actively cooling candidate before egress. After the
   deadline, admission persists one invocation-bound half-open lease; competing
   invocations skip that candidate. Success removes its health entry; a
   transient probe failure reopens cooldown.
6. An existing route journal restores its exact candidate ids rather than
   re-ranking against current health. Immediately before each egress it still
   observes cooldown/probe admission, so concurrent health changes cannot cause
   a retry storm. This preserves frozen Run semantics while protecting the
   Provider lifecycle.
7. Retry observations enter the Kernel event stream and Worker Checkpoint
   before the journal marks them reported. Health state stores classifications,
   status and deadlines only; API keys and raw Provider messages are excluded.
8. A state root remains a single-writer domain. The daemon's existing
   single-instance fence and the in-process shared health mutex serialize
   supported execution. This ADR does not claim safe active-active mutation by
   two unrelated OS processes sharing one local state directory.

## Consequences

### Positive

- Codex-style bounded retry and OpenClaw-style cooldown/half-open lifecycle now
  survive Host replacement without depending on Java, NATS or a database.
- `Retry-After` and transient failures suppress needless network egress, while
  one probe can restore a recovered Provider without a thundering herd.
- An ambiguous request left by a crash is counted durably, preventing an
  infinite replacement/replay loop.
- Authentication failures remain visible and local to the configured
  credential rather than silently shifting work or poisoning Provider health.

### Negative and incomplete

- There is no Auth Profile set, credential rotation, OAuth/SecretRef lifecycle
  or per-credential cooldown. The current key is Provider-route scoped.
- Circuit-open, cooldown-skip and half-open transitions are durable in the
  health file but do not yet have dedicated public event types.
- Failed requests without Provider usage data may still incur unknown vendor
  cost; the Runtime can bound attempts and duration but cannot prove billing
  exactly from a failed response.
- Codex WebSocket-to-HTTP transport fallback and OpenClaw's broader session
  suspension/error compatibility are not implemented.
- The health companion file is local single-writer state, not a distributed
  Checkpoint Gateway object or multi-region consensus record.

## References

- Codex `codex-rs/core/src/responses_retry.rs`
- OpenClaw `src/agents/model-fallback-runner.ts`
- OpenClaw `src/agents/model-fallback-attempt.ts`
- OpenClaw `src/agents/model-fallback-cooldown.ts`
- `runtime/apps/model-gateway/src/openai_compatible.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/src/main.rs`
- `runtime/apps/runtime-host/tests/multi_provider.rs`
- `runtime/apps/runtime-host/tests/daemon_recovery.rs`
