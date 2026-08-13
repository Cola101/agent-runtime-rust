# Schema-five launch boundary and legacy Starting safety evidence — 2026-08-10

## Behavioral RED/GREEN proof

- RED: schema-3 Linux `Starting` with a missing group became `Terminated`.
  GREEN: it migrates as `legacy_unknown` and persists `Indeterminate`.
- RED: schema-2 Unix `Starting` with no live identity became `Terminated`.
  GREEN: it also migrates as `legacy_unknown` and persists `Indeterminate`.
- RED: a `Starting/prepared` group with `populated=0` fell into terminal cleanup
  and returned an error. GREEN: it writes `cgroup.kill=1` and persists
  `Indeterminate/cleanup_pending` without claiming the Tool never ran.
- RED: the first schema-4 Unix `Starting/unprepared` fixture became
  `RecoveredMissing`. GREEN: schema 5 migrates it to `legacy_unknown` and keeps
  the result indeterminate.
- RED: a real Unix start published `Running` at operation sequence 2, proving no
  independent prepared transition existed. GREEN: the real child reaches
  `Running` at sequence 3 after a separate durable pre-spawn transition.
- A real active schema-2 Unix process was then handed to a replacement Manager.
  The Manager returned `Reattached`, preserved the original PID, rewrote schema
  5 `active` with recovery count 1, and closed the original process normally.

## Validation

- `agent-tool-runtime` passed 82 tests under default parallelism.
- `cargo check --workspace --all-targets` passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo fmt --all -- --check` passed.
- `cargo test --workspace --all-targets --quiet` passed under default
  parallelism with zero failures.
- Five external live tests remain explicitly ignored: four external MCP server
  cases and one live TLS NATS case.

## Reference comparison

- Codex `ff352fab6209` remains stronger in live process-group setup,
  parent-death signaling, PTY, sandbox integration and product-path coverage.
  Its inspected process helpers are not a durable replacement-Host launch
  journal for multi-tenant Tool sessions.
- OpenClaw `58b4b9430457` remains stronger in Unix/Windows tree termination,
  timeout arbitration, PTY/adapters and cross-platform operations. Its inspected
  supervisor is process-local and does not preserve this launch phase across a
  replacement Host.
- This Runtime is stricter only in the narrow crash/replay decision: absence of
  a process is not accepted as proof that a side effect never occurred.

## Validation boundary

- The active schema-2 test uses a real native child and real PID/PGID/identity
  lock on macOS.
- cgroup cases use ordinary directories and prove byte protocol plus durable
  decisions, not real Linux enforcement.
- Graphify AST navigation reported dangling/collapsed relationship edges, so it
  was used only to locate migration/recovery code; source and runtime behavior
  remained authoritative.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider or API key was used.
- Production Linux cgroup selection remains fail-closed.
