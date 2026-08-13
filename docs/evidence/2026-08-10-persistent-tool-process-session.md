# Persistent Tool process session evidence — 2026-08-10

## Real process and replacement proof

- A real `/bin/sh` script printed `ready`, accepted later stdin and returned an
  exact echo. A replacement manager read the existing manifest, retained the
  original PID, wrote new input through the FIFO and closed the process group.
- A separate test executable started the session and exited with
  `std::process::exit(73)`, bypassing destructors. The Tool process survived in
  its own group; a new manager returned `reattached`, interacted with the same
  PID and terminated it. This is operating-system process replacement, not an
  in-memory object swap.
- A second standalone Host test interrupted the Agent after the durable start
  result had entered the next model invocation. A replacement Host resumed the
  Checkpoint, consumed one explicitly budgeted Provider retry, wrote/polled/
  closed the original session and proved `process.start` occurred exactly once.

## Model-visible Agent loop proof

- A real loopback OpenAI-compatible HTTP/SSE model called
  `process.start → process.poll → process.write → process.poll → process.close`.
- Every Tool used the normal Kernel planning, delegated scope, approval,
  durable-start and bound Tool Result path. The final Assistant answer was
  `persistent process complete`, the Run succeeded and its Checkpoint existed.
- The executable was configured explicitly. No implicit shell, external model
  key, Java control plane, PostgreSQL, NATS, Docker or Kubernetes was used.

## Fail-closed and cleanup proof

- A wrong tenant and a wrong canonical Workspace both returned `AccessDenied`.
  An output cursor beyond the spool length returned `InvalidCursor`.
- Modifying the manifest without updating its digest returned `Indeterminate`;
  the manager did not signal the recorded PID.
- Explicit close killed a TERM-ignoring background descendant with TERM→KILL.
  A separate regression proved natural leader exit applies the same escalation,
  so a stubborn background process cannot become an orphan.
- SIGINT reached the registered process group and converged to a terminal state.
- Sixty-five sequential one-shot sessions completed successfully, proving the
  64-session limit counts live processes rather than retained terminal history.

## Reference comparison

- Codex remains ahead in shipped interactive command UX and mature in-memory
  process-store handling. This milestone adds a durable tenant/Workspace-bound
  reattach contract not found in the inspected `ProcessStore` path.
- OpenClaw remains ahead in PTY resize, pause/resume, node-host integration and
  cross-platform terminal handling. This Runtime now has stronger explicit
  cross-Host identity evidence, but not the same terminal feature breadth.
- Graphify traced `LocalRuntimeHost → WorkerProcessor → ToolExecutor` and the
  durable `record_tool_execution_started → Checkpoint` boundary before the Tool
  family was attached, preventing a direct executor shortcut around policy.

## Validation boundary

- `cargo test --workspace --all-targets --quiet` completed with exit code 0.
  The authoritative test listing contains 489 tests: 484 executed successfully
  and 5 external live tests are explicitly ignored.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.
- Contract JSON, diff whitespace and process-residue checks passed. No external
  Provider key, Java service, PostgreSQL, NATS, Docker or Kubernetes was used.

This is the historical first-phase evidence. Its next-gap statement is closed
by ADR-0071 and `2026-08-10-persistent-process-session-governance.md`; the test
counts above remain the authoritative snapshot for ADR-0070 rather than being
rewritten as if the later governance cases existed then.
