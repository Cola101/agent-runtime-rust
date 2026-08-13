# MCP accepted-call response-loss evidence — 2026-08-11

## Behavioral RED/GREEN proof

- RED: a stdio session actor accepted `tools/call`, recorded its side effect and
  lost the response channel. The client silently opened a real replacement MCP
  process, called the Tool again and returned success. The test observed the
  duplicated side effect and failed with `an accepted ambiguous Tool call was
  retried`.
- GREEN: response-channel loss after accepted `tools/call` now returns an
  unreachable/unknown-outcome error immediately. No replacement MCP process is
  started and the side-effect marker contains exactly one call. Failed queue
  admission and safe discovery operations keep their existing reconnect path.

## Real Agent Loop gate

- A real loopback Streamable HTTP MCP server completes initialize and
  `tools/list`, accepts `tools/call`, increments an observable call counter, then
  sends a deliberately truncated HTTP body.
- A real loopback OpenAI-compatible model asks for that MCP Tool. The standalone
  Host writes `tool.execution.started`, receives the transport loss and produces
  one durable `run.indeterminate` with `effect=unknown` and
  `replay_safe=false`.
- The MCP counter remains one, no `tool.result` exists, no second model turn is
  requested, and the terminal Checkpoint is `indeterminate`.

## Reference comparison

- Codex limits transient service-operation retries to `tools/list`; its HTTP
  retryable-status allowlist excludes `tools/call`. It has much broader MCP
  lifecycle/OAuth/elicitation support, but the inspected path returns an error
  rather than a durable per-call Run reconciliation record.
- OpenClaw issues one guarded MCP `callTool`. Its Tool terminal observer records
  `executionStarted`, `replaySafe` and invalidates automatic replay after a
  potential side effect. That is materially closer than a generic Tool error,
  but the inspected MCP path does not expose this Runtime's durable
  `run.indeterminate` plus operator reconciliation contract.

## Validation boundary

- The HTTP MCP server, HTTP/SSE model peer, Agent Loop, event log, Checkpoint and
  side-effect counter are real local components.
- The stdio actor-loss test injects the actor failure after acceptance; the
  replacement attempt uses the real JSONL MCP fixture and proves the old replay.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider or API key was used.
- Production NATS publication and real Linux cgroup kernel behavior remain
  explicitly unverified.
