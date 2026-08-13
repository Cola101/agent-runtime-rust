# Edge device enrollment evidence — 2026-08-13

## Proven

- A stable owner-only Ed25519 device identity survives restart and signs a
  challenge-bound enrollment request containing the exact capability manifest.
- A control-plane trust set verifies a grant bound to the exact device key,
  node/generation and approved capability subset. Another device,
  overprivileged grant or authorization longer than 24 hours is rejected.
- `VerifiedEdgeEnrollment` is not constructible or mutable by external callers;
  Edge boot and durable-store activation require the verifier output.
- Task schema 2 binds enrollment ID, capability-manifest digest and required
  capabilities. Another enrollment or an unapproved capability is rejected
  before Runtime execution.
- Durable state admits only a same-device, same-node, strictly newer generation
  after all receipts are terminal. Old generation evidence is preserved.
- One native integration path performs device identity → signed enrollment
  request → verified signed grant → signed task → real Embedded Runtime → local
  HTTP/SSE model → enrollment-bound Runtime events and terminal receipt →
  restart replay with one provider request.

## Observed RED to GREEN

- A grant with a lifetime one millisecond above 24 hours was accepted. The
  maximum offline authorization window is now 24 hours.
- Early fixtures could directly construct `VerifiedEdgeEnrollment` with fake
  keys and an infinite expiry. Verified fields are now crate-private and all
  integration fixtures obtain the type through real signature verification.
- A task signature alone authorized execution after enrollment replacement.
  Schema 2 and the active-enrollment verifier now require exact grant and
  approved-capability binding.
- Receipt completion could previously be presented under another enrollment.
  Reservation, event and terminal receipt commits now preserve one enrollment
  digest chain.

## Validation boundary

All acceptance is native and uses temporary files plus a loopback HTTP/SSE
provider. No Java, Docker, database, broker, VM or Kubernetes service is
required. This evidence does not prove mTLS transport, online revocation,
authenticated remote ACK, operator approval UI, hardware key protection,
remote task delivery or approval/cancel/resume orchestration.

Targeted results: `agent-edge-node` passed 21/21 tests and the protocol Edge
contract passed 5/5. Workspace `cargo check --all-targets`, formatting and
workspace Clippy with `-D warnings` passed. The workspace test gate did not
reach green: two parallel runs each failed only the pre-existing
`agent-tool-runtime` PTY test
`process_start_tty_allocates_a_real_terminal` with a supervisor-identity race;
that test passes in isolation. No Edge or protocol test failed. This unrelated
PTY concurrency defect is not silently counted as an Edge failure or fixed as
scope creep in ADR-0105.
