# Signed Edge Task and durable local outbox evidence — 2026-08-13

## Proven

- A tampered Ed25519 task, an unknown signing key, an expired task and a stale
  node generation are rejected before Runtime execution.
- The signed task carries complete tenant/application/workload/Workspace/
  AgentVersion/ModelPolicy identity and the exact Workspace owner epoch into a
  pre-registered `EmbeddedRuntime` profile.
- A real local HTTP/SSE OpenAI-compatible provider completes the normal
  Runtime Agent Loop. Runtime events and the terminal receipt enter the local
  outbox in order with the complete identity and verified payload digest.
- Re-delivering the same signed task after node restart returns the persisted
  result and produces only one provider request, even after its authority to
  start new execution has expired.
- If the Runtime terminal event exists but the Edge terminal receipt does not,
  replacement reconciliation produces the same result without contacting a
  deliberately dead provider endpoint.
- State roots reject concurrent writers, identity/generation replacement,
  outbox gaps, Runtime event gaps, oversized event payloads, cursor regression
  and ACK beyond the last emitted sequence.
- Different task IDs cannot name one Run; a lower Workspace owner epoch is
  rejected after the local high-water mark advances.
- Embedded profiles canonicalize state roots so tenant Workspaces cannot alias
  one state directory. Runtime event I/O errors fail closed; event data and
  Checkpoint commit points are synchronized before recovery uses them.
- Edge task lifetime arithmetic rejects pathological signed timestamps without
  panicking.

## Observed RED to GREEN

- The first real loop failed because the receipt cursor referred to four
  Runtime events while the outbox contained none. Runtime events are now
  identity-validated and committed before the terminal receipt.
- A damaged snapshot with an empty unacknowledged outbox reopened successfully.
  Reload now requires a contiguous sequence through `next_outbox_sequence`.
- `i64::MIN → i64::MAX` task lifetime panicked in subtraction. Validation now
  uses checked arithmetic and returns `InvalidLifetime`.
- A public store commit accepted Runtime event 2 without event 1. Commits now
  require exact `previous + 1` sequence continuity.
- The same state root reopened under another node ID/generation. Node creation
  now durably binds the root to one exact identity and generation.
- A Runtime event larger than 1 MiB entered the atomic snapshot. It is now
  rejected before persistence.
- Two lexical paths to one state root bypassed Workspace ownership; the Runtime
  now creates, canonicalizes and indexes the resolved directory.
- Event-log read errors were treated as empty history; only a missing log is
  empty now, while other read/parse errors fail closed.
- A lower Workspace epoch remained executable after a higher owner; the node
  now persists and enforces a Workspace-scoped epoch high-water mark.

## Targeted validation

- `agent-edge-node`: 15 signature/generation, store/outbox, real
  Runtime/restart and crash-window tests pass.
- `agent-protocol --test edge_task_contract`: 4/4 pass.
- `agent-runtime-host --test embedded_multi_tenant`: 7/7 pass.
- Edge Node and Runtime Host Clippy `--all-targets -D warnings` pass.
- The PTY supervisor removes its global Unix socket even when a short-lived
  embedding deletes the state root before the idle lifecycle write; this
  prevents native test runs from accumulating stale socket files. Its Start
  request now also fences the expected supervisor generation before spawn,
  upgrades the wire protocol/capability, and re-handshakes at most once.
- The complete Rust workspace passes 642 tests with 6 environment-dependent
  tests ignored when run with eight test threads; `cargo check --workspace
  --all-targets`, workspace Clippy with warnings denied, formatting and diff
  whitespace checks also pass.
- All tests are Mac-native; no Java, database, broker, Docker, VM or
  Kubernetes service was started.

## Remaining boundary

This evidence proves a transport-neutral local substrate. It does not prove
device enrollment, device-held signing keys, Edge mTLS, an outbound reconnect
loop, capability discovery, authenticated remote ACK, approval continuation,
distributed owner leases, autonomous Accepted-task scanning, safe receipt GC,
offline Workspace merge or a production retention store. The Edge binary still
does not run a network daemon.
