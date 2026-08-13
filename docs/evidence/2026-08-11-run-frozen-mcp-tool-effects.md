# Run-frozen MCP Tool effect evidence — 2026-08-11

## Behavioral RED/GREEN proof

- Protocol RED: RunExecution v18 decoded an operator effect map but silently
  discarded it. GREEN preserves the map, rejects pre-v18 smuggling, and rejects
  entries outside the signed Skill Tool declaration.
- Registry RED: an explicit `web_search=idempotent` policy still materialized
  as `Unknown`. GREEN applies the Run-frozen value while retaining
  `ApprovalMode::Ask` and `SandboxClass::Federated`.
- Recovery RED: changing only the effect map allowed a replacement Host to
  resume the old Checkpoint. GREEN binds the map into the MCP server identity
  digest and rejects the drift before model egress.
- Configuration RED: the standalone Host accepted an effect declaration for a
  Tool outside its allowlist. GREEN rejects it during Host startup.

## Real Agent Loop gate

- A real loopback OpenAI-compatible model requests `mcp:local/search`.
- A real Streamable HTTP MCP server accepts the call, increments an observable
  side-effect counter, and then truncates its HTTP response.
- With no operator override, the server also advertises `readOnlyHint=true` and
  `idempotentHint=true`; the Runtime ignores those claims, records one
  `run.indeterminate`, emits no Tool Result, and calls MCP exactly once.
- With the local Run-frozen override `search=idempotent`, the same transport
  failure becomes a redacted error Tool Result, the model receives a second
  turn, and the Run succeeds. MCP is still called exactly once; the override
  does not reopen generic transport replay.

## Reference comparison

- Codex has server defaults and per-Tool approval overrides, plus a
  server-level `supports_parallel_tool_calls`. In Auto/Writes modes its
  inspected approval path uses MCP `read_only_hint`; this is richer UX but is
  not a durable per-Run effect/reconciliation contract.
- OpenClaw has a mature Tool replay guard and explicitly treats MCP plugin Tools
  as not restart-safe in the inspected preparation path. MCP servers may select
  sequential versus parallel execution, but the inspected path does not expose
  a durable per-Tool operator effect snapshot.
- This Runtime is narrower in MCP breadth, but its effect authority, Checkpoint
  binding, and indeterminate reconciliation are more explicit in this one
  failure-safety dimension.

## Validation boundary

- Real local sockets, model streaming, MCP initialization/discovery/call,
  event log, Checkpoint, replacement Host, and side-effect counter were used.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider, or API key was used.
- Real Linux cgroup behavior and external MCP interoperability for this v18
  field remain explicitly unverified.
