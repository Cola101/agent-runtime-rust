# Indeterminate Tool reconciliation evidence — 2026-08-10

## Real crash and side-effect proof

- A real OpenAI-compatible loopback model requested the trusted native
  `workspace.write_text` Tool, classified `NonIdempotent`.
- The Tool was a real child process. It wrote and `fsync`ed `side-effect.txt`,
  then stayed alive long enough for the test to abort the first Host after the
  durable started Checkpoint but before a Tool result existed.
- A replacement Host restored the same Run and emitted exactly
  `run.restored` followed by `run.indeterminate`. It did not call the source
  Provider again and the marker contained one write, proving no automatic
  replay.
- The terminal event binds call ID, Tool name, binding digest, effect, sandbox,
  original attempt, started event/sequence and `replay_safe=false`; the terminal
  Checkpoint retains the outstanding request and durable started evidence.

## Reconciliation proof

- An `Applied` version-1 command persisted its receipt before egress, kept the
  source Run `indeterminate`, and started a fresh deterministic Run whose ID is
  the reconciliation ID.
- The continuation model received a Tool Result bound to the original call and
  the operator-supplied result. The old Tool process was not executed again.
- Repeating the exact command after the Provider had exited returned the stored
  result without network or Tool work. Changing the decision at the same version
  failed with a version conflict.
- A separate `Unresolved` version-1 command persisted without continuation.
  Version 2 `NotApplied` then started a new Run with explicit
  `operator_confirmed_not_applied` evidence. Exact unresolved duplicates were
  idempotent and the original marker was unchanged.

## Contract and recovery proof

- Protocol tests reject final decisions without bounded continuation input and
  reject `Unresolved` commands that attempt to continue. Applied Tool Result
  content above 256 KiB is rejected before persistence or model egress.
- A reconciliation identity that aliases the source Run is rejected instead of
  treating that unrelated terminal Checkpoint as a continuation.
- Worker restore now distinguishes replay-safe unfinished calls from exactly one
  ambiguous side effect. The latter materializes a stable terminal event rather
  than escaping as an internal restore error.
- The optional NATS Worker uses the same `TerminateIndeterminate` path, publishes
  and Checkpoints before acknowledging, but no external NATS service was started
  in this milestone.

## Reference comparison

- Codex normalizes Tool history and has a richer interactive Tool/session
  lifecycle. The inspected paths do not provide this generic durable
  operator-decision-to-new-Run contract.
- OpenClaw refuses ambiguous restart replay and dead-letters delivery when exact
  side-effect evidence is absent. This Runtime applies the same fail-closed
  principle to arbitrary Tool effects, then adds an immutable source Run and a
  versioned manual continuation contract.
- Graphify traced Checkpoint restore through `recovery_action` into both Host and
  Worker publication paths before the change. That prevented fixing only the
  standalone wrapper while leaving the Kernel/Worker state transition divergent.

## Validation boundary

- Full Rust workspace: 474 passed, 0 failed, with 5 external live tests
  explicitly ignored; 479 tests total.
- `cargo check --workspace --all-targets`, Clippy with all targets/all features
  and `-D warnings`, plus Rust formatting all passed in the same turn.
- The protocol reconciliation contract contributed three tests; the standalone
  Host suite contributed the real crash/reconciliation test. Kernel and Worker
  recovery assertions were strengthened without inflating the case count.

No Docker, Java, PostgreSQL, NATS, external daemon or external API key was used.
Loopback HTTP and a real child process prove local Runtime semantics, not live
vendor or distributed broker compatibility.
