# Tree-wide subagent budget reservation evidence — 2026-08-10

## Behavioural proof

- RED: two completed async handles under a parent budget of 700 Tokens, 50
  cents and 30 seconds each received `400/30/20` on follow-up. The second
  handle had reused the first handle's reserved balance.
- GREEN: the first handle receives `400/30/20`; the second receives the exact
  unreserved `300/20/10`. Parent model admission is refused while those child
  reservations consume all three dimensions.
- Bound child settlement atomically replaces the maximum reservation with
  actual usage. In the recovery test, settling 100 actual Tokens on the first
  child while the second retains 300 exposes exactly 300 Tokens to the parent;
  after both settle, 600 remain.
- Closing a terminal handle marks its queued receipt cancelled and releases the
  queued reservation. Parent cancellation clears an active child reservation.

## Checkpoint and fencing proof

- Checkpoint schema 22 persists two entries while two handles are active.
- A replacement Worker restores the same zero-availability fence and releases
  the correct entry when an old bound result arrives.
- Removing one reservation and recomputing the outer Checkpoint digest is
  rejected because recovery independently derives the expected ledger from
  pending, active and queued work.
- Queue activation changes conversation binding without changing budget or
  allocation identity. Interrupt recovery, schema-14 migration and the full
  Worker assignment suite remain green.

## Real Host loop

A deterministic HTTP model uses the production OpenAI-compatible adapter and
standalone Host to:

1. create handle A and await its initial terminal result;
2. create handle B and await its initial terminal result;
3. issue two generation-bound `agent.send` calls plus waits in one Tool turn;
4. run both child model requests concurrently;
5. observe provider request ceilings of 400 and 300 Tokens;
6. settle both results, resume the parent and finish successfully;
7. write a terminal Checkpoint with an empty reservation ledger.

This is a real Runtime task loop and filesystem Checkpoint path. The model peer
is deterministic loopback HTTP, not a third-party provider quality test.

## Reference comparison

- Codex `ff352fab6209` shares actual weighted Token usage across the root Thread
  tree and has reminder delivery. This Runtime now adds pre-execution child cap
  reservation, cost/duration dimensions and digest-checked crash recovery, but
  lacks Codex's weighted policy and root Thread product integration.
- OpenClaw `58b4b9430457` remains ahead on mature subagent registry, global and
  per-agent concurrency, orphan cleanup and usage presentation. No equivalent
  tree-wide future-budget reservation was found in the inspected paths.
- The local result is a narrower multi-tenant safety invariant, not a claim of
  overall parity or superiority.

## Validation

- Worker tree-ledger, migration, tamper, close and cancellation test: passed.
- Worker assignment suite: 59 passed, 0 failed.
- Standalone Host real tree-budget loop: passed.
- Host subagent concurrency suite: 20 passed, 0 failed on the final serial run.
- Standalone Host suite: 22 passed, 0 failed.
- Full Rust workspace: 434 passed, 0 failed, 5 external live tests explicitly
  ignored; 439 total.
- `cargo check --workspace --all-targets`, Clippy over
  workspace/all-targets/all-features with `-D warnings`, Rust formatting, JSON,
  diff and residue gates passed.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used.
