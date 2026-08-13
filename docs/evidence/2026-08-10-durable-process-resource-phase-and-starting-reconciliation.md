# Durable process resource phase and Starting reconciliation evidence — 2026-08-10

## Behavioral TDD proof

- RED: a real running Process Session serialized no resource phase. GREEN:
  schema 4 records `active`, and a real close records `cleaned`.
- RED: `Starting` with no process identity and no group returned an unclassified
  indeterminate error. GREEN: replacement sweep persists `RecoveredMissing`
  and `cleaned`.
- RED: `Starting` with a populated group left `cgroup.kill` untouched. GREEN:
  replacement sweep writes `1`, persists `Indeterminate`, and leaves durable
  `cleanup_pending` for later removal.
- RED: malformed controller evidence returned an error without a durable state.
  GREEN: the resource is quarantined with a kill attempt and the manifest
  remains persistently `Indeterminate` instead of being retried.
- RED: an invalid Unix terminal manifest could claim cleanup was pending.
  GREEN: backend/state/resource-phase invariants reject it.

The existing terminal cleanup fixture was corrected to include the same
identity lock as a real session. This preserved the ownership check instead of
bypassing it for the test.

## Validation

- `agent-tool-runtime` passed 76 tests under default parallelism.
- `cargo check --workspace --all-targets` passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo fmt --all -- --check` passed.
- `cargo test --workspace --all-targets --quiet` passed under default
  parallelism.

The first whole-workspace run exposed two existing tests whose 3-second test
observation deadline was too small under concurrent machine load. Both passed
individually, and the complete eight-test governance suite passed 20 consecutive
runs (160/160). Their test-only observation deadlines are now 15 seconds; the
Runtime's execution deadline, cancellation, and resource semantics are
unchanged. The second default-parallel workspace run passed.

## Reference comparison

- Codex `ff352fab6209` remains stronger in live process groups, parent-death
  behavior, PTY, sandbox integration and product-path coverage. The inspected
  process helpers do not expose an equivalent durable multi-tenant resource
  phase ledger for a replacement Host.
- OpenClaw `58b4b9430457` remains stronger in Unix/Windows process-tree kill,
  timeout arbitration, PTY/adapters and operational breadth. Its inspected
  process supervisor is process-local rather than a replacement-Host resource
  journal.
- This Runtime is stricter only in the narrow digest-bound phase transition and
  conservative ambiguous-start recovery. It is not ahead in platform breadth.

## Validation boundary

- All cgroup protocol tests in this run used ordinary directories on macOS.
  They prove durable decisions, ordering, fd-relative authority and byte-level
  protocol only.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider, or API key was used.
- Production selection still returns `linux_cgroup_v2_backend_not_wired`.
- Direct legacy migration fixtures and real Linux pressure/recovery are the next
  required gates.
