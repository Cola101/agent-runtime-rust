# Standalone multi-Provider safe failover evidence — 2026-08-10

## Protocol and policy proof

- One standalone Host instantiated OpenAI Responses, Anthropic Messages and
  OpenAI-compatible adapters in-process from one provider-neutral model IR.
- A real loopback Run received 503 before output from Responses, then from
  Anthropic, and completed through OpenAI-compatible SSE. Captured requests
  prove each Adapter used its own path and payload shape.
- Health, region, data class, Tool capability and maximum cost were filtered
  before egress. Five excluded endpoints observed no TCP connection; the route
  journal froze only the eligible candidate.
- The shipped binary consumed a strict multi-Provider JSON file and resolved
  each API key through the named environment variable. The JSON schema has no
  field that accepts an embedded secret, and Provider debug output redacts the
  in-memory credential.

## Failover and crash proof

- A partial compatible SSE stream emitted text and then timed out. The text was
  preserved, the Run ended `timed_out`, and the configured fallback endpoint
  observed no connection.
- After the primary failure was journaled and the fallback connection was
  interrupted, a replacement Host resumed the fallback cursor. The primary
  endpoint observed exactly one request.
- A separate crash reconstruction restored the pre-response Worker Checkpoint
  while retaining the staged successful response. The replacement applied the
  exact batch and completed without any second Provider request.
- Failure and selection observations are idempotently emitted around Worker
  Checkpoints. Journals bind Run, invocation, route configuration, candidate
  order and cursor; stored diagnostics contain SHA-256 message digests rather
  than raw Provider messages.

## Context-compaction proof

A real Streamable HTTP MCP Tool produced two large Tool results. The primary
Provider served both Tool turns, returned HTTP 503 before the summary emitted
anything, and the fallback Provider completed the no-Tool summary request. The
next ordinary Turn returned to the ranked primary. The MCP server observed
exactly two `tools/call` requests, the fallback summary had `max_tokens = 256`
and no Tool catalog, and `context.compacted` plus `model.provider.failed` were
present in the durable event stream.

## Recovery-test correction

The asynchronous child recovery fixture used to require the recovered parent
request to arrive before the recovered child request, although the Runtime
contract promises identity and durability, not network scheduling order. The
fixture now identifies both requests by content and asserts the stable agent
handle, no spawn replay and parent wait semantics. Its socket-close deadline
now starts only after the simulated crash is triggered, eliminating a false
failure under workspace-wide parallel tests without extending the actual
shutdown allowance.

## Validation

- Standalone Host test targets: 88 passed, 0 failed.
- Full Rust workspace: 447 passed, 0 failed, 5 external live tests explicitly
  ignored; 452 total.
- `cargo check --workspace --all-targets`, Clippy over
  workspace/all-targets/all-features with `-D warnings`, Rust formatting, JSON,
  diff and residue gates passed.

No Docker, Java, PostgreSQL, NATS, external daemon or external API key was
started or required. Loopback HTTP/SSE and MCP peers were real sockets with
deterministic responses; they do not constitute live-vendor acceptance.
