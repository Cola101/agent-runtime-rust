# Interrupted Tool uncertainty evidence — 2026-08-11

## Behavioral RED/GREEN proof

- Worker RED: cancelling an already-started `NonIdempotent` or `Unknown` Tool
  produced `run.cancelled`; duration expiry produced `run.timed_out`. GREEN
  produces one bound `run.indeterminate` with the original Tool evidence,
  `replay_safe=false`, the interruption cause and requested status.
- Host RED: a real trusted shell process and a real Streamable HTTP MCP call
  were both reaped/closed, but the Run still claimed cancelled or timed out.
  GREEN closes the same resources while preserving the unsafe Tool outcome as
  indeterminate.
- Durability RED: the terminal event said indeterminate while the on-disk
  Checkpoint remained Running. GREEN persists the terminal Checkpoint before
  publishing the event.
- Control cases prove that an interruption before an unsafe start, or after a
  replay-safe Tool start, retains the requested cancelled/timed-out terminal.

## Real Agent Loop gate

- A loopback OpenAI-compatible model requests a real trusted `shell.exec` or a
  real `mcp:local/search` Tool.
- The Runtime durably emits `tool.execution.started`; the native process starts
  or the MCP server accepts the HTTP request.
- Caller cancellation or the Run duration budget closes the real process tree
  or HTTP socket within the bounded test deadline.
- Because both Tools are frozen as `NonIdempotent`/`Unknown`, the final event
  and Checkpoint are `run.indeterminate`, with no automatic Tool replay.
- Cancelling MCP discovery before any Tool start remains `run.cancelled`, and a
  discovery duration expiry remains `run.timed_out`.

## Reference comparison

- Codex owns cancellation tokens and aborts unfinished Tool dispatch, returning
  an aborted Tool response. The inspected path has broader Tool/MCP lifecycle
  integration but no durable per-Run effect check that converts an interrupted
  started Tool into an indeterminate terminal.
- OpenClaw preserves timeout/abort alongside monotonic `replayInvalid` and
  `hadPotentialSideEffects` metadata. This is the closest reference behavior;
  the Rust Runtime expresses the unsafe outcome as a first-class Run terminal
  and binds it to a durable started event and Checkpoint.
- This is a narrow recovery-safety advantage, not an overall maturity claim:
  both references remain far ahead in MCP breadth, PTY/terminal UX and
  cross-platform execution.

## Validation boundary

- Real local sockets, model streaming, MCP discovery/call, trusted native child
  process, process-tree reaping, event log and Checkpoint were exercised.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider or API key was used.
- MCP protocol cancellation notifications, live NATS publication and real
  Linux cgroup behavior remain explicitly unverified.
