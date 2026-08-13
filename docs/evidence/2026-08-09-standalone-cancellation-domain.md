# Standalone cancellation domain evidence — 2026-08-09

## Scope

This evidence covers process-crash-safe local cancellation intent and real
model, trusted native Tool, MCP discovery and MCP Tool-call cancellation. It
does not claim power-loss durability, a synthetic Kernel event before the first
Checkpoint, ownership-aware independent child kill, nested approval routing or
parallel child supervision.

## Behavioural proof

- `an_acknowledged_cancellation_survives_a_daemon_crash_without_reinvoking_the_model`
  observed the original RED: IPC returned `Accepted` while `run.json` still said
  `Running`. It now requires non-running state before acknowledgement returns.
- `a_restarted_daemon_finishes_a_durable_cancellation_intent_without_reinvoking_the_model`
  leaves `Cancelling` plus a real Checkpoint, starts a replacement daemon, emits
  exactly one `run.cancelled` and proves the loopback provider saw no second call.
- `cancelling_a_running_native_tool_reaps_it_and_ends_the_run_as_cancelled`
  starts the shipped trusted Shell Tool, observes its PID and marker, cancels the
  root token, verifies the process exits and receives `RunStatus::Cancelled`.
- `cancelling_an_inflight_mcp_tool_call_closes_http_and_ends_the_run_as_cancelled`
  drives a real model Tool Call into a blocking HTTP MCP `tools/call`, observes
  peer close and receives `RunStatus::Cancelled` rather than `ToolExecution`.
- `cancelling_mcp_discovery_closes_initialize_and_ends_the_run_as_cancelled`
  blocks real MCP `initialize`, cancels before catalog admission, observes peer
  close, emits `run.cancelled` and proves no model request was accepted.
- `recovery_reconciles_a_terminal_event_before_a_stale_cancellation_intent`
  recreates the terminal-event/local-record crash window. Its RED resumed one
  cancelled attempt after an already durable `run.succeeded`; recovery now
  repairs `run.json` from that event, appends nothing and calls no provider.

## Source comparison used

- Codex `ff352fab6209`: `exec.rs` combines timeout and cancellation tokens;
  `codex_delegate.rs` uses downward child tokens; `session/mcp.rs` has explicit
  MCP startup cancellation.
- OpenClaw `58b4b9430457`: `subagent-control.ts` checks controller ownership,
  aborts the live run, clears follow-up/lane queues, persists `abortedLastRun`,
  rechecks the latest target state and cascades to descendants.

## Validation

- Targeted RED failures were the stale `Running` record, two
  `ToolExecution("tool execution was cancelled")` results and an MCP initialize
  connection that survived the root cancellation. The kill/completion race RED
  also resumed one Run whose successful terminal event was already durable.
- Host tests: 46 passed, 0 failed.
- Host/Worker Clippy with all targets and features under `-D warnings`: passed.
- Full workspace: 384 passed, 0 failed, 5 explicitly ignored live integration
  cases; 389 test items listed. Workspace Clippy and formatting passed.
- Residue audit found no Tool test temporary directory, runtime child process,
  Unix socket or partial file. The reusable Rust `target` cache was retained.
