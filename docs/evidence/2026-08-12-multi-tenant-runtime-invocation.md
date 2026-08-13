# Multi-tenant Runtime invocation evidence — 2026-08-12

## Proven in this milestone

- `RunExecutionCommand` v20 rejects nil tenant/application/workload/Run/Session/
  Workspace/AgentVersion/attempt/worker/fencing identities and rejects a Skill
  signed for another application. v19 remains read-compatible.
- A real standalone loopback model Run starts with an explicit invocation,
  writes the same tenant/application/workload/Workspace identity into events
  and Worker Checkpoint 26, then refuses recovery under another application.
- One `EmbeddedRuntime` registers two isolated tenant profiles and runs A1, A2,
  and B1 with global capacity one. After A1 releases, the observed provider
  request order is B1 then A2.
- Admission enforces global, per-tenant, and per-Workspace active limits,
  bounded global/per-tenant queues, immediate cancelled-waiter cleanup, and no
  head-of-line block between two Workspaces of the same tenant.
- A forged/unregistered Workspace identity is rejected before Host construction
  or provider egress.
- One tenant/application/Workspace can register multiple immutable
  AgentVersion/workload/model profiles against one stable root pair; another
  Workspace identity cannot alias either persistent root.

## Validation

- `cargo test --workspace --quiet` — default parallel execution passed after
  synchronizing all schema-26/v20 fixtures; only tests explicitly requiring an
  external TLS NATS service remained ignored.
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- all contract JSON parsed with `jq`; `git diff --check` passed.

The Process Session acceptance tests use a 20-second bounded-yield observation
window around a script that emits after one second. A bounded yield is allowed
to return empty at its deadline; the wider test-only window prevents a
saturated full-workspace run from confusing macOS child scheduling delay with
an Agent-loop polling regression. The isolated file and the default parallel
workspace gate both passed.

Two `line-session` process groups left behind by earlier intentionally failed
acceptance runs exposed a test-fixture cleanup gap. The fixture now writes a
PID file, removes it on normal exit, and installs a Unix process-group cleanup
guard for all three users of that long-lived script. The Process Session file
then passed 9/9 with no residual matching process; its targeted Clippy gate also
passed. The two old groups were terminated before final cleanup.

All commands are Mac-native. No Java service, database, NATS, Docker, VM, or
Kubernetes workload is started.

## Remaining boundary

This milestone proves process-local multi-tenant invocation and admission. It
does not yet bind application/workload identity into the signed cloud workload
token or the remote Model/MCP gRPC request, and does not provide durable or
cross-node admission. Those are required before claiming cloud multi-tenant
end-to-end completion.
