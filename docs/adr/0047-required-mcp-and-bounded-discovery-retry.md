# ADR-0047: Required MCP servers and bounded discovery retry

## Status

Accepted and implemented in the protocol-neutral Rust Runtime.

## Context

The standalone HTTP/stdio MCP path could discover and execute Tools, but every
server failure was implicitly optional and discovery had one attempt. A Run
could therefore enter the model with a missing Tool that its Agent actually
required. Conversely, retrying all MCP operations would be unsafe: a
`tools/call` transport failure may happen after an external side effect.

The decision preserves these non-functional requirements:

- **Determinism:** availability and retry semantics are frozen before Run
  admission and survive Checkpoint recovery.
- **Safety:** only idempotent discovery (`initialize`/`tools/list`) is retried;
  Tool calls are never replayed automatically.
- **Bounded latency:** attempts and backoff share the existing per-server and
  total discovery deadlines.
- **Fairness:** a retrying tenant releases the shared discovery slot while
  backing off and must re-enter tenant-fair admission.
- **Standalone operation:** the same behavior works in the native Rust Host
  without Java, NATS, PostgreSQL, Docker or Kubernetes.

## Decision

1. RunExecution v11 adds `required` to each MCP server. Missing values on v10
   deserialize as `false`; a pre-v11 command cannot carry `required=true`.
2. Runtime policy schema 2 adds `max_attempts_per_server` (1–4) and
   `initial_retry_backoff_ms` (0–5000). Schema 1 remains exactly one attempt
   with no backoff.
3. Discovery retries only retryable transport, unavailable and deadline
   failures. Protocol, authentication, authorization and invalid-response
   failures stop immediately.
4. Backoff is exponential, bounded by the frozen attempt count, per-server
   deadline and total discovery deadline. Shared admission covers only network
   work, never retry sleep.
5. Discovery returns one ordered status per configured server: `ready` or
   `unavailable`, required flag, completed attempt count and error detail.
6. Any unavailable required server rejects catalog attachment before model
   egress. Optional failures remain visible in the standalone Run result while
   the Agent Loop may continue with the successfully frozen catalog.
7. Worker Checkpoint schema 10 persists retry policy. The MCP server binding
   digest includes `required` for v11 commands. Recovery rejects policy or
   required/optional drift before model execution.
8. A failed stdio session is removed before a safe discovery retry. Unix stdio
   process groups retain the PGID captured at spawn so descendants are still
   terminated if the direct child exits before cleanup.

## Reference comparison

Codex waits for every required server before session initialization completes,
allows optional startup grace, caches healthy catalogs and reconnects failed
startup sessions. Its HTTP initialize path retries selected transport failures;
it does not treat Tool calls as generally replay-safe.

OpenClaw retires closed or expired sessions, revalidates requester-scoped
connections, retains usable catalog state while unhealthy servers recover, and
adds idle TTL, active leases and LRU disposal.

This implementation now aligns with both projects on required/optional startup
and safe discovery-only retry. ADR-0048 subsequently added a healthy stdio
catalog cache, active leases, idle TTL and zero-lease LRU. It remains behind
Codex on background reconnect and behind OpenClaw on requester-scoped continuous
connection revalidation. Its stronger boundary is a portable, checkpoint-bound
retry and availability identity across Worker replacement.

## Consequences

### Positive

- A required Tool dependency cannot silently disappear from the model-visible
  catalog.
- Optional degradation is explicit rather than hidden.
- Transient stdio/HTTP startup failures can recover without replaying Tool side
  effects or monopolizing shared capacity.
- Replacement Hosts reproduce the same availability semantics or fail closed.

### Negative

- Health currently describes startup discovery, not a continuous active probe.
- A total-budget cancellation reports zero completed attempts when the
  discovery future produced no result, even if transport work had begun.
- Background reconnect is not proactive; a later discovery observes the closed
  process and triggers bounded replacement (ADR-0048).
- Retry policy is uniform per Run, not configurable per server.

## Failure modes and mitigations

- **Required server exhausts retries:** reject before model egress with its
  ordered status and error.
- **Optional server exhausts retries:** continue with a visible unavailable
  status and no Tools from that server.
- **Total deadline expires:** cancel unfinished futures, release admission and
  mark unfinished servers unavailable without inventing completed attempts.
- **stdio leader exits but descendants survive:** terminate the captured process
  group rather than looking up a now-missing leader PID.
- **Recovery changes required flag or retry budget:** Checkpoint identity
  mismatch fails before model execution.
- **Tool call transport becomes ambiguous:** do not retry; existing Tool effect
  and `indeterminate` rules remain authoritative.

## Alternatives Considered

- **Treat every server as optional:** rejected because the model can hallucinate
  around a missing required capability.
- **Retry all MCP RPCs:** rejected because Tool calls may have non-idempotent
  external effects.
- **Keep retry settings as Host defaults:** rejected because replacement would
  change Run semantics without changing its identity.
- **Hold one shared slot during backoff:** rejected because one failing tenant
  would reduce capacity for unrelated Runs.
- **Implement full background cache/TTL/LRU now:** deferred as the next lifecycle
  stage; startup correctness and recovery identity are prerequisites.

## References

- ADR-0041 Runtime execution policy snapshot
- ADR-0042 Shared MCP discovery admission
- ADR-0046 Standalone stdio MCP lifecycle
- Codex `codex-rs/codex-mcp/src/connection_manager/required.rs`
- Codex `codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs`
- Codex `codex-rs/rmcp-client/src/streamable_http_retry.rs`
- OpenClaw `src/agents/agent-bundle-mcp-runtime.ts`
- OpenClaw `src/agents/agent-bundle-mcp-manager-lifecycle.ts`
