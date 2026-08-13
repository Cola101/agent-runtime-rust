# ADR-0048: stdio MCP catalog cache and bounded session lifecycle

## Status

Accepted and implemented in the standalone Rust Runtime.

## Context

ADR-0047 made discovery retry safe and bounded, but every healthy Run still
issued `tools/list`, and persistent stdio processes had no idle or population
limit. Returning a cached directory without checking its process would trust a
dead authority; reaping a session during `tools/call` could create an ambiguous
side effect. The solution must remain process-local and must not add a control
plane, database, container or message-bus dependency.

Codex commit `ff352fab6209dc0f9d13fc0036ed3f9404682b2c` uses a process-scoped,
stdio-only Tool catalog cache with a 30-minute TTL and exact connection identity.
OpenClaw commit `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28` uses active requester
leases, a 10-minute idle TTL and a bounded LRU manager that skips leased sessions.

## Decision

1. Cache only a successful stdio `tools/list` result, keyed by the immutable
   configured server ID. The server's command, args, environment and cwd remain
   part of the authority digest frozen in the Run and Checkpoint.
2. Default catalog TTL is 30 minutes. A cache hit must first receive a bounded
   MCP `ping` response from the exact initialized session. PID/process liveness
   alone is not authority: a live but unresponsive server is retired and the
   cache is not returned. A replacement initialized session may reuse the
   still-fresh cache only after its own successful `ping`.
3. Never cache or automatically retry `tools/call`. Tool execution re-lists the
   live catalog and checks its frozen digest immediately before the call.
4. Every request holds an active session lease until its response is known.
   Idle and LRU collection may remove only zero-lease sessions.
5. Default idle TTL is 10 minutes, sweep interval 60 seconds and maximum live
   stdio sessions 32. Zero idle TTL plus zero sweep interval disables idle
   collection; session capacity remains mandatory and bounded to 1–64.
6. The configured session capacity must cover the smaller of the stdio server
   count and the Run's frozen discovery concurrency. Invalid combinations fail
   during Host construction, before any process or model request starts.
7. Lifecycle observability exposes cache hit/miss, failed retirement, live
   sessions, active leases, cached catalogs, idle evictions and LRU evictions.
8. Explicit shutdown awaits the sweeper and every session process group. Last
   client-handle drop also cancels actors so embedded Host teardown remains safe.

HTTP/gRPC directories are deliberately not cached here: those transports cross
workload-token and remote revocation boundaries and require their own freshness
contract. Lifecycle settings are local optimizations, not Run authority, so they
do not alter the Checkpoint schema.

## Consequences

### Positive

- Repeated healthy discovery avoids redundant `tools/list` traffic.
- Dead stdio sessions cannot masquerade as healthy through a cached catalog.
- Slow or side-effecting Tool calls cannot be evicted while their result is
  ambiguous.
- Long-running embedded Hosts have explicit process-count and idle bounds.
- Cache entries survive safe session replacement without authorizing any call.

### Negative

- Every cache hit adds one bounded MCP `ping` round trip.
- Cache state is process-local and disappears on Host restart.
- The Runtime does not yet consume Codex's server opt-out annotation.
- Capacity replacement can briefly overlap old-process cleanup and new actor
  creation, although the old process is reaped before the request proceeds.

## Alternatives rejected

- **Cache Tool results:** rejected because side effects and external freshness
  make replay unsafe.
- **Return cached catalogs without a live session:** rejected because authority
  availability would be fabricated.
- **Evict oldest session regardless of active work:** rejected because it can
  turn a completed external side effect into an indeterminate local failure.
- **Use Docker or an external connection service:** rejected because this is an
  embedded Runtime lifecycle concern and must work natively on the target Mac.
