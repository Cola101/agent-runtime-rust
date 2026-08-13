# MCP negotiated capabilities and default-deny evidence — 2026-08-11

## Behavioral RED/GREEN proof

- RED: real Streamable HTTP and stdio MCP peers sent unadvertised reverse
  requests during `tools/list` and `tools/call`. The old HTTP path ignored them;
  the old stdio path replied `-32601` but accepted the following success result.
  Both Tool Runs continued into a second model turn and succeeded.
- GREEN: both transports return `-32601` with the server's exact string request
  ID, retire the violating session, and never accept the later Tool result.
- Required discovery violations fail before model egress. The real counting
  Provider observed zero calls for HTTP `roots/list` and stdio `roots/list`.
- Violations after an Unknown Tool started create durable
  `run.indeterminate`, one `tool.execution.started`, no `tool.result`, and one
  Provider call. HTTP exercised `sampling/createMessage`; stdio exercised
  `elicitation/create`.
- Separate live HTTP handshake tests prove an unsupported selected protocol or
  missing server `tools` capability stops after the initialize response, before
  the initialized notification or `tools/list`. A response carrying another
  request's JSON-RPC ID is independently rejected.

## Authority boundary

- Client capabilities remain empty, so reverse sampling, elicitation, and roots
  have no authority to enter model routing, approvals, budgets, or Workspace.
- HTTP handles JSON and incremental SSE responses; stdio handles JSONL and gives
  the peer a bounded rejection-drain window before process-group cleanup.
- A protocol rejection is not evidence that a previously started remote side
  effect was rolled back. Existing effect-aware `indeterminate` semantics stay
  authoritative.
- Resources and Prompts are client-initiated server features, not server-initiated
  requests. They remain unimplemented and are reported separately.

## Reference comparison

- Codex explicitly advertises an elicitation capability and routes
  `elicitation/create` through an elicitation request manager and interactive
  response path. It remains ahead in enabled MCP breadth and UX. The inspected
  client capability builder did not advertise sampling or roots.
- OpenClaw's inspected loopback Gateway is primarily an MCP server. It negotiates
  its `tools` server capability and handles client calls, but no equivalent
  sampling/elicitation/roots client reverse-request path was found. This is not
  evidence about every OpenClaw integration outside the inspected Gateway.
- The Rust Runtime is ahead only in its narrow, tested rule that an ungranted
  reverse request is connected to model-egress prevention and durable Tool
  uncertainty. It is still behind Codex on elicitation and behind both projects
  on overall MCP product breadth.

## Validation boundary

- Exercised real loopback TCP, HTTP JSON, HTTP SSE, sibling response POST, local
  stdio process, exact JSON-RPC IDs, Provider request counts, Kernel events,
  event replay, Checkpoint, session retirement, and process cleanup.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, VM, external Provider, API key,
  or external MCP service was used.
- Approved elicitation/sampling, roots, Resources, Prompts, OAuth, Apps, and the
  cloud gRPC bidirectional path remain unverified and unimplemented.
