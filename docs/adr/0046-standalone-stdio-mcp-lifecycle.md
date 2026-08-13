# ADR-0046: Standalone stdio MCP session and process lifecycle

## Status

Accepted and implemented for the native standalone Rust Runtime Host.

## Context

ADR-0045 removed the mandatory gRPC Gateway from local MCP execution, but only
for Streamable HTTP. A large part of the MCP ecosystem is launched as a local
stdio child process. Merely spawning such a process is insufficient: MCP
initialization is stateful, a Tool call must use the same catalog authority as
discovery, cancellation must interrupt initialization as well as calls, and a
shell-based server may leave grandchildren behind after its direct child exits.

The decision preserves these non-functional requirements:

- **Standalone operation:** no Java, PostgreSQL, NATS, Docker, Kubernetes or
  auxiliary Gateway process is required.
- **Security:** inherited environment is an allowlist plus explicit operator
  overrides; executable and working directory are absolute and validated;
  output is bounded.
- **Reliability:** one server session survives discovery and Tool calls, while
  Host shutdown, timeout and caller cancellation reap the entire process group.
- **Recovery:** the canonical stdio command, arguments, environment and working
  directory contribute to the server-authority digest stored in Checkpoint
  schema 9, so another process configuration cannot resume the Run.

Codex uses a persistent JSONL stdio transport, clears the environment before
adding a small default set, starts a new process group and escalates group
termination. OpenClaw also uses a persistent SDK stdio transport, detached
process groups and explicit close/force-close tree cleanup. Both support a
broader MCP surface and lifecycle than this implementation.

## Decision

1. `LocalMcpTransportConfig` is a tagged transport enum. `streamable_http`
   carries an endpoint; `stdio` carries an absolute executable, bounded
   arguments/environment and an optional absolute working directory.
2. The binary reads an optional JSON server list from
   `AGENT_RUNTIME_LOCAL_MCP_CONFIG`. The file must be regular and at most 1 MiB;
   floating discovery from the user's environment is not allowed.
3. Each configured stdio server owns one persistent background session actor.
   It performs MCP initialize once, serializes JSONL requests and reuses the
   same process for `tools/list` and `tools/call`.
4. A Tool call re-lists the catalog and checks its frozen digest immediately
   before execution. HTTP and stdio share the same catalog and result
   conversion helpers.
5. The process starts in its own process group with a cleared, allowlisted
   environment. Responses are limited to 256 KiB. Unknown server-to-client
   requests receive a JSON-RPC method-not-supported response.
6. Request cancellation covers initialize, discovery and Tool calls. Closing a
   session sends TERM to the group, waits briefly, then sends KILL if any group
   member remains. Windows uses `taskkill /T /F` as the current fallback.
7. `LocalRuntimeHost::shutdown` drains all stdio sessions and awaits their
   cleanup. The one-shot binary and each daemon-owned Run invoke it before their
   async runtime/task can disappear.
8. The Checkpoint authority endpoint for stdio is a SHA-256 URI over canonical
   serialized transport configuration. It never embeds environment values in
   events or logs.

## Consequences

### Positive

- A shipped `runtime-host run` binary can execute a real stdio MCP Tool and exit
  without leaving its server or descendants behind.
- Stateful MCP servers remain compatible across discovery and execution.
- Timeout, initialization hang, normal exit and recovery all share one cleanup
  and authority-binding path.
- The Agent Loop, approval, budget and Checkpoint logic remain transport
  neutral.

### Negative

- Sessions are serialized per server and have no idle TTL, LRU cap, continuous
  health probe or cached background reconnect. ADR-0047 adds bounded
  failed-startup discovery retry and startup health reporting.
- The minimal client covers initialize, `tools/list` and `tools/call`; it does
  not yet implement Resources, Prompts, elicitation, sampling, roots or logging.
- Windows cleanup is less precise than Unix process-group handling and has not
  been exercised in this macOS stage.
- Local stdio processes are operator-trusted native programs, not tenant code in
  a strong sandbox. This decision does not authorize stdio in multi-tenant cloud
  Workers.

## Failure modes and mitigations

- **Initialize or request hangs:** caller cancellation wakes the actor and
  enters process-group cleanup.
- **Direct child exits before a TERM-ignoring grandchild:** group existence is
  checked independently of direct-child reaping and escalated to KILL.
- **Tokio runtime exits before detached cleanup finishes:** explicit Host
  shutdown cancels and awaits every actor.
- **Server configuration changes before recovery:** schema 9 authority digest
  mismatch fails closed before model execution.
- **Oversized or malformed JSONL:** rejected as a bounded protocol error; the
  session closes.

## Alternatives Considered

- **Spawn a new process for every RPC:** rejected because initialize/session
  state and catalog authority would not survive from discovery to execution.
- **Rely on `kill_on_drop` or kill only the direct child:** rejected by a real
  TERM-ignoring grandchild test.
- **Inherit the full user environment:** rejected because ambient credentials
  and configuration would become undeclared Tool capabilities.
- **Make the standalone Host call a separate Gateway for stdio:** rejected
  because it reintroduces the process dependency ADR-0045 removed.
- **Adopt the full MCP SDK immediately:** deferred, not treated as equivalent;
  the current bounded Tool subset is explicit and the missing protocol methods
  remain tracked as gaps.

## References

- ADR-0040 MCP tool federation
- ADR-0045 protocol-neutral MCP backend and standalone Host
- Codex `codex-rs/rmcp-client/src/local_stdio_transport.rs`
- Codex `codex-rs/rmcp-client/src/stdio_server_launcher.rs`
- Codex `codex-rs/rmcp-client/src/utils.rs`
- OpenClaw `src/agents/mcp-stdio-transport.ts`
