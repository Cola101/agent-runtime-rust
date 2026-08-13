# ADR-0045: Protocol-neutral MCP backend in the standalone Runtime Host

## Status

Accepted. Implemented for credential-free Streamable HTTP MCP endpoints in the
standalone Rust Host; local stdio is added by ADR-0046. The cloud Worker
continues to use the credential-holding gRPC Gateway backend.

## Context

ADR-0044 made MCP discovery and Kernel mutation independent of NATS, but the
shared client still constructed a tonic gRPC stub. `runtime-host` therefore
could not execute a real MCP Tool without starting another Gateway process,
which contradicted the requirement that the Rust Runtime run independently of
Java, PostgreSQL, NATS, Docker, Kubernetes and auxiliary data-plane processes.

The decision must preserve these non-functional requirements:

- **Security:** a local in-process client must not silently gain access to the
  cloud credential-unsealing domain; endpoint validation, redirect rejection,
  DNS pinning and response limits remain mandatory.
- **Reliability:** recovery must bind both the frozen Tool catalog and the
  remote authority that supplied it. Equal schemas from another endpoint are
  not equivalent.
- **Performance and scale:** the process-wide fair discovery scheduler and the
  per-Run concurrency/deadline policy must remain above the transport backend.
- **Operations:** a credential-free local MCP Run must need only one Runtime
  Host process and its explicitly configured model/MCP endpoints.

Codex has a substantially broader MCP client: local/remote stdio, Streamable
HTTP, OAuth, cached catalogs, failed-startup reconnect and required/optional
server semantics. OpenClaw now also has stdio, SSE, Streamable HTTP, OAuth,
session/requester-scoped runtimes, connection revalidation and idle/LRU
lifecycle management. Neither project, however, has this platform's need to
resume a fenced Run on another Worker while proving the same remote MCP
authority, Tool catalog and execution policy.

## Decision

1. The Worker-facing client depends on `McpFederationBackend`, not tonic. The
   existing gRPC implementation becomes one backend rather than the client
   identity.
2. The standalone Host supplies an in-process backend that reuses the Model
   Gateway's hardened HTTP MCP implementation. It does not duplicate protocol,
   DNS, redirect or response-boundary code.
3. Local direct MCP accepts only an empty credential envelope. A sealed cloud
   credential is rejected because the local Host owns no unsealing key or
   separate credential trust domain.
4. The Host uses the same `McpDiscoveryCoordinator` for new and restored Runs.
   It discovers and attaches Tools before `run.started`; on recovery it attaches
   the rediscovered catalog before validating the Checkpoint.
5. Checkpoint schema 9 stores a canonical digest of each configured server's
   ID, name, endpoint and credential-envelope digest. Recovery requires that
   digest in addition to the frozen Tool catalog and Runtime policy. MCP
   Checkpoints below schema 9 fail closed because they cannot prove authority.
6. Transport construction remains outside the Kernel. ADR-0046 adds stdio
   without changing Agent Loop, approval, budget or Checkpoint semantics; SSE
   and OAuth must preserve the same boundary.

## Consequences

### Positive

- A real MCP Tool can complete discovery, model selection, approval, execution,
  result feedback, Checkpoint and recovery in one native Rust Host process.
- Cloud credential isolation remains unchanged; no private key or plaintext
  provider credential enters the Worker-facing interface.
- Discovery fairness, deadlines and single-writer Kernel mutation are shared by
  local and cloud modes instead of being reimplemented per transport.
- A same-schema impostor endpoint cannot inherit a recovered Run's transcript
  or approval state.

### Negative

- Standalone mode supports credential-free HTTP and local stdio Tool methods,
  but remains a much smaller protocol and authentication surface than Codex or
  OpenClaw.
- Checkpoint schema 9 intentionally strands older MCP Runs rather than restoring
  them without an authority proof.
- The binary configuration entry is a bounded JSON file rather than a mature
  discovery, onboarding or credential flow.

### Neutral

- The compatibility alias `GrpcMcpFederationClient` remains for existing cloud
  callers, but new protocol-neutral code uses `McpFederationClient`.

## Failure modes and mitigations

- **Changed endpoint with the same catalog:** rejected by schema 9 server
  binding before model work resumes.
- **Sealed credential in local mode:** rejected before any network call.
- **Slow or unreachable server:** bounded by the frozen per-server and total
  discovery deadlines; cancellation releases shared admission capacity.
- **Host crash during discovery:** discovery state is ephemeral and reruns from
  the durable command/Checkpoint; no partial catalog mutates the Kernel.
- **Host crash after a Tool result:** recovery rebuilds the catalog but does not
  replay the completed Tool call recorded in the Checkpoint.

## Alternatives Considered

- **Require a local gRPC Gateway process:** rejected because it preserves an
  unnecessary process boundary and violates standalone operation.
- **Copy the HTTP MCP client into `runtime-host`:** rejected because security
  fixes would drift across two protocol implementations.
- **Let the local Host unseal cloud credentials:** rejected because it collapses
  the credential isolation boundary into the Agent process.
- **Bind only the Tool catalog digest:** rejected by a real regression test; a
  second endpoint can advertise the same catalog and still be a different
  authority.

## References

- ADR-0040 MCP tool federation
- ADR-0041 runtime execution policy snapshot
- ADR-0044 single-writer MCP discovery coordinator
- ADR-0046 standalone stdio MCP session and process lifecycle
- Codex `codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs`
- Codex `codex-rs/rmcp-client/src/stdio_server_launcher.rs`
- OpenClaw `src/agents/mcp-transport.ts`
- OpenClaw `src/agents/mcp-stdio-transport.ts`
- OpenClaw `src/agents/agent-bundle-mcp-manager-lifecycle.ts`
- OpenClaw `src/agents/mcp-oauth.ts`
