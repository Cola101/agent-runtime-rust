# ADR-0086: Run-frozen operator authority for MCP Tool effects

## Status

Accepted and implemented in the protocol-neutral Rust execution contract,
Worker registry, and standalone Host. No Java control plane, NATS transport, or
external MCP service was required for this stage.

## Context

MCP Tool catalogs may contain annotations such as `readOnlyHint` and
`idempotentHint`. Those values are emitted by the same remote server whose Tool
the Runtime is about to call. Treating them as replay authority would let an
untrusted endpoint lower the failure boundary that protects external side
effects.

Freezing every MCP Tool as `Unknown` was safe but incomplete. A trusted local
operator may know that one concrete Tool is Pure, Idempotent, or
NonIdempotent. That decision must survive placement and recovery without also
removing the approval gate.

## Decision

1. RunExecution schema 18 adds `tool_effect_overrides` to each
   `McpServerSnapshot`, keyed by server-local Tool name. Absence means
   `ToolEffect::Unknown`.
2. A v17 or older command carrying an override is rejected. A v18 override is
   accepted only when the same qualified Tool appears in a signed Skill
   snapshot and the server scope is delegated to the Run.
3. The standalone Host exposes the same field in its explicit MCP JSON config
   and rejects an override outside that server's configured Tool allowlist at
   startup.
4. Discovery output and MCP annotations never select the effect. Catalog
   material provides name, description, input schema, and implementation
   digest only.
5. Every federated Tool remains `ApprovalMode::Ask` and
   `SandboxClass::Federated`, including Pure and Idempotent overrides.
6. The effect map enters the MCP server binding digest for schema 18. A
   replacement Host rejects a Checkpoint if the operator changes the map.
7. An Idempotent effect does not re-enable transport retries after an accepted
   `tools/call`. It changes failure/recovery classification only. Automatic
   retry would additionally require an end-to-end idempotency-key contract.

## Consequences

### Positive

- Replay semantics have an explicit trusted authority and are frozen per Run.
- Remote annotations cannot silently lower approval or turn an ambiguous side
  effect into a replay-safe result.
- Correctly declared Pure/Idempotent MCP failures can remain model-visible
  without forcing every failure into `run.indeterminate`.
- Recovery detects policy drift even when endpoint and catalog are unchanged.

### Negative and incomplete

- A wrong operator declaration can still misclassify a Tool; approval remains
  mandatory to limit that risk.
- There is no standard MCP idempotency key in this Runtime, so accepted calls
  remain one-shot at the transport layer.
- Codex and OpenClaw still have much broader MCP lifecycle, OAuth, Apps,
  resource, prompt, and cross-platform support.
- Real Linux cgroup enforcement remains unverified and disabled.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/config/src/mcp_types.rs` and
  `codex-rs/core/src/mcp_tool_call.rs`
- OpenClaw revision `58b4b9430457`,
  `src/agents/tool-replay-safety.ts` and
  `src/agents/embedded-agent-runner/run/attempt-tool-base-prepare.ts`
- `contracts/events/run-execution-requested.v18.example.json`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/{lib,mcp_gateway}.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `docs/evidence/2026-08-11-run-frozen-mcp-tool-effects.md`
